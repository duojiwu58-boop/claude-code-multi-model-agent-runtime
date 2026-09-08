//! 后台静默输入:PostMessageW 直投窗口消息队列,不抢焦点、不动物理光标。
//!
//! 与 [`super::input`] 的前台注入互补,面向"目标窗口在后台仍要驱动"的场景
//! (自动化测试、无人值守流程)。诚实边界必须在调用方可见:
//! - 消息**投递**(posted)到目标线程队列,不保证目标应用处理 —— 使用
//!   DirectInput/RawInput 或消息泵不健全的应用可能完全无反应;
//! - 投递鼠标消息不会移动物理光标、不改变前台窗口(目标应用若在响应
//!   WM_LBUTTONDOWN 时自行 SetForegroundWindow,仍受 Windows 焦点保护限制);
//! - 坐标命中测试(RealChildWindowFromPoint)基于窗口当前可见矩形,
//!   最小化/完全遮挡时返回的结构仍可能成立,但应用是否响应取决于其自身;
//! - UIPI 与前台注入一致:目标提权且本服务未提权时提前结构化拒绝。
//!
//! 文本路径:标准 EDIT/RichEdit 控件走 EM_SETSEL+EM_REPLACESEL
//! (系统跨进程封送的置换消息,瞬间整段替换,免击键漏字);非编辑控件
//! 走 WM_CHAR 字符流投递。两种方式均不触碰键盘状态。

use serde_json::{json, Value};
use windows::Win32::{
    Foundation::{HWND, LPARAM, POINT, WPARAM},
    Graphics::Gdi::ScreenToClient,
    System::SystemServices::{MK_LBUTTON, MK_MBUTTON, MK_RBUTTON},
    UI::{
        Controls::{EM_REPLACESEL, EM_SETSEL},
        WindowsAndMessaging::{
            PostMessageW, RealChildWindowFromPoint, SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_CHAR,
            WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE,
            WM_RBUTTONDOWN, WM_RBUTTONUP,
        },
    },
};

use crate::error::{ToolError, INPUT_FAILED, INVALID_ARGUMENT, LIMIT_EXCEEDED};
use crate::parse::json_i32;

use super::window::{client_rect_of, entry_for_hwnd, hwnd_str, window_class};

/// 后台批次动作上限:消息直投成本低,但仍约束响应体与预校验规模。
const MAX_BG_ACTIONS: usize = 64;
/// 文本上限:避免单个请求投递海量 WM_CHAR 拖垮目标线程队列。
const MAX_TEXT_CHARS: usize = 4096;
/// SendMessageTimeoutW 等待:目标线程挂起时编辑框消息必须放弃。
const TEXT_MESSAGE_TIMEOUT_MS: u32 = 5000;

/// 后台鼠标按钮:消息与 UIPI 语义与前台一致。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BgButton {
    Left,
    Right,
    Middle,
}

impl BgButton {
    fn parse(raw: &str) -> Result<Self, ToolError> {
        match raw {
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            "middle" => Ok(Self::Middle),
            _ => Err(ToolError::new(
                INVALID_ARGUMENT,
                format!("未知鼠标按钮 '{raw}'（left/right/middle）"),
            )),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Middle => "middle",
        }
    }

    fn mk_flag(self) -> u32 {
        match self {
            Self::Left => MK_LBUTTON.0,
            Self::Right => MK_RBUTTON.0,
            Self::Middle => MK_MBUTTON.0,
        }
    }

    fn down_msg(self) -> u32 {
        match self {
            Self::Left => WM_LBUTTONDOWN,
            Self::Right => WM_RBUTTONDOWN,
            Self::Middle => WM_MBUTTONDOWN,
        }
    }

    fn up_msg(self) -> u32 {
        match self {
            Self::Left => WM_LBUTTONUP,
            Self::Right => WM_RBUTTONUP,
            Self::Middle => WM_MBUTTONUP,
        }
    }
}

