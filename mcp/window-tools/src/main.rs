use std::io::{BufRead, Write};

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde_json::{json, Value};
use windows::{
    core::{Error, BOOL},
    Win32::{
        Foundation::{HWND, LPARAM, POINT, RECT},
        Graphics::Gdi::{
            BitBlt, ClientToScreen, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC,
            DeleteObject, EnumDisplayMonitors, GetDC, GetDIBits, GetMonitorInfoW, ReleaseDC,
            SelectObject, StretchBlt, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
            HBITMAP, HDC, HGDIOBJ, HMONITOR, MONITORINFO, SRCCOPY,
        },
        Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS},
        UI::{
            HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2},
            WindowsAndMessaging::{
                EnumWindows, GetClientRect, GetForegroundWindow, GetWindowRect, GetWindowTextW,
                GetWindowThreadProcessId, IsWindowVisible, SetForegroundWindow, SetWindowPos,
                SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER,
            },
        },
    },
};

const MCP_PROTOCOL_VERSION: &str = "2025-11-25";
const MAX_WINDOWS_LIMIT: usize = 500;
/// 单张窗口图的硬上限：保留原始 4K，同时约束 BGRA 与 RGBA 双缓冲的峰值内存。
const MAX_CAPTURE_DIM: i32 = 7680;
const MAX_WINDOW_CAPTURE_PIXELS: i64 = 12_000_000;
/// `screenshot_all` 的总预算：三台 UHD 4K（3840x2160）均不缩放。
/// 超过此预算时，所有显示器使用同一比例缩放，保持相对视觉布局一致。
const MAX_ALL_CAPTURE_PIXELS: i64 = 3 * 3840 * 2160;
/// PW_RENDERFULLCONTENT=2 — 捕获窗口自身渲染内容(含标题栏,可截被遮挡窗口)。
const PW_RENDERFULLCONTENT: PRINT_WINDOW_FLAGS = PRINT_WINDOW_FLAGS(2);
/// PrintWindow 的 PW_CLIENTONLY：只渲染客户区，不包含标题栏、边框或阴影。
const PW_CLIENTONLY: PRINT_WINDOW_FLAGS = PRINT_WINDOW_FLAGS(1);

#[derive(Clone)]
struct WindowEntry {
    hwnd: isize,
    pid: u32,
    title: String,
    visible: bool,
    left: i32,
    top: i32,
    width: i32,
    height: i32,
}

fn hwnd_num(hwnd: HWND) -> isize {
    hwnd.0 as isize
}

fn hwnd_str(hwnd: HWND) -> String {
    hwnd_num(hwnd).to_string()
}

fn rect_json(rect: RECT) -> Value {
    json!({
        "left": rect.left,
        "top": rect.top,
        "width": rect.right - rect.left,
        "height": rect.bottom - rect.top
    })
}

/// 所有位图分配前先以 i64 计算面积，避免 i32 乘法溢出造成小分配/越界访问。
fn checked_pixels(width: i32, height: i32) -> Result<i64, String> {
    if width <= 0 || height <= 0 {
        return Err(format!("截图尺寸必须为正数，实际为 {width}x{height}"));
    }
    Ok(i64::from(width) * i64::from(height))
}

/// 在给定像素预算内计算输出尺寸；只缩小、不放大。
///
/// `additional_scale` 是多屏总预算得出的统一比例，保证超预算时各屏缩放一致。
fn fit_capture_dimensions(
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

/// JSON 整数不能直接 `as i32`：先验证范围，避免窗口坐标/尺寸被静默截断。
fn json_i32(args: &Value, key: &str, required: bool) -> Result<Option<i32>, String> {
    let Some(value) = args.get(key) else {
        return if required {
            Err(format!("{key} 参数缺失"))
        } else {
            Ok(None)
        };
    };
    let raw = value
        .as_i64()
        .ok_or_else(|| format!("{key} 必须是 32 位整数"))?;
    i32::try_from(raw)
        .map(Some)
        .map_err(|_| format!("{key} 超出 32 位整数范围"))
}

/// Windows PID 是 u32；拒绝超范围 JSON 数字，而不是截断后指向其他进程。
fn json_u32(args: &Value, key: &str) -> Result<Option<u32>, String> {
    let Some(value) = args.get(key) else {
        return Ok(None);
    };
    let raw = value
        .as_u64()
        .ok_or_else(|| format!("{key} 必须是非负整数"))?;
    u32::try_from(raw)
        .map(Some)
        .map_err(|_| format!("{key} 超出 Windows PID 范围"))
}

fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
    if len <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..len as usize])
}

unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let out = unsafe { &mut *(lparam.0 as *mut Vec<WindowEntry>) };
    let mut pid = 0u32;
    unsafe {
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let _ = unsafe { GetWindowRect(hwnd, &mut rect) };
    out.push(WindowEntry {
        hwnd: hwnd_num(hwnd),
        pid,
        title: window_title(hwnd),
        visible,
        left: rect.left,
        top: rect.top,
        width: rect.right - rect.left,
        height: rect.bottom - rect.top,
    });
    BOOL(1)
}

fn collect_windows() -> Vec<WindowEntry> {
    let mut all: Vec<WindowEntry> = Vec::new();
    unsafe {
        let _ = EnumWindows(
            Some(enum_windows_proc),
            LPARAM((&mut all as *mut Vec<WindowEntry>) as isize),
        );
    }
    all
}

fn list_windows(args: &Value) -> Result<Value, String> {
    let title = args.get("title").and_then(Value::as_str).unwrap_or("");
    let pid = json_u32(args, "pid")?;
    let visible_only = args
        .get("visible_only")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or(100)
        .clamp(1, MAX_WINDOWS_LIMIT);

    let mut entries = collect_windows();
    if !title.is_empty() {
        let needle = title.to_lowercase();
        entries.retain(|e| e.title.to_lowercase().contains(&needle));
    }
    if let Some(p) = pid {
        entries.retain(|e| e.pid == p);
    }
    if visible_only {
        entries.retain(|e| e.visible);
    }
    entries.sort_by(|a, b| {
        // 可见优先,不可标题为空,面积从大到小
        (b.visible as u8)
            .cmp(&(a.visible as u8))
            .then((a.title.is_empty() as u8).cmp(&(b.title.is_empty() as u8)))
            .then((b.width as i64 * b.height as i64).cmp(&(a.width as i64 * a.height as i64)))
    });
    entries.truncate(limit);

    let rows: Vec<Value> = entries
        .iter()
        .map(|e| {
            json!({
                "hwnd": e.hwnd.to_string(),
                "pid": e.pid,
                "title": e.title,
                "visible": e.visible,
                "rect": {
                    "left": e.left, "top": e.top,
                    "width": e.width, "height": e.height
                }
            })
        })
        .collect();
    let text = serde_json::to_string_pretty(&json!({
        "count": rows.len(),
        "windows": rows
    }))
    .map_err(|e| format!("JSON 序列化失败: {e}"))?;
    Ok(json!({"content": [{"type": "text", "text": text}]}))
}

// ---------------------------------------------------------------------------
// 目标窗口解析: hwnd / title / pid 三选一
// ---------------------------------------------------------------------------

fn entry_for_hwnd(hwnd: HWND) -> Result<WindowEntry, String> {
    let rect = rect_of(hwnd)?;
    let mut pid = 0u32;
    unsafe {
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    Ok(WindowEntry {
        hwnd: hwnd_num(hwnd),
        pid,
        title: window_title(hwnd),
        visible: unsafe { IsWindowVisible(hwnd) }.as_bool(),
        left: rect.left,
        top: rect.top,
        width: rect.right - rect.left,
        height: rect.bottom - rect.top,
    })
}

/// 解析 hwnd 字符串:带 0x/0X 前缀按十六进制,否则按十进制。
/// 之前的实现只是剥掉前缀后仍按十进制 parse,含 A-F 的十六进制句柄必然失败。
fn parse_hwnd_str(raw: &str, display: &str) -> Result<i64, String> {
    let invalid = || format!("hwnd '{display}' 不是有效的十进制/十六进制句柄");
    let trimmed = raw.trim();
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        let value = u64::from_str_radix(hex, 16).map_err(|_| invalid())?;
        i64::try_from(value).map_err(|_| format!("hwnd '{display}' 超出 64 位句柄范围"))
    } else {
        trimmed.parse::<i64>().map_err(|_| invalid())
    }
}

