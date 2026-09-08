//! GDI 采集:窗口/屏幕/显示器截图与像素采样。
//! 预算约束 + RAII 句柄守卫保证峰值内存有界、崩溃路径零句柄泄漏。

use windows::{
    core::{Error, BOOL},
    Win32::{
        Foundation::{HWND, LPARAM, RECT},
        Graphics::Gdi::{
            BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
            EnumDisplayMonitors, GetDC, GetDIBits, GetMonitorInfoW, ReleaseDC, SelectObject,
            StretchBlt, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC,
            HGDIOBJ, HMONITOR, MONITORINFO, SRCCOPY,
        },
        Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS},
        UI::WindowsAndMessaging::IsIconic,
    },
};

use crate::error::{
    ToolError, CAPTURE_FAILED, INTERNAL_ERROR, INVALID_ARGUMENT, LIMIT_EXCEEDED, WINDOW_HUNG,
    WINDOW_NOT_VISIBLE,
};

use super::window::{client_rect_of, is_window_hung, rect_of};

/// 单张窗口图的硬上限：保留原始 4K，同时约束 BGRA 与 RGBA 双缓冲的峰值内存。
pub const MAX_CAPTURE_DIM: i32 = 7680;
pub const MAX_WINDOW_CAPTURE_PIXELS: i64 = 12_000_000;
/// `screenshot_all` 的总预算：三台 UHD 4K（3840x2160）均不缩放。
/// 超过此预算时，所有显示器使用同一比例缩放，保持相对视觉布局一致。
pub const MAX_ALL_CAPTURE_PIXELS: i64 = 3 * 3840 * 2160;
/// PW_RENDERFULLCONTENT=2 — 捕获窗口自身渲染内容(含标题栏,可截被遮挡窗口)。
const PW_RENDERFULLCONTENT: PRINT_WINDOW_FLAGS = PRINT_WINDOW_FLAGS(2);
/// PrintWindow 的 PW_CLIENTONLY：只渲染客户区，不包含标题栏、边框或阴影。
const PW_CLIENTONLY: PRINT_WINDOW_FLAGS = PRINT_WINDOW_FLAGS(1);
/// 截图硬超时:超过则放弃等待并返回结构化错误,服务绝不被单个窗口卡死。
pub const CAPTURE_TIMEOUT_MS: u64 = 15_000;

/// 在独立线程执行采集并限时等待。超时后返回 `WINDOW_HUNG`,主循环继续服务。
///
/// 边界说明:Rust 无法安全终止线程,超时后 worker 可能仍被 PrintWindow 卡住
/// (线程与当次 GDI 句柄泄漏,进程退出时回收)。挂起预检(SMTO_ABORTIFHUNG 标题读取
/// + IsHungAppWindow)已把该概率压到趋近于零,剩余风险是驱动级卡死的极端场景。
pub fn run_capture_with_timeout<T: Send + 'static>(
    timeout_ms: u64,
    capture: impl FnOnce() -> Result<T, ToolError> + Send + 'static,
) -> Result<T, ToolError> {
    run_with_timeout(timeout_ms, capture, |ms| {
        ToolError::new(
            WINDOW_HUNG,
            format!("截图操作超过 {ms}ms 未完成，目标窗口可能无响应"),
        )
    })
}

/// 通用限时执行原语:独立线程运行 + 限时等待,截图与 UIA 查询共用。
/// 超时后 worker 无法被终止,可能继续阻塞并泄漏当次资源(进程退出回收),
/// 边界与 [`run_capture_with_timeout`] 相同。
pub fn run_with_timeout<T: Send + 'static>(
    timeout_ms: u64,
    op: impl FnOnce() -> Result<T, ToolError> + Send + 'static,
    timeout_error: impl FnOnce(u64) -> ToolError,
) -> Result<T, ToolError> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(op());
    });
    match rx.recv_timeout(std::time::Duration::from_millis(timeout_ms)) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(timeout_error(timeout_ms)),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err(ToolError::new(INTERNAL_ERROR, "执行线程异常退出"))
        }
    }
}

/// 所有位图分配前先以 i64 计算面积，避免 i32 乘法溢出造成小分配/越界访问。
pub fn checked_pixels(width: i32, height: i32) -> Result<i64, ToolError> {
    if width <= 0 || height <= 0 {
        return Err(ToolError::new(
            INVALID_ARGUMENT,
            format!("截图尺寸必须为正数，实际为 {width}x{height}"),
        ));
    }
    Ok(i64::from(width) * i64::from(height))
}

/// 在给定像素预算内计算输出尺寸；只缩小、不放大。
///
/// `additional_scale` 是多屏总预算得出的统一比例，保证超预算时各屏缩放一致。
pub fn fit_capture_dimensions(
    width: i32,
    height: i32,
    pixel_budget: i64,
    additional_scale: f64,
) -> Result<(i32, i32), ToolError> {
    let pixels = checked_pixels(width, height)?;
    if pixel_budget <= 0 || !additional_scale.is_finite() || additional_scale <= 0.0 {
        return Err(ToolError::new(INVALID_ARGUMENT, "无效的截图像素预算"));
    }
    let dimension_scale = (f64::from(MAX_CAPTURE_DIM) / f64::from(width.max(height))).min(1.0);
    let budget_scale = ((pixel_budget as f64 / pixels as f64).sqrt()).min(1.0);
    let scale = dimension_scale
        .min(budget_scale)
        .min(additional_scale)
        .min(1.0);
    if scale >= 1.0 {
        return Ok((width, height));
    }
    Ok((
        (f64::from(width) * scale).floor().max(1.0) as i32,
        (f64::from(height) * scale).floor().max(1.0) as i32,
    ))
}

/// GDI 句柄 RAII 守卫:正常错误路径与 panic 展开路径都保证释放,
/// 配合协议层 catch_unwind 隔离使采集函数在崩溃时也不泄漏 GDI 句柄。
/// 释放方式必须与获取方式配对:GetDC(None) 的屏幕 DC 用 ReleaseDC,
/// CreateCompatibleDC 的内存 DC 用 DeleteDC,位图用 DeleteObject。
/// Drop 顺序为声明逆序(内存 DC → 位图 → 屏幕 DC):DeleteDC 不删除选入对象,
/// 随后 DeleteObject 在该 DC 已不存在时仍能成功释放位图。
enum GdiHandle {
    ScreenDc(HDC),
    MemDc(HDC),
    Bitmap(HBITMAP),
}

impl Drop for GdiHandle {
    fn drop(&mut self) {
        unsafe {
            match self {
                GdiHandle::ScreenDc(hdc) => {
                    let _ = ReleaseDC(None, *hdc);
                }
                GdiHandle::MemDc(hdc) => {
                    let _ = DeleteDC(*hdc);
                }
                GdiHandle::Bitmap(hbm) => {
                    let _ = DeleteObject(HGDIOBJ(hbm.0));
                }
            }
        }
    }
}

