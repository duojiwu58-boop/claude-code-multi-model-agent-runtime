//! WGC (Windows Graphics Capture) 采集:DWM GPU 合成帧,实验路径。
//!
//! 与 PrintWindow 的区别:WGC 从 DWM 合成器直接取帧,GPU 加速窗口不再出
//! 近纯黑,也无需屏幕采样回退;被遮挡窗口照样产出完整内容。代价是全新的
//! 依赖面(D3D11/DXGI/WinRT 互操作)与三处能力边界:
//! - 最小化窗口确定性拒绝(v0.6.1 起 IsIconic → WINDOW_NOT_VISIBLE;
//!   GetWindowRect 对最小化窗口位置 -32000 但尺寸仍为正,尺寸预检拦不住,
//!   缓存会话还会复用最小化前的旧帧);
//! - DRM 保护内容被拒或黑区;
//! - 系统级 CaptureSession 存在并发上限(本模块缓存上限 4 主动规避)。
//!
//! v0.5.0 起核心路径升级为跨调用会话缓存 + 增量差分:
//! - **设备缓存**:D3D11 设备 + WinRT 封装进程级只建一次(实测每次重建
//!   ~60ms);每 `(hwnd, client_only, incremental)` 一个捕获会话,LRU 上限 4,
//!   每次调用顺手清理已销毁窗口的会话。
//! - **全帧复用**:窗口静态时 DWM 不产帧,`TryGetNextFrame` 以 null + S_OK
//!   返回;既有会话立即取帧,空则等一个 DWM 交付周期(实测 ≤100ms)再取
//!   一次,仍无帧则复用上次帧 BGRA 重编码(内容与上次一致,语义等价)。
//!   立即取帧保证常规"动作→截图"循环(帧早已入池)只付毫秒级成本,100ms
//!   等待窗口兜住与动作并发竞态的调用,避免误报"无变化"。
//! - **增量差分**(`incremental=true`):逐行差分 + 变化行聚带,只回传变化
//!   区域裁剪 PNG;无变化时纯文本零图像。会话禁用光标捕获保证差分确定性
//!   (否则光标移动=帧=伪变化)。不信任原生 DirtyRegions:帧池缓冲有限,
//!   长间隔后脏区相对的是中间帧而非客户端基线,会漏报;像素差分不依赖
//!   帧历史,任何间隔下都精确。
//!
//! 实测(v0.5.0 探针,902x632 硬件设备):每次调用全新建链 210-230ms
//! (设备 ~60 + CreateForWindow ~34 + 首帧 ~100 + 编码 ~4);会话命中且
//! 有变化时 ~10-30ms,静态窗口确认无新帧 ~110ms(增量零图像)。
//!
//! COM 初始化与 UIA 同策略:限定在 worker 线程 MTA;进程内 MTA 唯一,
//! 缓存对象跨线程直接访问合法,并发由缓存 Mutex 串行化(并行工具调用
//! 在同一缓存上排队,等待以毫秒计)。

use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use windows::{
    core::{Interface as _, HRESULT},
    Graphics::{
        Capture::{
            Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem,
            GraphicsCaptureSession,
        },
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
    },
    Win32::{
        Foundation::{HMODULE, HWND},
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP},
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource,
                ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
                D3D11_USAGE_STAGING,
            },
            Dxgi::{Common::DXGI_FORMAT_B8G8R8A8_UNORM, IDXGIDevice},
            Gdi::HMONITOR,
        },
        System::{
            Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED},
            WinRT::{
                Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
                Graphics::Capture::IGraphicsCaptureItemInterop,
            },
        },
        UI::WindowsAndMessaging::{IsIconic, IsWindow},
    },
};

use crate::backend::{
    capture::{
        bgra_to_rgba_png, checked_pixels, encode_frame, run_with_timeout, CaptureFormat,
        DirtyRegion, IncrementalInfo, WindowCapture, CAPTURE_TIMEOUT_MS, MAX_CAPTURE_DIM,
        MAX_WINDOW_CAPTURE_PIXELS,
    },
    window::{client_rect_of, is_window_hung, rect_of},
};
use crate::error::{
    ToolError, INVALID_ARGUMENT, LIMIT_EXCEEDED, WGC_UNAVAILABLE, WINDOW_HUNG, WINDOW_NOT_VISIBLE,
};

/// 会话缓存上限:与系统级 CaptureSession 并发上限保持距离,同时约束
/// BGRA 基线内存(每会话 ≤ 12MP×4B = 48MB,4 会话最坏 192MB 仅理论值)。
const MAX_CACHED_SESSIONS: usize = 4;
/// 新会话首帧轮询:间隔 100ms,预算 10s(外层 15s 硬超时兜底);
/// 既有会话的空闲等待复用同间隔(一次,即一个 DWM 交付周期)。
const POLL_INTERVAL_MS: u64 = 100;
const POLL_BUDGET_MS: u64 = 10_000;
/// 显示器会话首帧预算:显示器正常情况下帧立即到达,长预算只会把
/// 多屏超时拖到 15s 硬超时,失去"单屏失败快速返回"的意义。
const MONITOR_FRAME_BUDGET_MS: u64 = 3_000;
/// 变化区域数量上限:超过则合并为单一外接盒(如光标拖动产生的离散变化),
/// 保证响应体(图像块数量与总字节)有界。
pub const MAX_DIRTY_REGIONS: usize = 8;

/// WGC 采集模式:全帧编码(格式可选) / 增量差分(固定 PNG) / 原始 BGRA
/// (像素采样,不编码)。Copy:尺寸变化重建循环内可能多次 match。
/// pub(crate):auto 链(capture.rs)需要构造 Full 模式。
#[derive(Clone, Copy)]
pub(crate) enum CaptureMode {
    Full(CaptureFormat),
    Incremental,
    Raw,
}