fn resolve_target(args: &Value) -> Result<HWND, String> {
    let has_hwnd = args.get("hwnd").is_some();
    let has_title = args
        .get("title")
        .and_then(Value::as_str)
        .is_some_and(|title| !title.trim().is_empty());
    let has_pid = args.get("pid").is_some();
    let selector_count = usize::from(has_hwnd) + usize::from(has_title) + usize::from(has_pid);
    // 模糊标题、PID 与 HWND 叠加会掩盖调用者意图；强制唯一定位方式。
    if selector_count != 1 {
        return Err("必须且只能提供一种定位方式：hwnd / title / pid".into());
    }

    if let Some(hs) = args.get("hwnd") {
        let raw: i64 = if let Some(s) = hs.as_str() {
            // `@active` 是显式快捷方式，不与任意标题模糊匹配混用；便于截图前后对比。
            if s.trim().eq_ignore_ascii_case("@active") {
                let hwnd = unsafe { GetForegroundWindow() };
                if hwnd.0.is_null() {
                    return Err("当前没有可用的前台窗口".into());
                }
                let entry = entry_for_hwnd(hwnd)?;
                if !entry.visible {
                    return Err("当前前台窗口不可见，无法作为 @active 目标".into());
                }
                return Ok(hwnd);
            }
            parse_hwnd_str(s, s)?
        } else if let Some(n) = hs.as_i64() {
            n
        } else {
            return Err("hwnd 必须是十进制字符串或整数".into());
        };
        let hwnd = HWND(raw as isize as *mut core::ffi::c_void);
        entry_for_hwnd(hwnd)?;
        return Ok(hwnd);
    }

    let title = args.get("title").and_then(Value::as_str).unwrap_or("");
    let pid = json_u32(args, "pid")?;

    let mut entries = collect_windows();
    if !title.is_empty() {
        let needle = title.to_lowercase();
        entries.retain(|e| e.title.to_lowercase().contains(&needle));
    }
    if let Some(p) = pid {
        entries.retain(|e| e.pid == p);
    }
    // 优先可见窗口;若确实没有可见匹配,则全部候选都可能不可见
    let visible_entries: Vec<WindowEntry> = entries.iter().filter(|e| e.visible).cloned().collect();
    let pool = if visible_entries.is_empty() {
        entries
    } else {
        visible_entries
    };
    if pool.is_empty() {
        return Err(format!(
            "未找到匹配窗口 (title='{title}', pid={})。可先运行 list_windows 查句柄",
            pid.map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string())
        ));
    }
    // 前台窗口优先,否则取面积最大的
    let foreground = unsafe { GetForegroundWindow() };
    let mut pool = pool;
    pool.sort_by(|a, b| {
        let fa = a.hwnd == hwnd_num(foreground);
        let fb = b.hwnd == hwnd_num(foreground);
        fb.cmp(&fa)
            .then((b.width as i64 * b.height as i64).cmp(&(a.width as i64 * a.height as i64)))
    });
    let best = &pool[0];
    if !best.visible {
        return Err(format!(
            "匹配到窗口但不可见: hwnd={} title='{}' 。请改用 hwnd 精确指定",
            best.hwnd, best.title
        ));
    }
    Ok(HWND(best.hwnd as *mut core::ffi::c_void))
}

/// 带副作用或单查工具不接受模糊标题/PID，避免调用方在多窗口场景误操作。
fn resolve_explicit_hwnd(args: &Value) -> Result<HWND, String> {
    if args.get("hwnd").is_none() || args.get("title").is_some() || args.get("pid").is_some() {
        return Err("此工具必须只提供 hwnd（可为 @active），不接受 title 或 pid".into());
    }
    resolve_target(args)
}

// ---------------------------------------------------------------------------
// 截图
// ---------------------------------------------------------------------------

fn rect_of(hwnd: HWND) -> Result<RECT, String> {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    unsafe { GetWindowRect(hwnd, &mut rect) }.map_err(|e| format!("GetWindowRect 失败: {e}"))?;
    Ok(rect)
}

