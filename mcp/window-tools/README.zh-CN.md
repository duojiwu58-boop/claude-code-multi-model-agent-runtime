# mcp-window-tools

个人 Windows 窗口/截图辅助工具集，以 **stdio MCP** 方式提供给 Claude Code 调用。独立 crate，不依赖桥接器，也不替代 `gemini-computer` 的原生 Computer Use 执行器。

## 工具

| 工具 | 功能 |
|---|---|
| `list_windows` | 枚举顶层窗口，支持标题子串 / PID 过滤，返回 hwnd（十进制字符串）、pid、标题、可见性和屏幕矩形 |
| `get_window_rect` | 单查一个窗口的外框与客户区矩形；可传精确 hwnd 或 `@active` |
| `screenshot_window` | 截取指定窗口渲染内容；`client_only` 可只截客户区，`hwnd: "@active"` 可截当前前台窗口 |
| `screenshot_all` | 逐台显示器全屏截图，每台返回 PNG + 标签 |
| `resize_window` | 调整目标窗口尺寸（可选移动），受目标窗口自身最小尺寸约束 |
| `window_activate` | 请求将精确 hwnd 放到前台；受 Windows 焦点保护约束，不发送输入 |

窗口定位三选一：`hwnd`（十进制字符串，或 `0x`/`0X` 前缀的十六进制，十六进制解析修复后支持）/ `title`（标题子串）/ `pid`（该进程面积最大的可见窗口）。一次调用必须且只能提供其中一种；`resize_window` 仅接受由 `list_windows` 返回的精确 `hwnd`，避免误改同名窗口。

进程为 **PerMonitorV2 DPI 感知**，所有坐标均为物理像素。

## 工具参数

### `list_windows`

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `title` | string | — | 标题子串过滤（忽略大小写） |
| `pid` | integer | — | 按进程 PID 过滤 |
| `visible_only` | boolean | `true` | 只返回可见窗口 |
| `limit` | integer | `100` | 最多返回条数，上限 `500` |

### `get_window_rect`

| 参数 | 类型 | 说明 |
|---|---|---|
| `hwnd` | string | 精确窗口句柄，或 `@active` |

返回窗口外框与客户区的屏幕坐标矩形。

### `screenshot_window`

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `hwnd` | string | — | 窗口句柄（十进制或 `0x` 前缀十六进制）；`@active` 可用 |
| `title` | string | — | 窗口标题子串 |
| `pid` | integer | — | 窗口所属进程 PID |
| `client_only` | boolean | `false` | 仅截客户区，不含标题栏与边框 |

`hwnd` / `title` / `pid` 必须且只能提供一个。

### `screenshot_all`

无参数。每台显示器一张 PNG，并返回各显示器标签。

### `resize_window`

| 参数 | 类型 | 说明 |
|---|---|---|
| `hwnd` | string | 必须由 `list_windows` 返回的精确句柄 |
| `width` | integer（最小 40） | 目标宽度 |
| `height` | integer（最小 40） | 目标高度 |
| `x` | integer | 可选，目标 X 坐标 |
| `y` | integer | 可选，目标 Y 坐标 |

这是有副作用的窗口布局操作，受目标窗口自身最小尺寸约束。

### `window_activate`

| 参数 | 类型 | 说明 |
|---|---|---|
| `hwnd` | string | 精确窗口句柄，或 `@active` |

只请求置前，不发送鼠标或键盘输入。

## HWND 格式

hwnd 默认按十进制解析；带 `0x`/`0X` 前缀按十六进制解析，并做 u64→i64 范围检查。非法或越界值以错误文本报出所尝试的值。`@active` 只可作为 `hwnd` 的值使用（例如 `{"hwnd":"@active"}`）；它读取 Windows 当前前台 HWND，不会模糊匹配标题。

## 截图预算

- `screenshot_window` 保留原尺寸，最高单边 7680 px、总计 1200 万像素；超过时请先用 `resize_window` 缩小目标窗口。
- `screenshot_all` 保证最多三台 UHD 4K（3840×2160）显示器原尺寸输出，总预算为 24,883,200 像素。超过时每台按相同比例缩放，并在结果中报告原始与实际尺寸。
- `PrintWindow` 对 GPU 加速、受保护或无响应的窗口不保证可用；遇到黑屏或失败，应改用其他观察方式，不会退化为全屏截图。

此工具用于窗口观察和布局；不提供鼠标、键盘、剪贴板或进程控制。桌面内容会作为 MCP 工具结果进入当前 Claude Code 会话，应只在可信会话中调用。

`window_activate`、`resize_window` 是布局操作，其余工具均为只读观察。

## 构建

```bash
cd mcp/window-tools
cargo build --release
# 产物: target/x86_64-pc-windows-msvc/release/mcp-window-tools.exe
```

测试（共 8 项）覆盖 hwnd 解析（十进制、`0x`/`0X` 十六进制、空白、负数、溢出、非法值）与工具清单：

```bash
cargo test --locked
```

## 部署与注册

```powershell
Copy-Item target\x86_64-pc-windows-msvc\release\mcp-window-tools.exe `
  "C:\Program Files\ClaudeCodeBridge\claude-mcp-window-tools.exe"
# ~/.claude.json 的 mcpServers 增加:
# "window-tools": {"type": "stdio", "command": "C:\\Program Files\\ClaudeCodeBridge\\claude-mcp-window-tools.exe", "args": []}
```

重启 Claude Code 会话后生效（新 MCP 仅在会话启动时加载）。

本 crate 独立于桥接器安装器：任何桥接器发行包都不包含它，MCP 注册按用户写在 `~/.claude.json` 中。

## 协议

换行分隔 JSON-RPC 2.0（与 claude-computer-host 同一模式）：`initialize` → `notifications/initialized` → `tools/list` → `tools/call`；工具失败返回 `isError: true` 文本块；协议版本 `2025-11-25`（兼容 2025-06-18 / 2025-03-26）。
