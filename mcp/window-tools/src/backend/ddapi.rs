//! DDAPI (DXGI Desktop Duplication) 采集:显存零拷贝桌面复制,GPU 合成路径
//! 最快的屏幕来源 —— 与 WGC 不同,它直接复制整个显示器输出,无 WinRT 消息
//! 循环延迟,也不需要目标窗口合成帧。
//!
//! 能力与边界:
//! - **显示器级**:复制的是显示器输出,天然不含窗口归属;窗口级截图仍走
//!   WGC/PrintWindow。因此本模块服务 `screenshot_all method=ddapi` 与
//!   像素工具的 monitor 模式(找色引擎直接扫描 staging Map 内存,免整图
//!   连续拷贝)。
//! - **帧语义**:桌面变化才产帧;静态桌面 AcquireNextFrame 超时,复用上次
//!   帧缓存(语义等价)。首次调用且桌面静止时会等满帧预算。
//! - **不含光标**:DDAPI 帧不含光标位图(指针形状是独立元数据),与 BitBlt
//!   一致;需要光标请用 method=wgc。
//! - 会话失效(ACCESS_LOST:锁屏/UAC 安全桌面/驱动重置)自动重建一次;
//!   锁定会话下 DuplicateOutput 不产帧,调用方回退其他采集方式。
//! - 进程级 D3D11 设备与 WGC 共用创建路径,各模块独立缓存。
//!
//! 安全模型:桌面资源 GPU 拷贝到**每调用独立**的 staging 纹理后立刻
//! ReleaseFrame,再 Map 暴露给扫描引擎;`DupMappedFrame` Drop 时 Unmap。
//! 帧映射仅在本调用内有效,绝不过夜持有指针。

use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use windows::{
    core::Interface as _,
    Win32::{
        Foundation::RECT,
        Graphics::{
            Direct3D11::{
                ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
                D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
                D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
            },
            Dxgi::{
                Common::DXGI_FORMAT_B8G8R8A8_UNORM, CreateDXGIFactory1, IDXGIFactory1,
                IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST,
                DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_INVALID_CALL, DXGI_ERROR_WAIT_TIMEOUT,
                DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO,
            },
        },
    },
};

use crate::backend::capture::{checked_pixels, MAX_CAPTURE_DIM, MAX_WINDOW_CAPTURE_PIXELS};
use crate::backend::pixelsimd::PixelReader;
use crate::error::{ToolError, DDAPI_UNAVAILABLE, INTERNAL_ERROR, LIMIT_EXCEEDED};

/// 新帧等待预算:桌面静止时 AcquireNextFrame 以 10ms 间隔空转,预算内无帧
/// 则复用缓存/报错。首次调用静态桌面即等满此预算。
const FRAME_BUDGET_MS: u64 = 500;
const POLL_INTERVAL_MS: u64 = 10;

// ---------------------------------------------------------------------------
// 设备缓存
// ---------------------------------------------------------------------------

struct DupDevice {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
}

impl Clone for DupDevice {
    fn clone(&self) -> Self {
        Self {
            device: self.device.clone(),
            context: self.context.clone(),
        }
    }
}

static DUP_DEVICE: OnceLock<Mutex<Option<DupDevice>>> = OnceLock::new();

/// 进程级 D3D11 设备(WGC 与 DDAPI 共用创建路径,各自缓存)。
fn ensure_device() -> Result<DupDevice, ToolError> {
    let mut guard = DUP_DEVICE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        let (device, context) = crate::backend::wgc::create_device()
            .map_err(|e| ddapi_err(format!("D3D11 设备创建失败: {e}")))?;
        *guard = Some(DupDevice { device, context });
    }
    Ok(guard.as_ref().expect("上方已确保 Some").clone())
}

// ---------------------------------------------------------------------------
// 复制会话缓存(按显示器矩形)
// ---------------------------------------------------------------------------

struct DupSession {
    dupl: IDXGIOutputDuplication,
    desc: DXGI_OUTDUPL_DESC,
    rect: RECT,
    /// 上次成功帧的连续 BGRA:桌面静止时 AcquireNextFrame 超时复用。
    last_bgra: Option<Vec<u8>>,
    last_used: Instant,
}

static DUP_SESSIONS: OnceLock<Mutex<Vec<DupSession>>> = OnceLock::new();

fn dup_sessions() -> &'static Mutex<Vec<DupSession>> {
    DUP_SESSIONS.get_or_init(|| Mutex::new(Vec::new()))
}

/// 采集失败分类:会话失效(重建重试)与其他(直接上抛)。
enum AcquireFailure {
    SessionInvalid(ToolError),
    Other(ToolError),
}