/// 将客户区左上/右下坐标转换成虚拟桌面物理坐标，供 `client_only` 返回一致的屏幕矩形。
fn client_rect_of(hwnd: HWND) -> Result<RECT, String> {
    let mut client = RECT::default();
    unsafe { GetClientRect(hwnd, &mut client) }.map_err(|e| format!("GetClientRect 失败: {e}"))?;
    let mut top_left = POINT {
        x: client.left,
        y: client.top,
    };
    let mut bottom_right = POINT {
        x: client.right,
        y: client.bottom,
    };
    if !unsafe { ClientToScreen(hwnd, &mut top_left) }.as_bool() {
        return Err("ClientToScreen(左上) 失败".into());
    }
    if !unsafe { ClientToScreen(hwnd, &mut bottom_right) }.as_bool() {
        return Err("ClientToScreen(右下) 失败".into());
    }
    Ok(RECT {
        left: top_left.x,
        top: top_left.y,
        right: bottom_right.x,
        bottom: bottom_right.y,
    })
}

fn capture_window(hwnd: HWND, client_only: bool) -> Result<(Vec<u8>, RECT), String> {
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
    let hbm = unsafe { CreateCompatibleBitmap(hdc_screen, width, height) };
    if hbm.0.is_null() {
        unsafe {
            let _ = ReleaseDC(None, hdc_screen);
        }
        return Err("CreateCompatibleBitmap 未能分配窗口截图位图".into());
    }
    let hdc_mem = unsafe { CreateCompatibleDC(Some(hdc_screen)) };
    if hdc_mem.0.is_null() {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(hbm.0));
            let _ = ReleaseDC(None, hdc_screen);
        }
        return Err("CreateCompatibleDC 未能创建窗口截图设备上下文".into());
    }

    let flags = if client_only {
        PW_CLIENTONLY
    } else {
        PW_RENDERFULLCONTENT
    };
    let png = render_window_png(hwnd, hdc_mem, hbm, width, height, flags);
    unsafe {
        let _ = DeleteDC(hdc_mem);
        let _ = DeleteObject(HGDIOBJ(hbm.0));
        let _ = ReleaseDC(None, hdc_screen);
    }
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
        return Err("PrintWindow 失败(PW_RENDERFULLCONTENT 未成功)".into());
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

struct MonitorShot {
    label: String,
    source_width: i32,
    source_height: i32,
    output_width: i32,
    output_height: i32,
    png: Vec<u8>,
    error: Option<String>,
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

fn capture_all_monitors() -> Result<Vec<MonitorShot>, String> {
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
    unsafe {
        let _ = ReleaseDC(None, hdc_screen);
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
    let hdc_mem = unsafe { CreateCompatibleDC(Some(hdc_screen)) };
    if hdc_mem.0.is_null() {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(hbm.0));
        }
        return Err("CreateCompatibleDC 未能创建屏幕截图设备上下文".into());
    }
    let old = unsafe { SelectObject(hdc_mem, HGDIOBJ(hbm.0)) };

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
    let result = if let Err(e) = copied {
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
    };

