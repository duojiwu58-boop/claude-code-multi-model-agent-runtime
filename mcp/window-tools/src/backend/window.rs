//! 窗口枚举、矩形查询与窗口操作（resize / activate / 前台校验）。

use serde_json::{json, Value};
use windows::{
    core::BOOL,
    Win32::{
        Foundation::{HWND, LPARAM, POINT, RECT, WPARAM},
        Graphics::Gdi::ClientToScreen,
        System::Threading::{AttachThreadInput, GetCurrentThreadId},
        UI::{
            Input::KeyboardAndMouse::{keybd_event, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VK_MENU},
            WindowsAndMessaging::{
                EnumWindows, GetClassNameW, GetClientRect, GetForegroundWindow, GetWindow,
                GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, IsHungAppWindow,
                IsIconic, IsWindowVisible, SendMessageTimeoutW, SetForegroundWindow, SetWindowPos,
                GWL_EXSTYLE, GW_HWNDPREV, SMTO_ABORTIFHUNG, SWP_NOACTIVATE, SWP_NOOWNERZORDER,
                SWP_NOZORDER, WM_GETTEXT, WS_EX_TOOLWINDOW,
            },
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
    /// 扩展样式(WS_EX_* 位),供交互窗口启发式过滤(如排除 WS_EX_TOOLWINDOW)。
    pub ex_style: u32,
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
///
/// 注意:SendMessageTimeoutW 的返回 LRESULT 只是"发送是否成功"(成功=1),
/// WM_GETTEXT 实际拷贝的字符数在 lpdwResult 里,二者不可混淆。
pub(crate) fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let mut len = 0usize;
    let ok = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_GETTEXT,
            WPARAM(buf.len()),
            LPARAM(buf.as_mut_ptr() as isize),
            SMTO_ABORTIFHUNG,
            TITLE_QUERY_TIMEOUT_MS,
            Some(&mut len),
        )
    };
    if ok.0 <= 0 {
        return String::new();
    }
    let len = len.min(buf.len() - 1);
    String::from_utf16_lossy(&buf[..len])
}

/// 目标窗口是否无响应(消息队列挂起)。截图等进入目标线程的操作必须先预检。
pub fn is_window_hung(hwnd: HWND) -> bool {
    unsafe { IsHungAppWindow(hwnd) }.as_bool()
}