// ---------------------------------------------------------------------------
// 帧类型
// ---------------------------------------------------------------------------

/// 已映射的桌面帧:staging 纹理 Map 后的 CPU 可读内存。
/// Drop 时 Unmap;映射仅在本调用内有效。零整图拷贝 —— 找色引擎直接
/// 按行距扫描(`reader()`),只有编码路径才调用 `to_bgra()` 连续化。
pub struct DupMappedFrame {
    staging: ID3D11Texture2D,
    context: ID3D11DeviceContext,
    mapped: Option<D3D11_MAPPED_SUBRESOURCE>,
    pub width: u32,
    pub height: u32,
    /// 显示器在虚拟桌面中的原点(屏幕物理坐标)。
    pub origin: (i32, i32),
}

impl DupMappedFrame {
    /// 映射内存的像素读取器(行距步进)。
    pub fn reader(&self) -> PixelReader<'_> {
        PixelReader::Strided {
            base: self.mapped.as_ref().expect("构造后必有映射").pData as *const u8,
            pitch: self.mapped.as_ref().expect("构造后必有映射").RowPitch as usize,
            buf_w: self.width,
            buf_h: self.height,
        }
    }

    /// 连续 BGRA(编码路径;去行距填充)。
    pub fn to_bgra(&self) -> Vec<u8> {
        let mapped = self.mapped.as_ref().expect("构造后必有映射");
        let row_bytes = self.width as usize * 4;
        let mut out = Vec::with_capacity(row_bytes * self.height as usize);
        for y in 0..self.height as usize {
            let src = unsafe {
                std::slice::from_raw_parts(
                    (mapped.pData as *const u8).add(y * mapped.RowPitch as usize),
                    row_bytes,
                )
            };
            out.extend_from_slice(src);
        }
        out
    }
}

impl Drop for DupMappedFrame {
    fn drop(&mut self) {
        if self.mapped.is_some() {
            if let Ok(resource) = self.staging.cast::<ID3D11Resource>() {
                unsafe {
                    self.context.Unmap(&resource, 0);
                }
            }
        }
    }
}

/// 采集产物:本帧映射内存,或桌面静止时复用的上次缓存帧。
pub enum DupFrame {
    Mapped(DupMappedFrame),
    Cached {
        bgra: Vec<u8>,
        width: u32,
        height: u32,
        origin: (i32, i32),
    },
}

impl DupFrame {
    pub fn width(&self) -> u32 {
        match self {
            DupFrame::Mapped(f) => f.width,
            DupFrame::Cached { width, .. } => *width,
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            DupFrame::Mapped(f) => f.height,
            DupFrame::Cached { height, .. } => *height,
        }
    }

    pub fn origin(&self) -> (i32, i32) {
        match self {
            DupFrame::Mapped(f) => f.origin,
            DupFrame::Cached { origin, .. } => *origin,
        }
    }

    pub fn reader(&self) -> PixelReader<'_> {
        match self {
            DupFrame::Mapped(f) => f.reader(),
            DupFrame::Cached {
                bgra,
                width,
                height,
                ..
            } => PixelReader::Contiguous {
                bgra,
                buf_w: *width,
                buf_h: *height,
            },
        }
    }

    pub fn to_bgra(&self) -> Vec<u8> {
        match self {
            DupFrame::Mapped(f) => f.to_bgra(),
            DupFrame::Cached { bgra, .. } => bgra.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// 会话建立与采集
// ---------------------------------------------------------------------------

fn rects_equal(a: RECT, b: RECT) -> bool {
    a.left == b.left && a.top == b.top && a.right == b.right && a.bottom == b.bottom
}

/// 枚举 DXGI 输出,为显示器矩形建立 DuplicateOutput 会话。
fn create_duplication(
    device: &ID3D11Device,
    monitor_rect: RECT,
) -> Result<(IDXGIOutputDuplication, DXGI_OUTDUPL_DESC), ToolError> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
        .map_err(|e| ddapi_err(format!("CreateDXGIFactory1 失败: {e}")))?;
    let mut found = None;
    'adapters: for adapter_index in 0..8u32 {
        let Ok(adapter) = (unsafe { factory.EnumAdapters(adapter_index) }) else {
            break;
        };
        for output_index in 0..32u32 {
            let Ok(output) = (unsafe { adapter.EnumOutputs(output_index) }) else {
                break;
            };
            let output1: IDXGIOutput1 = output
                .cast()
                .map_err(|e| ddapi_err(format!("IDXGIOutput → IDXGIOutput1 失败: {e}")))?;
            let Ok(desc) = (unsafe { output1.GetDesc() }) else {
                continue;
            };
            if !rects_equal(desc.DesktopCoordinates, monitor_rect) {
                continue;
            }
            let dupl = unsafe { output1.DuplicateOutput(device) }
                .map_err(|e| ddapi_err(format!("DuplicateOutput 失败: {e}")))?;
            let dup_desc = unsafe { dupl.GetDesc() };
            found = Some((dupl, dup_desc));
            break 'adapters;
        }
    }
    found.ok_or_else(|| {
        ToolError::new(
            DDAPI_UNAVAILABLE,
            "未找到与目标矩形匹配的 DXGI 输出(显示器热插拔或驱动异常)",
        )
    })
}

