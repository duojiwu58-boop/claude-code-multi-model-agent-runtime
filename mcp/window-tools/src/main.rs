use std::io::{BufRead, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};

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
            Input::KeyboardAndMouse::{
                keybd_event, mouse_event, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, MOUSEEVENTF_LEFTDOWN,
                MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
                MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSE_EVENT_FLAGS,
            },
            WindowsAndMessaging::{
                EnumWindows, GetClassNameW, GetClientRect, GetCursorPos, GetForegroundWindow,
                GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
                SetCursorPos, SetForegroundWindow, SetWindowPos, SWP_NOACTIVATE, SWP_NOOWNERZORDER,
                SWP_NOZORDER,
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
/// 输入批次必须保持短小，避免单线程 stdio 服务被一个请求长期占用。
const MAX_INPUT_ACTIONS: usize = 128;
const MAX_INPUT_SCHEDULED_DELAY_MS: u64 = 60_000;
/// PW_RENDERFULLCONTENT=2 — 捕获窗口自身渲染内容(含标题栏,可截被遮挡窗口)。
const PW_RENDERFULLCONTENT: PRINT_WINDOW_FLAGS = PRINT_WINDOW_FLAGS(2);
/// PrintWindow 的 PW_CLIENTONLY：只渲染客户区，不包含标题栏、边框或阴影。
const PW_CLIENTONLY: PRINT_WINDOW_FLAGS = PRINT_WINDOW_FLAGS(1);

#[derive(Clone)]
struct WindowEntry {
    hwnd: isize,
    pid: u32,
    title: String,
    class: String,
    visible: bool,
    minimized: bool,
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

fn window_class(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    if len <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..len as usize])
}