/// 预校验后的后台动作(执行前整批解析,错误绝不产生半批投递)。
#[derive(Debug)]
enum PreparedBgAction {
    Move { x: i32, y: i32 },
    Click { x: i32, y: i32, button: BgButton },
    Down { x: i32, y: i32, button: BgButton },
    Up { x: i32, y: i32, button: BgButton },
}

fn validate_client_point(x: i32, y: i32, width: i32, height: i32) -> Result<(), ToolError> {
    if x < 0 || y < 0 || x >= width || y >= height {
        return Err(ToolError::new(
            INVALID_ARGUMENT,
            format!("输入坐标 ({x},{y}) 超出窗口客户区 {width}x{height}"),
        ));
    }
    Ok(())
}

/// 解析并校验整批后台动作:坐标必须落在客户区,down 必须同批配平 up。
fn prepare_bg_actions(
    actions: &[Value],
    base: (i32, i32),
    client_width: i32,
    client_height: i32,
) -> Result<Vec<PreparedBgAction>, ToolError> {
    if actions.is_empty() {
        return Err(ToolError::new(INVALID_ARGUMENT, "actions 不能为空"));
    }
    if actions.len() > MAX_BG_ACTIONS {
        return Err(ToolError::new(
            LIMIT_EXCEEDED,
            format!(
                "actions 数量不能超过 {MAX_BG_ACTIONS}，实际为 {}",
                actions.len()
            ),
        ));
    }

    let mut prepared = Vec::with_capacity(actions.len());
    let mut held: Vec<BgButton> = Vec::new();
    for (index, action) in actions.iter().enumerate() {
        let kind = action
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::new(INVALID_ARGUMENT, format!("动作 {index} 必须有 kind")))?;
        // 动作自身坐标缺省继承批次基准点(client_x/client_y)。
        let x = json_i32(action, "x", false)?.unwrap_or(base.0);
        let y = json_i32(action, "y", false)?.unwrap_or(base.1);
        validate_client_point(x, y, client_width, client_height)?;
        let button = BgButton::parse(
            action
                .get("button")
                .and_then(Value::as_str)
                .unwrap_or("left"),
        )?;
        let parsed = match kind {
            "move" => PreparedBgAction::Move { x, y },
            "click" => {
                if held.contains(&button) {
                    return Err(ToolError::new(
                        INVALID_ARGUMENT,
                        format!("动作 {index} 不能 click 已由 down 按住的 {}", button.name()),
                    ));
                }
                PreparedBgAction::Click { x, y, button }
            }
            "right_click" => PreparedBgAction::Click {
                x,
                y,
                button: BgButton::Right,
            },
            "down" => {
                if held.contains(&button) {
                    return Err(ToolError::new(
                        INVALID_ARGUMENT,
                        format!("动作 {index} 重复按下 {}，必须先释放", button.name()),
                    ));
                }
                held.push(button);
                PreparedBgAction::Down { x, y, button }
            }
            "up" => {
                let Some(position) = held.iter().position(|h| *h == button) else {
                    return Err(ToolError::new(
                        INVALID_ARGUMENT,
                        format!("动作 {index} 释放了未在本批次按下的 {}", button.name()),
                    ));
                };
                held.remove(position);
                PreparedBgAction::Up { x, y, button }
            }
            other => {
                return Err(ToolError::new(
                    INVALID_ARGUMENT,
                    format!("未知动作 kind '{other}'（click/down/up/right_click/move）"),
                ))
            }
        };
        prepared.push(parsed);
    }
    if !held.is_empty() {
        return Err(ToolError::new(
            INVALID_ARGUMENT,
            "本批次存在未配平的 down；必须在同一批次内发送对应 up",
        ));
    }
    Ok(prepared)
}

/// MAKELPARAM:子控件局部坐标打包(low= x, high= y,各 16 位)。
fn make_lparam(x: i32, y: i32) -> LPARAM {
    LPARAM((((y & 0xFFFF) << 16) | (x & 0xFFFF)) as isize)
}