/// 采集前置检查(与 GDI 路径一致):挂起预检 + 最小化确定性拒绝 + 尺寸预检,
/// 返回目标矩形。所有 WGC 入口共用。
fn wgc_precheck(
    hwnd: HWND,
    client_only: bool,
) -> Result<windows::Win32::Foundation::RECT, ToolError> {
    if is_window_hung(hwnd) {
        return Err(ToolError::new(
            WINDOW_HUNG,
            format!(
                "窗口 hwnd={} 无响应，拒绝截图；等待窗口恢复或选择其他目标",
                hwnd.0 as isize
            ),
        ));
    }
    // 最小化窗口确定性拒绝:缓存会话对最小化窗口不产帧,若复用旧帧会返回
    // 最小化前的过期内容;尺寸预检拦不住(最小化窗口矩形位置 -32000 但尺寸
    // 仍为正),须显式检查。
    if unsafe { IsIconic(hwnd) }.as_bool() {
        return Err(ToolError::new(
            WINDOW_NOT_VISIBLE,
            format!(
                "窗口 hwnd={} 已最小化，拒绝截图；恢复窗口后重试",
                hwnd.0 as isize
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
    Ok(rect)
}

/// WGC 采集入口(全帧):挂起预检 + 尺寸预检与 GDI 路径一致,采集在独立
/// 线程执行并限时等待。`format` 控制编码与 max_dim 降采样。
pub fn capture_window_wgc(
    hwnd: HWND,
    client_only: bool,
    format: CaptureFormat,
) -> Result<WindowCapture, ToolError> {
    run_wgc_entry(hwnd, client_only, CaptureMode::Full(format))
}

/// WGC 增量差分入口:首次调用返回完整基线帧,后续调用只返回相对上次的
/// 变化区域(无变化时纯文本)。会话经全局缓存跨调用复用。固定 PNG 输出
/// (差分区域的像素保真优先)。
pub fn capture_window_wgc_incremental(
    hwnd: HWND,
    client_only: bool,
) -> Result<WindowCapture, ToolError> {
    run_wgc_entry(hwnd, client_only, CaptureMode::Incremental)
}

fn run_wgc_entry(
    hwnd: HWND,
    client_only: bool,
    mode: CaptureMode,
) -> Result<WindowCapture, ToolError> {
    wgc_precheck(hwnd, client_only)?;
    let raw = hwnd.0 as isize;
    run_with_timeout(
        CAPTURE_TIMEOUT_MS,
        move || capture_window_wgc_cached(HWND(raw as *mut core::ffi::c_void), client_only, mode),
        |ms| {
            ToolError::new(
                WINDOW_HUNG,
                format!("WGC 在 {ms}ms 内未产出帧，窗口可能已最小化或合成停止"),
            )
        },
    )
}

/// WGC 缓存采集(auto 链复用,不设超时包装,由调用方保证):
/// 全局设备 + 按 (hwnd, client_only, incremental) 的会话缓存。
/// auto 链在 capture_window_inner 的 worker 内直接调用,外层 15s 硬超时
/// 已覆盖首帧轮询;method=wgc / incremental 入口经 run_wgc_entry 包装。
pub(crate) fn capture_window_wgc_cached(
    hwnd: HWND,
    client_only: bool,
    mode: CaptureMode,
) -> Result<WindowCapture, ToolError> {
    let rect = wgc_precheck(hwnd, client_only)?;
    with_mta(|| {
        let output = capture_cached_inner(hwnd, rect, client_only, mode)?;
        output.into_window_capture(rect)
    })
}

/// 原始 BGRA 帧(像素采样用,不编码):与截图共用缓存会话,窗口静态时
/// 直接复用上次帧,不重新拷贝 GPU 纹理。返回 (bgra, frame_w, frame_h)。
pub(crate) fn capture_window_wgc_bgra(
    hwnd: HWND,
    client_only: bool,
) -> Result<(Vec<u8>, u32, u32), ToolError> {
    // 前置检查即全部意图:返回值仅用于校验,原始帧路径不需要矩形。
    let _rect = wgc_precheck(hwnd, client_only)?;
    with_mta(
        || match capture_cached_inner(hwnd, _rect, client_only, CaptureMode::Raw)? {
            CaptureOutput::Raw { bgra, fw, fh, .. } => Ok((bgra, fw, fh)),
            _ => Err(wgc_err("Raw 模式未返回原始帧(内部状态异常)".to_string())),
        },
    )
}

/// 客户区矩形像素采样(WGC 帧):`capture_client_region_bgra` 的 wgc 对应物。
/// 复用 (hwnd, client_only=true, incremental=false) 缓存会话;client_only
/// 会话帧即客户区内容,坐标直接对应、无边框偏移。帧来自 DWM 合成,与屏幕
/// 现状无关——被其他窗口遮挡也给出目标窗口的真实像素。
pub(crate) fn capture_client_region_wgc(
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
    let (bgra, fw, fh) = capture_window_wgc_bgra(hwnd, true)?;
    if x < 0
        || y < 0
        || i64::from(x) + i64::from(width) > i64::from(fw)
        || i64::from(y) + i64::from(height) > i64::from(fh)
    {
        return Err(ToolError::new(
            INVALID_ARGUMENT,
            format!("扫描区域 ({x},{y}) {width}x{height} 超出 WGC 帧 {fw}x{fh}"),
        ));
    }
    Ok(crop_bgra(
        &bgra,
        fw,
        x as u32,
        y as u32,
        width as u32,
        height as u32,
    ))
}

/// 会话缓存键:窗口 + 是否仅客户区 + 是否增量。任一维不同 = 不同会话
/// (边框策略/光标策略不同,帧内容不同,不能共用)。
#[derive(Clone, Copy, PartialEq, Eq)]
struct SessionKey {
    hwnd: isize,
    client_only: bool,
    incremental: bool,
}

/// 进程级 D3D11 设备缓存:硬件优先,WARP 兜底,只建一次。
/// 不缓存 WinRT `IDirect3DDevice` 封装:windows-rs 未给其实现 Send,
/// 且封装转换实测 ~0ms(纯 QI),建会话时现取即可。
/// Clone 为 COM 引用计数 +1,便于在锁内传出后释放缓存借用。
struct CachedDevice {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
}

impl CachedDevice {
    /// D3D11 设备 → WinRT IDirect3DDevice(WGC 帧池要求 WinRT 封装的设备)。
    /// 每次建会话时现取,不在缓存中持有(Send 边界)。
    fn direct3d(&self) -> Result<IDirect3DDevice, ToolError> {
        d3d11_to_winrt_device(&self.device)
    }
}

impl Clone for CachedDevice {
    fn clone(&self) -> Self {
        Self {
            device: self.device.clone(),
            context: self.context.clone(),
        }
    }
}

/// 单窗口捕获会话:帧池 + 会话 + 上次帧 BGRA(全帧复用/增量差分基线)。
/// 不变量:会话在缓存中 ⇒ `last_bgra` 已就绪(首帧读取成功后才会入库)。
struct CachedSession {
    key: SessionKey,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    /// 最近一帧的实际尺寸(帧纹理尺寸,非 GDI 矩形)。
    size: (u32, u32),
    /// 最近一帧的 ContentSize(捕获项当前尺寸)。帧纹理固定于建池时的
    /// item 尺寸,窗口调整大小后只有 ContentSize 变化,尺寸变化检测以此为准。
    content_size: (u32, u32),
    /// 上次成功读取的帧 BGRA:全帧模式无新帧时重编码复用,
    /// 增量模式作差分基线。
    last_bgra: Option<Vec<u8>>,
    last_used: Instant,
}

impl Drop for CachedSession {
    fn drop(&mut self) {
        // 关闭顺序与会话生命周期契约一致:先会话后池(帧不落缓存,
        // 每次调用内 FrameGuard 已归还)。
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

struct WgcCache {
    device: Option<CachedDevice>,
    sessions: Vec<CachedSession>,
}

static WGC_CACHE: OnceLock<Mutex<WgcCache>> = OnceLock::new();

fn wgc_cache() -> &'static Mutex<WgcCache> {
    WGC_CACHE.get_or_init(|| {
        Mutex::new(WgcCache {
            device: None,
            sessions: Vec::new(),
        })
    })
}

/// 采集输出三态:原始 BGRA(像素采样) / 编码图像(全帧) / 增量信息。
enum CaptureOutput {
    Raw {
        bgra: Vec<u8>,
        fw: u32,
        fh: u32,
    },
    Image {
        image: Vec<u8>,
        mime: &'static str,
        image_dims: (u32, u32),
        reused: bool,
    },
    Incremental {
        /// 基线帧的 PNG(baseline=true 时非空;差分/无变化时为空)。
        image: Vec<u8>,
        baseline: bool,
        regions: Vec<DirtyRegion>,
        reused: bool,
    },
}

impl CaptureOutput {
    fn into_window_capture(
        self,
        rect: windows::Win32::Foundation::RECT,
    ) -> Result<WindowCapture, ToolError> {
        match self {
            CaptureOutput::Image {
                image,
                mime,
                image_dims,
                reused,
            } => Ok(WindowCapture {
                image,
                mime,
                image_dims,
                rect,
                fallback: None,
                incremental: None,
                reused,
            }),
            CaptureOutput::Incremental {
                image,
                baseline,
                regions,
                reused,
            } => {
                let image_dims = if image.is_empty() {
                    (0, 0)
                } else {
                    (
                        (rect.right - rect.left) as u32,
                        (rect.bottom - rect.top) as u32,
                    )
                };
                Ok(WindowCapture {
                    image,
                    mime: "image/png",
                    image_dims,
                    rect,
                    fallback: None,
                    incremental: Some(IncrementalInfo { baseline, regions }),
                    reused,
                })
            }
            CaptureOutput::Raw { .. } => Err(wgc_err(
                "Raw 模式结果被错误地转为 MCP 响应(内部状态异常)".to_string(),
            )),
        }
    }
}

fn capture_cached_inner(
    hwnd: HWND,
    _rect: windows::Win32::Foundation::RECT,
    client_only: bool,
    mode: CaptureMode,
) -> Result<CaptureOutput, ToolError> {
    let key = SessionKey {
        hwnd: hwnd.0 as isize,
        client_only,
        incremental: matches!(mode, CaptureMode::Incremental),
    };
    let mut guard = wgc_cache().lock().unwrap_or_else(|e| e.into_inner());
    let cache = &mut *guard;
    cache.sweep_dead();
    let dev = ensure_device(&mut cache.device)?;
    let idx = cache.sessions.iter().position(|s| s.key == key);
    let mut sess = match idx {
        Some(i) => cache.sessions.remove(i),
        None => {
            evict_lru(&mut cache.sessions);
            create_session(hwnd, &dev, key)?
        }
    };
    let mut fresh = idx.is_none();
    // 尺寸变化时重建会话(最多一次):帧纹理固定于建池时的 item 尺寸,
    // 窗口调整大小后只有 ContentSize 跟随变化——以 ContentSize 为准检测;
    // 纹理与 ContentSize 不符(建池时 item 尺寸异常)同样触发重建。
    for attempt in 0..2u8 {
        match poll_or_wait_frame(&dev, &sess, fresh)? {
            FrameArrival::Frame {
                bgra,
                fw,
                fh,
                cw,
                ch,
            } => {
                let content_changed = !fresh && (cw, ch) != sess.content_size;
                let tex_mismatch = (fw, fh) != (cw, ch);
                if attempt == 0 && (content_changed || tex_mismatch) {
                    crate::log_info!(
                        "wgc-cache: 帧尺寸变化 {}x{} -> {}x{} (tex {}x{}),重建会话",
                        sess.content_size.0,
                        sess.content_size.1,
                        cw,
                        ch,
                        fw,
                        fh
                    );
                    sess = create_session(hwnd, &dev, key)?;
                    fresh = true;
                    continue;
                }
                sess.size = (fw, fh);
                sess.content_size = (cw, ch);
                let output = match mode {
                    CaptureMode::Raw => {
                        sess.last_bgra = Some(bgra.clone());
                        CaptureOutput::Raw { bgra, fw, fh }
                    }
                    CaptureMode::Full(format) => {
                        let (image, mime, image_dims) = encode_frame(&bgra, fw, fh, format)?;
                        sess.last_bgra = Some(bgra);
                        CaptureOutput::Image {
                            image,
                            mime,
                            image_dims,
                            reused: false,
                        }
                    }
                    CaptureMode::Incremental => incremental_from_frame(&mut sess, bgra, fw, fh)?,
                };
                sess.last_used = Instant::now();
                cache.sessions.push(sess);
                return Ok(output);
            }
            FrameArrival::None => {
                // 无新帧:静态窗口 DWM 不产帧,内容与上次一致。
                let output = reuse_last(&sess, mode)?;
                sess.last_used = Instant::now();
                cache.sessions.push(sess);
                return Ok(output);
            }
        }
    }
    unreachable!("size-recreate loop 有界:attempt=1 时直接接受新尺寸")
}

enum FrameArrival {
    Frame {
        bgra: Vec<u8>,
        fw: u32,
        fh: u32,
        /// 帧 ContentSize:捕获项当前尺寸,尺寸变化检测的依据
        /// (帧纹理固定于建池尺寸,窗口调整后纹理不变)。
        cw: u32,
        ch: u32,
    },
    None,
}

/// 帧就位:读纹理 BGRA + ContentSize,组装 FrameArrival。
fn frame_arrival(
    dev: &CachedDevice,
    frame: &Direct3D11CaptureFrame,
) -> Result<FrameArrival, ToolError> {
    let (bgra, fw, fh) = frame_to_bgra(dev, frame)?;
    let (cw, ch) = frame
        .ContentSize()
        .map(|s| (s.Width as u32, s.Height as u32))
        .unwrap_or((fw, fh));
    Ok(FrameArrival::Frame {
        bgra,
        fw,
        fh,
        cw,
        ch,
    })
}

/// 取帧:新会话等待首帧(10s 预算,与 v0.4.0 行为一致);既有会话立即取帧,
/// 空则等待一个 DWM 交付周期(实测 ≤100ms)再取一次,仍空即"无新帧"。
/// 立即取帧保证常规"动作→截图"循环(帧早已入池)只付毫秒级成本;
/// 100ms 等待窗口兜住与动作并发竞态的调用,避免误报"无变化"。
fn poll_or_wait_frame(
    dev: &CachedDevice,
    sess: &CachedSession,
    fresh: bool,
) -> Result<FrameArrival, ToolError> {
    if fresh {
        let frame = wait_first_frame(&sess.pool, POLL_BUDGET_MS)?;
        let _guard = FrameGuard(frame);
        return frame_arrival(dev, &_guard.0);
    }
    if let Some(frame) = try_next_frame(&sess.pool)? {
        let _guard = FrameGuard(frame);
        return frame_arrival(dev, &_guard.0);
    }
    std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
    if let Some(frame) = try_next_frame(&sess.pool)? {
        let _guard = FrameGuard(frame);
        return frame_arrival(dev, &_guard.0);
    }
    Ok(FrameArrival::None)
}

/// 单显示器 WGC 采集(一次性会话,不落缓存):screenshot_all 的 wgc 路径。
/// 与窗口路径同管线,CreateForWindow 换 CreateForMonitor;光标默认包含
/// (屏幕 BitBlt 不含光标,这是 WGC 显示器路径的主要增益)。会话不缓存:
/// 显示器会话长期持有会挤占系统 CaptureSession 配额,且 screenshot_all
/// 属低频调用,建链成本可接受。
pub(crate) fn capture_monitor_wgc(hmon: HMONITOR) -> Result<(Vec<u8>, u32, u32), ToolError> {
    with_mta(|| {
        let mut guard = wgc_cache().lock().unwrap_or_else(|e| e.into_inner());
        let cache = &mut *guard;
        let dev = ensure_device(&mut cache.device)?;
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .map_err(|e| wgc_err(format!("创建 WGC 互操作工厂失败: {e}")))?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(hmon) }.map_err(|e| {
            wgc_err(format!(
                "CreateForMonitor 失败(显示器不可捕获或跨权限级): {e}"
            ))
        })?;
        let size = item
            .Size()
            .map_err(|e| wgc_err(format!("读取捕获项尺寸失败: {e}")))?;
        if size.Width <= 0 || size.Height <= 0 {
            return Err(ToolError::new(
                WGC_UNAVAILABLE,
                "显示器未产生可捕获内容（会话可能已断开或锁定）",
            ));
        }
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &dev.direct3d()?,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )
        .map_err(|e| wgc_err(format!("创建捕获帧池失败: {e}")))?;
        let session = pool
            .CreateCaptureSession(&item)
            .map_err(|e| wgc_err(format!("创建捕获会话失败: {e}")))?;
        session
            .StartCapture()
            .map_err(|e| wgc_err(format!("启动捕获失败: {e}")))?;
        let _session_guard = OneShotSessionGuard { session, pool };
        let frame = wait_first_frame(&_session_guard.pool, MONITOR_FRAME_BUDGET_MS)?;
        let _frame_guard = FrameGuard(frame);
        frame_to_bgra(&dev, &_frame_guard.0)
    })
}

/// 轮询等待 WGC 首帧。TryGetNextFrame 无帧时以 null + S_OK 返回
/// (windows-core 表现为 `Err(Error::empty())`),并非协议错误;
/// 每次间隔 100ms,总预算由调用方指定,超预算后按不可用报错。
fn wait_first_frame(
    pool: &Direct3D11CaptureFramePool,
    budget_ms: u64,
) -> Result<Direct3D11CaptureFrame, ToolError> {
    let deadline = Instant::now() + std::time::Duration::from_millis(budget_ms);
    let mut polls = 0u32;
    loop {
        match try_next_frame(pool) {
            Ok(Some(frame)) => {
                let size = frame
                    .ContentSize()
                    .map(|s| (s.Width, s.Height))
                    .unwrap_or((0, 0));
                crate::log_info!(
                    "wgc: 首帧到达 ContentSize={}x{} polls={polls}",
                    size.0,
                    size.1
                );
                return Ok(frame);
            }
            Ok(None) if Instant::now() < deadline => {
                polls += 1;
                std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
            }
            Ok(None) => {
                return Err(wgc_err(format!(
                    "窗口在 {budget_ms}ms 预算内未产出帧(可能已最小化或合成停止)"
                )))
            }
            Err(e) => return Err(e),
        }
    }
}

/// TryGetNextFrame 结果规约:null + S_OK = 尚无新帧(非错误);
/// 其他 HRESULT 为真实失败(会话死亡/设备异常),升级为错误。
fn try_next_frame(
    pool: &Direct3D11CaptureFramePool,
) -> Result<Option<Direct3D11CaptureFrame>, ToolError> {
    match pool.TryGetNextFrame() {
        Ok(frame) => Ok(Some(frame)),
        Err(e) if e.code().is_ok() => Ok(None),
        Err(e) => Err(wgc_err(format!("TryGetNextFrame 失败: {e}"))),
    }
}

/// 增量模式的帧就位结果构造:基线→差分→无变化三态(固定 PNG)。
fn incremental_from_frame(
    sess: &mut CachedSession,
    bgra: Vec<u8>,
    fw: u32,
    fh: u32,
) -> Result<CaptureOutput, ToolError> {
    let Some(prev) = sess.last_bgra.take() else {
        // 首次采集(基线):全帧返回,存基线。
        let png = bgra_to_rgba_png(fw as i32, fh as i32, &bgra)?;
        sess.last_bgra = Some(bgra);
        return Ok(CaptureOutput::Incremental {
            image: png,
            baseline: true,
            regions: Vec::new(),
            reused: false,
        });
    };
    if prev.len() != bgra.len() {
        // 尺寸不同的基线(理论不可达:尺寸变化路径已重建会话):
        // 重建基线,全帧返回。
        let png = bgra_to_rgba_png(fw as i32, fh as i32, &bgra)?;
        sess.last_bgra = Some(bgra);
        return Ok(CaptureOutput::Incremental {
            image: png,
            baseline: true,
            regions: Vec::new(),
            reused: false,
        });
    }
    let rects = diff_dirty_bands(&prev, &bgra, fw, fh, MAX_DIRTY_REGIONS);
    sess.last_bgra = Some(bgra);
    if rects.is_empty() {
        return Ok(CaptureOutput::Incremental {
            image: Vec::new(),
            baseline: false,
            regions: Vec::new(),
            reused: true,
        });
    }
    let full = sess.last_bgra.as_ref().expect("刚写入的基线必存在");
    let mut regions = Vec::with_capacity(rects.len());
    for (x, y, w, h) in rects {
        let cropped = crop_bgra(full, fw, x, y, w, h);
        let png = bgra_to_rgba_png(w as i32, h as i32, &cropped)?;
        regions.push(DirtyRegion { x, y, w, h, png });
    }
    Ok(CaptureOutput::Incremental {
        image: Vec::new(),
        baseline: false,
        regions,
        reused: false,
    })
}

/// 无新帧路径:内容与上次一致。增量=纯文本零图像;全帧=基线 BGRA 重编码;
/// Raw=基线 BGRA 直接返回(克隆)。
fn reuse_last(sess: &CachedSession, mode: CaptureMode) -> Result<CaptureOutput, ToolError> {
    let bgra = sess
        .last_bgra
        .as_ref()
        .ok_or_else(|| wgc_err("会话基线缺失(内部状态异常),本次调用将重建会话".to_string()))?;
    let (fw, fh) = sess.size;
    match mode {
        CaptureMode::Raw => Ok(CaptureOutput::Raw {
            bgra: bgra.clone(),
            fw,
            fh,
        }),
        CaptureMode::Full(format) => {
            let (image, mime, image_dims) = encode_frame(bgra, fw, fh, format)?;
            Ok(CaptureOutput::Image {
                image,
                mime,
                image_dims,
                reused: true,
            })
        }
        CaptureMode::Incremental => Ok(CaptureOutput::Incremental {
            image: Vec::new(),
            baseline: false,
            regions: Vec::new(),
            reused: true,
        }),
    }
}

/// 逐行差分 + 变化行聚带:返回变化区域矩形列表(帧坐标 x,y,w,h)。
/// 行内按 4 字节像素步进;连续变化行并入同一带,带内记录最小/最大 x。
/// 超过 `max_regions` 个带时合并为单一外接盒,保证响应体有界。
pub fn diff_dirty_bands(
    prev: &[u8],
    curr: &[u8],
    w: u32,
    h: u32,
    max_regions: usize,
) -> Vec<(u32, u32, u32, u32)> {
    if prev.len() != curr.len() || w == 0 || h == 0 {
        return vec![(0, 0, w, h)];
    }
    let row_bytes = (w as usize) * 4;
    // (y0, y1, min_x, max_x) 闭区间;封带时转 (x, y, w, h)
    let mut bands: Vec<(u32, u32, u32, u32)> = Vec::new();
    let mut band: Option<(u32, u32, u32, u32)> = None;
    for y in 0..h as usize {
        let row = y * row_bytes;
        let mut min_x = u32::MAX;
        let mut max_x = 0u32;
        for x in 0..w as usize {
            let o = row + x * 4;
            if prev[o..o + 4] != curr[o..o + 4] {
                let x = x as u32;
                if x < min_x {
                    min_x = x;
                }
                if x > max_x {
                    max_x = x;
                }
            }
        }
        if min_x <= max_x {
            match &mut band {
                Some(b) => {
                    b.1 = y as u32;
                    b.2 = b.2.min(min_x);
                    b.3 = b.3.max(max_x);
                }
                None => band = Some((y as u32, y as u32, min_x, max_x)),
            }
        } else if let Some(b) = band.take() {
            bands.push(b);
        }
    }
    if let Some(b) = band {
        bands.push(b);
    }
    if bands.is_empty() {
        return bands;
    }
    if bands.len() > max_regions {
        let x0 = bands.iter().map(|b| b.2).min().unwrap_or(0);
        let x1 = bands.iter().map(|b| b.3).max().unwrap_or(0);
        let y0 = bands[0].0;
        let y1 = bands.last().map(|b| b.1).unwrap_or(0);
        return vec![(x0, y0, x1 - x0 + 1, y1 - y0 + 1)];
    }
    bands
        .into_iter()
        .map(|(y0, y1, x0, x1)| (x0, y0, x1 - x0 + 1, y1 - y0 + 1))
        .collect()
}

/// 从整帧 BGRA 中裁剪矩形区域(帧坐标),行距为整帧宽度。
fn crop_bgra(bgra: &[u8], frame_w: u32, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
    let row_bytes = (frame_w as usize) * 4;
    let out_row = (w as usize) * 4;
    let mut out = Vec::with_capacity(out_row * h as usize);
    for row in 0..h as usize {
        let start = ((y as usize + row) * row_bytes) + (x as usize) * 4;
        out.extend_from_slice(&bgra[start..start + out_row]);
    }
    out
}

fn wgc_err(message: String) -> ToolError {
    ToolError::new(WGC_UNAVAILABLE, message)
}

/// 进程级设备:不存在则创建(硬件→WARP)并缓存,之后每次返回引用计数副本。
fn ensure_device(slot: &mut Option<CachedDevice>) -> Result<CachedDevice, ToolError> {
    if let Some(dev) = slot {
        return Ok(dev.clone());
    }
    let (device, context) = create_device()?;
    let cached = CachedDevice { device, context };
    crate::log_info!("wgc-cache: D3D11 设备缓存建立");
    *slot = Some(cached.clone());
    Ok(cached)
}

/// 创建捕获会话(帧池 + 会话 + 启动)。增量会话禁用光标捕获:
/// 光标移动不触发帧/差分,保证"无变化"判定确定。
fn create_session(
    hwnd: HWND,
    dev: &CachedDevice,
    key: SessionKey,
) -> Result<CachedSession, ToolError> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
        .map_err(|e| wgc_err(format!("创建 WGC 互操作工厂失败: {e}")))?;
    let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd) }.map_err(|e| {
        wgc_err(format!(
            "CreateForWindow 失败(窗口可能受 DRM 保护、已销毁或跨权限级): {e}"
        ))
    })?;
    let size = item
        .Size()
        .map_err(|e| wgc_err(format!("读取捕获项尺寸失败: {e}")))?;
    if size.Width <= 0 || size.Height <= 0 {
        return Err(ToolError::new(
            WGC_UNAVAILABLE,
            "窗口未产生可捕获内容（可能已最小化）",
        ));
    }
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &dev.direct3d()?,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )
    .map_err(|e| wgc_err(format!("创建捕获帧池失败: {e}")))?;
    let session = pool
        .CreateCaptureSession(&item)
        .map_err(|e| wgc_err(format!("创建捕获会话失败: {e}")))?;
    // 边框策略尽力而为:Win10 不支持去边框,Win11 默认去边框。
    let _ = session.SetIsBorderRequired(!key.client_only);
    if key.incremental {
        if let Err(e) = session.SetIsCursorCaptureEnabled(false) {
            crate::log_debug!("wgc-cache: 禁用光标捕获失败(忽略): {e}");
        }
    }
    session
        .StartCapture()
        .map_err(|e| wgc_err(format!("启动捕获失败: {e}")))?;
    crate::log_info!(
        "wgc-cache: 会话新建 hwnd={} client_only={} incremental={}",
        key.hwnd,
        key.client_only,
        key.incremental
    );
    Ok(CachedSession {
        key,
        pool,
        session,
        size: (size.Width as u32, size.Height as u32),
        content_size: (size.Width as u32, size.Height as u32),
        last_bgra: None,
        last_used: Instant::now(),
    })
}