/// 窗口截图结果:PNG 数据、屏幕矩形与来源标注。
/// `fallback=Some(...)` 表示 PrintWindow 路径未产出可用图像,实际来源改为
/// 其他路径:`wgc (...)` = WGC 合成帧,`screen (...)` = 屏幕采样(窗口矩形
/// 区域在屏幕上的当前显示,遮挡与阴影按屏幕现状)。
/// `incremental=Some(...)` 表示增量差分调用(WGC 会话缓存),语义见
/// [`IncrementalInfo`];`reused=true` 表示窗口静态无新帧,复用上次帧内容
/// (与重新采集语义等价)。
pub struct WindowCapture {
    pub image: Vec<u8>,
    pub mime: &'static str,
    /// 实际输出像素尺寸(编码前经 max_dim 缩放,与 `rect` 尺寸可能不同)。
    pub image_dims: (u32, u32),
    pub rect: RECT,
    pub fallback: Option<String>,
    pub incremental: Option<IncrementalInfo>,
    pub reused: bool,
}

/// 增量差分信息(incremental 调用时 Some):
/// - `baseline=true`:本帧为基线(首次采集或基线重建),`png` 为全帧;
/// - `regions` 空且 `baseline=false`:与上次采集无变化,`png` 为空;
/// - 其余:`regions` 为变化区域(帧坐标),各带裁剪后的 PNG。
pub struct IncrementalInfo {
    pub baseline: bool,
    pub regions: Vec<DirtyRegion>,
}

/// 一个变化区域:帧坐标 (x, y, w, h) 与裁剪后的 PNG(仅含该区域内容)。
/// 增量差分固定 PNG:差分区域的像素保真优先,压缩收益次要。
pub struct DirtyRegion {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub png: Vec<u8>,
}

/// 输出编码与降采样选项(多模型接入):
/// - `jpeg_quality=Some(q)` = JPEG(4:4:4 无下采样,彩色文字边缘可读),
///   体积比 PNG 小 60-90%,用于全局语义观察与网络上行;
/// - `None` = PNG(无损,像素断言保真,默认);
/// - `max_dim` = 输出最长边上限(物理像素),超过按比例双线性缩小,
///   0 不限制——视觉模型 token 按分辨率计费,降采样才是省 token 的主杠杆。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureFormat {
    pub jpeg_quality: Option<u8>,
    pub max_dim: u32,
}

impl CaptureFormat {
    pub fn png() -> Self {
        Self {
            jpeg_quality: None,
            max_dim: 0,
        }
    }

    pub fn mime(self) -> &'static str {
        match self.jpeg_quality {
            Some(_) => "image/jpeg",
            None => "image/png",
        }
    }
}