/// 帧预算内取新帧:桌面静止则空转到超时。会话级失败归类为 SessionInvalid。
fn acquire_raw_frame(
    dupl: &IDXGIOutputDuplication,
    budget_ms: u64,
) -> Result<(ID3D11Texture2D, DXGI_OUTDUPL_FRAME_INFO), AcquireFailure> {
    let deadline = Instant::now() + std::time::Duration::from_millis(budget_ms);
    loop {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe { dupl.AcquireNextFrame(0, &mut info, &mut resource) } {
            Ok(()) => match resource {
                Some(res) => {
                    let texture: ID3D11Texture2D = res.cast().map_err(|e| {
                        AcquireFailure::Other(ddapi_err(format!(
                            "IDXGIResource → ID3D11Texture2D 失败: {e}"
                        )))
                    })?;
                    return Ok((texture, info));
                }
                None => continue,
            },
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                if Instant::now() >= deadline {
                    return Err(AcquireFailure::Other(ToolError::new(
                        DDAPI_UNAVAILABLE,
                        format!("{budget_ms}ms 内桌面无新帧(静态桌面)且无缓存帧可复用"),
                    )));
                }
                std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
            }
            Err(e)
                if e.code() == DXGI_ERROR_ACCESS_LOST
                    || e.code() == DXGI_ERROR_INVALID_CALL
                    || e.code() == DXGI_ERROR_DEVICE_REMOVED =>
            {
                return Err(AcquireFailure::SessionInvalid(ToolError::new(
                    DDAPI_UNAVAILABLE,
                    format!("桌面复制会话失效(锁屏/安全桌面/驱动重置): {e}"),
                )));
            }
            Err(e) => {
                return Err(AcquireFailure::Other(ddapi_err(format!(
                    "AcquireNextFrame 失败: {e}"
                ))));
            }
        }
    }
}

/// 会话内采集:取帧 → CopyResource 到独立 staging → ReleaseFrame → Map。
/// 无新帧时复用会话缓存(last_bgra,桌面静止语义等价)。
fn acquire_with_session(
    device: &DupDevice,
    session: &mut DupSession,
) -> Result<DupFrame, AcquireFailure> {
    match acquire_raw_frame(&session.dupl, FRAME_BUDGET_MS) {
        Err(AcquireFailure::Other(_)) => {
            if let Some(bgra) = session.last_bgra.as_ref() {
                // 静态桌面:缓存帧即当前内容
                let (w, h) = (session.desc.ModeDesc.Width, session.desc.ModeDesc.Height);
                let cached = bgra.clone();
                session.last_used = Instant::now();
                return Ok(DupFrame::Cached {
                    bgra: cached,
                    width: w,
                    height: h,
                    origin: (session.rect.left, session.rect.top),
                });
            }
            // 无缓存:把原始错误上抛(调用方回退)
            Err(AcquireFailure::Other(ToolError::new(
                DDAPI_UNAVAILABLE,
                "桌面无新帧且无缓存帧,首次调用需桌面发生变化或改用其他采集方式",
            )))
        }
        Err(invalid @ AcquireFailure::SessionInvalid(_)) => Err(invalid),
        Ok((texture, _info)) => {
            let width = session.desc.ModeDesc.Width;
            let height = session.desc.ModeDesc.Height;
            let pixels =
                checked_pixels(width as i32, height as i32).map_err(AcquireFailure::Other)?;
            if width > MAX_CAPTURE_DIM as u32
                || height > MAX_CAPTURE_DIM as u32
                || pixels > MAX_WINDOW_CAPTURE_PIXELS
            {
                return Err(AcquireFailure::Other(ToolError::new(
                    LIMIT_EXCEEDED,
                    format!(
                        "DDAPI 帧尺寸 {width}x{height} 超过截图上限（单边 {} px、总计 {} 像素）",
                        MAX_CAPTURE_DIM, MAX_WINDOW_CAPTURE_PIXELS
                    ),
                )));
            }
            let staging = make_staging(device, width, height).map_err(AcquireFailure::Other)?;
            let staging_res: ID3D11Resource = staging
                .cast()
                .map_err(|e| AcquireFailure::Other(ddapi_err(format!("staging 转换失败: {e}"))))?;
            let texture_res: ID3D11Resource = texture
                .cast()
                .map_err(|e| AcquireFailure::Other(ddapi_err(format!("帧纹理转换失败: {e}"))))?;
            unsafe { device.context.CopyResource(&staging_res, &texture_res) };
            // 拷贝已入队,桌面资源即可归还;Map 会同步等待 GPU 完成。
            unsafe {
                let _ = session.dupl.ReleaseFrame();
            }
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            unsafe {
                device
                    .context
                    .Map(&staging_res, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            }
            .map_err(|e| AcquireFailure::Other(ddapi_err(format!("Map 失败: {e}"))))?;
            let frame = DupMappedFrame {
                staging,
                context: device.context.clone(),
                mapped: Some(mapped),
                width,
                height,
                origin: (session.rect.left, session.rect.top),
            };
            // 更新缓存:每次新帧连续化一次(~2-4ms 的 4K 内存拷贝),换得
            // 静态桌面后续调用的毫秒级复用;语义与"桌面未变"等价。
            session.last_bgra = Some(frame.to_bgra());
            session.last_used = Instant::now();
            Ok(DupFrame::Mapped(frame))
        }
    }
}

fn make_staging(device: &DupDevice, width: u32, height: u32) -> Result<ID3D11Texture2D, ToolError> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging: Option<ID3D11Texture2D> = None;
    unsafe {
        device
            .device
            .CreateTexture2D(&desc, None, Some(&mut staging))
    }
    .map_err(|e| ddapi_err(format!("创建 staging 纹理失败: {e}")))?;
    staging.ok_or_else(|| ToolError::new(INTERNAL_ERROR, "CreateTexture2D 成功但纹理为空"))
}