impl WgcCache {
    /// 清理已销毁窗口的会话(句柄复用风险另说,至少回收资源)。
    fn sweep_dead(&mut self) {
        let before = self.sessions.len();
        self.sessions.retain(|s| {
            unsafe { IsWindow(Some(HWND(s.key.hwnd as *mut core::ffi::c_void))) }.as_bool()
        });
        if self.sessions.len() != before {
            crate::log_info!(
                "wgc-cache: 清理已销毁窗口会话 {} -> {}",
                before,
                self.sessions.len()
            );
        }
    }
}

/// LRU 淘汰:新建会话前调用,保持缓存 ≤ MAX_CACHED_SESSIONS。
/// 移除即 Drop(先停会话再关池)。
fn evict_lru(sessions: &mut Vec<CachedSession>) {
    while sessions.len() >= MAX_CACHED_SESSIONS {
        let oldest = sessions
            .iter()
            .enumerate()
            .min_by_key(|(_, s)| s.last_used)
            .map(|(i, _)| i)
            .unwrap_or(0);
        let removed = sessions.remove(oldest);
        crate::log_info!(
            "wgc-cache: LRU 淘汰 hwnd={} client_only={} incremental={}",
            removed.key.hwnd,
            removed.key.client_only,
            removed.key.incremental
        );
    }
}

