//! 通用 Windows GUI 工具集 (stdio MCP for Claude Code)。
//!
//! 模块分层:
//! - [`protocol`] — JSON-RPC(stdio,换行分隔):请求分发、panic 隔离、主循环;
//! - [`tools`] — 工具目录与调度:选择器解析、参数校验、MCP 工具 schema;
//! - [`backend`] — Win32 后端:窗口枚举/矩形、GDI 采集、输入注入;
//! - [`parse`] — 纯参数解析原语(JSON 整数、hwnd 字符串、十六进制颜色),无副作用、易测试。

pub mod backend;
pub mod parse;
pub mod protocol;
pub mod tools;
