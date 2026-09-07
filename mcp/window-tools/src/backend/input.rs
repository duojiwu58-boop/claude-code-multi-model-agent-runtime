//! 鼠标/键盘输入注入:批次预校验、配平约束、RAII 失败释放与插值执行。

use serde_json::{json, Value};
use windows::{
    Win32::{
        Foundation::{HWND, POINT},
        UI::{
            Input::KeyboardAndMouse::{
                keybd_event, mouse_event, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, MOUSEEVENTF_LEFTDOWN,
                MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
                MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSE_EVENT_FLAGS,
            },
            WindowsAndMessaging::{GetCursorPos, SetCursorPos, SetForegroundWindow},
        },
    },
};

use crate::error::{
    ToolError, ACTIVATION_DENIED, INVALID_ARGUMENT, INPUT_FAILED, LIMIT_EXCEEDED,
    WINDOW_NOT_VISIBLE,
};
use crate::parse::json_i32;

use super::window::{
    client_rect_of, ensure_input_foreground, entry_for_hwnd, foreground_hwnd, hwnd_str,
};

/// 输入批次必须保持短小，避免单线程 stdio 服务被一个请求长期占用。
pub const MAX_INPUT_ACTIONS: usize = 128;
pub const MAX_INPUT_SCHEDULED_DELAY_MS: u64 = 60_000;