/// D3D11 设备:硬件优先,WARP 兜底(远程桌面等无硬件加速场景)。
fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext), ToolError> {
    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let hr = unsafe {
            D3D11CreateDevice(
                None,
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        };
        if hr.is_ok() {
            crate::log_info!("wgc: D3D11 设备就绪 driver={}", driver.0);
            return Ok((
                device.expect("HRESULT 成功时设备必已写入"),
                context.expect("HRESULT 成功时上下文必已写入"),
            ));
        }
        crate::log_debug!(
            "wgc: D3D11 driver={} 创建失败: {}",
            driver.0,
            hr.unwrap_err()
        );
    }
    Err(wgc_err(
        "创建 D3D11 设备失败（硬件与 WARP 均不可用）".to_string(),
    ))
}

/// D3D11 设备 → WinRT IDirect3DDevice(WGC 帧池要求 WinRT 封装的设备)。
fn d3d11_to_winrt_device(device: &ID3D11Device) -> Result<IDirect3DDevice, ToolError> {
    let dxgi_device: IDXGIDevice = device
        .cast()
        .map_err(|e| wgc_err(format!("ID3D11Device → IDXGIDevice 转换失败: {e}")))?;
    let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device) }
        .map_err(|e| wgc_err(format!("CreateDirect3D11DeviceFromDXGIDevice 失败: {e}")))?;
    inspectable
        .cast::<IDirect3DDevice>()
        .map_err(|e| wgc_err(format!("IInspectable → IDirect3DDevice 转换失败: {e}")))
}

