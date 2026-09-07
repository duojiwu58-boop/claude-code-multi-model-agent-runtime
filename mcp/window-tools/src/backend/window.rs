//! 窗口枚举、矩形查询与窗口操作（resize / activate / 前台校验）。

use serde_json::{json, Value};
use windows::{
    core::BOOL,
    Win32::{
        Foundation::{HWND, LPARAM, POINT, RECT, WPARAM},
        Graphics::Gdi::ClientToScreen,
        UI::WindowsAndMessaging::{
            EnumWindows, GetClassNameW, GetClientRect, GetForegroundWindow, GetWindowRect,
            GetWindowThreadProcessId, IsHungAppWindow, IsIconic, IsWindowVisible,
            SendMessageTimeoutW, SetForegroundWindow, SetWindowPos, SMTO_ABORTIFHUNG,
            SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER, WM_GETTEXT,
        },
    },
};

use crate::error::{
    ToolError, ACTIVATION_DENIED, INVALID_ARGUMENT, LIMIT_EXCEEDED, NOT_FOREGROUND,
    WINDOW_NOT_FOUND,
};
use crate::parse::json_i32;

use super::capture::MAX_CAPTURE_DIM;

#[derive(Clone)]
pub struct WindowEntry {
    pub hwnd: isize,
    pub pid: u32,
    pub title: String,
    pub class: String,
    pub visible: bool,
    pub minimized: bool,
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

pub fn hwnd_num(hwnd: HWND) -> isize {
    hwnd.0 as isize
}

pub fn hwnd_str(hwnd: HWND) -> String {
    hwnd_num(hwnd).to_string()
}

pub fn rect_json(rect: RECT) -> Value {
    json!({
        "left": rect.left,
        "top": rect.top,
        "width": rect.right - rect.left,
        "height": rect.bottom - rect.top
    })
}

/// 当前前台窗口;无前台窗口时返回 NULL。
pub fn foreground_hwnd() -> HWND {
    unsafe { GetForegroundWindow() }
}

/// 标题获取超时:挂起窗口必须在限定时间内放弃,枚举绝不能卡死服务。
const TITLE_QUERY_TIMEOUT_MS: u32 = 500;

/// 标题读取不走 `GetWindowTextW`:它对其他进程窗口内部发 WM_GETTEXT,
/// 目标线程挂起时调用方也会被拖死。改用 SendMessageTimeoutW + SMTO_ABORTIFHUNG,
/// 挂起窗口的标题为空字符串,枚举照常完成。
fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let result = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_GETTEXT,
            WPARAM(buf.len()),
            LPARAM(buf.as_mut_ptr() as isize),
            SMTO_ABORTIFHUNG,
            TITLE_QUERY_TIMEOUT_MS,
            None,
        )
    };
    if result.0 <= 0 {
        return String::new();
    }
    let len = result.0 as usize;
    String::from_utf16_lossy(&buf[..len])
}

