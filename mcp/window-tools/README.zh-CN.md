# mcp-window-tools

> 维护者向：架构、内部机制与 Windows API 陷阱见 [TECHNICAL.md](TECHNICAL.md)。

个人 Windows 窗口/截图辅助工具集，以 **stdio MCP** 方式提供给 Claude Code 调用。独立 crate，不依赖桥接器，也不替代 `gemini-computer` 的原生 Computer Use 执行器。

## 工具

| 工具 | 功能 |
|---|---|
| `list_windows` | 枚举顶层窗口，支持标题子串 / 类名 / PID 过滤，返回 hwnd（十进制字符串）、pid、标题、类名、可见性、最小化状态和屏幕矩形 |
| `get_window_rect` | 单查一个窗口的外框与客户区矩形；可传精确 hwnd 或 `@active` |
| `screenshot_window` | 截取指定窗口渲染内容；`client_only` 可只截客户区，`hwnd: "@active"` 可截当前前台窗口 |
| `screenshot_all` | 逐台显示器全屏截图，每台返回 PNG + 标签 |
| `resize_window` | 调整目标窗口尺寸（可选移动），受目标窗口自身最小尺寸约束 |
| `window_activate` | 请求将精确 hwnd 放到前台；受 Windows 焦点保护约束，不发送输入 |
| `pixel_scan_line` | 沿客户区一列扫描屏幕像素并合并颜色带，量化面板边界与高度（布局断言基元） |
| `pixel_find_color` | 在客户区矩形内查找目标颜色，返回匹配数与最小包围盒 |
| `send_input` | 向指定窗口注入鼠标/键盘输入（move/click/down/up/scroll/key_*），非前台窗口默认拒绝 |

窗口定位三选一：`hwnd`（十进制字符串，或 `0x`/`0X` 前缀的十六进制，十六进制解析修复后支持）/ `title`（标题子串）/ `pid`（该进程面积最大的可见窗口）。一次调用必须且只能提供其中一种；`resize_window` 仅接受由 `list_windows` 返回的精确 `hwnd`，避免误改同名窗口。

进程为 **PerMonitorV2 DPI 感知**，所有坐标均为物理像素。

## 工具参数

### `list_windows`

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `title` | string | — | 标题子串过滤（忽略大小写） |
| `class` | string | — | 窗口类名子串过滤（忽略大小写） |
| `pid` | integer | — | 按进程 PID 过滤 |
| `visible_only` | boolean | `true` | 只返回可见窗口 |
| `limit` | integer | `100` | 最多返回条数，上限 `500` |

每行同时报告 `class`（窗口类名）与 `minimized`（是否最小化）。

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
| `method` | string | `auto` | 采集方式：`auto`=PrintWindow，近纯黑或失败先试 WGC 合成帧再回退屏幕采样；`wgc`=直接走 Windows Graphics Capture GPU 合成帧（实验） |
| `incremental` | boolean | `false` | 增量差分（实验，仅 `method=wgc` 有效）：首次调用返回基线全帧，后续只返回相对上次的变化区域图像，无变化时仅返回文本 |

`hwnd` / `title` / `pid` 必须且只能提供一个。最小化窗口对两种采集方式都确定性返回
`WINDOW_NOT_VISIBLE`（快速结构化错误）。

### `screenshot_all`

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `method` | string | `auto` | 采集方式：`auto`=屏幕 BitBlt（不含光标）；`wgc`=Windows Graphics Capture 合成帧（实验，含光标） |

每台显示器一张 PNG，并返回各显示器标签。

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

### `pixel_scan_line`

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `hwnd` | string | — | 精确句柄，或 `@active` |
| `x` | integer | — | 扫描列（客户区 X） |
| `y_from` | integer | — | 起始行（客户区 Y，含） |
| `y_to` | integer | — | 结束行（客户区 Y，不含） |
| `tolerance` | integer | `2` | 相邻行每通道颜色差阈值（0–255） |
| `min_band` | integer | `2` | 短于该高度的颜色带被忽略 |
| `max_bands` | integer | `256` | 最多返回颜色带数（1–256） |

沿 `x` 列采样窗口客户区的屏幕显示，把相邻（每通道差 ≤ `tolerance`）的连续行合并为颜色带；每带返回 `y_from`/`y_to`（客户区行号，含）、`height` 与平均颜色 `#RRGGBB`。适合做布局断言（面板边界、控件高度、空白占位）。被遮挡或最小化的区域按屏幕当前显示内容采样。

### `pixel_find_color`

