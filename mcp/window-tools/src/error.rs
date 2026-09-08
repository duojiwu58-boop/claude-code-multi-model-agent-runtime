//! 稳定错误契约:所有工具错误携带机器可读的稳定码与可重试语义,
//! 同时保留人类可读的中文明细。调用方按 [`ToolError::code`] 分支决策。
//!
//! 错误码一旦发布即保持稳定(只增不改);语义约定:
//! - `retryable=true` — 重试(或先 list_windows/window_activate 再重试)可能得到不同结果;
//! - `retryable=false` — 调用方参数或状态问题,重试无意义,需修改请求。

use std::fmt;

/// 调用方参数问题:缺失、越界、格式错误、批次未配平。重试无意义。
pub const INVALID_ARGUMENT: &str = "INVALID_ARGUMENT";
/// 窗口不存在/句柄已失效。先 list_windows 刷新句柄后可重试。
pub const WINDOW_NOT_FOUND: &str = "WINDOW_NOT_FOUND";
/// 窗口存在但不可见/已最小化。window_activate 或 resize 后可重试。
pub const WINDOW_NOT_VISIBLE: &str = "WINDOW_NOT_VISIBLE";
/// 目标窗口无响应(消息队列挂起)。等待窗口恢复或换目标后可重试。
pub const WINDOW_HUNG: &str = "WINDOW_HUNG";
/// 目标不是前台窗口,输入注入被拒绝。window_activate / activate:true 后可重试。
pub const NOT_FOREGROUND: &str = "NOT_FOREGROUND";
/// Windows 焦点保护拒绝激活请求。人工干预或换时机可重试。
pub const ACTIVATION_DENIED: &str = "ACTIVATION_DENIED";
/// GDI 采集/编码失败(设备上下文、位图、PrintWindow、PNG 编码)。可重试。
pub const CAPTURE_FAILED: &str = "CAPTURE_FAILED";
/// 尺寸/像素/批次规模/计划延迟超出预算。需先 resize_window 或拆分批次。
pub const LIMIT_EXCEEDED: &str = "LIMIT_EXCEEDED";
/// 输入执行期失败(SetCursorPos 等)。可重试。
pub const INPUT_FAILED: &str = "INPUT_FAILED";
/// UIA 服务不可用或元素属性读取失败(COM 初始化失败、目标未实现 UIA 提供程序)。可重试。
pub const UIA_UNAVAILABLE: &str = "UIA_UNAVAILABLE";
/// WGC(Windows Graphics Capture)不可用:D3D11 设备创建失败、目标窗口受 DRM 保护、
/// 捕获被系统拒绝或帧格式意外。环境/能力问题,重试无意义。
pub const WGC_UNAVAILABLE: &str = "WGC_UNAVAILABLE";
/// DDAPI(DXGI Desktop Duplication)不可用:DuplicateOutput 被拒、会话失效
/// (锁定/安全桌面/驱动重置)、显示器不匹配或桌面无新帧且无缓存。
/// 环境/能力问题,同一次调用内重试无意义(调用方可按需回退其他采集方式)。
pub const DDAPI_UNAVAILABLE: &str = "DDAPI_UNAVAILABLE";
/// 目标进程以更高完整性级别运行(典型:管理员提权),UIPI 会静默拦截本服务
/// (未提权)注入的输入——不报错、无效。需以管理员权限重启 MCP 服务或改选
/// 非提权窗口,会话内重试无意义。
pub const UIPI_BLOCKED: &str = "UIPI_BLOCKED";
/// 不可预期内部错误(panic 路径)。应上报维护者。
pub const INTERNAL_ERROR: &str = "INTERNAL_ERROR";

/// 稳定错误码 → 默认可重试语义。新码必须在此登记。
fn retryable_by_code(code: &str) -> bool {
    matches!(
        code,
        WINDOW_NOT_FOUND
            | WINDOW_NOT_VISIBLE
            | WINDOW_HUNG
            | NOT_FOREGROUND
            | ACTIVATION_DENIED
            | CAPTURE_FAILED
            | INPUT_FAILED
            | UIA_UNAVAILABLE
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    /// 稳定错误码(见本模块常量),调用方按此分支。
    pub code: &'static str,
    /// 人类可读中文明细,直接展示给用户或模型。
    pub message: String,
    /// 重试是否可能得到不同结果。
    pub retryable: bool,
}

impl ToolError {
    /// 按错误码构造;retryable 由码表推导,保证全部错误路径语义一致。
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: retryable_by_code(code),
        }
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ToolError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryability_follows_code_table() {
        assert!(!ToolError::new(INVALID_ARGUMENT, "x").retryable);
        assert!(!ToolError::new(LIMIT_EXCEEDED, "x").retryable);
        assert!(!ToolError::new(INTERNAL_ERROR, "x").retryable);
        assert!(!ToolError::new(WGC_UNAVAILABLE, "x").retryable);
        assert!(!ToolError::new(DDAPI_UNAVAILABLE, "x").retryable);
        assert!(!ToolError::new(UIPI_BLOCKED, "x").retryable);
        for code in [
            WINDOW_NOT_FOUND,
            WINDOW_NOT_VISIBLE,
            WINDOW_HUNG,
            NOT_FOREGROUND,
            ACTIVATION_DENIED,
            CAPTURE_FAILED,
            INPUT_FAILED,
            UIA_UNAVAILABLE,
        ] {
            assert!(ToolError::new(code, "x").retryable, "{code} 应可重试");
        }
    }

    #[test]
    fn display_preserves_human_message() {
        let err = ToolError::new(NOT_FOREGROUND, "目标窗口不是前台窗口");
        assert_eq!(err.to_string(), "目标窗口不是前台窗口");
        assert_eq!(err.code, NOT_FOREGROUND);
    }

    #[test]
    fn error_round_trips_as_boxed_error() {
        let err = ToolError::new(CAPTURE_FAILED, "PrintWindow 失败");
        let boxed: Box<dyn std::error::Error> = Box::new(err);
        assert_eq!(boxed.to_string(), "PrintWindow 失败");
    }
}