/// 命中测试:客户区坐标 → 该点下最深子控件句柄与其局部坐标。
/// RealChildWindowFromPoint 找不到更深子控件时返回父窗口自身。
fn child_at_point(
    parent: HWND,
    client_x: i32,
    client_y: i32,
) -> Result<(HWND, i32, i32), ToolError> {
    let client = client_rect_of(parent)?;
    let mut screen = POINT {
        x: client.left + client_x,
        y: client.top + client_y,
    };
    let child = unsafe { RealChildWindowFromPoint(parent, screen) };
    let child = if child.0.is_null() { parent } else { child };
    if !unsafe { ScreenToClient(child, &mut screen) }.as_bool() {
        return Err(ToolError::new(
            INPUT_FAILED,
            format!("ScreenToClient 失败(child hwnd={})", hwnd_str(child)),
        ));
    }
    Ok((child, screen.x, screen.y))
}

fn post_mouse(hwnd: HWND, msg: u32, mk_flags: u32, x: i32, y: i32) -> Result<(), ToolError> {
    unsafe {
        PostMessageW(
            Some(hwnd),
            msg,
            WPARAM(mk_flags as usize),
            make_lparam(x, y),
        )
    }
    .map_err(|e| {
        ToolError::new(
            INPUT_FAILED,
            format!("PostMessageW(0x{msg:04X}) 失败(目标窗口可能已销毁): {e}"),
        )
    })
}

/// 执行 send_input_background 工具。
/// 参数:hwnd(父窗口)、client_x/client_y(批次基准点)、actions、
/// delay_ms(动作间隔,默认 50)。响应逐动作携带命中子控件句柄与类名。
pub fn send_input_background(hwnd: HWND, args: &Value) -> Result<Value, ToolError> {
    let entry = entry_for_hwnd(hwnd)?;
    if !entry.visible {
        return Err(ToolError::new(
            crate::error::WINDOW_NOT_VISIBLE,
            format!(
                "窗口 hwnd={} 不可见（可能已最小化），拒绝后台投递",
                hwnd_str(hwnd)
            ),
        ));
    }
    crate::backend::elevation::ensure_uipi_injectable(entry.pid)?;

    let client_x = json_i32(args, "client_x", true)?.expect("required value checked above");
    let client_y = json_i32(args, "client_y", true)?.expect("required value checked above");
    let actions = args
        .get("actions")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::new(INVALID_ARGUMENT, "actions 必须是数组"))?;
    let delay_ms = json_i32(args, "delay_ms", false)?
        .unwrap_or(50)
        .clamp(0, 5000) as u64;

    let client = client_rect_of(hwnd)?;
    let prepared = prepare_bg_actions(
        actions,
        (client_x, client_y),
        client.right - client.left,
        client.bottom - client.top,
    )?;

    // 执行:整批已校验,投递失败(目标线程退出)即中止。
    let mut reported: Vec<Value> = Vec::new();
    let mut held_flags = 0u32;
    let mut posted = 0usize;
    for (index, action) in prepared.iter().enumerate() {
        if index > 0 && delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        }
        match *action {
            PreparedBgAction::Move { x, y } => {
                let (child, lx, ly) = child_at_point(hwnd, x, y)?;
                post_mouse(child, WM_MOUSEMOVE, held_flags, lx, ly)?;
                posted += 1;
                reported.push(json!({"kind": "move", "child_hwnd": hwnd_str(child),
                    "child_class": window_class(child), "local": [lx, ly]}));
            }
            PreparedBgAction::Click { x, y, button } => {
                let (child, lx, ly) = child_at_point(hwnd, x, y)?;
                post_mouse(
                    child,
                    button.down_msg(),
                    held_flags | button.mk_flag(),
                    lx,
                    ly,
                )?;
                post_mouse(child, button.up_msg(), held_flags, lx, ly)?;
                posted += 2;
                reported.push(
                    json!({"kind": "click", "button": button.name(), "child_hwnd": hwnd_str(child),
                    "child_class": window_class(child), "local": [lx, ly]}),
                );
            }
            PreparedBgAction::Down { x, y, button } => {
                let (child, lx, ly) = child_at_point(hwnd, x, y)?;
                post_mouse(
                    child,
                    button.down_msg(),
                    held_flags | button.mk_flag(),
                    lx,
                    ly,
                )?;
                held_flags |= button.mk_flag();
                posted += 1;
                reported.push(
                    json!({"kind": "down", "button": button.name(), "child_hwnd": hwnd_str(child),
                    "child_class": window_class(child), "local": [lx, ly]}),
                );
            }
            PreparedBgAction::Up { x, y, button } => {
                let (child, lx, ly) = child_at_point(hwnd, x, y)?;
                post_mouse(
                    child,
                    button.up_msg(),
                    held_flags & !button.mk_flag(),
                    lx,
                    ly,
                )?;
                held_flags &= !button.mk_flag();
                posted += 1;
                reported.push(
                    json!({"kind": "up", "button": button.name(), "child_hwnd": hwnd_str(child),
                    "child_class": window_class(child), "local": [lx, ly]}),
                );
            }
        }
    }

    Ok(json!({
        "content": [{"type": "text", "text": json!({
            "hwnd": hwnd_str(hwnd),
            "delivery": "posted",
            "foreground_untouched": true,
            "physical_cursor_untouched": true,
            "posted_count": posted,
            "action_count": reported.len(),
            "actions": reported,
            "note": "消息已投递到目标线程队列;使用 RawInput/DirectInput 或消息泵不健全的应用可能不响应,请以截图/像素断言验证实际效果"
        }).to_string()}]
    }))
}