    unsafe {
        let _ = SelectObject(hdc_mem, old);
        let _ = DeleteDC(hdc_mem);
        let _ = DeleteObject(HGDIOBJ(hbm.0));
    }
    result
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

// ---------------------------------------------------------------------------
// 调整窗口
// ---------------------------------------------------------------------------

fn resize_window(hwnd: HWND, args: &Value) -> Result<Value, String> {
    // 所有外部数值都先经 json_i32 验证；SetWindowPos 接收 i32，不能接受截断值。
    let width = json_i32(args, "width", true)?.expect("required value checked above");
    let height = json_i32(args, "height", true)?.expect("required value checked above");
    if width < 40 || height < 40 {
        return Err("width/height 不能小于 40".into());
    }
    if width > MAX_CAPTURE_DIM || height > MAX_CAPTURE_DIM {
        return Err(format!("width/height 不能超过 {}", MAX_CAPTURE_DIM));
    }
    let before = rect_of(hwnd)?;
    let x = json_i32(args, "x", false)?.unwrap_or(before.left);
    let y = json_i32(args, "y", false)?.unwrap_or(before.top);
    unsafe {
        SetWindowPos(
            hwnd,
            None,
            x,
            y,
            width,
            height,
            SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_NOACTIVATE,
        )
    }
    .map_err(|e| format!("SetWindowPos 失败: {e}"))?;
    let after = rect_of(hwnd)?;
    let text = json!({
        "hwnd": hwnd_str(hwnd),
        "requested": {"x": x, "y": y, "width": width, "height": height},
        "applied": {"left": after.left, "top": after.top, "width": after.right - after.left, "height": after.bottom - after.top}
    });
    Ok(json!({"content": [{"type": "text", "text": text.to_string()}]}))
}

/// 返回单一窗口的外框与客户区，避免在窗口很多时让模型重复读取完整列表。
fn get_window_rect(hwnd: HWND) -> Result<Value, String> {
    let entry = entry_for_hwnd(hwnd)?;
    let client = client_rect_of(hwnd)?;
    Ok(json!({
        "content": [{"type": "text", "text": json!({
            "hwnd": hwnd_str(hwnd),
            "pid": entry.pid,
            "title": entry.title,
            "visible": entry.visible,
            "window_rect": rect_json(RECT {
                left: entry.left,
                top: entry.top,
                right: entry.left + entry.width,
                bottom: entry.top + entry.height
            }),
            "client_rect": rect_json(client)
        }).to_string()}]
    }))
}

/// 请求 Windows 将一个精确 HWND 放到前台。Windows 焦点窃取保护可能拒绝该请求；
/// 失败时明确返回错误，而不是假装截图已聚焦。
fn window_activate(hwnd: HWND) -> Result<Value, String> {
    entry_for_hwnd(hwnd)?;
    if !unsafe { SetForegroundWindow(hwnd) }.as_bool() {
        return Err(format!(
            "SetForegroundWindow 拒绝激活 hwnd={}（Windows 焦点限制或窗口已失效）",
            hwnd_str(hwnd)
        ));
    }
    let foreground = unsafe { GetForegroundWindow() };
    if foreground != hwnd {
        return Err(format!(
            "窗口激活请求未生效：前台 hwnd={}，目标 hwnd={}",
            hwnd_str(foreground),
            hwnd_str(hwnd)
        ));
    }
    Ok(json!({"content": [{"type": "text", "text": json!({
        "hwnd": hwnd_str(hwnd),
        "activated": true
    }).to_string()}]}))
}

// ---------------------------------------------------------------------------
// MCP JSON-RPC (stdio, 换行分隔)
// ---------------------------------------------------------------------------

const TOOL_LIST: &str = "list_windows";
const TOOL_GET_RECT: &str = "get_window_rect";
const TOOL_SCREENSHOT_WINDOW: &str = "screenshot_window";
const TOOL_SCREENSHOT_ALL: &str = "screenshot_all";
const TOOL_RESIZE_WINDOW: &str = "resize_window";
const TOOL_ACTIVATE: &str = "window_activate";

fn jsonrpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn jsonrpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

fn mcp_tool_error(message: impl Into<String>) -> Value {
    json!({
        "content": [{"type": "text", "text": message.into()}],
        "isError": true
    })
}

fn tools_list() -> Value {
    json!({"tools": [
        {
            "name": TOOL_LIST,
            "description": "枚举当前桌面的顶层窗口,支持按标题子串或进程 PID 过滤。返回每个窗口的句柄(hwnd,十进制字符串)、pid、标题、可见性和屏幕矩形。",
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "标题子串过滤(忽略大小写)"},
                    "pid": {"type": "integer", "description": "按进程 PID 过滤"},
                    "visible_only": {"type": "boolean", "description": "只返回可见窗口,默认 true"},
                    "limit": {"type": "integer", "description": "最多返回条数,默认 100,上限 500"}
                }
            }
        },
        {
            "name": TOOL_GET_RECT,
            "description": "查询一个窗口的外框与客户区屏幕矩形。hwnd 可使用 list_windows 返回的十进制句柄，或使用 @active 查询当前前台窗口。",
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "hwnd": {"type": "string", "description": "精确窗口句柄，或 @active"}
                },
                "required": ["hwnd"],
                "additionalProperties": false
            }
        },
        {
            "name": TOOL_SCREENSHOT_WINDOW,
            "description": "截取指定窗口的渲染内容。client_only=false（默认）包含标题栏和边框；true 仅截客户区。hwnd 可为 @active；其余定位必须且只能用 hwnd / title / pid 之一。单窗口最高 1200 万像素，超限请先 resize_window。部分 GPU 加速、受保护或无响应窗口可能无法截图。",
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "hwnd": {"type": "string", "description": "窗口句柄(十进制字符串)"},
                    "title": {"type": "string", "description": "窗口标题子串"},
                    "pid": {"type": "integer", "description": "窗口所属进程 PID"},
                    "client_only": {"type": "boolean", "description": "仅截客户区，默认 false"}
                },
                "oneOf": [{"required": ["hwnd"]}, {"required": ["title"]}, {"required": ["pid"]}],
                "additionalProperties": false
            }
        },
        {
            "name": TOOL_SCREENSHOT_ALL,
            "description": "截取所有显示器（每台一张 PNG）。最高保留三台 UHD 4K（3840x2160）显示器的总像素；超过时按相同比例缩放，并返回原始与实际尺寸。",
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        },
        {
            "name": TOOL_RESIZE_WINDOW,
            "description": "调整一个由 hwnd 精确指定的窗口尺寸（可选移动到 x,y）。这是有副作用的窗口布局操作；受目标窗口自身最小尺寸约束。",
            "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "hwnd": {"type": "string", "description": "窗口句柄(十进制字符串，必须由 list_windows 返回)"},
                    "width": {"type": "integer", "minimum": 40, "description": "目标宽度"},
                    "height": {"type": "integer", "minimum": 40, "description": "目标高度"},
                    "x": {"type": "integer", "description": "可选,目标 X 坐标"},
                    "y": {"type": "integer", "description": "可选,目标 Y 坐标"}
                },
                "required": ["hwnd", "width", "height"],
                "additionalProperties": false
            }
        },
        {
            "name": TOOL_ACTIVATE,
            "description": "将一个由 hwnd 精确指定的窗口请求置为前台，用于截图前的人工对比或布局检查。Windows 焦点保护拒绝时返回错误；不发送鼠标或键盘输入。",
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "hwnd": {"type": "string", "description": "精确窗口句柄，或 @active"}
                },
                "required": ["hwnd"],
                "additionalProperties": false
            }
        }
    ]})
}