/// 帧 → CPU 可读 BGRA。
/// WinRT Direct3DSurface 对 COM QI 返回 E_NOINTERFACE,必须经
/// IDirect3DDxgiInterfaceAccess::GetInterface 取 ID3D11Texture2D,
/// 再拷贝到 CPU 可读 staging 纹理,按行距逐行取出(行距可能大于宽度×4)。
fn frame_to_bgra(
    dev: &CachedDevice,
    frame: &Direct3D11CaptureFrame,
) -> Result<(Vec<u8>, u32, u32), ToolError> {
    let surface = frame
        .Surface()
        .map_err(|e| wgc_err(format!("读取帧表面失败: {e}")))?;
    let access: IDirect3DDxgiInterfaceAccess = surface.cast().map_err(|e| {
        wgc_err(format!(
            "IDirect3DSurface → IDirect3DDxgiInterfaceAccess 转换失败: {e}"
        ))
    })?;
    let texture: ID3D11Texture2D = unsafe { access.GetInterface() }
        .map_err(|e| wgc_err(format!("GetInterface(ID3D11Texture2D) 失败: {e}")))?;

    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    let (frame_w, frame_h) = (desc.Width, desc.Height);
    if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
        return Err(wgc_err(format!(
            "帧格式意外: {:?}(预期 B8G8R8A8_UNORM)",
            desc.Format.0
        )));
    }
    let pixels = checked_pixels(frame_w as i32, frame_h as i32)?;
    if frame_w > MAX_CAPTURE_DIM as u32
        || frame_h > MAX_CAPTURE_DIM as u32
        || pixels > MAX_WINDOW_CAPTURE_PIXELS
    {
        return Err(ToolError::new(
            LIMIT_EXCEEDED,
            format!(
                "WGC 帧尺寸 {}x{} 超过截图上限（单边 {} px、总计 {} 像素）",
                frame_w, frame_h, MAX_CAPTURE_DIM, MAX_WINDOW_CAPTURE_PIXELS
            ),
        ));
    }

    // 帧纹理是 GPU 资源,拷贝到 CPU 可读 staging 纹理再 Map。
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    desc.MiscFlags = 0;
    let mut staging: Option<ID3D11Texture2D> = None;
    unsafe { dev.device.CreateTexture2D(&desc, None, Some(&mut staging)) }
        .map_err(|e| wgc_err(format!("创建 staging 纹理失败: {e}")))?;
    let staging = staging.expect("HRESULT 成功时纹理必已写入");
    let staging_res: ID3D11Resource = staging
        .cast()
        .map_err(|e| wgc_err(format!("staging → ID3D11Resource 转换失败: {e}")))?;
    let texture_res: ID3D11Resource = texture
        .cast()
        .map_err(|e| wgc_err(format!("帧纹理 → ID3D11Resource 转换失败: {e}")))?;
    unsafe { dev.context.CopyResource(&staging_res, &texture_res) };

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        dev.context
            .Map(&staging_res, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
    }
    .map_err(|e| wgc_err(format!("映射 staging 纹理失败: {e}")))?;
    let row_bytes = (frame_w as usize) * 4;
    let mut bgra = Vec::with_capacity(row_bytes * frame_h as usize);
    for row in 0..frame_h as usize {
        let src = unsafe {
            std::slice::from_raw_parts(
                (mapped.pData as *const u8).add(row * mapped.RowPitch as usize),
                row_bytes,
            )
        };
        bgra.extend_from_slice(src);
    }
    unsafe { dev.context.Unmap(&staging_res, 0) };

    Ok((bgra, frame_w, frame_h))
}