// ---------------------------------------------------------------------------
// 对外入口
// ---------------------------------------------------------------------------

/// 按显示器屏幕矩形采集桌面帧(DDAPI 优先)。
/// 会话按矩形缓存;失效自动重建一次(分辨率切换/锁屏恢复)。
pub fn capture_frame_for_rect(monitor_rect: RECT) -> Result<DupFrame, ToolError> {
    let device = ensure_device()?;
    let mut guard = dup_sessions().lock().unwrap_or_else(|e| e.into_inner());
    let sessions = &mut *guard;
    // 现有会话或新建
    let index = sessions
        .iter()
        .position(|s| rects_equal(s.rect, monitor_rect));
    if index.is_none() {
        let (dupl, desc) = create_duplication(&device.device, monitor_rect)?;
        crate::log_info!(
            "ddapi: 新建复制会话 ({},{} {}x{})",
            monitor_rect.left,
            monitor_rect.top,
            monitor_rect.right - monitor_rect.left,
            monitor_rect.bottom - monitor_rect.top
        );
        sessions.push(DupSession {
            dupl,
            desc,
            rect: monitor_rect,
            last_bgra: None,
            last_used: Instant::now(),
        });
    }
    for attempt in 0..2 {
        let index = sessions
            .iter()
            .position(|s| rects_equal(s.rect, monitor_rect))
            .ok_or_else(|| ToolError::new(INTERNAL_ERROR, "会话缓存状态异常"))?;
        match acquire_with_session(&device, &mut sessions[index]) {
            Ok(frame) => return Ok(frame),
            Err(AcquireFailure::SessionInvalid(error)) => {
                crate::log_debug!("ddapi: 会话失效重建: {error}");
                sessions.remove(index);
                if attempt == 0 {
                    let (dupl, desc) = create_duplication(&device.device, monitor_rect)?;
                    sessions.push(DupSession {
                        dupl,
                        desc,
                        rect: monitor_rect,
                        last_bgra: None,
                        last_used: Instant::now(),
                    });
                } else {
                    return Err(error);
                }
            }
            Err(AcquireFailure::Other(error)) => return Err(error),
        }
    }
    Err(ToolError::new(INTERNAL_ERROR, "DDAPI 采集循环异常退出"))
}

fn ddapi_err(message: String) -> ToolError {
    ToolError::new(DDAPI_UNAVAILABLE, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_equality_is_fieldwise() {
        let a = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        assert!(rects_equal(a, a));
        assert!(!rects_equal(
            a,
            RECT {
                left: 1920,
                top: 0,
                right: 3840,
                bottom: 1080
            }
        ));
    }

    #[test]
    fn ddapi_errors_carry_stable_code() {
        let err = ToolError::new(DDAPI_UNAVAILABLE, "DuplicateOutput 失败");
        assert_eq!(err.code, DDAPI_UNAVAILABLE);
        assert!(!err.retryable, "环境/能力问题,会话内重试无意义");
    }
}