fn handle_tool_call(name: &str, args: &Value) -> Result<Value, String> {
    match name {
        TOOL_LIST => list_windows(args),
        TOOL_GET_RECT => get_window_rect(resolve_explicit_hwnd(args)?),
        TOOL_SCREENSHOT_WINDOW => {
            let hwnd = resolve_target(args)?;
            let client_only = args
                .get("client_only")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let (png, rect) = capture_window(hwnd, client_only)?;
            Ok(json!({
                "content": [
                    {"type": "text", "text": format!(
                        "窗口截图: hwnd={} client_only={} 矩形=({},{}) {}x{} 图像={} bytes",
                        hwnd_str(hwnd), client_only, rect.left, rect.top,
                        rect.right - rect.left, rect.bottom - rect.top, png.len()
                    )},
                    {"type": "image", "data": B64.encode(&png), "mimeType": "image/png"}
                ]
            }))
        }
        TOOL_SCREENSHOT_ALL => {
            let shots = capture_all_monitors()?;
            let mut content = Vec::new();
            for shot in shots {
                let dimensions = format!(
                    "原始 {}x{}，输出 {}x{}",
                    shot.source_width, shot.source_height, shot.output_width, shot.output_height
                );
                if let Some(error) = shot.error {
                    content.push(json!({"type": "text", "text": format!("{}: {dimensions}; 错误: {error}", shot.label)}));
                } else {
                    content.push(json!({"type": "text", "text": format!("{}: {dimensions}; {} bytes", shot.label, shot.png.len())}));
                    content.push(json!({"type": "image", "data": B64.encode(&shot.png), "mimeType": "image/png"}));
                }
            }
            Ok(json!({"content": content}))
        }
        TOOL_RESIZE_WINDOW => {
            let hwnd = resolve_explicit_hwnd(args)?;
            resize_window(hwnd, args)
        }
        TOOL_ACTIVATE => window_activate(resolve_explicit_hwnd(args)?),
        _ => Err(format!("未知工具 '{name}'")),
    }
}