fn entry_minimized(hwnd: HWND) -> bool {
    unsafe { IsIconic(hwnd) }.as_bool()
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
        class: window_class(hwnd),
        visible,
        minimized: entry_minimized(hwnd),
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
    let class = args.get("class").and_then(Value::as_str).unwrap_or("");
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
    if !class.is_empty() {
        let needle = class.to_lowercase();
        entries.retain(|e| e.class.to_lowercase().contains(&needle));
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
                "class": e.class,
                "visible": e.visible,
                "minimized": e.minimized,
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
        class: window_class(hwnd),
        visible: unsafe { IsWindowVisible(hwnd) }.as_bool(),
        minimized: entry_minimized(hwnd),
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
// 像素扫描 (布局验证基元)
// ---------------------------------------------------------------------------

const MAX_SCAN_BANDS: usize = 256;

/// 解析 "#RRGGBB" / "RRGGBB" 颜色，返回 (r, g, b)。
fn parse_hex_color(raw: &str) -> Result<(u8, u8, u8), String> {
    let s = raw.trim().trim_start_matches('#');
    if s.len() != 6 || !s.is_ascii() {
        return Err(format!("颜色必须是 6 位十六进制 RRGGBB，实际为 '{raw}'"));
    }
    let hex_byte = |range: std::ops::Range<usize>| {
        u8::from_str_radix(&s[range], 16).map_err(|_| format!("颜色 '{raw}' 含非法十六进制字符"))
    };
    Ok((hex_byte(0..2)?, hex_byte(2..4)?, hex_byte(4..6)?))
}

/// 从屏幕采样窗口客户区内的一个矩形区域，返回 BGRA 像素（从上到下）。
/// 坐标均为相对客户区的物理像素；被遮挡/最小化区域按屏幕当前显示内容采样。
fn capture_client_region_bgra(
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
    let hbm = unsafe { CreateCompatibleBitmap(hdc_screen, width, height) };
    if hbm.0.is_null() {
        unsafe {
            let _ = ReleaseDC(None, hdc_screen);
        }
        return Err("CreateCompatibleBitmap 未能分配扫描位图".into());
    }
    let hdc_mem = unsafe { CreateCompatibleDC(Some(hdc_screen)) };
    if hdc_mem.0.is_null() {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(hbm.0));
            let _ = ReleaseDC(None, hdc_screen);
        }
        return Err("CreateCompatibleDC 未能创建扫描设备上下文".into());
    }
    let old = unsafe { SelectObject(hdc_mem, HGDIOBJ(hbm.0)) };

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
    let result = if !copied {
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
    };

    unsafe {
        let _ = SelectObject(hdc_mem, old);
        let _ = DeleteDC(hdc_mem);
        let _ = DeleteObject(HGDIOBJ(hbm.0));
        let _ = ReleaseDC(None, hdc_screen);
    }
    result
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ColorBand {
    /// 相对扫描起点（0 基）的包含式行区间。
    y_from: u32,
    y_to: u32,
    /// 带内像素的通道平均值。
    color: (u8, u8, u8),
}

fn similar_color(a: (u8, u8, u8), b: (u8, u8, u8), tolerance: u8) -> bool {
    let t = i32::from(tolerance);
    (i32::from(a.0) - i32::from(b.0)).abs() <= t
        && (i32::from(a.1) - i32::from(b.1)).abs() <= t
        && (i32::from(a.2) - i32::from(b.2)).abs() <= t
}

/// 将一条像素列的逐行颜色合并为连续颜色带：相邻行各通道差 ≤ tolerance 归入同一带。
/// 返回的带只保留长度 ≥ min_band 的段，每带颜色取带内平均值（四舍五入）。
fn merge_color_bands(colors: &[(u8, u8, u8)], tolerance: u8, min_band: usize) -> Vec<ColorBand> {
    struct BandAcc {
        y_from: u32,
        y_to: u32,
        sum: (i64, i64, i64),
        count: i64,
        last: (u8, u8, u8),
    }
    let mut accs: Vec<BandAcc> = Vec::new();
    for (index, color) in colors.iter().enumerate() {
        let fits = accs
            .last()
            .is_some_and(|acc| similar_color(acc.last, *color, tolerance));
        if fits {
            let acc = accs.last_mut().expect("checked above");
            acc.y_to = index as u32;
            acc.sum.0 += i64::from(color.0);
            acc.sum.1 += i64::from(color.1);
            acc.sum.2 += i64::from(color.2);
            acc.count += 1;
            acc.last = *color;
        } else {
            accs.push(BandAcc {
                y_from: index as u32,
                y_to: index as u32,
                sum: (i64::from(color.0), i64::from(color.1), i64::from(color.2)),
                count: 1,
                last: *color,
            });
        }
    }
    accs.into_iter()
        .filter(|acc| (acc.y_to - acc.y_from + 1) as usize >= min_band)
        .map(|acc| {
            let round =
                |sum: i64| ((sum as f64 / acc.count as f64).round() as i32).clamp(0, 255) as u8;
            ColorBand {
                y_from: acc.y_from,
                y_to: acc.y_to,
                color: (round(acc.sum.0), round(acc.sum.1), round(acc.sum.2)),
            }
        })
        .collect()
}

/// 在窗口客户区内沿 x 指定的垂直列扫描屏幕像素，输出颜色带列表。
/// 用于布局验证：面板边界、控件高度、占位空隙一类的断言。
fn pixel_scan_line(hwnd: HWND, args: &Value) -> Result<Value, String> {
    let x = json_i32(args, "x", true)?.expect("required value checked above");
    let y_from = json_i32(args, "y_from", true)?.expect("required value checked above");
    let y_to = json_i32(args, "y_to", true)?.expect("required value checked above");
    if y_to <= y_from {
        return Err("y_to 必须大于 y_from".into());
    }
    let scan_height = y_to
        .checked_sub(y_from)
        .ok_or("y_to - y_from 超出 32 位整数范围")?;
    let tolerance = json_i32(args, "tolerance", false)?
        .unwrap_or(2)
        .clamp(0, 255) as u8;
    let min_band = json_i32(args, "min_band", false)?
        .unwrap_or(2)
        .clamp(1, 65535) as usize;
    let max_bands = json_i32(args, "max_bands", false)?
        .unwrap_or(MAX_SCAN_BANDS as i32)
        .clamp(1, MAX_SCAN_BANDS as i32) as usize;

    let bgra = capture_client_region_bgra(hwnd, x, y_from, 1, scan_height)?;
    // BGRA → (r,g,b)
    let colors: Vec<(u8, u8, u8)> = bgra
        .chunks_exact(4)
        .map(|px| (px[2], px[1], px[0]))
        .collect();
    let mut bands = merge_color_bands(&colors, tolerance, min_band);
    let truncated = bands.len() > max_bands;
    bands.truncate(max_bands);

    let rows: Vec<Value> = bands
        .iter()
        .map(|band| {
            let (y_lo, y_hi) = (y_from + band.y_from as i32, y_from + band.y_to as i32);
            json!({
                "y_from": y_lo,
                "y_to": y_hi,
                "height": y_hi - y_lo + 1,
                "color": format!("#{:02X}{:02X}{:02X}", band.color.0, band.color.1, band.color.2)
            })
        })
        .collect();

    Ok(json!({
        "content": [{"type": "text", "text": json!({
            "hwnd": hwnd_str(hwnd),
            "x": x,
            "y_from": y_from,
            "y_to": y_to,
            "tolerance": tolerance,
            "min_band": min_band,
            "band_count": rows.len(),
            "truncated": truncated,
            "bands": rows
        }).to_string()}]
    }))
}

/// 在窗口客户区的矩形区域内查找与目标颜色匹配的像素，返回计数与最小包围盒。
fn pixel_find_color(hwnd: HWND, args: &Value) -> Result<Value, String> {
    let color_arg = args
        .get("color")
        .and_then(Value::as_str)
        .ok_or("color 参数缺失（#RRGGBB 格式）")?;
    let (r, g, b) = parse_hex_color(color_arg)?;
    let tolerance = json_i32(args, "tolerance", false)?
        .unwrap_or(0)
        .clamp(0, 255) as u8;
    let x = json_i32(args, "x", true)?.expect("required value checked above");
    let y = json_i32(args, "y", true)?.expect("required value checked above");
    let width = json_i32(args, "width", true)?.expect("required value checked above");
    let height = json_i32(args, "height", true)?.expect("required value checked above");

    let bgra = capture_client_region_bgra(hwnd, x, y, width, height)?;
    let mut count: i64 = 0;
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;
    for (index, px) in bgra.chunks_exact(4).enumerate() {
        let (px_r, px_g, px_b) = (px[2], px[1], px[0]);
        if (i32::from(px_r) - i32::from(r)).abs() <= i32::from(tolerance)
            && (i32::from(px_g) - i32::from(g)).abs() <= i32::from(tolerance)
            && (i32::from(px_b) - i32::from(b)).abs() <= i32::from(tolerance)
        {
            let (cx, cy) = (index % width as usize, index / width as usize);
            count += 1;
            min_x = min_x.min(cx as i32);
            min_y = min_y.min(cy as i32);
            max_x = max_x.max(cx as i32);
            max_y = max_y.max(cy as i32);
        }
    }

    let bbox = if count > 0 {
        Some(json!({
            "left": x + min_x,
            "top": y + min_y,
            "width": max_x - min_x + 1,
            "height": max_y - min_y + 1
        }))
    } else {
        None
    };

    Ok(json!({
        "content": [{"type": "text", "text": json!({
            "hwnd": hwnd_str(hwnd),
            "color": format!("#{:02X}{:02X}{:02X}", r, g, b),
            "tolerance": tolerance,
            "match_count": count,
            "bbox": bbox
        }).to_string()}]
    }))
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
// 鼠标/键盘输入注入
// ---------------------------------------------------------------------------

/// 解析虚拟键码：整数 0..255，或常用按键名（ASCII 字母/数字、Enter、F1..F24 等）。
/// 只支持按键名是为了让模型可读；组合键请用多个 key_down/key_up 动作。
fn parse_vkey(value: &Value) -> Result<u8, String> {
    if let Some(n) = value.as_i64() {
        return u8::try_from(n).map_err(|_| format!("vkey 超出 0..255 范围: {n}"));
    }
    let name = value
        .as_str()
        .ok_or("vkey 必须是整数或命名按键")?
        .trim()
        .to_uppercase();
    match name.as_str() {
        "ENTER" | "RETURN" => Ok(0x0D),
        "SPACE" => Ok(0x20),
        "TAB" => Ok(0x09),
        "ESC" | "ESCAPE" => Ok(0x1B),
        "BACKSPACE" | "BACK" => Ok(0x08),
        "DELETE" | "DEL" => Ok(0x2E),
        "INSERT" | "INS" => Ok(0x2D),
        "HOME" => Ok(0x24),
        "END" => Ok(0x23),
        "PAGEUP" | "PGUP" => Ok(0x21),
        "PAGEDOWN" | "PGDN" => Ok(0x22),
        "LEFT" => Ok(0x25),
        "RIGHT" => Ok(0x27),
        "UP" => Ok(0x26),
        "DOWN" => Ok(0x28),
        "CTRL" | "CONTROL" => Ok(0x11),
        "SHIFT" => Ok(0x10),
        "ALT" | "MENU" => Ok(0x12),
        "WIN" | "WINDOWS" => Ok(0x5B),
        _ => {
            let bytes = name.as_bytes();
            if bytes.len() == 1 && bytes[0].is_ascii() {
                return Ok(bytes[0]); // A..Z → 0x41.., 0..9 → 0x30..
            }
            if name.len() >= 2
                && name.starts_with('F')
                && name[1..].bytes().all(|b| b.is_ascii_digit())
            {
                if let Ok(n) = name[1..].parse::<u8>() {
                    if (1..=24).contains(&n) {
                        return Ok(0x6F + n); // F1 → 0x70
                    }
                }
            }
            Err(format!("未知按键名 '{name}'"))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            "middle" => Ok(Self::Middle),
            _ => Err(format!("未知鼠标按钮 '{raw}'（left/right/middle）")),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Middle => "middle",
        }
    }

    fn flags(self) -> (MOUSE_EVENT_FLAGS, MOUSE_EVENT_FLAGS) {
        match self {
            Self::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
            Self::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
            Self::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreparedInputAction {
    Move { x: i32, y: i32, steps: i32 },
    Click { x: i32, y: i32, button: MouseButton },
    Down { x: i32, y: i32, button: MouseButton },
    Up { x: i32, y: i32, button: MouseButton },
    Scroll { x: i32, y: i32, delta: i32 },
    KeyTap { vkey: u8 },
    KeyDown { vkey: u8 },
    KeyUp { vkey: u8 },
}

fn validate_client_point(x: i32, y: i32, width: i32, height: i32) -> Result<(), String> {
    if x < 0 || y < 0 || x >= width || y >= height {
        return Err(format!(
            "输入坐标 ({x},{y}) 超出窗口客户区 {width}x{height}"
        ));
    }
    Ok(())
}

fn action_position(
    action: &Value,
    client_width: i32,
    client_height: i32,
) -> Result<(i32, i32), String> {
    let x = json_i32(action, "x", true)?.expect("required value checked above");
    let y = json_i32(action, "y", true)?.expect("required value checked above");
    validate_client_point(x, y, client_width, client_height)?;
    Ok((x, y))
}

fn scheduled_input_delay_ms(actions: &[PreparedInputAction], delay_ms: u64) -> Option<u64> {
    let between_actions = delay_ms.checked_mul(actions.len().saturating_sub(1) as u64)?;
    actions.iter().try_fold(between_actions, |total, action| {
        let move_delay = match action {
            PreparedInputAction::Move { steps, .. } => {
                delay_ms.max(10).checked_mul(*steps as u64)?
            }
            _ => 0,
        };
        total.checked_add(move_delay)
    })
}

/// 在产生任何真实输入前解析并验证整个批次。
/// 按下/释放必须在同一批次内配平，避免跨请求或错误路径留下全局输入状态。
fn prepare_input_actions(
    actions: &[Value],
    client_width: i32,
    client_height: i32,
    delay_ms: u64,
) -> Result<(Vec<PreparedInputAction>, u64), String> {
    if actions.is_empty() {
        return Err("actions 不能为空".into());
    }
    if actions.len() > MAX_INPUT_ACTIONS {
        return Err(format!(
            "actions 数量不能超过 {MAX_INPUT_ACTIONS}，实际为 {}",
            actions.len()
        ));
    }

    let mut prepared = Vec::with_capacity(actions.len());
    let mut held_buttons: Vec<MouseButton> = Vec::new();
    let mut held_keys: Vec<u8> = Vec::new();
    for (index, action) in actions.iter().enumerate() {
        let kind = action
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("动作 {index} 必须有 kind"))?;
        let parsed = match kind {
            "move" => {
                let (x, y) = action_position(action, client_width, client_height)?;
                let steps = json_i32(action, "steps", false)?.unwrap_or(1);
                if !(1..=200).contains(&steps) {
                    return Err(format!("动作 {index} 的 steps 必须在 1..200 范围内"));
                }
                PreparedInputAction::Move { x, y, steps }
            }
            "click" | "down" | "up" => {
                let (x, y) = action_position(action, client_width, client_height)?;
                let button = MouseButton::parse(
                    action
                        .get("button")
                        .and_then(Value::as_str)
                        .unwrap_or("left"),
                )?;
                if kind == "down" {
                    if held_buttons.contains(&button) {
                        return Err(format!(
                            "动作 {index} 重复按下 {}，必须先释放",
                            button.name()
                        ));
                    }
                    held_buttons.push(button);
                    PreparedInputAction::Down { x, y, button }
                } else if kind == "up" {
                    let Some(position) = held_buttons.iter().position(|held| *held == button)
                    else {
                        return Err(format!(
                            "动作 {index} 释放了未在本批次按下的 {}",
                            button.name()
                        ));
                    };
                    held_buttons.remove(position);
                    PreparedInputAction::Up { x, y, button }
                } else {
                    if held_buttons.contains(&button) {
                        return Err(format!(
                            "动作 {index} 不能 click 已由 down 按住的 {}",
                            button.name()
                        ));
                    }
                    PreparedInputAction::Click { x, y, button }
                }
            }
            "scroll" => {
                let (x, y) = action_position(action, client_width, client_height)?;
                let delta = json_i32(action, "delta", true)?.expect("required value checked above");
                PreparedInputAction::Scroll { x, y, delta }
            }
            "key_tap" | "key_down" | "key_up" => {
                let vkey = parse_vkey(
                    action
                        .get("vkey")
                        .ok_or_else(|| format!("动作 {index} 缺少 vkey"))?,
                )?;
                if kind == "key_down" {
                    if held_keys.contains(&vkey) {
                        return Err(format!("动作 {index} 重复按下 vkey 0x{vkey:02X}"));
                    }
                    held_keys.push(vkey);
                    PreparedInputAction::KeyDown { vkey }
                } else if kind == "key_up" {
                    let Some(position) = held_keys.iter().position(|held| *held == vkey) else {
                        return Err(format!(
                            "动作 {index} 释放了未在本批次按下的 vkey 0x{vkey:02X}"
                        ));
                    };
                    held_keys.remove(position);
                    PreparedInputAction::KeyUp { vkey }
                } else {
                    if held_keys.contains(&vkey) {
                        return Err(format!(
                            "动作 {index} 不能 key_tap 已由 key_down 按住的 vkey 0x{vkey:02X}"
                        ));
                    }
                    PreparedInputAction::KeyTap { vkey }
                }
            }
            other => return Err(format!("未知动作 kind '{other}'")),
        };
        prepared.push(parsed);
    }

    if !held_buttons.is_empty() || !held_keys.is_empty() {
        return Err("本批次存在未配平的 down/key_down；必须在同一批次内发送对应 up/key_up".into());
    }
    let scheduled_delay =
        scheduled_input_delay_ms(&prepared, delay_ms).ok_or("输入批次预计耗时计算溢出")?;
    if scheduled_delay > MAX_INPUT_SCHEDULED_DELAY_MS {
        return Err(format!(
            "输入批次计划延迟 {scheduled_delay}ms 超过上限 {MAX_INPUT_SCHEDULED_DELAY_MS}ms"
        ));
    }
    Ok((prepared, scheduled_delay))
}

struct InputReleaseGuard {
    buttons: Vec<MouseButton>,
    keys: Vec<u8>,
}

impl InputReleaseGuard {
    fn new() -> Self {
        Self {
            buttons: Vec::new(),
            keys: Vec::new(),
        }
    }

    fn mouse_down(&mut self, button: MouseButton) {
        unsafe { mouse_event(button.flags().0, 0, 0, 0, 0) };
        self.buttons.push(button);
    }

    fn mouse_up(&mut self, button: MouseButton) {
        unsafe { mouse_event(button.flags().1, 0, 0, 0, 0) };
        if let Some(position) = self.buttons.iter().position(|held| *held == button) {
            self.buttons.remove(position);
        }
    }

    fn key_down(&mut self, vkey: u8) {
        unsafe { keybd_event(vkey, 0, KEYBD_EVENT_FLAGS(0), 0) };
        self.keys.push(vkey);
    }

    fn key_up(&mut self, vkey: u8) {
        unsafe { keybd_event(vkey, 0, KEYEVENTF_KEYUP, 0) };
        if let Some(position) = self.keys.iter().position(|held| *held == vkey) {
            self.keys.remove(position);
        }
    }
}

impl Drop for InputReleaseGuard {
    fn drop(&mut self) {
        for vkey in self.keys.drain(..).rev() {
            unsafe { keybd_event(vkey, 0, KEYEVENTF_KEYUP, 0) };
        }
        for button in self.buttons.drain(..).rev() {
            unsafe { mouse_event(button.flags().1, 0, 0, 0, 0) };
        }
    }
}

/// 把屏幕坐标位置的指针按 steps 步插值移动；已按下按钮时即为拖拽。
fn move_cursor_steps(
    hwnd: HWND,
    from: POINT,
    to: POINT,
    steps: i32,
    per_step_ms: u64,
) -> Result<(), String> {
    let mut current = from;
    for step in 1..=steps {
        ensure_input_foreground(hwnd)?;
        let f = f64::from(step) / f64::from(steps);
        current.x = from.x + ((f64::from(to.x) - f64::from(from.x)) * f) as i32;
        current.y = from.y + ((f64::from(to.y) - f64::from(from.y)) * f) as i32;
        unsafe {
            SetCursorPos(current.x, current.y).map_err(|e| format!("SetCursorPos 失败: {e}"))?;
        }
        std::thread::sleep(std::time::Duration::from_millis(per_step_ms));
    }
    Ok(())
}

fn client_to_screen_checked(hwnd: HWND, x: i32, y: i32) -> Result<POINT, String> {
    let client = client_rect_of(hwnd)?;
    validate_client_point(x, y, client.right - client.left, client.bottom - client.top)?;
    Ok(POINT {
        x: client.left + x,
        y: client.top + y,
    })
}

fn ensure_input_foreground(hwnd: HWND) -> Result<(), String> {
    let foreground = unsafe { GetForegroundWindow() };
    if foreground != hwnd {
        return Err(format!(
            "输入执行前目标窗口失去前台状态：前台 hwnd={}，目标 hwnd={}",
            hwnd_str(foreground),
            hwnd_str(hwnd)
        ));
    }
    Ok(())
}

/// 向指定窗口发送鼠标/键盘输入。默认要求目标窗口已是前台窗口；
/// activate:true 可先请求置前（Windows 焦点保护可能拒绝）。
/// 所有坐标为窗口客户区物理像素；建议动作间保持 delay_ms。
fn send_input(hwnd: HWND, args: &Value) -> Result<Value, String> {
    let entry = entry_for_hwnd(hwnd)?;
    if !entry.visible {
        return Err(format!(
            "窗口 hwnd={} 不可见（可能已最小化），拒绝注入输入",
            hwnd_str(hwnd)
        ));
    }
    let activate = args
        .get("activate")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if activate {
        if !unsafe { SetForegroundWindow(hwnd) }.as_bool() {
            return Err(format!(
                "SetForegroundWindow 拒绝激活 hwnd={}（Windows 焦点保护或窗口已失效）",
                hwnd_str(hwnd)
            ));
        }
        if unsafe { GetForegroundWindow() } != hwnd {
            return Err("窗口激活请求未生效，请改用 window_activate".into());
        }
    } else if unsafe { GetForegroundWindow() } != hwnd {
        return Err(format!(
            "目标窗口 hwnd={} 不是前台窗口，输入将发送到其他窗口；请先调用 window_activate 或设置 activate:true",
            hwnd_str(hwnd)
        ));
    }

    let actions = args
        .get("actions")
        .and_then(Value::as_array)
        .ok_or("actions 必须是数组")?;
    let delay_raw = json_i32(args, "delay_ms", false)?.unwrap_or(50);
    if !(0..=5000).contains(&delay_raw) {
        return Err("delay_ms 必须在 0..5000 范围内".into());
    }
    let delay_ms = delay_raw as u64;
    let client = client_rect_of(hwnd)?;
    let (prepared, scheduled_delay_ms) = prepare_input_actions(
        actions,
        client.right - client.left,
        client.bottom - client.top,
        delay_ms,
    )?;

    let mut reported: Vec<Value> = Vec::new();
    let mut release_guard = InputReleaseGuard::new();
    for (index, action) in prepared.iter().enumerate() {
        if index > 0 && delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        }
        ensure_input_foreground(hwnd)?;

        match *action {
            PreparedInputAction::Move { x, y, steps } => {
                let target = client_to_screen_checked(hwnd, x, y)?;
                let mut from = POINT { x: 0, y: 0 };
                unsafe {
                    GetCursorPos(&mut from).map_err(|e| format!("GetCursorPos 失败: {e}"))?;
                }
                move_cursor_steps(hwnd, from, target, steps, delay_ms.max(10))?;
                reported.push(json!({"kind": "move", "steps": steps}));
            }
            PreparedInputAction::Click { x, y, button } => {
                let target = client_to_screen_checked(hwnd, x, y)?;
                unsafe {
                    SetCursorPos(target.x, target.y)
                        .map_err(|e| format!("SetCursorPos 失败: {e}"))?;
                }
                release_guard.mouse_down(button);
                release_guard.mouse_up(button);
                reported.push(json!({"kind": "click", "button": button.name()}));
            }
            PreparedInputAction::Down { x, y, button } => {
                let target = client_to_screen_checked(hwnd, x, y)?;
                unsafe {
                    SetCursorPos(target.x, target.y)
                        .map_err(|e| format!("SetCursorPos 失败: {e}"))?;
                }
                release_guard.mouse_down(button);
                reported.push(json!({"kind": "down", "button": button.name()}));
            }
            PreparedInputAction::Up { x, y, button } => {
                let target = client_to_screen_checked(hwnd, x, y)?;
                unsafe {
                    SetCursorPos(target.x, target.y)
                        .map_err(|e| format!("SetCursorPos 失败: {e}"))?;
                }
                release_guard.mouse_up(button);
                reported.push(json!({"kind": "up", "button": button.name()}));
            }
            PreparedInputAction::Scroll { x, y, delta } => {
                let target = client_to_screen_checked(hwnd, x, y)?;
                unsafe {
                    SetCursorPos(target.x, target.y)
                        .map_err(|e| format!("SetCursorPos 失败: {e}"))?;
                    mouse_event(MOUSEEVENTF_WHEEL, 0, 0, delta, 0);
                }
                reported.push(json!({"kind": "scroll", "delta": delta}));
            }
            PreparedInputAction::KeyTap { vkey } => {
                release_guard.key_down(vkey);
                release_guard.key_up(vkey);
                reported.push(json!({"kind": "key_tap", "vkey": format!("0x{vkey:02X}")}));
            }
            PreparedInputAction::KeyDown { vkey } => {
                release_guard.key_down(vkey);
                reported.push(json!({"kind": "key_down", "vkey": format!("0x{vkey:02X}")}));
            }
            PreparedInputAction::KeyUp { vkey } => {
                release_guard.key_up(vkey);
                reported.push(json!({"kind": "key_up", "vkey": format!("0x{vkey:02X}")}));
            }
        }
    }

    Ok(json!({
        "content": [{"type": "text", "text": json!({
            "hwnd": hwnd_str(hwnd),
            "foreground_verified": true,
            "action_count": reported.len(),
            "scheduled_delay_ms": scheduled_delay_ms,
            "actions": reported
        }).to_string()}]
    }))
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
const TOOL_PIXEL_SCAN_LINE: &str = "pixel_scan_line";
const TOOL_PIXEL_FIND_COLOR: &str = "pixel_find_color";
const TOOL_SEND_INPUT: &str = "send_input";

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
            "description": "枚举当前桌面的顶层窗口,支持按标题子串、窗口类名或进程 PID 过滤。返回每个窗口的句柄(hwnd,十进制字符串)、pid、标题、类名、可见性、最小化状态和屏幕矩形。",
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "标题子串过滤(忽略大小写)"},
                    "class": {"type": "string", "description": "窗口类名子串过滤(忽略大小写)"},
                    "pid": {"type": "integer", "description": "按进程 PID 过滤"},
                    "visible_only": {"type": "boolean", "description": "只返回可见窗口,默认 true"},
                    "limit": {"type": "integer", "description": "最多返回条数,默认 100,上限 500"}
                },
                "additionalProperties": false
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
        },
        {
            "name": TOOL_PIXEL_SCAN_LINE,
            "description": "在窗口客户区内沿指定垂直列扫描屏幕像素，将颜色相近的连续行合并为颜色带（每个带含起止行、高度和平均颜色）。用于布局验证：测量面板边界、控件高度、空白占位。坐标均为相对客户区的物理像素；窗口被遮挡的区域按屏幕当前显示内容采样。",
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "hwnd": {"type": "string", "description": "精确窗口句柄，或 @active"},
                    "x": {"type": "integer", "description": "扫描列（客户区 X）"},
                    "y_from": {"type": "integer", "description": "起始行（客户区 Y，含）"},
                    "y_to": {"type": "integer", "description": "结束行（客户区 Y，不含）"},
                    "tolerance": {"type": "integer", "minimum": 0, "maximum": 255, "description": "相邻行颜色差阈值，默认 2"},
                    "min_band": {"type": "integer", "minimum": 1, "description": "小于该高度的颜色带被忽略，默认 2"},
                    "max_bands": {"type": "integer", "minimum": 1, "maximum": 256, "description": "最多返回颜色带数，默认 256"}
                },
                "required": ["hwnd", "x", "y_from", "y_to"],
                "additionalProperties": false
            }
        },
        {
            "name": TOOL_PIXEL_FIND_COLOR,
            "description": "在窗口客户区矩形区域内查找与指定颜色匹配的像素，返回匹配数量和最小包围盒。可与 pixel_scan_line 配合做布局断言；color 为 #RRGGBB，坐标均为相对客户区的物理像素。",
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "hwnd": {"type": "string", "description": "精确窗口句柄，或 @active"},
                    "x": {"type": "integer", "description": "区域左上角（客户区 X）"},
                    "y": {"type": "integer", "description": "区域左上角（客户区 Y）"},
                    "width": {"type": "integer", "minimum": 1, "description": "区域宽度"},
                    "height": {"type": "integer", "minimum": 1, "description": "区域高度"},
                    "color": {"type": "string", "description": "目标颜色 #RRGGBB"},
                    "tolerance": {"type": "integer", "minimum": 0, "maximum": 255, "description": "每通道颜色差阈值，默认 0"}
                },
                "required": ["hwnd", "x", "y", "width", "height", "color"],
                "additionalProperties": false
            }
        },
        {
            "name": TOOL_SEND_INPUT,
            "description": "向指定窗口发送鼠标/键盘输入。目标必须保持为前台窗口，每个鼠标坐标必须位于客户区；activate:true 可先请求置前。动作先整体校验再执行，down/key_down 必须在同一批次内由 up/key_up 配平，异常路径会自动释放。每批最多 128 个动作且计划延迟不超过 60 秒。",
            "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "hwnd": {"type": "string", "description": "精确窗口句柄，或 @active"},
                    "activate": {"type": "boolean", "description": "先请求激活目标窗口再注入，默认 false"},
                    "delay_ms": {"type": "integer", "minimum": 0, "maximum": 5000, "description": "动作间延迟，默认 50"},
                    "actions": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 128,
                        "items": {
                            "type": "object",
                            "properties": {
                                "kind": {"type": "string", "enum": ["move", "click", "down", "up", "scroll", "key_tap", "key_down", "key_up"]},
                                "x": {"type": "integer", "description": "客户区 X（鼠标动作必填）"},
                                "y": {"type": "integer", "description": "客户区 Y（鼠标动作必填）"},
                                "button": {"type": "string", "enum": ["left", "right", "middle"], "description": "鼠标键，默认 left"},
                                "delta": {"type": "integer", "description": "scroll：滚轮增量（正=上滚）"},
                                "vkey": {"type": ["integer", "string"], "description": "虚拟键码或按键名（key_tap/key_down/key_up 必填）"},
                                "steps": {"type": "integer", "minimum": 1, "maximum": 200, "description": "move 插值步数，默认 1"}
                            },
                            "required": ["kind"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["hwnd", "actions"],
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
        TOOL_PIXEL_SCAN_LINE => pixel_scan_line(resolve_explicit_hwnd(args)?, args),
        TOOL_PIXEL_FIND_COLOR => pixel_find_color(resolve_explicit_hwnd(args)?, args),
        TOOL_SEND_INPUT => send_input(resolve_explicit_hwnd(args)?, args),
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
                    "instructions": "本地窗口工具集:枚举窗口、窗口截图、全屏截图、调整窗口尺寸、像素扫描(布局验证)、鼠标键盘输入。进程为 PerMonitorV2 DPI 感知,坐标均为物理像素。"
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

/// 提取 panic 负载的文本；非字符串负载给出占位说明。
/// 注意接收 `&Box<dyn Any>` 而不是 `&dyn Any`：前者经 Deref 强制转换后
/// downcast_ref 会静默失败（实测），后者与 std panic hook 的形态一致但暴露
/// 不出 Box 的自带方法，这里直接对 Box 调 downcast_ref 最稳。
fn panic_payload_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "未知 panic 负载".to_string()
}

/// 单请求隔离：任何 panic 都转成 JSON-RPC error 并继续服务，
/// 避免一次未预期的崩溃把整个 stdio 会话带走（此前表现为 Connection closed）。
fn handle_request_isolated(request: &Value) -> Option<Value> {
    match catch_unwind(AssertUnwindSafe(|| handle_request(request))) {
        Ok(response) => response,
        Err(payload) => Some(jsonrpc_error(
            request.get("id").cloned().unwrap_or(Value::Null),
            -32603,
            format!(
                "请求发生可展开的 Rust panic，已隔离（进程继续运行）: {}",
                panic_payload_message(&payload)
            ),
        )),
    }
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
        if let Some(response) = handle_request_isolated(&request) {
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
        assert_eq!(
            parse_hwnd_str(" 0X12e05fe ", " 0X12e05fe ").unwrap(),
            19793406
        );
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
            TOOL_PIXEL_SCAN_LINE,
            TOOL_PIXEL_FIND_COLOR,
            TOOL_SEND_INPUT,
        ] {
            assert!(names.contains(&expected), "缺少工具 {expected}");
        }
        let screenshot = tools
            .iter()
            .find(|tool| tool["name"] == TOOL_SCREENSHOT_WINDOW)
            .unwrap();
        assert_eq!(
            screenshot["inputSchema"]["properties"]["client_only"]["type"],
            "boolean"
        );
        let list = tools.iter().find(|tool| tool["name"] == TOOL_LIST).unwrap();
        assert_eq!(list["inputSchema"]["properties"]["class"]["type"], "string");
        // 新扫描工具要求 hwnd 精确指定
        for name in [TOOL_PIXEL_SCAN_LINE, TOOL_PIXEL_FIND_COLOR, TOOL_SEND_INPUT] {
            let scan = tools.iter().find(|tool| tool["name"] == name).unwrap();
            assert!(scan["inputSchema"]["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "hwnd"));
            assert!(scan["annotations"]["destructiveHint"] == json!(name == TOOL_SEND_INPUT));
        }
        let input = tools
            .iter()
            .find(|tool| tool["name"] == TOOL_SEND_INPUT)
            .unwrap();
        assert_eq!(
            input["inputSchema"]["properties"]["actions"]["maxItems"],
            MAX_INPUT_ACTIONS
        );
    }

    #[test]
    fn parses_hex_colors_with_and_without_hash() {
        assert_eq!(parse_hex_color("#FAFAFA").unwrap(), (250, 250, 250));
        assert_eq!(parse_hex_color("F5F5F5").unwrap(), (245, 245, 245));
        assert_eq!(parse_hex_color(" #ff00AA ").unwrap(), (255, 0, 170));
        assert!(parse_hex_color("#FFF").is_err());
        assert!(parse_hex_color("GGGGGG").is_err());
        assert!(parse_hex_color("FAFAF").is_err());
        assert!(parse_hex_color("中文中文").is_err());
    }

    #[test]
    fn merges_adjacent_similar_rows_into_bands() {
        let colors: Vec<(u8, u8, u8)> = vec![
            (250, 250, 250),
            (250, 250, 250),
            (252, 252, 252),
            (245, 245, 245),
            (245, 245, 245),
        ];
        let bands = merge_color_bands(&colors, 2, 1);
        assert_eq!(bands.len(), 2);
        assert_eq!(bands[0].y_from, 0);
        assert_eq!(bands[0].y_to, 2);
        assert_eq!(bands[0].color, (251, 251, 251)); // 250+250+252 平均
        assert_eq!(bands[1].y_from, 3);
        assert_eq!(bands[1].y_to, 4);
        // min_band 过滤过短的颜色带
        let filtered = merge_color_bands(&colors, 2, 3);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].y_from, 0);
        // 相邻行各通道差超过 tolerance 时不合并
        let split: Vec<(u8, u8, u8)> = vec![(0, 0, 0), (10, 0, 0)];
        assert_eq!(merge_color_bands(&split, 0, 1).len(), 2);
        assert_eq!(merge_color_bands(&split, 11, 1).len(), 1);
    }

    #[test]
    fn parses_common_vkey_names() {
        assert_eq!(parse_vkey(&json!("Enter")).unwrap(), 0x0D);
        assert_eq!(parse_vkey(&json!("A")).unwrap(), 0x41);
        assert_eq!(parse_vkey(&json!("7")).unwrap(), 0x37);
        assert_eq!(parse_vkey(&json!("F5")).unwrap(), 0x74);
        assert_eq!(parse_vkey(&json!("f12")).unwrap(), 0x7B);
        assert_eq!(parse_vkey(&json!(0x20)).unwrap(), 0x20);
        assert_eq!(parse_vkey(&json!("Left")).unwrap(), 0x25);
        assert!(parse_vkey(&json!("NOT_A_KEY")).is_err());
        assert!(parse_vkey(&json!(300)).is_err());
    }

    #[test]
    fn validates_and_balances_input_batches_before_execution() {
        let actions = vec![
            json!({"kind": "down", "x": 20, "y": 30, "button": "left"}),
            json!({"kind": "move", "x": 40, "y": 50, "steps": 2}),
            json!({"kind": "up", "x": 40, "y": 50, "button": "left"}),
            json!({"kind": "key_down", "vkey": "Ctrl"}),
            json!({"kind": "key_tap", "vkey": "A"}),
            json!({"kind": "key_up", "vkey": "Ctrl"}),
        ];
        let (prepared, scheduled) = prepare_input_actions(&actions, 800, 600, 50).unwrap();
        assert_eq!(prepared.len(), actions.len());
        assert_eq!(scheduled, 350); // 5 个动作间隔 + move 的 2 个步进

        assert!(
            prepare_input_actions(&[json!({"kind": "click", "x": -1, "y": 0})], 800, 600, 50)
                .is_err()
        );
        assert!(
            prepare_input_actions(&[json!({"kind": "click", "x": 800, "y": 0})], 800, 600, 50)
                .is_err()
        );
        assert!(
            prepare_input_actions(&[json!({"kind": "down", "x": 1, "y": 1})], 800, 600, 50)
                .is_err()
        );
    }

    #[test]
    fn rejects_oversized_or_long_running_input_batches() {
        let too_many = vec![json!({"kind": "key_tap", "vkey": "A"}); MAX_INPUT_ACTIONS + 1];
        assert!(prepare_input_actions(&too_many, 800, 600, 0).is_err());

        let too_slow = [json!({"kind": "move", "x": 1, "y": 1, "steps": 200})];
        assert!(prepare_input_actions(&too_slow, 800, 600, 5000).is_err());
    }

    #[test]
    fn isolates_panic_into_jsonrpc_error() {
        // 模拟 handle_request 的崩溃路径：隔离后返回 error 并保留请求 id。
        let panicking = json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": {"name": "does_not_exist_panic", "arguments": {}}
        });
        // 未知工具是普通 Err（isError result），不 panic：验证正常路径仍返回 result。
        let response = handle_request_isolated(&panicking).unwrap();
        assert_eq!(response["id"], 7);
        assert!(response.get("result").is_some());
        // 与 catch_unwind 一致的负载形态：Box<dyn Any + Send>，内在类型保持 panic! 的字面量
        let as_str: Box<dyn std::any::Any + Send> = Box::new("boom");
        assert_eq!(panic_payload_message(&as_str), "boom");
        let as_string: Box<dyn std::any::Any + Send> = Box::new(String::from("暴雷"));
        assert_eq!(panic_payload_message(&as_string), "暴雷");
        let as_int: Box<dyn std::any::Any + Send> = Box::new(42_i64);
        assert_eq!(panic_payload_message(&as_int), "未知 panic 负载");
    }
}