/// COM MTA 初始化限定在本函数:WGC 全链路在 worker 线程内完成,
/// 接口指针随闭包返回释放,之后才 CoUninitialize。
fn with_mta<T>(f: impl FnOnce() -> Result<T, ToolError>) -> Result<T, ToolError> {
    const RPC_E_CHANGED_MODE: HRESULT = HRESULT(-2147417850); // 0x80010106
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_err() && hr != RPC_E_CHANGED_MODE {
        return Err(wgc_err(format!("COM 初始化失败: 0x{:08X}", hr.0 as u32)));
    }
    let result = f();
    unsafe { CoUninitialize() };
    result
}

/// 帧关闭守卫:归还帧给池(帧不落缓存,每次调用内归还)。
struct FrameGuard(Direct3D11CaptureFrame);

impl Drop for FrameGuard {
    fn drop(&mut self) {
        let _ = self.0.Close();
    }
}

/// 一次性会话守卫(显示器路径):先停会话再关池,声明顺序先于帧守卫,
/// Drop 逆序执行保证帧先归还。
struct OneShotSessionGuard {
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
}

impl Drop for OneShotSessionGuard {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wgc_errors_carry_stable_code() {
        let err = wgc_err("测试".to_string());
        assert_eq!(err.code, WGC_UNAVAILABLE);
        assert!(!err.retryable);
    }