| 参数 | 类型 | 说明 |
|---|---|---|
| `hwnd` | string | 精确句柄，或 `@active` |
| `x` / `y` | integer | 区域左上角（客户区） |
| `width` / `height` | integer（≥1） | 区域宽高 |
| `color` | string | 目标颜色 `#RRGGBB`（`#` 可省略） |
| `tolerance` | integer（0–255，默认 `0`） | 每通道差 |

在客户区矩形区域内查找匹配像素，返回 `match_count` 与最小包围盒（`bbox`，无匹配时 null）。与 `pixel_scan_line` 配合可做"该面板在 y=773 处恰好高 74 px"这类断言。

### `send_input`

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `hwnd` | string | — | 精确句柄，或 `@active` |
| `activate` | boolean | `false` | 先请求置前；Windows 焦点保护可能拒绝 |
| `delay_ms` | integer | `50` | 动作间延迟（0–5000） |
| `actions` | array | — | 有序输入动作（见下），1–128 项 |

每个动作形如 `{kind, x, y, button, delta, vkey, steps}`，只使用与 kind 相关的字段。鼠标坐标为客户区物理像素；`move` 的 `steps`（1–200）用于插值，按住按钮时即为拖拽。

| kind | 相关字段 | 效果 |
|---|---|---|
| `move` | `x`, `y`, `steps` | 移动指针（`steps`>1 时插值） |
| `click` | `x`, `y`, `button` | 按下+释放 |
| `down` / `up` | `x`, `y`, `button` | 按下/释放按钮（`left`/`right`/`middle`） |
| `scroll` | `x`, `y`, `delta` | 滚轮格数（120/格，正=上滚） |
| `key_tap` / `key_down` / `key_up` | `vkey` | 虚拟键码：整数 0–255 或按键名（`Enter`、`A`、`F5`、`Left`、`Ctrl` 等） |

**安全**：目标窗口必须在整个批次中保持为前台窗口，每个鼠标坐标必须位于当前客户区内。所有动作会先整体校验，`down`/`key_down` 必须在同一批次内由 `up`/`key_up` 配平；执行途中失败时会自动释放本批仍按下的键。每批计划延迟不超过 60 秒。这是除 `resize_window` 外唯一具有明显副作用的工具，只在可信会话中使用。

## HWND 格式

hwnd 默认按十进制解析；带 `0x`/`0X` 前缀按十六进制解析，并做 u64→i64 范围检查。非法或越界值以错误文本报出所尝试的值。`@active` 只可作为 `hwnd` 的值使用（例如 `{"hwnd":"@active"}`）；它读取 Windows 当前前台 HWND，不会模糊匹配标题。

## 截图预算

- `screenshot_window` 保留原尺寸，最高单边 7680 px、总计 1200 万像素；超过时请先用 `resize_window` 缩小目标窗口。
- `screenshot_all` 保证最多三台 UHD 4K（3840×2160）显示器原尺寸输出，总预算为 24,883,200 像素。超过时每台按相同比例缩放，并在结果中报告原始与实际尺寸。
- `PrintWindow` 对 GPU 加速、受保护或无响应的窗口不保证可用；遇到黑屏或失败，应改用其他观察方式，不会退化为全屏截图。

此工具用于窗口观察、布局验证与 UI 驱动；`send_input` 的鼠标/键盘注入会改变真实桌面状态，因此带前台窗口检查守卫，且只在可信会话中调用。无剪贴板或进程控制。桌面内容会作为 MCP 工具结果进入当前 Claude Code 会话，应只在可信会话中调用。

`window_activate`、`resize_window`、`send_input` 是改变状态的操作；`list_windows`、`get_window_rect`、`screenshot_window`、`screenshot_all`、`pixel_scan_line`、`pixel_find_color` 为只读观察。

## 构建

```bash
cd mcp/window-tools
cargo build --release
# 产物: target/x86_64-pc-windows-msvc/release/mcp-window-tools.exe
```

测试（共 14 项）覆盖 hwnd 解析（十进制、`0x`/`0X` 十六进制、空白、负数、溢出、非法值）、颜色解析、颜色带合并、按键名解析、输入批次安全约束、panic 隔离与工具清单：

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

换行分隔 JSON-RPC 2.0（与 claude-computer-host 同一模式）：`initialize` → `notifications/initialized` → `tools/list` → `tools/call`；工具失败返回 `isError: true` 文本块；协议版本 `2025-11-25`（兼容 2025-06-18 / 2025-03-26）。每个请求都做 Rust panic unwind 隔离：可展开 panic 返回 `-32603` 错误并继续服务；访问违规、abort、OOM abort 等原生进程故障不在此保证范围内。