/// 目标窗口是否无响应(消息队列挂起)。截图等进入目标线程的操作必须先预检。
pub fn is_window_hung(hwnd: HWND) -> bool {
    unsafe { IsHungAppWindow(hwnd) }.as_bool()
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

pub fn collect_windows() -> Vec<WindowEntry> {
    let mut all: Vec<WindowEntry> = Vec::new();
    unsafe {
        let _ = EnumWindows(
            Some(enum_windows_proc),
            LPARAM((&mut all as *mut Vec<WindowEntry>) as isize),
        );
    }
    all
}

pub fn rect_of(hwnd: HWND) -> Result<RECT, ToolError> {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    unsafe { GetWindowRect(hwnd, &mut rect) }
        .map_err(|e| ToolError::new(WINDOW_NOT_FOUND, format!("GetWindowRect 失败: {e}")))?;
    Ok(rect)
}

/// 将客户区左上/右下坐标转换成虚拟桌面物理坐标，供 `client_only` 返回一致的屏幕矩形。
pub fn client_rect_of(hwnd: HWND) -> Result<RECT, ToolError> {
    let mut client = RECT::default();
    unsafe { GetClientRect(hwnd, &mut client) }
        .map_err(|e| ToolError::new(WINDOW_NOT_FOUND, format!("GetClientRect 失败: {e}")))?;
    let mut top_left = POINT {
        x: client.left,
        y: client.top,
    };
    let mut bottom_right = POINT {
        x: client.right,
        y: client.bottom,
    };
    if !unsafe { ClientToScreen(hwnd, &mut top_left) }.as_bool() {
        return Err(ToolError::new(
            WINDOW_NOT_FOUND,
            "ClientToScreen(左上) 失败",
        ));
    }
    if !unsafe { ClientToScreen(hwnd, &mut bottom_right) }.as_bool() {
        return Err(ToolError::new(
            WINDOW_NOT_FOUND,
            "ClientToScreen(右下) 失败",
        ));
    }
    Ok(RECT {
        left: top_left.x,
        top: top_left.y,
        right: bottom_right.x,
        bottom: bottom_right.y,
    })
}

/// 单个窗口的条目快照（含可见性/最小化状态），供选择器解析与查询工具使用。
pub fn entry_for_hwnd(hwnd: HWND) -> Result<WindowEntry, ToolError> {
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

/// 输入注入前的前台校验:目标已不是前台窗口时拒绝,避免输入落到其他窗口。
pub fn ensure_input_foreground(hwnd: HWND) -> Result<(), ToolError> {
    let foreground = foreground_hwnd();
    if foreground != hwnd {
        return Err(ToolError::new(
            NOT_FOREGROUND,
            format!(
                "输入执行前目标窗口失去前台状态：前台 hwnd={}，目标 hwnd={}",
                hwnd_str(foreground),
                hwnd_str(hwnd)
            ),
        ));
    }
    Ok(())
}

/// 调整窗口尺寸(可选移动)。所有外部数值先经 json_i32 验证。
/// 返回应用后的外框,调用方负责组装 MCP 响应。
pub fn resize_window(hwnd: HWND, args: &Value) -> Result<Value, ToolError> {
    let width = json_i32(args, "width", true)?.expect("required value checked above");
    let height = json_i32(args, "height", true)?.expect("required value checked above");
    if width < 40 || height < 40 {
        return Err(ToolError::new(INVALID_ARGUMENT, "width/height 不能小于 40"));
    }
    if width > MAX_CAPTURE_DIM || height > MAX_CAPTURE_DIM {
        return Err(ToolError::new(
            LIMIT_EXCEEDED,
            format!("width/height 不能超过 {}", MAX_CAPTURE_DIM),
        ));
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
    .map_err(|e| ToolError::new(WINDOW_NOT_FOUND, format!("SetWindowPos 失败: {e}")))?;
    let after = rect_of(hwnd)?;
    let text = json!({
        "hwnd": hwnd_str(hwnd),
        "requested": {"x": x, "y": y, "width": width, "height": height},
        "applied": {"left": after.left, "top": after.top, "width": after.right - after.left, "height": after.bottom - after.top}
    });
    Ok(json!({"content": [{"type": "text", "text": text.to_string()}]}))
}

/// 返回单一窗口的外框与客户区，避免在窗口很多时让模型重复读取完整列表。
pub fn get_window_rect(hwnd: HWND) -> Result<Value, ToolError> {
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
pub fn window_activate(hwnd: HWND) -> Result<Value, ToolError> {
    entry_for_hwnd(hwnd)?;
    if !unsafe { SetForegroundWindow(hwnd) }.as_bool() {
        return Err(ToolError::new(
            ACTIVATION_DENIED,
            format!(
                "SetForegroundWindow 拒绝激活 hwnd={}（Windows 焦点限制或窗口已失效）",
                hwnd_str(hwnd)
            ),
        ));
    }
    let foreground = foreground_hwnd();
    if foreground != hwnd {
        return Err(ToolError::new(
            ACTIVATION_DENIED,
            format!(
                "窗口激活请求未生效：前台 hwnd={}，目标 hwnd={}",
                hwnd_str(foreground),
                hwnd_str(hwnd)
            ),
        ));
    }
    Ok(json!({"content": [{"type": "text", "text": json!({
        "hwnd": hwnd_str(hwnd),
        "activated": true
    }).to_string()}]}))
}