    #[test]
    fn frame_size_budget_is_enforced() {
        // 预算判定复用在纯函数层面验证:超限返回 LIMIT_EXCEEDED。
        // 4000x4000 = 1600 万像素,超过 1200 万单窗口预算。
        assert!(checked_pixels(4000, 4000).unwrap() > MAX_WINDOW_CAPTURE_PIXELS);
        let too_big = ToolError::new(
            LIMIT_EXCEEDED,
            format!(
                "WGC 帧尺寸 4000x4000 超过截图上限（单边 {} px、总计 {} 像素）",
                MAX_CAPTURE_DIM, MAX_WINDOW_CAPTURE_PIXELS
            ),
        );
        assert_eq!(too_big.code, LIMIT_EXCEEDED);
        assert!(!too_big.retryable);
    }

    fn frame(w: u32, h: u32, val: u8) -> Vec<u8> {
        vec![val; (w * h * 4) as usize]
    }

    #[test]
    fn diff_identical_frames_yields_no_regions() {
        let a = frame(16, 8, 42);
        let b = a.clone();
        assert!(diff_dirty_bands(&a, &b, 16, 8, 8).is_empty());
    }

    #[test]
    fn diff_single_region_returns_exact_box() {
        let a = frame(16, 8, 0);
        let mut b = a.clone();
        // 改 (x=4..=6, y=2..=3) 像素
        for y in 2..4 {
            for x in 4..7 {
                let o = (y * 16 + x) * 4;
                b[o] = 1;
                b[o + 1] = 2;
            }
        }
        assert_eq!(diff_dirty_bands(&a, &b, 16, 8, 8), vec![(4, 2, 3, 2)]);
    }