fn handle_request(request: &Value) -> Option<Value> {
    let id = request.get("id").cloned();
    if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Some(jsonrpc_error(
            id.unwrap_or(Value::Null),
            -32600,
            "Invalid JSON-RPC request",
        ));
    }
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let id = id?;
    let response = match method {
        "initialize" => {
            let requested = request
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(MCP_PROTOCOL_VERSION);
            let protocol_version = match requested {
                "2025-11-25" | "2025-06-18" | "2025-03-26" => requested,
                _ => MCP_PROTOCOL_VERSION,
            };
            jsonrpc_result(
                id,
                json!({
                    "protocolVersion": protocol_version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "claude-code-window-tools", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": "本地窗口工具集:枚举窗口、窗口截图、全屏截图、调整窗口尺寸。进程为 PerMonitorV2 DPI 感知,坐标均为物理像素。"
                }),
            )
        }
        "ping" => jsonrpc_result(id, json!({})),
        "tools/list" => jsonrpc_result(id, tools_list()),
        "tools/call" => {
            let name = request
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let args = request
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match handle_tool_call(name, &args) {
                Ok(result) => jsonrpc_result(id, result),
                Err(message) => jsonrpc_result(id, mcp_tool_error(message)),
            }
        }
        _ => jsonrpc_error(id, -32601, "Method not found"),
    };
    Some(response)
}

fn main() {
    // 与计算机宿主一致的 DPI 感知:确保 EnumWindows/GetWindowRect/BitBlt 全部使用物理坐标
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Cannot read MCP stdin: {e}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let response = jsonrpc_error(Value::Null, -32700, format!("Parse error: {e}"));
                let _ = writeln!(stdout, "{response}");
                let _ = stdout.flush();
                continue;
            }
        };
        if let Some(response) = handle_request(&request) {
            if writeln!(stdout, "{response}")
                .and_then(|_| stdout.flush())
                .is_err()
            {
                break;
            }
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

    #[test]
    fn rejects_out_of_range_json_integers() {
        assert!(json_i32(&json!({"x": 2_147_483_648_i64}), "x", true).is_err());
        assert!(json_u32(&json!({"pid": 4_294_967_296_u64}), "pid").is_err());
    }

    #[test]
    fn parses_decimal_and_hex_hwnds() {
        assert_eq!(parse_hwnd_str("19793406", "19793406").unwrap(), 19793406);
        assert_eq!(parse_hwnd_str("0x12E05FE", "0x12E05FE").unwrap(), 19793406);
        assert_eq!(parse_hwnd_str(" 0X12e05fe ", " 0X12e05fe ").unwrap(), 19793406);
        // 十进制负值保持旧行为兼容
        assert_eq!(parse_hwnd_str("-123", "-123").unwrap(), -123);
    }

    #[test]
    fn rejects_invalid_hwnd_strings() {
        assert!(parse_hwnd_str("0xGGGG", "0xGGGG").is_err());
        assert!(parse_hwnd_str("123abc", "123abc").is_err());
        assert!(parse_hwnd_str("", "").is_err());
        // 超出 64 位句柄范围
        assert!(parse_hwnd_str("0xFFFFFFFFFFFFFFFFF", "x").is_err());
    }

    #[test]
    fn target_requires_exactly_one_selector() {
        assert!(resolve_target(&json!({})).is_err());
        assert!(resolve_target(&json!({"hwnd": "1", "pid": 1})).is_err());
    }

    #[test]
    fn explicit_hwnd_tools_reject_ambiguous_selectors() {
        assert!(resolve_explicit_hwnd(&json!({"title": "Editor"})).is_err());
        assert!(resolve_explicit_hwnd(&json!({"hwnd": "1", "pid": 1})).is_err());
    }

    #[test]
    fn publishes_window_observation_and_layout_tools() {
        let catalog = tools_list();
        let tools = catalog["tools"].as_array().unwrap();
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        for expected in [
            TOOL_LIST,
            TOOL_GET_RECT,
            TOOL_SCREENSHOT_WINDOW,
            TOOL_SCREENSHOT_ALL,
            TOOL_RESIZE_WINDOW,
            TOOL_ACTIVATE,
        ] {
            assert!(names.contains(&expected));
        }
        let screenshot = tools
            .iter()
            .find(|tool| tool["name"] == TOOL_SCREENSHOT_WINDOW)
            .unwrap();
        assert_eq!(
            screenshot["inputSchema"]["properties"]["client_only"]["type"],
            "boolean"
        );
    }
}