/// 窗口类名(后台文本工具用它判定 EDIT 控件)。
pub(crate) fn window_class(hwnd: HWND) -> String {
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
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    out.push(WindowEntry {
        hwnd: hwnd_num(hwnd),
        pid,
        title: window_title(hwnd),
        class: window_class(hwnd),
        visible,
        minimized: entry_minimized(hwnd),
        ex_style,
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

/// 人类可交互窗口启发式:排除输入法候选框、DWM 骨架、托盘隐藏宿主等噪点窗口。
/// 尺寸阈值为物理像素;工具窗口(WS_EX_TOOLWINDOW)与无标题窗口默认不算交互目标。
/// 仅用于 list_windows 的默认过滤,不影响按 hwnd/title/pid 的精确选择。
pub fn entry_is_interactive(entry: &WindowEntry) -> bool {
    entry.visible
        && !entry.minimized
        && entry.width >= 100
        && entry.height >= 100
        && entry.ex_style & WS_EX_TOOLWINDOW.0 == 0
        && !entry.title.is_empty()
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
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    Ok(WindowEntry {
        hwnd: hwnd_num(hwnd),
        pid,
        title: window_title(hwnd),
        class: window_class(hwnd),
        visible: unsafe { IsWindowVisible(hwnd) }.as_bool(),
        minimized: entry_minimized(hwnd),
        ex_style,
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

/// 请求 Windows 将一个精确 HWND 放到前台。Windows 焦点窃取保护可能拒绝
/// 直连请求;升级阶梯(AttachThreadInput / Alt 点击)尽力突破,全部失败时
/// 返回结构化错误而不是假装已聚焦。`allow_alt_trick` 允许最后手段
/// (向用户当前前台窗口注入一次真实 Alt 按键,有副作用,默认关闭)。
pub fn window_activate(hwnd: HWND, args: &Value) -> Result<Value, ToolError> {
    let entry = entry_for_hwnd(hwnd)?;
    crate::backend::elevation::ensure_uipi_injectable(entry.pid)?;
    let allow_alt_trick = args
        .get("allow_alt_trick")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let method = activate_with_ladder(hwnd, allow_alt_trick)?;
    Ok(json!({"content": [{"type": "text", "text": json!({
        "hwnd": hwnd_str(hwnd),
        "activated": true,
        "activation_method": method.name()
    }).to_string()}]}))
}

/// 激活方式:直连 / 前台线程输入附着 / Alt 点击绕过焦点锁。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivationMethod {
    Direct,
    AttachInput,
    AltTrick,
}

impl ActivationMethod {
    pub fn name(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::AttachInput => "attach_input",
            Self::AltTrick => "alt_trick",
        }
    }
}

/// SetForegroundWindow + 前台验证一步完成。
fn set_foreground_and_verify(hwnd: HWND) -> bool {
    unsafe { SetForegroundWindow(hwnd) }.as_bool() && foreground_hwnd() == hwnd
}

/// 焦点激活升级阶梯:直连 → AttachThreadInput → (可选)Alt 点击。
///
/// AttachThreadInput 的护栏:①前台线程挂起时挂接会把本进程输入队列一起
/// 冻死,先 IsHungAppWindow 预检;②UIPI 拦跨完整性级别挂接,调用方已做
/// 提权前置检查;③挂接立即解绑,绝不过夜。
///
/// Alt 点击机制(LockSetForegroundWindow 文档行为):前台窗口处理 Alt 时
/// 系统自动解除焦点锁。副作用是向用户当前正在使用的前台窗口注入一次
/// 真实按键,仅在 `allow_alt_trick` 时执行。
pub fn activate_with_ladder(
    hwnd: HWND,
    allow_alt_trick: bool,
) -> Result<ActivationMethod, ToolError> {
    if set_foreground_and_verify(hwnd) {
        return Ok(ActivationMethod::Direct);
    }
    let foreground = foreground_hwnd();
    if !foreground.0.is_null() && foreground != hwnd && !is_window_hung(foreground) {
        let target_tid = unsafe { GetWindowThreadProcessId(foreground, None) };
        let self_tid = unsafe { GetCurrentThreadId() };
        if target_tid != 0 && target_tid != self_tid {
            unsafe {
                let _ = AttachThreadInput(self_tid, target_tid, true);
            }
            let attached = set_foreground_and_verify(hwnd);
            unsafe {
                let _ = AttachThreadInput(self_tid, target_tid, false);
            }
            if attached {
                return Ok(ActivationMethod::AttachInput);
            }
        }
    }
    if allow_alt_trick {
        unsafe {
            keybd_event(VK_MENU.0 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
            keybd_event(VK_MENU.0 as u8, 0, KEYEVENTF_KEYUP, 0);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        if set_foreground_and_verify(hwnd) {
            return Ok(ActivationMethod::AltTrick);
        }
    }
    Err(ToolError::new(
        ACTIVATION_DENIED,
        format!(
            "SetForegroundWindow 与焦点锁绕过均未生效:目标 hwnd={}(Windows 焦点保护或窗口已失效)",
            hwnd_str(hwnd)
        ),
    ))
}

/// 矩形相交判定(含边界接触)。纯函数,便于单测。
fn rects_intersect(a: RECT, b: RECT) -> bool {
    a.left < b.right && a.right > b.left && a.top < b.bottom && a.bottom > b.top
}

/// 客户区矩形区域是否被其他顶层窗口遮挡:自目标窗口沿 z 序向上走查,
/// 任意可见且非最小化窗口的外框与该区域(屏幕坐标)相交即判定遮挡。
/// 半透明/分层窗口不保证像素级判定,但对"断言结果可信度"足够。
/// 探测失败(窗口已销毁等)按不遮挡处理,由后续采样自行报错。
pub fn client_region_occluded(hwnd: HWND, x: i32, y: i32, width: i32, height: i32) -> bool {
    let Ok(client) = client_rect_of(hwnd) else {
        return false;
    };
    let region = RECT {
        left: client.left + x,
        top: client.top + y,
        right: client.left + x + width,
        bottom: client.top + y + height,
    };
    // GetWindow 在 0.61 返回 Result;失败(目标已销毁)按无遮挡处理。
    let mut above = unsafe { GetWindow(hwnd, GW_HWNDPREV) }.unwrap_or_default();
    while !above.0.is_null() {
        if unsafe { IsWindowVisible(above) }.as_bool() && !unsafe { IsIconic(above) }.as_bool() {
            let mut r = RECT::default();
            if unsafe { GetWindowRect(above, &mut r) }.is_ok() && rects_intersect(region, r) {
                return true;
            }
        }
        above = unsafe { GetWindow(above, GW_HWNDPREV) }.unwrap_or_default();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_heuristic_filters_noise_windows() {
        let base = WindowEntry {
            hwnd: 1,
            pid: 2,
            title: "Editor".to_string(),
            class: "x".to_string(),
            visible: true,
            minimized: false,
            ex_style: 0,
            left: 0,
            top: 0,
            width: 800,
            height: 600,
        };
        assert!(entry_is_interactive(&base));
        assert!(!entry_is_interactive(&WindowEntry {
            width: 99,
            ..base.clone()
        }));
        assert!(!entry_is_interactive(&WindowEntry {
            height: 99,
            ..base.clone()
        }));
        assert!(!entry_is_interactive(&WindowEntry {
            minimized: true,
            ..base.clone()
        }));
        assert!(!entry_is_interactive(&WindowEntry {
            title: String::new(),
            ..base.clone()
        }));
        assert!(!entry_is_interactive(&WindowEntry {
            visible: false,
            ..base.clone()
        }));
        assert!(!entry_is_interactive(&WindowEntry {
            ex_style: WS_EX_TOOLWINDOW.0,
            ..base
        }));
    }

    #[test]
    fn rect_intersection_detects_overlap_but_not_contact() {
        let r = RECT {
            left: 100,
            top: 100,
            right: 200,
            bottom: 200,
        };
        assert!(rects_intersect(
            r,
            RECT {
                left: 150,
                top: 150,
                right: 300,
                bottom: 300
            }
        ));
        assert!(rects_intersect(
            r,
            RECT {
                left: 50,
                top: 50,
                right: 150,
                bottom: 150
            }
        ));
        // 边界接触不算遮挡
        assert!(!rects_intersect(
            r,
            RECT {
                left: 200,
                top: 100,
                right: 300,
                bottom: 200
            }
        ));
        assert!(!rects_intersect(
            r,
            RECT {
                left: 100,
                top: 200,
                right: 200,
                bottom: 300
            }
        ));
    }

    #[test]
    fn activation_method_names_are_stable() {
        assert_eq!(ActivationMethod::Direct.name(), "direct");
        assert_eq!(ActivationMethod::AttachInput.name(), "attach_input");
        assert_eq!(ActivationMethod::AltTrick.name(), "alt_trick");
    }
}
