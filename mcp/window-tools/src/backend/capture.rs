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
    },
};

use super::window::{client_rect_of, rect_of};

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

/// 所有位图分配前先以 i64 计算面积，避免 i32 乘法溢出造成小分配/越界访问。
pub fn checked_pixels(width: i32, height: i32) -> Result<i64, String> {
    if width <= 0 || height <= 0 {
        return Err(format!("截图尺寸必须为正数，实际为 {width}x{height}"));
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
) -> Result<(i32, i32), String> {
    let pixels = checked_pixels(width, height)?;
    if pixel_budget <= 0 || !additional_scale.is_finite() || additional_scale <= 0.0 {
        return Err("无效的截图像素预算".into());
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

pub fn capture_window(hwnd: HWND, client_only: bool) -> Result<(Vec<u8>, RECT), String> {
    let rect = if client_only {
        client_rect_of(hwnd)?
    } else {
        rect_of(hwnd)?
    };
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width <= 0 || height <= 0 {
        return Err(format!(
            "窗口矩形为空 ({}x{}),可能已最小化(最小化窗口坐标为 -32000)",
            width, height
        ));
    }
    // PrintWindow 必须以原窗口尺寸绘制，不能像屏幕 BitBlt 那样在采集阶段缩放；
    // 所以异常大的窗口明确拒绝，避免先分配巨型原始缓冲再缩小。
    let pixels = checked_pixels(width, height)?;
    if width > MAX_CAPTURE_DIM || height > MAX_CAPTURE_DIM || pixels > MAX_WINDOW_CAPTURE_PIXELS {
        return Err(format!(
            "窗口尺寸 {}x{} 超过截图上限（单边 {} px、总计 {} 像素）；请先用 resize_window 缩小窗口",
            width, height, MAX_CAPTURE_DIM, MAX_WINDOW_CAPTURE_PIXELS
        ));
    }

    let hdc_screen = unsafe { GetDC(None) };
    if hdc_screen.0.is_null() {
        return Err("GetDC 未返回屏幕设备上下文".into());
    }
    let _screen_dc = GdiHandle::ScreenDc(hdc_screen);
    let hbm = unsafe { CreateCompatibleBitmap(hdc_screen, width, height) };
    if hbm.0.is_null() {
        return Err("CreateCompatibleBitmap 未能分配窗口截图位图".into());
    }
    let _bitmap = GdiHandle::Bitmap(hbm);
    let hdc_mem = unsafe { CreateCompatibleDC(Some(hdc_screen)) };
    if hdc_mem.0.is_null() {
        return Err("CreateCompatibleDC 未能创建窗口截图设备上下文".into());
    }
    let _mem_dc = GdiHandle::MemDc(hdc_mem);

    let flags = if client_only {
        PW_CLIENTONLY
    } else {
        PW_RENDERFULLCONTENT
    };
    let png = render_window_png(hwnd, hdc_mem, hbm, width, height, flags);
    png.map(|bytes| (bytes, rect))
}

/// PrintWindow(PW_RENDERFULLCONTENT) → 32bpp DIB → BGRA→RGBA → PNG
fn render_window_png(
    hwnd: HWND,
    hdc_mem: HDC,
    hbm: HBITMAP,
    width: i32,
    height: i32,
    flags: PRINT_WINDOW_FLAGS,
) -> Result<Vec<u8>, String> {
    let old = unsafe { SelectObject(hdc_mem, HGDIOBJ(hbm.0)) };
    let printed = unsafe { PrintWindow(hwnd, hdc_mem, flags) }.as_bool();
    let _ = unsafe { SelectObject(hdc_mem, old) };

    if !printed {
        return Err(format!("PrintWindow 失败(flags=0x{:X})", flags.0));
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
        return Err("GetDIBits 未返回任何位图行".into());
    }

    let mut rgba = Vec::with_capacity(bgra.len());
    for px in bgra.chunks_exact(4) {
        rgba.extend_from_slice(&[px[2], px[1], px[0], 0xFF]); // BGRA → RGBA
    }
    encode_png(width as u32, height as u32, &rgba)
}

struct MonitorRecord {
    rect: RECT,
    flags: u32,
}

/// 单台显示器的采集结果:tools 层读取元数据组装 MCP 响应,单台失败不中断其余。
pub struct MonitorShot {
    pub label: String,
    pub source_width: i32,
    pub source_height: i32,
    pub output_width: i32,
    pub output_height: i32,
    pub png: Vec<u8>,
    pub error: Option<String>,
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

/// 截取所有显示器,返回每台一张 PNG(带标签与尺寸元数据,单台失败不中断其余)。
pub fn capture_all_monitors() -> Result<Vec<MonitorShot>, String> {
    let monitors = enum_monitors();
    if monitors.is_empty() {
        return Err("未找到任何显示器".into());
    }
    let total_pixels = monitors.iter().try_fold(0i64, |total, monitor| {
        let width = monitor.rect.right - monitor.rect.left;
        let height = monitor.rect.bottom - monitor.rect.top;
        Ok::<_, String>(total + checked_pixels(width, height)?)
    })?;
    // 三台 UHD 4K 及以下保持 1.0；更多/更大的屏幕统一缩放，避免偏屏缩放。
    let total_scale = if total_pixels > MAX_ALL_CAPTURE_PIXELS {
        (MAX_ALL_CAPTURE_PIXELS as f64 / total_pixels as f64).sqrt()
    } else {
        1.0
    };
    let hdc_screen = unsafe { GetDC(None) };
    if hdc_screen.0.is_null() {
        return Err("GetDC 未返回屏幕设备上下文".into());
    }
    let _screen_dc = GdiHandle::ScreenDc(hdc_screen);

    let mut results = Vec::new();
    for (index, m) in monitors.iter().enumerate() {
        let width = m.rect.right - m.rect.left;
        let height = m.rect.bottom - m.rect.top;
        let label = format!(
            "monitor {} ({}x{} at {},{} as primary={})",
            index + 1,
            width,
            height,
            m.rect.left,
            m.rect.top,
            m.flags != 0
        );
        // 对每台再套单图预算；这里会在 StretchBlt 期间直接生成缩小位图，
        // 不会为超大原图额外分配一个完整的像素缓冲。
        let (output_width, output_height) =
            fit_capture_dimensions(width, height, MAX_WINDOW_CAPTURE_PIXELS, total_scale)?;
        match capture_rect_from_screen(
            hdc_screen,
            width,
            height,
            output_width,
            output_height,
            m.rect.left,
            m.rect.top,
        ) {
            Ok(png) => results.push(MonitorShot {
                label,
                source_width: width,
                source_height: height,
                output_width,
                output_height,
                png,
                error: None,
            }),
            Err(error) => results.push(MonitorShot {
                label,
                source_width: width,
                source_height: height,
                output_width,
                output_height,
                png: Vec::new(),
                error: Some(error),
            }),
        }
    }
    Ok(results)
}

fn capture_rect_from_screen(
    hdc_screen: HDC,
    source_width: i32,
    source_height: i32,
    output_width: i32,
    output_height: i32,
    left: i32,
    top: i32,
) -> Result<Vec<u8>, String> {
    // source_* 是真实桌面区域，output_* 是已按预算计算的目标位图尺寸。
    // 只有尺寸不同才走 StretchBlt；正常三台 4K 情况仍是无缩放的 BitBlt。
    checked_pixels(source_width, source_height)?;
    let output_pixels = checked_pixels(output_width, output_height)?;
    if output_width > MAX_CAPTURE_DIM
        || output_height > MAX_CAPTURE_DIM
        || output_pixels > MAX_WINDOW_CAPTURE_PIXELS
    {
        return Err(format!(
            "显示器输出尺寸 {}x{} 超出截图限制",
            output_width, output_height
        ));
    }
    let hbm = unsafe { CreateCompatibleBitmap(hdc_screen, output_width, output_height) };
    if hbm.0.is_null() {
        return Err("CreateCompatibleBitmap 未能分配屏幕截图位图".into());
    }
    let _bitmap = GdiHandle::Bitmap(hbm);
    let hdc_mem = unsafe { CreateCompatibleDC(Some(hdc_screen)) };
    if hdc_mem.0.is_null() {
        return Err("CreateCompatibleDC 未能创建屏幕截图设备上下文".into());
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
        Err(format!("屏幕像素复制失败: {e}"))
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
            Err("GetDIBits 未返回任何位图行".into())
        } else {
            let mut rgba = Vec::with_capacity(bgra.len());
            for px in bgra.chunks_exact(4) {
                rgba.extend_from_slice(&[px[2], px[1], px[0], 0xFF]);
            }
            encode_png(output_width as u32, output_height as u32, &rgba)
        }
    }
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("PNG 编码失败: {e}"))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| format!("PNG 写图像失败: {e}"))?;
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
) -> Result<Vec<u8>, String> {
    if width <= 0 || height <= 0 {
        return Err(format!("扫描区域尺寸必须为正数，实际为 {width}x{height}"));
    }
    let pixels = checked_pixels(width, height)?;
    if pixels > MAX_WINDOW_CAPTURE_PIXELS {
        return Err(format!(
            "扫描区域 {width}x{height} 超过上限（{} 像素）",
            MAX_WINDOW_CAPTURE_PIXELS
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
        return Err(format!(
            "扫描区域 ({x},{y}) {width}x{height} 超出窗口客户区 {client_width}x{client_height}"
        ));
    }

    let hdc_screen = unsafe { GetDC(None) };
    if hdc_screen.0.is_null() {
        return Err("GetDC 未返回屏幕设备上下文".into());
    }
    let _screen_dc = GdiHandle::ScreenDc(hdc_screen);
    let hbm = unsafe { CreateCompatibleBitmap(hdc_screen, width, height) };
    if hbm.0.is_null() {
        return Err("CreateCompatibleBitmap 未能分配扫描位图".into());
    }
    let _bitmap = GdiHandle::Bitmap(hbm);
    let hdc_mem = unsafe { CreateCompatibleDC(Some(hdc_screen)) };
    if hdc_mem.0.is_null() {
        return Err("CreateCompatibleDC 未能创建扫描设备上下文".into());
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
            client.left + x,
            client.top + y,
            SRCCOPY,
        )
    }
    .is_ok();
    if !copied {
        Err("扫描区域像素复制失败".to_string())
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
            Err("GetDIBits 未返回任何扫描像素行".into())
        } else {
            Ok(bgra)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