/// 编码产物:(字节, MIME, 实际输出尺寸)。
pub(crate) type EncodedImage = (Vec<u8>, &'static str, (u32, u32));

/// max_dim 缩放到输出尺寸(只缩小不放大)。纯函数,便于单测。
pub fn apply_max_dim(width: i32, height: i32, max_dim: u32) -> (i32, i32) {
    if max_dim > 0 && width.max(height) as u32 > max_dim {
        let scale = f64::from(max_dim) / f64::from(width.max(height));
        (
            (f64::from(width) * scale).floor().max(1.0) as i32,
            (f64::from(height) * scale).floor().max(1.0) as i32,
        )
    } else {
        (width, height)
    }
}

/// BGRA → 目标格式编码:max_dim 缩放(等大直通)→ RGBA→PNG 或 RGB→JPEG。
/// 返回 (字节, MIME, 实际输出尺寸)。PrintWindow/WGC 原尺寸路径在此缩放,
/// 屏幕路径在 StretchBlt 阶段已缩到预算内,此处等大直通。
pub(crate) fn encode_frame(
    bgra: &[u8],
    width: u32,
    height: u32,
    format: CaptureFormat,
) -> Result<EncodedImage, ToolError> {
    let (out_w, out_h, frame) = if format.max_dim > 0 && width.max(height) > format.max_dim {
        let (w, h) = apply_max_dim(width as i32, height as i32, format.max_dim);
        (
            w as u32,
            h as u32,
            resize_bgra(bgra, width, height, w as u32, h as u32)?,
        )
    } else {
        (width, height, bgra.to_vec())
    };
    match format.jpeg_quality {
        Some(q) => Ok((
            encode_jpeg(out_w, out_h, &frame, q)?,
            "image/jpeg",
            (out_w, out_h),
        )),
        None => Ok((
            bgra_to_rgba_png(out_w as i32, out_h as i32, &frame)?,
            "image/png",
            (out_w, out_h),
        )),
    }
}

/// BGRA → JPEG。强制 4:4:4 采样(F_1_1),避免色度下采样糊掉彩色文字边缘;
/// quality 1-100。jpeg-encoder 纯 Rust 实现,无原生依赖。
fn encode_jpeg(width: u32, height: u32, bgra: &[u8], quality: u8) -> Result<Vec<u8>, ToolError> {
    use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
    let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
    for px in bgra.chunks_exact(4) {
        rgb.extend_from_slice(&[px[2], px[1], px[0]]); // BGRA → RGB
    }
    let mut out = Vec::new();
    {
        let mut encoder = Encoder::new(&mut out, quality);
        encoder.set_sampling_factor(SamplingFactor::F_1_1);
        encoder
            .encode(&rgb, width as u16, height as u16, ColorType::Rgb)
            .map_err(|e| ToolError::new(CAPTURE_FAILED, format!("JPEG 编码失败: {e}")))?;
    }
    Ok(out)
}

pub fn capture_window(
    hwnd: HWND,
    client_only: bool,
    format: CaptureFormat,
) -> Result<WindowCapture, ToolError> {
    // 挂起预检在主线程完成(快速内核查询),挂起窗口直接给结构化错误,
    // 绝不进入 PrintWindow——那是把 WM_PRINT 投递进目标线程的阻塞调用。
    if is_window_hung(hwnd) {
        return Err(ToolError::new(
            WINDOW_HUNG,
            format!(
                "窗口 hwnd={} 无响应，拒绝截图；等待窗口恢复或选择其他目标",
                hwnd_num_str(hwnd)
            ),
        ));
    }
    // HWND 是裸指针包装,不满足 Send;以 isize 跨界传递,worker 内重建。
    let raw = hwnd.0 as isize;
    run_capture_with_timeout(CAPTURE_TIMEOUT_MS, move || {
        capture_window_inner(HWND(raw as *mut core::ffi::c_void), client_only, format)
    })
}

fn hwnd_num_str(hwnd: HWND) -> String {
    (hwnd.0 as isize).to_string()
}

fn capture_window_inner(
    hwnd: HWND,
    client_only: bool,
    format: CaptureFormat,
) -> Result<WindowCapture, ToolError> {
    // 最小化窗口确定性拒绝:GetWindowRect 对最小化窗口返回 (-32000,-32000)
    // 位置但尺寸仍为正,尺寸预检拦不住;WGC 缓存会话还会复用最小化前的旧帧。
    if unsafe { IsIconic(hwnd) }.as_bool() {
        return Err(ToolError::new(
            WINDOW_NOT_VISIBLE,
            format!(
                "窗口 hwnd={} 已最小化，拒绝截图；恢复窗口后重试",
                hwnd_num_str(hwnd)
            ),
        ));
    }
    let rect = if client_only {
        client_rect_of(hwnd)?
    } else {
        rect_of(hwnd)?
    };
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width <= 0 || height <= 0 {
        return Err(ToolError::new(
            WINDOW_NOT_VISIBLE,
            format!(
                "窗口矩形为空 ({}x{}),可能已最小化(最小化窗口坐标为 -32000)",
                width, height
            ),
        ));
    }
    // PrintWindow 必须以原窗口尺寸绘制，不能像屏幕 BitBlt 那样在采集阶段缩放；
    // 所以异常大的窗口明确拒绝，避免先分配巨型原始缓冲再缩小。
    let pixels = checked_pixels(width, height)?;
    if width > MAX_CAPTURE_DIM || height > MAX_CAPTURE_DIM || pixels > MAX_WINDOW_CAPTURE_PIXELS {
        return Err(ToolError::new(
            LIMIT_EXCEEDED,
            format!(
                "窗口尺寸 {}x{} 超过截图上限（单边 {} px、总计 {} 像素）；请先用 resize_window 缩小窗口",
                width, height, MAX_CAPTURE_DIM, MAX_WINDOW_CAPTURE_PIXELS
            ),
        ));
    }

    let hdc_screen = unsafe { GetDC(None) };
    if hdc_screen.0.is_null() {
        return Err(ToolError::new(CAPTURE_FAILED, "GetDC 未返回屏幕设备上下文"));
    }
    let _screen_dc = GdiHandle::ScreenDc(hdc_screen);
    let hbm = unsafe { CreateCompatibleBitmap(hdc_screen, width, height) };
    if hbm.0.is_null() {
        return Err(ToolError::new(
            CAPTURE_FAILED,
            "CreateCompatibleBitmap 未能分配窗口截图位图",
        ));
    }
    let _bitmap = GdiHandle::Bitmap(hbm);
    let hdc_mem = unsafe { CreateCompatibleDC(Some(hdc_screen)) };
    if hdc_mem.0.is_null() {
        return Err(ToolError::new(
            CAPTURE_FAILED,
            "CreateCompatibleDC 未能创建窗口截图设备上下文",
        ));
    }
    let _mem_dc = GdiHandle::MemDc(hdc_mem);

    let flags = if client_only {
        PW_CLIENTONLY
    } else {
        PW_RENDERFULLCONTENT
    };
    match print_window_bgra(hwnd, hdc_mem, hbm, width, height, flags) {
        Ok(bgra) if !bgra_looks_black(&bgra) => {
            let (image, mime, image_dims) =
                encode_frame(&bgra, width as u32, height as u32, format)?;
            Ok(WindowCapture {
                image,
                mime,
                image_dims,
                rect,
                fallback: None,
                incremental: None,
                reused: false,
            })
        }
        printed => {
            // GPU 加速窗口的 PrintWindow 常产出近纯黑,或直接失败:auto 链
            // 先试 WGC(DWM 合成帧,免疫黑屏与被遮挡;会话缓存让非白名单
            // 路径的重复采集也只需 ~10-30ms),WGC 不可用再回退屏幕采样
            // (与像素扫描工具同源)。WGC 失败只影响来源标注,不改变错误契约。
            let print_reason = if printed.is_ok() {
                "PrintWindow 近纯黑"
            } else {
                "PrintWindow 失败"
            };
            match crate::backend::wgc::capture_window_wgc_cached(
                hwnd,
                client_only,
                crate::backend::wgc::CaptureMode::Full(format),
            ) {
                Ok(wgc) => {
                    crate::log_info!("capture: {print_reason},WGC 产出 {} bytes", wgc.image.len());
                    Ok(WindowCapture {
                        image: wgc.image,
                        mime: wgc.mime,
                        image_dims: wgc.image_dims,
                        rect,
                        fallback: Some(wgc_fallback_reason(print_reason)),
                        incremental: None,
                        reused: wgc.reused,
                    })
                }
                Err(wgc_error) => {
                    let (image, mime, image_dims) = capture_rect_from_screen(
                        hdc_screen,
                        ScreenCaptureSpec {
                            source_width: width,
                            source_height: height,
                            output_width: width,
                            output_height: height,
                            left: rect.left,
                            top: rect.top,
                        },
                        format,
                    )?;
                    Ok(WindowCapture {
                        image,
                        mime,
                        image_dims,
                        rect,
                        fallback: Some(screen_fallback_reason(print_reason, &wgc_error)),
                        incremental: None,
                        reused: false,
                    })
                }
            }
        }
    }
}

/// auto 链经 WGC 成功的来源标注。纯函数,便于单测。
fn wgc_fallback_reason(print_reason: &str) -> String {
    format!("wgc ({print_reason})")
}

/// auto 链 WGC 失败、屏幕采样兜底的来源标注(携带 WGC 失败原因供诊断)。
/// 纯函数,便于单测。
fn screen_fallback_reason(print_reason: &str, wgc_error: &ToolError) -> String {
    format!("screen ({print_reason}; WGC 不可用: {wgc_error})")
}

/// PrintWindow → 32bpp DIB,返回自上而下的原始 BGRA(由调用方检测黑屏后再编码)。
fn print_window_bgra(
    hwnd: HWND,
    hdc_mem: HDC,
    hbm: HBITMAP,
    width: i32,
    height: i32,
    flags: PRINT_WINDOW_FLAGS,
) -> Result<Vec<u8>, ToolError> {
    let old = unsafe { SelectObject(hdc_mem, HGDIOBJ(hbm.0)) };
    let printed = unsafe { PrintWindow(hwnd, hdc_mem, flags) }.as_bool();
    let _ = unsafe { SelectObject(hdc_mem, old) };

    if !printed {
        return Err(ToolError::new(
            CAPTURE_FAILED,
            format!("PrintWindow 失败(flags=0x{:X})", flags.0),
        ));
    }

    let mut bmi = unsafe { std::mem::zeroed::<BITMAPINFO>() };
    bmi.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        biHeight: -height, // 自上而下
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let mut bgra = vec![0u8; (width * height * 4) as usize];
    let lines = unsafe {
        GetDIBits(
            hdc_mem,
            hbm,
            0,
            height as u32,
            Some(bgra.as_mut_ptr() as *mut core::ffi::c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        )
    };
    if lines <= 0 {
        return Err(ToolError::new(CAPTURE_FAILED, "GetDIBits 未返回任何位图行"));
    }
    Ok(bgra)
}

/// 近纯黑检测:PrintWindow 对 GPU 加速窗口常产出全黑/透明帧。
/// 亮度 < BLACK_PIXEL_THRESHOLD 的像素占比超过 BLACK_RATIO 即判定为不可用。
/// 注意:兼容位图经 GetDIBits 转 32bpp 后 alpha 通道不可靠,只按 RGB 判定。
const BLACK_PIXEL_THRESHOLD: u8 = 8;
const BLACK_RATIO: f64 = 0.98;

fn bgra_looks_black(bgra: &[u8]) -> bool {
    if bgra.len() < 4 {
        return true;
    }
    let mut dark = 0u64;
    for px in bgra.chunks_exact(4) {
        if px[2] < BLACK_PIXEL_THRESHOLD
            && px[1] < BLACK_PIXEL_THRESHOLD
            && px[0] < BLACK_PIXEL_THRESHOLD
        {
            dark += 1;
        }
    }
    dark as f64 / (bgra.len() / 4) as f64 > BLACK_RATIO
}

/// BGRA → RGBA → PNG(窗口/屏幕/WGC 三条采集路径共用)。
pub(crate) fn bgra_to_rgba_png(width: i32, height: i32, bgra: &[u8]) -> Result<Vec<u8>, ToolError> {
    let mut rgba = Vec::with_capacity(bgra.len());
    for px in bgra.chunks_exact(4) {
        rgba.extend_from_slice(&[px[2], px[1], px[0], 0xFF]); // BGRA → RGBA
    }
    encode_png(width as u32, height as u32, &rgba)
}

struct MonitorRecord {
    hmon: HMONITOR,
    rect: RECT,
    flags: u32,
}

/// 单台显示器的采集结果:tools 层读取元数据组装 MCP 响应,单台失败不中断其余。
pub struct MonitorShot {
    pub label: String,
    /// 实际来源:screen(BitBlt) / wgc / ddapi / bitblt(DDAPI 失败回退)。
    pub source: &'static str,
    pub source_width: i32,
    pub source_height: i32,
    pub output_width: i32,
    pub output_height: i32,
    pub image: Vec<u8>,
    pub mime: &'static str,
    pub error: Option<ToolError>,
}

unsafe extern "system" fn monitor_proc(
    hmon: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let out = unsafe { &mut *(lparam.0 as *mut Vec<MonitorRecord>) };
    let mut info = unsafe { std::mem::zeroed::<MONITORINFO>() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if unsafe { GetMonitorInfoW(hmon, &mut info) }.as_bool() {
        out.push(MonitorRecord {
            hmon,
            rect: info.rcMonitor,
            flags: info.dwFlags,
        });
    }
    BOOL(1)
}

fn enum_monitors() -> Vec<MonitorRecord> {
    let mut list: Vec<MonitorRecord> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(monitor_proc),
            LPARAM((&mut list as *mut Vec<MonitorRecord>) as isize),
        );
    }
    list
}

/// 截取所有显示器,返回每台一张图(带标签、尺寸与编码元数据,单台失败
/// 不中断其余)。`method`: `auto` = 屏幕 BitBlt(默认,不含光标);
/// `wgc` = Windows Graphics Capture 合成帧(实验,含光标;会话断开/锁定等
/// 无合成场景单屏报错);`ddapi` = DXGI Desktop Duplication 显存复制
/// (实验,显示器级、不含光标,单屏不可用时回退 BitBlt 并在 source 标注)。
/// `format` 控制编码与 max_dim 降采样。整体限时执行:GPU 驱动级卡死也不
/// 阻塞服务。
pub fn capture_all_monitors(
    method: &str,
    format: CaptureFormat,
) -> Result<Vec<MonitorShot>, ToolError> {
    match method {
        "auto" => run_capture_with_timeout(CAPTURE_TIMEOUT_MS, move || {
            capture_all_monitors_inner(format)
        }),
        "wgc" => run_capture_with_timeout(CAPTURE_TIMEOUT_MS, move || {
            capture_all_monitors_wgc_inner(format)
        }),
        "ddapi" => run_capture_with_timeout(CAPTURE_TIMEOUT_MS, move || {
            capture_all_monitors_ddapi_inner(format)
        }),
        other => Err(ToolError::new(
            INVALID_ARGUMENT,
            format!("未知采集方式 '{other}'，支持 auto / wgc / ddapi"),
        )),
    }
}

/// 总预算计算:BItBlt 与 WGC 两条路径共用(三台 UHD 4K 及以下保持 1.0)。
fn monitors_total_scale(monitors: &[MonitorRecord]) -> Result<f64, ToolError> {
    let total_pixels = monitors.iter().try_fold(0i64, |total, monitor| {
        let width = monitor.rect.right - monitor.rect.left;
        let height = monitor.rect.bottom - monitor.rect.top;
        Ok::<_, ToolError>(total + checked_pixels(width, height)?)
    })?;
    Ok(if total_pixels > MAX_ALL_CAPTURE_PIXELS {
        (MAX_ALL_CAPTURE_PIXELS as f64 / total_pixels as f64).sqrt()
    } else {
        1.0
    })
}

fn monitor_label(index: usize, m: &MonitorRecord) -> String {
    let width = m.rect.right - m.rect.left;
    let height = m.rect.bottom - m.rect.top;
    format!(
        "monitor {} ({}x{} at {},{} as primary={})",
        index + 1,
        width,
        height,
        m.rect.left,
        m.rect.top,
        m.flags != 0
    )
}

/// WGC 显示器路径:每台显示器一个一次性捕获会话(不落缓存),首帧预算 3s,
/// 帧含光标。超过预算时用与 BitBlt 路径相同的统一比例 CPU 缩放,
/// max_dim 在预算缩放后再套一层。
fn capture_all_monitors_wgc_inner(format: CaptureFormat) -> Result<Vec<MonitorShot>, ToolError> {
    let monitors = enum_monitors();
    if monitors.is_empty() {
        return Err(ToolError::new(CAPTURE_FAILED, "未找到任何显示器"));
    }
    let total_scale = monitors_total_scale(&monitors)?;
    let mut results = Vec::new();
    for (index, m) in monitors.iter().enumerate() {
        let width = m.rect.right - m.rect.left;
        let height = m.rect.bottom - m.rect.top;
        let label = monitor_label(index, m);
        let (fitted_w, fitted_h) =
            fit_capture_dimensions(width, height, MAX_WINDOW_CAPTURE_PIXELS, total_scale)?;
        let (output_width, output_height) = apply_max_dim(fitted_w, fitted_h, format.max_dim);
        match crate::backend::wgc::capture_monitor_wgc(m.hmon) {
            Ok((bgra, fw, fh)) => {
                // 帧尺寸与显示器矩形物理像素一致;不一致时以帧为准缩放
                // 到目标尺寸(等大时 resize 原样返回)。
                let scaled = resize_bgra(&bgra, fw, fh, output_width as u32, output_height as u32)?;
                let (image, mime, _) = encode_frame(
                    &scaled,
                    output_width as u32,
                    output_height as u32,
                    CaptureFormat {
                        max_dim: 0,
                        ..format
                    },
                )?;
                results.push(MonitorShot {
                    label,
                    source: "wgc",
                    source_width: width,
                    source_height: height,
                    output_width,
                    output_height,
                    image,
                    mime,
                    error: None,
                });
            }
            Err(error) => results.push(MonitorShot {
                label,
                source: "wgc",
                source_width: width,
                source_height: height,
                output_width,
                output_height,
                image: Vec::new(),
                mime: format.mime(),
                error: Some(error),
            }),
        }
    }
    Ok(results)
}

/// DDAPI 显示器路径:每台显示器经 DXGI Desktop Duplication 取显存帧
/// (首帧预算 500ms,静态桌面复用会话缓存帧),帧不含光标。单屏不可用
/// (锁定/安全桌面/驱动异常)时回退屏幕 BitBlt,来源在 source 标注。
/// 超预算缩放与 max_dim 语义与其余路径一致。
fn capture_all_monitors_ddapi_inner(format: CaptureFormat) -> Result<Vec<MonitorShot>, ToolError> {
    let monitors = enum_monitors();
    if monitors.is_empty() {
        return Err(ToolError::new(CAPTURE_FAILED, "未找到任何显示器"));
    }
    let total_scale = monitors_total_scale(&monitors)?;
    // 回退路径的屏幕 DC 每屏共用一份(失败屏才实际使用)。
    let hdc_screen = unsafe { GetDC(None) };
    if hdc_screen.0.is_null() {
        return Err(ToolError::new(CAPTURE_FAILED, "GetDC 未返回屏幕设备上下文"));
    }
    let _screen_dc = GdiHandle::ScreenDc(hdc_screen);

    let mut results = Vec::new();
    for (index, m) in monitors.iter().enumerate() {
        let width = m.rect.right - m.rect.left;
        let height = m.rect.bottom - m.rect.top;
        let label = monitor_label(index, m);
        let (fitted_w, fitted_h) =
            fit_capture_dimensions(width, height, MAX_WINDOW_CAPTURE_PIXELS, total_scale)?;
        let (output_width, output_height) = apply_max_dim(fitted_w, fitted_h, format.max_dim);
        match crate::backend::ddapi::capture_frame_for_rect(m.rect) {
            Ok(frame) => {
                let bgra = frame.to_bgra();
                let scaled = resize_bgra(
                    &bgra,
                    frame.width(),
                    frame.height(),
                    output_width as u32,
                    output_height as u32,
                )?;
                match encode_frame(
                    &scaled,
                    output_width as u32,
                    output_height as u32,
                    CaptureFormat {
                        max_dim: 0,
                        ..format
                    },
                ) {
                    Ok((image, mime, _)) => results.push(MonitorShot {
                        label,
                        source: "ddapi",
                        source_width: width,
                        source_height: height,
                        output_width,
                        output_height,
                        image,
                        mime,
                        error: None,
                    }),
                    Err(error) => results.push(MonitorShot {
                        label,
                        source: "ddapi",
                        source_width: width,
                        source_height: height,
                        output_width,
                        output_height,
                        image: Vec::new(),
                        mime: format.mime(),
                        error: Some(error),
                    }),
                }
            }
            Err(ddapi_error) => {
                // DDAPI 不可用(锁定/安全桌面/驱动异常/静态桌面无帧)→
                // 逐屏回退 BitBlt,来源标注携带原因,单屏失败不中断其余。
                crate::log_debug!("ddapi: 显示器回退 BitBlt: {ddapi_error}");
                match capture_rect_from_screen(
                    hdc_screen,
                    ScreenCaptureSpec {
                        source_width: width,
                        source_height: height,
                        output_width,
                        output_height,
                        left: m.rect.left,
                        top: m.rect.top,
                    },
                    format,
                ) {
                    Ok((image, mime, _)) => results.push(MonitorShot {
                        label: format!("{label} [DDAPI 不可用,已回退: {ddapi_error}]"),
                        source: "bitblt",
                        source_width: width,
                        source_height: height,
                        output_width,
                        output_height,
                        image,
                        mime,
                        error: None,
                    }),
                    Err(error) => results.push(MonitorShot {
                        label,
                        source: "bitblt",
                        source_width: width,
                        source_height: height,
                        output_width,
                        output_height,
                        image: Vec::new(),
                        mime: format.mime(),
                        error: Some(error),
                    }),
                }
            }
        }
    }
    Ok(results)
}

/// BGRA 双线性缩放(下采样为主):WGC 显示器帧超过总预算时按统一比例缩小,
/// 与 BitBlt 路径的 StretchBlt 语义对齐(平滑采样)。等大时原样返回。
pub fn resize_bgra(
    bgra: &[u8],
    w: u32,
    h: u32,
    out_w: u32,
    out_h: u32,
) -> Result<Vec<u8>, ToolError> {
    if w == 0 || h == 0 {
        return Err(ToolError::new(INVALID_ARGUMENT, "缩放源尺寸必须为正数"));
    }
    checked_pixels(out_w as i32, out_h as i32)?;
    if (w, h) == (out_w, out_h) {
        return Ok(bgra.to_vec());
    }
    let mut out = vec![0u8; (out_w * out_h * 4) as usize];
    let sx = w as f64 / out_w as f64;
    let sy = h as f64 / out_h as f64;
    for oy in 0..out_h {
        for ox in 0..out_w {
            // 目标像素中心映射回源坐标(半像素偏移),双线性插值。
            let fx = (ox as f64 + 0.5) * sx - 0.5;
            let fy = (oy as f64 + 0.5) * sy - 0.5;
            let x0 = fx.floor().max(0.0) as u32;
            let y0 = fy.floor().max(0.0) as u32;
            let x1 = (x0 + 1).min(w - 1);
            let y1 = (y0 + 1).min(h - 1);
            let dx = (fx - x0 as f64).clamp(0.0, 1.0);
            let dy = (fy - y0 as f64).clamp(0.0, 1.0);
            let px = |x: u32, y: u32, c: usize| bgra[((y * w + x) * 4) as usize + c] as f64;
            let o = ((oy * out_w + ox) * 4) as usize;
            for c in 0..4 {
                let p00 = px(x0, y0, c);
                let p10 = px(x1, y0, c);
                let p01 = px(x0, y1, c);
                let p11 = px(x1, y1, c);
                let top = p00 + (p10 - p00) * dx;
                let bottom = p01 + (p11 - p01) * dx;
                out[o + c] = (top + (bottom - top) * dy).round() as u8;
            }
        }
    }
    Ok(out)
}

fn capture_all_monitors_inner(format: CaptureFormat) -> Result<Vec<MonitorShot>, ToolError> {
    let monitors = enum_monitors();
    if monitors.is_empty() {
        return Err(ToolError::new(CAPTURE_FAILED, "未找到任何显示器"));
    }
    // 三台 UHD 4K 及以下保持 1.0；更多/更大的屏幕统一缩放，避免偏屏缩放。
    let total_scale = monitors_total_scale(&monitors)?;
    let hdc_screen = unsafe { GetDC(None) };
    if hdc_screen.0.is_null() {
        return Err(ToolError::new(CAPTURE_FAILED, "GetDC 未返回屏幕设备上下文"));
    }
    let _screen_dc = GdiHandle::ScreenDc(hdc_screen);

    let mut results = Vec::new();
    for (index, m) in monitors.iter().enumerate() {
        let width = m.rect.right - m.rect.left;
        let height = m.rect.bottom - m.rect.top;
        let label = monitor_label(index, m);
        // 对每台再套单图预算；这里会在 StretchBlt 期间直接生成缩小位图，
        // 不会为超大原图额外分配一个完整的像素缓冲。max_dim 再套一层。
        let (fitted_w, fitted_h) =
            fit_capture_dimensions(width, height, MAX_WINDOW_CAPTURE_PIXELS, total_scale)?;
        let (output_width, output_height) = apply_max_dim(fitted_w, fitted_h, format.max_dim);
        match capture_rect_from_screen(
            hdc_screen,
            ScreenCaptureSpec {
                source_width: width,
                source_height: height,
                output_width,
                output_height,
                left: m.rect.left,
                top: m.rect.top,
            },
            format,
        ) {
            Ok((image, mime, _)) => results.push(MonitorShot {
                label,
                source: "screen",
                source_width: width,
                source_height: height,
                output_width,
                output_height,
                image,
                mime,
                error: None,
            }),
            Err(error) => results.push(MonitorShot {
                label,
                source: "screen",
                source_width: width,
                source_height: height,
                output_width,
                output_height,
                image: Vec::new(),
                mime: format.mime(),
                error: Some(error),
            }),
        }
    }
    Ok(results)
}

/// 屏幕区域采集参数:来源矩形与目标输出尺寸(预算缩放后的最终值)。
struct ScreenCaptureSpec {
    source_width: i32,
    source_height: i32,
    output_width: i32,
    output_height: i32,
    left: i32,
    top: i32,
}

fn capture_rect_from_screen(
    hdc_screen: HDC,
    spec: ScreenCaptureSpec,
    format: CaptureFormat,
) -> Result<EncodedImage, ToolError> {
    let ScreenCaptureSpec {
        source_width,
        source_height,
        output_width,
        output_height,
        left,
        top,
    } = spec;
    // source_* 是真实桌面区域，output_* 是已按预算计算的目标位图尺寸。
    // 只有尺寸不同才走 StretchBlt；正常三台 4K 情况仍是无缩放的 BitBlt。
    checked_pixels(source_width, source_height)?;
    let output_pixels = checked_pixels(output_width, output_height)?;
    if output_width > MAX_CAPTURE_DIM
        || output_height > MAX_CAPTURE_DIM
        || output_pixels > MAX_WINDOW_CAPTURE_PIXELS
    {
        return Err(ToolError::new(
            LIMIT_EXCEEDED,
            format!(
                "显示器输出尺寸 {}x{} 超出截图限制",
                output_width, output_height
            ),
        ));
    }
    let hbm = unsafe { CreateCompatibleBitmap(hdc_screen, output_width, output_height) };
    if hbm.0.is_null() {
        return Err(ToolError::new(
            CAPTURE_FAILED,
            "CreateCompatibleBitmap 未能分配屏幕截图位图",
        ));
    }
    let _bitmap = GdiHandle::Bitmap(hbm);
    let hdc_mem = unsafe { CreateCompatibleDC(Some(hdc_screen)) };
    if hdc_mem.0.is_null() {
        return Err(ToolError::new(
            CAPTURE_FAILED,
            "CreateCompatibleDC 未能创建屏幕截图设备上下文",
        ));
    }
    let _mem_dc = GdiHandle::MemDc(hdc_mem);
    unsafe {
        let _ = SelectObject(hdc_mem, HGDIOBJ(hbm.0));
    }

    let mut bmi = unsafe { std::mem::zeroed::<BITMAPINFO>() };
    bmi.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: output_width,
        biHeight: -output_height,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let mut bgra = vec![0u8; (output_pixels * 4) as usize];
    let copied = if source_width == output_width && source_height == output_height {
        unsafe {
            BitBlt(
                hdc_mem,
                0,
                0,
                output_width,
                output_height,
                Some(hdc_screen),
                left,
                top,
                SRCCOPY,
            )
        }
    } else {
        if unsafe {
            StretchBlt(
                hdc_mem,
                0,
                0,
                output_width,
                output_height,
                Some(hdc_screen),
                left,
                top,
                source_width,
                source_height,
                SRCCOPY,
            )
        }
        .as_bool()
        {
            Ok(())
        } else {
            Err(Error::from_win32())
        }
    };
    if let Err(e) = copied {
        Err(ToolError::new(
            CAPTURE_FAILED,
            format!("屏幕像素复制失败: {e}"),
        ))
    } else {
        let lines = unsafe {
            GetDIBits(
                hdc_mem,
                hbm,
                0,
                output_height as u32,
                Some(bgra.as_mut_ptr() as *mut core::ffi::c_void),
                &mut bmi,
                DIB_RGB_COLORS,
            )
        };
        if lines <= 0 {
            Err(ToolError::new(CAPTURE_FAILED, "GetDIBits 未返回任何位图行"))
        } else {
            encode_frame(&bgra, output_width as u32, output_height as u32, format)
        }
    }
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, ToolError> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| ToolError::new(CAPTURE_FAILED, format!("PNG 编码失败: {e}")))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| ToolError::new(CAPTURE_FAILED, format!("PNG 写图像失败: {e}")))?;
    }
    Ok(out)
}

