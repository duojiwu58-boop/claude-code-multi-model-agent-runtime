//! WGC (Windows Graphics Capture) 采集:DWM GPU 合成帧,实验路径。
//!
//! 与 PrintWindow 的区别:WGC 从 DWM 合成器直接取帧,GPU 加速窗口不再出
//! 近纯黑,也无需屏幕采样回退;被遮挡窗口照样产出完整内容。代价是全新的
//! 依赖面(D3D11/DXGI/WinRT 互操作)与三处能力边界:
//! - 最小化窗口不产帧(首帧轮询预算 10s + 外层 15s 硬超时兜底);
//! - DRM 保护内容被拒或黑区;
//! - 系统级 CaptureSession 存在并发上限。
//!
//! 首帧需 DWM 合成往返,TryGetNextFrame 在帧到达前以 null + S_OK 返回,
//! 故用轮询等待(见 [`wait_first_frame`])而非单次阻塞调用。
//!
//! COM 初始化与 UIA 同策略:限定在 worker 线程、用完即 CoUninitialize;
//! 帧/会话/池用 Drop 守卫按逆序关闭,panic 展开路径同样释放。

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
        },
        System::{
            Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED},
            WinRT::{
                Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
                Graphics::Capture::IGraphicsCaptureItemInterop,
            },
        },
    },
};

use crate::backend::{
    capture::{
        bgra_to_rgba_png, checked_pixels, run_with_timeout, WindowCapture, CAPTURE_TIMEOUT_MS,
        MAX_CAPTURE_DIM, MAX_WINDOW_CAPTURE_PIXELS,
    },
    window::{client_rect_of, is_window_hung, rect_of},
};
use crate::error::{ToolError, LIMIT_EXCEEDED, WGC_UNAVAILABLE, WINDOW_HUNG, WINDOW_NOT_VISIBLE};

/// WGC 采集入口:挂起预检 + 尺寸预检与 GDI 路径一致,采集在独立线程执行。
/// TryGetNextFrame 在无帧时无限等待,外层硬超时保证服务不被拖死(超时后
/// worker 可能继续阻塞,边界与 PrintWindow 路径相同)。
pub fn capture_window_wgc(hwnd: HWND, client_only: bool) -> Result<WindowCapture, ToolError> {
    if is_window_hung(hwnd) {
        return Err(ToolError::new(
            WINDOW_HUNG,
            format!(
                "窗口 hwnd={} 无响应，拒绝截图；等待窗口恢复或选择其他目标",
                hwnd.0 as isize
            ),
        ));
    }
    let raw = hwnd.0 as isize;
    run_with_timeout(
        CAPTURE_TIMEOUT_MS,
        move || wgc_capture(HWND(raw as *mut core::ffi::c_void), client_only),
        |ms| {
            ToolError::new(
                WINDOW_HUNG,
                format!("WGC 在 {ms}ms 内未产出帧，窗口可能已最小化或合成停止"),
            )
        },
    )
}

fn wgc_capture(hwnd: HWND, client_only: bool) -> Result<WindowCapture, ToolError> {
    // 尺寸预检与 GDI 路径一致:最小化窗口(矩形 -32000)直接结构化拒绝。
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
    with_mta(|| {
        let (device, context) = create_device()?;
        let direct3d_device = d3d11_to_winrt_device(&device)?;
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
            &direct3d_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )
        .map_err(|e| wgc_err(format!("创建捕获帧池失败: {e}")))?;
        let session = pool
            .CreateCaptureSession(&item)
            .map_err(|e| wgc_err(format!("创建捕获会话失败: {e}")))?;
        // 边框策略尽力而为:Win10 不支持去边框,Win11 默认去边框。
        let _ = session.SetIsBorderRequired(!client_only);
        session
            .StartCapture()
            .map_err(|e| wgc_err(format!("启动捕获失败: {e}")))?;
        // 首帧需 DWM 合成往返,池在帧到达前 TryGetNextFrame 返回 null;
        // 轮询等待至 10s 预算(外层 15s 硬超时兜底),最小化等无帧场景
        // 最终由超时路径结构化报错。
        let frame = wait_first_frame(&pool)?;
        let _pool_guard = PoolGuard(pool);
        let _session_guard = SessionGuard(session);
        let _frame_guard = FrameGuard(frame);
        let png = frame_to_png(&device, &context, &_frame_guard.0)?;
        Ok(WindowCapture {
            png,
            rect,
            fallback: None,
        })
    })
}

/// 轮询等待 WGC 首帧。TryGetNextFrame 无帧时以 null + S_OK 返回
/// (windows-core 表现为 `Err(Error::empty())`),并非协议错误;
/// 每次间隔 100ms,总预算 10s,超预算后按不可用报错。
fn wait_first_frame(
    pool: &Direct3D11CaptureFramePool,
) -> Result<Direct3D11CaptureFrame, ToolError> {
    const POLL_INTERVAL_MS: u64 = 100;
    const POLL_BUDGET_MS: u64 = 10_000;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(POLL_BUDGET_MS);
    let mut polls = 0u32;
    loop {
        match pool.TryGetNextFrame() {
            Ok(frame) => {
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
            Err(_) if std::time::Instant::now() < deadline => {
                polls += 1;
                std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
            }
            Err(e) => return Err(wgc_err(format!("等待首帧失败: {e}"))),
        }
    }
}

fn wgc_err(message: String) -> ToolError {
    ToolError::new(WGC_UNAVAILABLE, message)
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

/// 帧 → CPU 可读 BGRA → PNG。
/// WinRT Direct3DSurface 对 COM QI 返回 E_NOINTERFACE,必须经
/// IDirect3DDxgiInterfaceAccess::GetInterface 取 ID3D11Texture2D,
/// 再拷贝到 CPU 可读 staging 纹理,按行距逐行取出(行距可能大于宽度×4)。
fn frame_to_png(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    frame: &Direct3D11CaptureFrame,
) -> Result<Vec<u8>, ToolError> {
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
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) }
        .map_err(|e| wgc_err(format!("创建 staging 纹理失败: {e}")))?;
    let staging = staging.expect("HRESULT 成功时纹理必已写入");
    let staging_res: ID3D11Resource = staging
        .clone()
        .cast()
        .map_err(|e| wgc_err(format!("staging → ID3D11Resource 转换失败: {e}")))?;
    let texture_res: ID3D11Resource = texture
        .cast()
        .map_err(|e| wgc_err(format!("帧纹理 → ID3D11Resource 转换失败: {e}")))?;
    unsafe { context.CopyResource(&staging_res, &texture_res) };

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { context.Map(&staging_res, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
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
    unsafe { context.Unmap(&staging_res, 0) };

    bgra_to_rgba_png(frame_w as i32, frame_h as i32, &bgra)
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

/// WGC 资源关闭守卫:声明顺序池→会话→帧,Drop 逆序执行(帧→会话→池),
/// 与 WGC 生命周期契约一致——帧先归还,再停会话,最后关池。
struct PoolGuard(Direct3D11CaptureFramePool);
struct SessionGuard(GraphicsCaptureSession);
struct FrameGuard(Direct3D11CaptureFrame);

impl Drop for FrameGuard {
    fn drop(&mut self) {
        let _ = self.0.Close();
    }
}
impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _ = self.0.Close();
    }
}
impl Drop for PoolGuard {
    fn drop(&mut self) {
        let _ = self.0.Close();
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
}