/// 解析虚拟键码：整数 0..255，或常用按键名（ASCII 字母/数字、Enter、F1..F24 等）。
/// 只支持按键名是为了让模型可读；组合键请用多个 key_down/key_up 动作。
fn parse_vkey(value: &Value) -> Result<u8, ToolError> {
    if let Some(n) = value.as_i64() {
        return u8::try_from(n)
            .map_err(|_| ToolError::new(INVALID_ARGUMENT, format!("vkey 超出 0..255 范围: {n}")));
    }
    let name = value
        .as_str()
        .ok_or_else(|| ToolError::new(INVALID_ARGUMENT, "vkey 必须是整数或命名按键"))?
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
            Err(ToolError::new(INVALID_ARGUMENT, format!("未知按键名 '{name}'")))
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

fn validate_client_point(x: i32, y: i32, width: i32, height: i32) -> Result<(), ToolError> {
    if x < 0 || y < 0 || x >= width || y >= height {
        return Err(ToolError::new(
            INVALID_ARGUMENT,
            format!("输入坐标 ({x},{y}) 超出窗口客户区 {width}x{height}"),
        ));
    }
    Ok(())
}

/// 客户区坐标 → 屏幕物理坐标,并复检坐标仍在客户区内(窗口可能在解析与执行间变化)。
fn client_to_screen_checked(hwnd: HWND, x: i32, y: i32) -> Result<POINT, ToolError> {
    let client = client_rect_of(hwnd)?;
    validate_client_point(x, y, client.right - client.left, client.bottom - client.top)?;
    Ok(POINT {
        x: client.left + x,
        y: client.top + y,
    })
}

fn action_position(
    action: &Value,
    client_width: i32,
    client_height: i32,
) -> Result<(i32, i32), ToolError> {
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
) -> Result<(Vec<PreparedInputAction>, u64), ToolError> {
    if actions.is_empty() {
        return Err(ToolError::new(INVALID_ARGUMENT, "actions 不能为空"));
    }
    if actions.len() > MAX_INPUT_ACTIONS {
        return Err(ToolError::new(
            LIMIT_EXCEEDED,
            format!(
                "actions 数量不能超过 {MAX_INPUT_ACTIONS}，实际为 {}",
                actions.len()
            ),
        ));
    }

    let mut prepared = Vec::with_capacity(actions.len());
    let mut held_buttons: Vec<MouseButton> = Vec::new();
    let mut held_keys: Vec<u8> = Vec::new();
    for (index, action) in actions.iter().enumerate() {
        let kind = action
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::new(INVALID_ARGUMENT, format!("动作 {index} 必须有 kind")))?;
        let parsed = match kind {
            "move" => {
                let (x, y) = action_position(action, client_width, client_height)?;
                let steps = json_i32(action, "steps", false)?.unwrap_or(1);
                if !(1..=200).contains(&steps) {
                    return Err(ToolError::new(
                        INVALID_ARGUMENT,
                        format!("动作 {index} 的 steps 必须在 1..200 范围内"),
                    ));
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
                        return Err(ToolError::new(
                            INVALID_ARGUMENT,
                            format!("动作 {index} 重复按下 {}，必须先释放", button.name()),
                        ));
                    }
                    held_buttons.push(button);
                    PreparedInputAction::Down { x, y, button }
                } else if kind == "up" {
                    let Some(position) = held_buttons.iter().position(|held| *held == button)
                    else {
                        return Err(ToolError::new(
                            INVALID_ARGUMENT,
                            format!("动作 {index} 释放了未在本批次按下的 {}", button.name()),
                        ));
                    };
                    held_buttons.remove(position);
                    PreparedInputAction::Up { x, y, button }
                } else {
                    if held_buttons.contains(&button) {
                        return Err(ToolError::new(
                            INVALID_ARGUMENT,
                            format!("动作 {index} 不能 click 已由 down 按住的 {}", button.name()),
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
                        .ok_or_else(|| ToolError::new(INVALID_ARGUMENT, format!("动作 {index} 缺少 vkey")))?,
                )?;
                if kind == "key_down" {
                    if held_keys.contains(&vkey) {
                        return Err(ToolError::new(
                            INVALID_ARGUMENT,
                            format!("动作 {index} 重复按下 vkey 0x{vkey:02X}"),
                        ));
                    }
                    held_keys.push(vkey);
                    PreparedInputAction::KeyDown { vkey }
                } else if kind == "key_up" {
                    let Some(position) = held_keys.iter().position(|held| *held == vkey) else {
                        return Err(ToolError::new(
                            INVALID_ARGUMENT,
                            format!("动作 {index} 释放了未在本批次按下的 vkey 0x{vkey:02X}"),
                        ));
                    };
                    held_keys.remove(position);
                    PreparedInputAction::KeyUp { vkey }
                } else {
                    if held_keys.contains(&vkey) {
                        return Err(ToolError::new(
                            INVALID_ARGUMENT,
                            format!(
                                "动作 {index} 不能 key_tap 已由 key_down 按住的 vkey 0x{vkey:02X}"
                            ),
                        ));
                    }
                    PreparedInputAction::KeyTap { vkey }
                }
            }
            other => {
                return Err(ToolError::new(
                    INVALID_ARGUMENT,
                    format!("未知动作 kind '{other}'"),
                ))
            }
        };
        prepared.push(parsed);
    }

    if !held_buttons.is_empty() || !held_keys.is_empty() {
        return Err(ToolError::new(
            INVALID_ARGUMENT,
            "本批次存在未配平的 down/key_down；必须在同一批次内发送对应 up/key_up",
        ));
    }
    let scheduled_delay = scheduled_input_delay_ms(&prepared, delay_ms)
        .ok_or_else(|| ToolError::new(INVALID_ARGUMENT, "输入批次预计耗时计算溢出"))?;
    if scheduled_delay > MAX_INPUT_SCHEDULED_DELAY_MS {
        return Err(ToolError::new(
            LIMIT_EXCEEDED,
            format!(
                "输入批次计划延迟 {scheduled_delay}ms 超过上限 {MAX_INPUT_SCHEDULED_DELAY_MS}ms"
            ),
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
) -> Result<(), ToolError> {
    let mut current = from;
    for step in 1..=steps {
        ensure_input_foreground(hwnd)?;
        let f = f64::from(step) / f64::from(steps);
        current.x = from.x + ((f64::from(to.x) - f64::from(from.x)) * f) as i32;
        current.y = from.y + ((f64::from(to.y) - f64::from(from.y)) * f) as i32;
        unsafe {
            SetCursorPos(current.x, current.y)
                .map_err(|e| ToolError::new(INPUT_FAILED, format!("SetCursorPos 失败: {e}")))?;
        }
        std::thread::sleep(std::time::Duration::from_millis(per_step_ms));
    }
    Ok(())
}

/// 执行一批已校验的输入动作。调用方已确认目标为前台窗口,但每步仍复检。
fn execute_prepared_actions(
    hwnd: HWND,
    prepared: &[PreparedInputAction],
    delay_ms: u64,
) -> Result<Vec<Value>, ToolError> {
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
                    GetCursorPos(&mut from)
                        .map_err(|e| ToolError::new(INPUT_FAILED, format!("GetCursorPos 失败: {e}")))?;
                }
                move_cursor_steps(hwnd, from, target, steps, delay_ms.max(10))?;
                reported.push(json!({"kind": "move", "steps": steps}));
            }
            PreparedInputAction::Click { x, y, button } => {
                let target = client_to_screen_checked(hwnd, x, y)?;
                unsafe {
                    SetCursorPos(target.x, target.y)
                        .map_err(|e| ToolError::new(INPUT_FAILED, format!("SetCursorPos 失败: {e}")))?;
                }
                release_guard.mouse_down(button);
                release_guard.mouse_up(button);
                reported.push(json!({"kind": "click", "button": button.name()}));
            }
            PreparedInputAction::Down { x, y, button } => {
                let target = client_to_screen_checked(hwnd, x, y)?;
                unsafe {
                    SetCursorPos(target.x, target.y)
                        .map_err(|e| ToolError::new(INPUT_FAILED, format!("SetCursorPos 失败: {e}")))?;
                }
                release_guard.mouse_down(button);
                reported.push(json!({"kind": "down", "button": button.name()}));
            }
            PreparedInputAction::Up { x, y, button } => {
                let target = client_to_screen_checked(hwnd, x, y)?;
                unsafe {
                    SetCursorPos(target.x, target.y)
                        .map_err(|e| ToolError::new(INPUT_FAILED, format!("SetCursorPos 失败: {e}")))?;
                }
                release_guard.mouse_up(button);
                reported.push(json!({"kind": "up", "button": button.name()}));
            }
            PreparedInputAction::Scroll { x, y, delta } => {
                let target = client_to_screen_checked(hwnd, x, y)?;
                unsafe {
                    SetCursorPos(target.x, target.y)
                        .map_err(|e| ToolError::new(INPUT_FAILED, format!("SetCursorPos 失败: {e}")))?;
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
    Ok(reported)
}

/// 执行 send_input 工具:前置校验(可见性/前台/批次)→ 预校验 → 执行。
/// 返回 MCP content 形状的结果;错误路径由调用方统一转 isError。
pub fn send_input(hwnd: HWND, args: &Value) -> Result<Value, ToolError> {
    let entry = entry_for_hwnd(hwnd)?;
    if !entry.visible {
        return Err(ToolError::new(
            WINDOW_NOT_VISIBLE,
            format!(
                "窗口 hwnd={} 不可见（可能已最小化），拒绝注入输入",
                hwnd_str(hwnd)
            ),
        ));
    }
    let activate = args
        .get("activate")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if activate {
        if !unsafe { SetForegroundWindow(hwnd) }.as_bool() {
            return Err(ToolError::new(
                ACTIVATION_DENIED,
                format!(
                    "SetForegroundWindow 拒绝激活 hwnd={}（Windows 焦点保护或窗口已失效）",
                    hwnd_str(hwnd)
                ),
            ));
        }
        if foreground_hwnd() != hwnd {
            return Err(ToolError::new(
                ACTIVATION_DENIED,
                "窗口激活请求未生效，请改用 window_activate",
            ));
        }
    } else if foreground_hwnd() != hwnd {
        return Err(ToolError::new(
            crate::error::NOT_FOREGROUND,
            format!(
                "目标窗口 hwnd={} 不是前台窗口，输入将发送到其他窗口；请先调用 window_activate 或设置 activate:true",
                hwnd_str(hwnd)
            ),
        ));
    }

    let actions = args
        .get("actions")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::new(INVALID_ARGUMENT, "actions 必须是数组"))?;
    let delay_raw = json_i32(args, "delay_ms", false)?.unwrap_or(50);
    if !(0..=5000).contains(&delay_raw) {
        return Err(ToolError::new(
            INVALID_ARGUMENT,
            "delay_ms 必须在 0..5000 范围内",
        ));
    }
    let delay_ms = delay_raw as u64;
    let client = client_rect_of(hwnd)?;
    let (prepared, scheduled_delay_ms) = prepare_input_actions(
        actions,
        client.right - client.left,
        client.bottom - client.top,
        delay_ms,
    )?;

    let reported = execute_prepared_actions(hwnd, &prepared, delay_ms)?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn batch_errors_carry_stable_codes() {
        let err = prepare_input_actions(&[json!({"kind": "down", "x": 1, "y": 1})], 800, 600, 0)
            .unwrap_err();
        assert_eq!(err.code, INVALID_ARGUMENT);
        assert!(!err.retryable);

        let too_slow = [json!({"kind": "move", "x": 1, "y": 1, "steps": 200})];
        let err = prepare_input_actions(&too_slow, 800, 600, 5000).unwrap_err();
        assert_eq!(err.code, LIMIT_EXCEEDED);
    }
}