/// 从屏幕采样窗口客户区内的一个矩形区域，返回 BGRA 像素（从上到下）。
/// 坐标均为相对客户区的物理像素；被遮挡/最小化区域按屏幕当前显示内容采样。
pub fn capture_client_region_bgra(
    hwnd: HWND,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Result<Vec<u8>, ToolError> {
    if width <= 0 || height <= 0 {
        return Err(ToolError::new(
            INVALID_ARGUMENT,
            format!("扫描区域尺寸必须为正数，实际为 {width}x{height}"),
        ));
    }
    let client = client_rect_of(hwnd)?;
    let client_width = client.right - client.left;
    let client_height = client.bottom - client.top;
    let region_right = i64::from(x) + i64::from(width);
    let region_bottom = i64::from(y) + i64::from(height);
    if x < 0
        || y < 0
        || region_right > i64::from(client_width)
        || region_bottom > i64::from(client_height)
    {
        return Err(ToolError::new(
            INVALID_ARGUMENT,
            format!(
                "扫描区域 ({x},{y}) {width}x{height} 超出窗口客户区 {client_width}x{client_height}"
            ),
        ));
    }
    capture_screen_rect_bgra(client.left + x, client.top + y, width, height)
}

/// 从屏幕 BitBlt 采样虚拟桌面上的一个矩形区域，返回原始 BGRA(像素工具与
/// DDAPI 回退共用)。左上角为屏幕物理坐标;被遮挡区域按屏幕当前显示内容。
pub(crate) fn capture_screen_rect_bgra(
    left: i32,
    top: i32,
    width: i32,
    height: i32,
) -> Result<Vec<u8>, ToolError> {
    if width <= 0 || height <= 0 {
        return Err(ToolError::new(
            INVALID_ARGUMENT,
            format!("采样区域尺寸必须为正数，实际为 {width}x{height}"),
        ));
    }
    let pixels = checked_pixels(width, height)?;
    if pixels > MAX_WINDOW_CAPTURE_PIXELS {
        return Err(ToolError::new(
            LIMIT_EXCEEDED,
            format!(
                "采样区域 {width}x{height} 超过上限（{} 像素）",
                MAX_WINDOW_CAPTURE_PIXELS
            ),
        ));
    }

    let hdc_screen = unsafe { GetDC(None) };
    if hdc_screen.0.is_null() {
        return Err(ToolError::new(CAPTURE_FAILED, "GetDC 未返回屏幕设备上下文"));
    }
    let _screen_dc = GdiHandle::ScreenDc(hdc_screen);
    let hbm = unsafe { CreateCompatibleBitmap(hdc_screen, width, height) };
    if hbm.0.is_null() {
        return Err(ToolError::new(
            CAPTURE_FAILED,
            "CreateCompatibleBitmap 未能分配扫描位图",
        ));
    }
    let _bitmap = GdiHandle::Bitmap(hbm);
    let hdc_mem = unsafe { CreateCompatibleDC(Some(hdc_screen)) };
    if hdc_mem.0.is_null() {
        return Err(ToolError::new(
            CAPTURE_FAILED,
            "CreateCompatibleDC 未能创建扫描设备上下文",
        ));
    }
    let _mem_dc = GdiHandle::MemDc(hdc_mem);
    unsafe {
        let _ = SelectObject(hdc_mem, HGDIOBJ(hbm.0));
    }

    let mut bmi = unsafe { std::mem::zeroed::<BITMAPINFO>() };
    bmi.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        biHeight: -height, // 自上而下
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let mut bgra = vec![0u8; (pixels * 4) as usize];
    let copied = unsafe {
        BitBlt(
            hdc_mem,
            0,
            0,
            width,
            height,
            Some(hdc_screen),
            left,
            top,
            SRCCOPY,
        )
    }
    .is_ok();
    if !copied {
        Err(ToolError::new(CAPTURE_FAILED, "采样区域像素复制失败"))
    } else {
        let lines = unsafe {
            GetDIBits(
                hdc_mem,
                hbm,
                0,
                height as u32,
                Some(bgra.as_mut_ptr() as *mut core::ffi::c_void),
                &mut bmi,
                DIB_RGB_COLORS,
            )
        };
        if lines <= 0 {
            Err(ToolError::new(
                CAPTURE_FAILED,
                "GetDIBits 未返回任何采样像素行",
            ))
        } else {
            Ok(bgra)
        }
    }
}

/// 按 EnumDisplayMonitors 顺序取第 index 台显示器的屏幕矩形(外部 1-based
/// 序号先减一)。显示器不存在返回 None。
pub(crate) fn monitor_rect_at(index: usize) -> Option<RECT> {
    enum_monitors().get(index).map(|m| m.rect)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::WGC_UNAVAILABLE;

    #[test]
    fn preserves_three_uhd_4k_monitors() {
        let one_uhd_4k = checked_pixels(3840, 2160).unwrap();
        assert_eq!(one_uhd_4k * 3, MAX_ALL_CAPTURE_PIXELS);
        assert_eq!(
            fit_capture_dimensions(3840, 2160, MAX_WINDOW_CAPTURE_PIXELS, 1.0).unwrap(),
            (3840, 2160)
        );
    }

    #[test]
    fn scales_four_uhd_4k_monitors_to_total_budget() {
        let total = checked_pixels(3840, 2160).unwrap() * 4;
        let scale = (MAX_ALL_CAPTURE_PIXELS as f64 / total as f64).sqrt();
        let (width, height) =
            fit_capture_dimensions(3840, 2160, MAX_WINDOW_CAPTURE_PIXELS, scale).unwrap();
        assert!(checked_pixels(width, height).unwrap() * 4 <= MAX_ALL_CAPTURE_PIXELS);
        assert!(width < 3840 && height < 2160);
    }

    #[test]
    fn capture_errors_carry_stable_codes() {
        let too_big = fit_capture_dimensions(3840, 2160, 1_000, 1.0);
        assert!(too_big.is_ok());
        let bad_pixels = checked_pixels(0, 10).unwrap_err();
        assert_eq!(bad_pixels.code, INVALID_ARGUMENT);
        assert!(!bad_pixels.retryable);
    }

    #[test]
    fn resize_identity_returns_original_bytes() {
        let src = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(resize_bgra(&src, 2, 1, 2, 1).unwrap(), src);
    }

    #[test]
    fn resize_2x2_to_1x1_averages_all_pixels() {
        // 半像素偏移映射:fx=fy=0.5 → 四像素均值
        let src: Vec<u8> = vec![
            0, 100, 10, 110, 200, 40, 30, 90, 4, 104, 14, 114, 204, 44, 34, 94,
        ];
        let out = resize_bgra(&src, 2, 2, 1, 1).unwrap();
        assert_eq!(out.len(), 4);
        for c in 0..4 {
            let avg = ((src[c] as f64 + src[4 + c] as f64 + src[8 + c] as f64 + src[12 + c] as f64)
                / 4.0)
                .round() as u8;
            assert_eq!(out[c], avg, "channel {c}");
        }
    }

    #[test]
    fn resize_downsample_preserves_channel_alignment() {
        // 4x1 → 2x1:每两像素均值,通道对齐
        let src: Vec<u8> = vec![
            10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120, 130, 140, 150, 160,
        ];
        let out = resize_bgra(&src, 4, 1, 2, 1).unwrap();
        assert_eq!(out.len(), 8);
        assert_eq!(out[0], 30); // (10+50)/2
        assert_eq!(out[1], 40);
        assert_eq!(out[2], 50);
        assert_eq!(out[3], 60);
        assert_eq!(out[4], 110); // (90+130)/2
        assert_eq!(out[7], 140);
    }

    #[test]
    fn resize_rejects_zero_source_dimensions() {
        let err = resize_bgra(&[], 0, 1, 1, 1).unwrap_err();
        assert_eq!(err.code, INVALID_ARGUMENT);
    }

    #[test]
    fn capture_timeout_yields_structured_hung_error() {
        // 正常路径:快速闭包原样返回。
        let ok = run_capture_with_timeout(500, || Ok::<_, ToolError>(42_u32)).unwrap();
        assert_eq!(ok, 42);
        // 卡死路径:超过限时返回 WINDOW_HUNG,worker 自行退出后不留额外句柄。
        let start = std::time::Instant::now();
        let err = run_capture_with_timeout(50, || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            Ok::<_, ToolError>(())
        })
        .unwrap_err();
        assert_eq!(err.code, WINDOW_HUNG);
        assert!(err.retryable);
        assert!(start.elapsed() < std::time::Duration::from_millis(250));
        // 等 worker 自然退出,避免测试进程残留线程。
        std::thread::sleep(std::time::Duration::from_millis(400));
    }

    #[test]
    fn detects_near_black_printwindow_output() {
        let all_black = vec![0u8; 100 * 4]; // RGB 全 0(alpha 无关,只看 RGB)
        assert!(bgra_looks_black(&all_black));
        // 99% 黑 + 1 个亮点仍判黑(比例 > 0.98)
        let mut sparse = vec![0u8; 100 * 4];
        sparse[..4].copy_from_slice(&[255, 255, 255, 255]);
        assert!(bgra_looks_black(&sparse));
        // 亮度 16 ≥ 阈值 8,不算黑
        let bright = vec![16u8; 100 * 4];
        assert!(!bgra_looks_black(&bright));
        assert!(bgra_looks_black(&[]));
    }

    #[test]
    fn auto_chain_annotates_wgc_and_screen_sources() {
        // WGC 成功:标注 wgc 来源与 PrintWindow 失效原因(近纯黑/失败两态)。
        assert_eq!(
            wgc_fallback_reason("PrintWindow 近纯黑"),
            "wgc (PrintWindow 近纯黑)"
        );
        assert_eq!(
            wgc_fallback_reason("PrintWindow 失败"),
            "wgc (PrintWindow 失败)"
        );
        // WGC 失败:标注 screen 来源并携带 WGC 失败原因供诊断。
        let err = ToolError::new(WGC_UNAVAILABLE, "CreateForWindow 失败(窗口受 DRM 保护)");
        assert_eq!(
            screen_fallback_reason("PrintWindow 近纯黑", &err),
            "screen (PrintWindow 近纯黑; WGC 不可用: CreateForWindow 失败(窗口受 DRM 保护))"
        );
        assert_eq!(
            screen_fallback_reason("PrintWindow 失败", &err),
            "screen (PrintWindow 失败; WGC 不可用: CreateForWindow 失败(窗口受 DRM 保护))"
        );
    }

    #[test]
    fn apply_max_dim_shrinks_longest_edge_only() {
        assert_eq!(apply_max_dim(800, 400, 200), (200, 100));
        assert_eq!(apply_max_dim(400, 800, 200), (100, 200));
        // 不放大;0 = 不限制
        assert_eq!(apply_max_dim(100, 50, 200), (100, 50));
        assert_eq!(apply_max_dim(100, 50, 0), (100, 50));
    }

    #[test]
    fn encode_frame_jpeg_has_magic_bytes_and_scaled_dims() {
        // 800x400 渐变 + max_dim 200 → 200x100 JPEG,FFD8 魔数
        let mut bgra = vec![0u8; 800 * 400 * 4];
        for y in 0..400usize {
            for x in 0..800usize {
                let o = (y * 800 + x) * 4;
                bgra[o] = (x % 256) as u8;
                bgra[o + 1] = (y % 256) as u8;
                bgra[o + 2] = ((x + y) % 256) as u8;
                bgra[o + 3] = 0xFF;
            }
        }
        let format = CaptureFormat {
            jpeg_quality: Some(85),
            max_dim: 200,
        };
        let (bytes, mime, dims) = encode_frame(&bgra, 800, 400, format).unwrap();
        assert_eq!(mime, "image/jpeg");
        assert_eq!(dims, (200, 100));
        assert_eq!(&bytes[..2], &[0xFF, 0xD8]);
    }

    #[test]
    fn encode_frame_png_path_preserves_dims_without_max_dim() {
        let bgra = vec![128u8; 16 * 8 * 4];
        let (bytes, mime, dims) = encode_frame(&bgra, 16, 8, CaptureFormat::png()).unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(dims, (16, 8));
        assert_eq!(&bytes[..4], &[0x89, b'P', b'N', b'G']);
    }

    #[test]
    fn jpeg_beats_png_on_noisy_content() {
        // 伪随机噪点:PNG 几乎无压缩收益,JPEG 有损显著更小
        let mut bgra = vec![0u8; 128 * 128 * 4];
        let mut seed = 42u32;
        for px in bgra.chunks_exact_mut(4) {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let v = (seed >> 24) as u8;
            px[0] = v;
            px[1] = v.wrapping_add(3);
            px[2] = v.wrapping_sub(7);
            px[3] = 0xFF;
        }
        let jpeg = encode_jpeg(128, 128, &bgra, 85).unwrap();
        let png = bgra_to_rgba_png(128, 128, &bgra).unwrap();
        assert!(
            jpeg.len() * 3 < png.len(),
            "jpeg={} png={}",
            jpeg.len(),
            png.len()
        );
    }
}