    #[test]
    fn diff_two_distant_regions_yields_two_bands() {
        let a = frame(32, 32, 0);
        let mut b = a.clone();
        for x in 0..4 {
            let top = (32 + x) * 4;
            let bottom = (30 * 32 + x) * 4;
            b[top] = 9;
            b[bottom] = 9;
        }
        let bands = diff_dirty_bands(&a, &b, 32, 32, 8);
        assert_eq!(bands.len(), 2);
        assert_eq!(bands[0], (0, 1, 4, 1));
        assert_eq!(bands[1], (0, 30, 4, 1));
    }

    #[test]
    fn diff_many_bands_coalesces_to_bounding_box() {
        // 每两行一个变化带(第 0,2,...,18 行),带数 > max_regions → 合并为外接盒
        let a = frame(8, 20, 0);
        let mut b = a.clone();
        for y in (0..20).step_by(2) {
            b[(y * 8) * 4] = 1;
        }
        let bands = diff_dirty_bands(&a, &b, 8, 20, 3);
        assert_eq!(bands, vec![(0, 0, 1, 19)]);
    }

    #[test]
    fn diff_length_mismatch_returns_full_frame_box() {
        let a = frame(8, 8, 0);
        let b = frame(8, 4, 0);
        assert_eq!(diff_dirty_bands(&a, &b, 8, 8, 8), vec![(0, 0, 8, 8)]);
    }

    #[test]
    fn crop_extracts_exact_subrect_with_frame_stride() {
        // 4x3 帧,每像素 4 字节递增写入,验证跨行距裁剪正确
        let w = 4u32;
        let h = 3u32;
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for i in 0..(w * h) as usize {
            bgra[i * 4] = i as u8;
        }
        // 裁剪 (x=1,y=1,w=2,h=1):像素索引 5、6
        let cropped = crop_bgra(&bgra, w, 1, 1, 2, 1);
        assert_eq!(cropped.len(), 8);
        assert_eq!(cropped[0], 5);
        assert_eq!(cropped[4], 6);
    }
}