/// 执行 send_text_background 工具。
/// 标准 EDIT/RichEdit 控件:EM_SETSEL(可选全选)+ EM_REPLACESEL 整段置换
/// (系统跨进程封送,免击键漏字);其他控件:WM_CHAR 字符流投递。
/// clear_first 只对编辑框控件有效。
pub fn send_text_background(hwnd: HWND, args: &Value) -> Result<Value, ToolError> {
    let entry = entry_for_hwnd(hwnd)?;
    crate::backend::elevation::ensure_uipi_injectable(entry.pid)?;

    let text = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::new(INVALID_ARGUMENT, "text 参数缺失"))?;
    let clear_first = args
        .get("clear_first")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if text.chars().count() > MAX_TEXT_CHARS {
        return Err(ToolError::new(
            LIMIT_EXCEEDED,
            format!(
                "text 不能超过 {MAX_TEXT_CHARS} 字符,实际为 {}",
                text.chars().count()
            ),
        ));
    }
    if text.is_empty() && !clear_first {
        return Err(ToolError::new(
            INVALID_ARGUMENT,
            "text 不能为空(clear_first=false)",
        ));
    }

    let class = window_class(hwnd);
    let is_edit = class.to_ascii_uppercase().contains("EDIT");
    let method = if is_edit {
        // EM_REPLACESEL 在系统跨进程封送列表内,UTF-16 + 结尾 NUL。
        let mut utf16: Vec<u16> = text.encode_utf16().collect();
        utf16.push(0);
        if clear_first {
            // 全选(0,-1)后整段置换,一步完成"清空+写入"。
            let ok = unsafe {
                SendMessageTimeoutW(
                    hwnd,
                    EM_SETSEL,
                    WPARAM(0),
                    LPARAM(-1),
                    SMTO_ABORTIFHUNG,
                    TEXT_MESSAGE_TIMEOUT_MS,
                    None,
                )
            };
            if ok.0 <= 0 {
                return Err(ToolError::new(
                    INPUT_FAILED,
                    "EM_SETSEL 投递失败(目标线程挂起或窗口已销毁)",
                ));
            }
        }
        let ok = unsafe {
            SendMessageTimeoutW(
                hwnd,
                EM_REPLACESEL,
                WPARAM(1),
                LPARAM(utf16.as_ptr() as isize),
                SMTO_ABORTIFHUNG,
                TEXT_MESSAGE_TIMEOUT_MS,
                None,
            )
        };
        if ok.0 <= 0 {
            return Err(ToolError::new(
                INPUT_FAILED,
                "EM_REPLACESEL 投递失败(目标线程挂起或窗口已销毁)",
            ));
        }
        "em_replacesel"
    } else if !clear_first {
        // 非编辑控件:WM_CHAR 字符流(UTF-16 码元逐一投递,代理对分两条)。
        // 换行统一转 CR:多数控件的 WM_CHAR 换行语义是 0x0D。
        let mut units = [0u16; 2];
        for ch in text.chars() {
            if ch == '\r' || ch == '\n' {
                unsafe {
                    PostMessageW(Some(hwnd), WM_CHAR, WPARAM(0x0D), LPARAM(1)).map_err(|e| {
                        ToolError::new(INPUT_FAILED, format!("PostMessageW(WM_CHAR) 失败: {e}"))
                    })?;
                }
                continue;
            }
            for unit in ch.encode_utf16(&mut units).iter() {
                unsafe {
                    PostMessageW(Some(hwnd), WM_CHAR, WPARAM(*unit as usize), LPARAM(1)).map_err(
                        |e| {
                            ToolError::new(INPUT_FAILED, format!("PostMessageW(WM_CHAR) 失败: {e}"))
                        },
                    )?;
                }
            }
        }
        "wm_char"
    } else {
        return Err(ToolError::new(
            INVALID_ARGUMENT,
            format!(
                "目标窗口类名 '{class}' 不是 EDIT 控件,clear_first 无法生效;请先用 inspect_element_at 定位输入框句柄,或设置 clear_first=false 走 WM_CHAR 投递"
            ),
        ));
    };

    Ok(json!({
        "content": [{"type": "text", "text": json!({
            "hwnd": hwnd_str(hwnd),
            "class": class,
            "method": method,
            "chars": text.chars().count(),
            "clear_first": clear_first,
            "delivery": if method == "em_replacesel" { "sent" } else { "posted" },
            "note": "em_replacesel=系统封送整段置换;wm_char=字符流投递,应用需泵消息,请以截图/像素断言验证实际效果"
        }).to_string()}]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_validate_coords_and_balance() {
        // 坐标越界
        assert!(prepare_bg_actions(
            &[json!({"kind": "click", "x": 800, "y": 0})],
            (0, 0),
            800,
            600
        )
        .is_err());
        // down 未配平
        assert!(
            prepare_bg_actions(&[json!({"kind": "down", "x": 1, "y": 1})], (0, 0), 800, 600)
                .is_err()
        );
        // 配平通过,缺省坐标继承基准点
        let prepared = prepare_bg_actions(
            &[
                json!({"kind": "down", "x": 10, "y": 20}),
                json!({"kind": "up"}), // 继承 client_x/client_y
            ],
            (30, 40),
            800,
            600,
        )
        .unwrap();
        assert_eq!(prepared.len(), 2);
        match prepared[1] {
            PreparedBgAction::Up { x, y, .. } => assert_eq!((x, y), (30, 40)),
            _ => panic!("动作类型不符"),
        }
    }

    #[test]
    fn rejects_oversized_batches_and_unknown_kinds() {
        let many = vec![json!({"kind": "click", "x": 1, "y": 1}); MAX_BG_ACTIONS + 1];
        assert_eq!(
            prepare_bg_actions(&many, (0, 0), 800, 600)
                .unwrap_err()
                .code,
            LIMIT_EXCEEDED
        );
        assert!(
            prepare_bg_actions(&[json!({"kind": "key_tap", "vkey": "A"})], (0, 0), 800, 600)
                .is_err()
        );
    }

    #[test]
    fn right_click_kind_maps_to_right_button() {
        let prepared = prepare_bg_actions(
            &[json!({"kind": "right_click", "x": 5, "y": 6})],
            (0, 0),
            800,
            600,
        )
        .unwrap();
        match prepared[0] {
            PreparedBgAction::Click {
                button: BgButton::Right,
                x,
                y,
            } => assert_eq!((x, y), (5, 6)),
            _ => panic!("right_click 应映射 Right 按钮"),
        }
    }

    #[test]
    fn lparam_packs_coordinates() {
        let lparam = make_lparam(100, 200);
        assert_eq!(lparam.0, ((200 << 16) | 100) as isize);
    }
}
