//! mcp-window-tools 可执行入口:进程级 DPI 初始化 + stdio 服务循环。

use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

fn main() {
    // 与计算机宿主一致的 DPI 感知:确保 EnumWindows/GetWindowRect/BitBlt 全部使用物理坐标
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    mcp_window_tools::protocol::serve_stdio();
}
