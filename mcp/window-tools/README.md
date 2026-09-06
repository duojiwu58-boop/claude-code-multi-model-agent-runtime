# mcp-window-tools

个人 Windows 窗口/截图辅助工具集,以 **stdio MCP** 方式提供给 Claude Code 调用。独立 crate，不依赖桥接器，也不替代 `gemini-computer` 的原生 Computer Use 执行器。

## 工具

| 工具 | 功能 |
|---|---|
| `list_windows` | 枚举顶层窗口,支持标题子串 / PID 过滤,返回 hwnd、标题、可见性、屏幕矩形 |
| `get_window_rect` | 单查一个窗口的外框与客户区矩形；可传精确 hwnd 或 `@active` |
| `screenshot_window` | 截取指定窗口渲染内容；`client_only` 可只截客户区，`hwnd: "@active"` 可截当前前台窗口 |
| `screenshot_all` | 逐台显示器全屏截图,每台返回 PNG + 标签 |
| `resize_window` | 调整目标窗口尺寸(可选移动),受目标窗口自身最小尺寸约束 |
| `window_activate` | 请求将精确 hwnd 放到前台；受 Windows 焦点保护约束，不发送输入 |

窗口定位三选一:`hwnd`(十进制字符串)/ `title`(标题子串)/ `pid`(该进程面积最大的可见窗口)。
一次调用必须且只能提供其中一种；`resize_window` 仅接受由 `list_windows` 返回的精确 `hwnd`，避免误改同名窗口。

进程为 **PerMonitorV2 DPI 感知**,所有坐标均为物理像素。

## 截图预算

- `screenshot_window` 保留原尺寸，最高单边 7680 px、总计 1200 万像素；超过时请先用 `resize_window` 缩小目标窗口。
- `screenshot_all` 保证最多三台 UHD 4K（3840×2160）显示器原尺寸输出，总预算为 24,883,200 像素。超过时每台按相同比例缩放，并在结果中报告原始与实际尺寸。
- `PrintWindow` 对 GPU 加速、受保护或无响应的窗口不保证可用；遇到黑屏或失败，应改用其他观察方式，不会退化为全屏截图。

此工具用于窗口观察和布局；不提供鼠标、键盘、剪贴板或进程控制。桌面内容会作为 MCP 工具结果进入当前 Claude Code 会话，应只在可信会话中调用。

`@active` 只可作为 `hwnd` 的值使用，例如 `{"hwnd":"@active"}`；它不会模糊匹配标题，而是读取 Windows 当前前台 HWND。`window_activate`、`resize_window` 是布局操作，其余工具均为只读观察。

## 构建

```bash
cd mcp/window-tools
cargo build --release
# 产物: target/x86_64-pc-windows-msvc/release/mcp-window-tools.exe
```

## 部署与注册

```powershell
Copy-Item target\x86_64-pc-windows-msvc\release\mcp-window-tools.exe `
  "C:\Program Files\ClaudeCodeBridge\claude-mcp-window-tools.exe"
# ~/.claude.json 的 mcpServers 增加:
# "window-tools": {"type": "stdio", "command": "C:\\Program Files\\ClaudeCodeBridge\\claude-mcp-window-tools.exe", "args": []}
```

重启 Claude Code 会话后生效(新 MCP 仅在会话启动时加载)。

## 协议

换行分隔 JSON-RPC 2.0(与 claude-computer-host 同一模式):`initialize` → `notifications/initialized` → `tools/list` → `tools/call`;工具失败返回 `isError: true` 文本块;协议版本 `2025-11-25`(兼容 2025-06-18 / 2025-03-26)。
