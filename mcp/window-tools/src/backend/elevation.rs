//! UIPI(用户界面特权隔离)输入完整性探测。
//!
//! 普通权限进程向更高完整性级别(典型:管理员提权)的窗口注入输入时,
//! Windows UIPI 在**无任何错误码**的情况下静默丢弃输入——调用方看到成功
//! 响应但界面毫无反应,模型会陷入"点了没反应"的重试死循环。本模块在注入
//! 前探测目标进程的提权状态,给出结构化拒绝([`UIPI_BLOCKED`])。
//!
//! 探测技巧:提权进程的 token 拒绝非提权调用方的 `OpenProcessToken(TOKEN_QUERY)`,
//! 返回 `ERROR_ACCESS_DENIED`——失败本身即提权信号,无需任何额外权限即可判定;
//! 打开成功则读 `TokenElevation` 确认。本服务自身提权时对任何目标都无
//! UIPI 障碍,跳过全部探测。

use std::sync::OnceLock;

use windows::{
    core::HRESULT,
    Win32::{
        Foundation::{CloseHandle, ERROR_ACCESS_DENIED, HANDLE},
        Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY},
        System::Threading::{
            GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
};

use crate::error::{ToolError, UIPI_BLOCKED};

/// 目标进程提权状态。`Unknown`:进程已退出或打开失败。
/// 未知按不拦截处理——UIPI 只拦"低注入高",漏报的代价是输入无效,
/// 错报的代价是误拒绝,倾向不明时不拒绝。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Elevation {
    Normal,
    Elevated,
    Unknown,
}

/// 进程 token 的提权查询。打开失败且为 ERROR_ACCESS_DENIED = 提权信号;
/// 打开成功读 `TokenElevation` 双确认;其余失败一律 Unknown。
fn token_elevated(process: HANDLE) -> Elevation {
    let mut token = HANDLE::default();
    if let Err(e) = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } {
        return if e.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0) {
            Elevation::Elevated
        } else {
            Elevation::Unknown
        };
    }
    let mut elevation = TOKEN_ELEVATION::default();
    let mut size = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut core::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
    };
    unsafe {
        let _ = CloseHandle(token);
    }
    match ok {
        Ok(()) if elevation.TokenIsElevated != 0 => Elevation::Elevated,
        Ok(()) => Elevation::Normal,
        Err(_) => Elevation::Unknown,
    }
}

/// 查询一个 PID 的提权状态(每查询打开/关闭一次进程句柄)。
pub fn process_elevated(pid: u32) -> Elevation {
    if pid == 0 {
        return Elevation::Unknown;
    }
    let process = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(handle) => handle,
        Err(_) => return Elevation::Unknown,
    };
    let result = token_elevated(process);
    unsafe {
        let _ = CloseHandle(process);
    }
    result
}

/// 本进程是否提权(进程级缓存,只判定一次)。
pub fn self_elevated() -> bool {
    static SELF_ELEVATED: OnceLock<bool> = OnceLock::new();
    *SELF_ELEVATED.get_or_init(|| {
        matches!(
            token_elevated(unsafe { GetCurrentProcess() }),
            Elevation::Elevated
        )
    })
}

/// list_windows 的逐行标志:pid 是否已确认提权(Unknown 记 false,不误报)。
pub fn elevated_flag(pid: u32) -> bool {
    process_elevated(pid) == Elevation::Elevated
}

/// send_input / window_activate 的 UIPI 前置检查:本服务未提权且目标进程
/// 提权时,注入(与 SetForegroundWindow)都会被 UIPI 静默过滤,提前结构化拒绝。
/// 鼠标动作实际落在"光标下的窗口"、键盘落在"前台窗口",检查目标 PID 是
/// 对两者的务实近似(目标既为前台又覆盖全部注入坐标时完全精确)。
pub fn ensure_uipi_injectable(target_pid: u32) -> Result<(), ToolError> {
    if self_elevated() {
        return Ok(());
    }
    if process_elevated(target_pid) == Elevation::Elevated {
        return Err(ToolError::new(
            UIPI_BLOCKED,
            format!(
                "目标进程 pid={target_pid} 以管理员权限运行,UIPI 会静默拦截本服务(未提权)注入的输入;请以管理员权限重启 MCP 服务,或改选非提权窗口"
            ),
        ));
    }
    Ok(())
}
