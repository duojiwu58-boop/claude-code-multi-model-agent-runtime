//! Win32 后端:窗口枚举/矩形、GDI 采集与输入注入。
//! 这一层只做 FFI 与执行,不涉及 MCP 协议分发;参数解析复用 [`crate::parse`]。

pub mod capture;
pub mod input;
pub mod uia;
pub mod window;

pub use window::{hwnd_num, hwnd_str, rect_json, WindowEntry};
