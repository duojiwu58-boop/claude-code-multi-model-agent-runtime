# mcp-window-tools

> 维护者向:架构、内部机制与 Windows API 陷阱见 [TECHNICAL.md](TECHNICAL.md)。

个人 Windows 窗口/截图/输入工具集,以 **stdio MCP** 方式提供给 Claude Code 调用。独立 crate,不依赖桥接器,也不替代 `gemini-computer` 的原生 Computer Use 执行器。当前版本 **v0.8.0-experimental**,16 个工具。

进程为 **PerMonitorV2 DPI 感知**,所有坐标均为**物理像素**;除标注外,`x`/`y` 类坐标均相对**窗口客户区**。

## 工具总览

| 工具 | 类别 | 功能 |
|---|---|---|
| `list_windows` | 观察 | 枚举顶层窗口,支持标题子串 / 类名 / PID 过滤,返回 hwnd、pid、标题、类名、可见性、最小化、提权状态与屏幕矩形 |
| `get_window_rect` | 观察 | 单查一个窗口的外框与客户区屏幕矩形;可传精确 hwnd 或 `@active` |
| `screenshot_window` | 观察 | 截取指定窗口渲染内容(PNG/JPEG、可选降采样、增量差分) |
| `screenshot_all` | 观察 | 逐台显示器全屏截图;`method` 支持 `auto`(BitBlt) / `wgc`(合成帧) / `ddapi`(DXGI 桌面复制) |
| `resize_window` | 副作用 | 调整目标窗口尺寸(可选移动),受目标窗口自身最小尺寸约束 |
| `window_activate` | 副作用 | 请求将精确 hwnd 放到前台;受 Windows 焦点保护约束,不发送输入 |
| `pixel_scan_line` | 观察 | 沿客户区一列扫描并合并颜色带,量化面板边界与高度(布局断言基元) |
| `pixel_find_color` | 观察 | 在客户区矩形内查找目标颜色,返回匹配数与最小包围盒 |
| `pixel_match_multi` | 观察 | 多点特征比色:锚点 + 相对偏移链,AVX2 引擎 4K 全区域毫秒级;支持 hwnd 客户区或显示器 DDAPI 显存直扫 |
| `send_input` | 副作用 | 向前台窗口注入鼠标/键盘输入;move 走三阶贝塞尔拟人轨迹 |
| `send_input_background` | 副作用 | 后台静默鼠标:PostMessageW 直投,不抢焦点、不动物理光标 |
| `send_text_background` | 副作用 | 后台写入文本:EDIT 控件 EM_REPLACESEL 整段置换 / 非编辑控件 WM_CHAR 字符流 |
| `wait_for_window` | 观察 | WinEvent 事件驱动等待窗口出现(零轮询),返回句柄、矩形、标题、类名、PID |
| `inspect_element_at` | 观察 | UIA 命中测试:客户区某点的无障碍元素属性 |
| `inspect_focused_element` | 观察 | 系统当前键盘聚焦元素的 UIA 属性 |
| `get_diagnostics` | 观察 | 服务自检:版本、请求/错误统计、GDI/USER 句柄数、最近错误 |

**窗口定位**三选一:`hwnd`(十进制字符串,支持 `0x`/`0X` 十六进制)/ `title`(标题子串)/ `pid`(该进程面积最大的可见窗口),一次调用必须且只能提供一种。`resize_window`/`send_input` 等带副作用或单查工具只接受精确 `hwnd`(可为 `@active`,即当前前台窗口)。

## 工具参数

### `list_windows`

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `title` | string | — | 标题子串过滤(忽略大小写) |
| `class` | string | — | 窗口类名子串过滤(忽略大小写) |
| `pid` | integer | — | 按进程 PID 过滤 |
| `visible_only` | boolean | `true` | 只返回可见窗口 |
| `interactive_only` | boolean | `true` | 只返回人类可交互窗口(非最小化、≥100×100、非工具窗口、有标题);false 返回全部(含输入法/托盘等噪点) |
| `limit` | integer | `100` | 最多返回条数,上限 500,超出按 500 钳制 |

每行报告 `hwnd`(十进制字符串)、`pid`、`title`、`class`、`visible`、`minimized`、`elevated`(目标进程是否提权:true 时本服务注入输入会被 UIPI 静默拦截)与屏幕矩形。

### `get_window_rect`

| 参数 | 类型 | 说明 |
|---|---|---|
| `hwnd` | string | 精确窗口句柄,或 `@active` |

返回窗口外框与客户区的屏幕坐标矩形。

### `screenshot_window`

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `hwnd` / `title` / `pid` | — | — | 定位三选一;`hwnd` 可为 `@active` |
| `client_only` | boolean | `false` | 仅截客户区,不含标题栏与边框 |
| `method` | string | `auto` | 采集方式:`auto`=PrintWindow,近纯黑/失败先试 WGC 合成帧再回退屏幕采样;`wgc`=直接走 Windows Graphics Capture GPU 合成帧(实验) |
| `format` | string | `png` | 输出编码:`png` 无损(像素断言);`jpeg` 有损但体积小 60-90%(全局语义观察) |
| `quality` | integer | `85` | jpeg 质量 1-100,仅 `format=jpeg` 有效 |
| `max_dim` | integer | `0` | 输出最长边上限,超过按比例双线性缩小;0=不限制。视觉模型 token 按分辨率计费,整体观察建议 1568~1920 |
| `incremental` | boolean | `false` | 增量差分(实验,仅 `method=wgc` 且 `format=png`、`max_dim=0`):首次返回基线全帧,后续只返回变化区域裁剪,无变化时仅文本 |

响应文本标注实际来源:`fallback=wgc (...)` / `fallback=screen (...)`(PrintWindow 未产出可用图像时);WGC 缓存会话复用时标注"复用上次帧"。最小化窗口对两种采集方式都确定性返回 `WINDOW_NOT_VISIBLE`(1ms 结构化拒绝,不挂超时)。

### `screenshot_all`

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `method` | string | `auto` | `auto`=屏幕 BitBlt(不含光标);`wgc`=Windows Graphics Capture 合成帧(实验,含光标);`ddapi`=DXGI Desktop Duplication 显存复制(实验,显示器级、不含光标,单屏不可用回退 BitBlt 并标注) |
| `format` | string | `png` | `png` 无损 / `jpeg` 有损 |
| `quality` | integer | `85` | jpeg 质量,仅 `format=jpeg` 有效 |
| `max_dim` | integer | `0` | 输出最长边上限,0=不限制 |

每台显示器一张图,文本标注尺寸、`method=` 与实际来源(`source=screen/wgc/ddapi/bitblt`);单屏失败不中断其余。

### `resize_window`

| 参数 | 类型 | 说明 |
|---|---|---|
| `hwnd` | string | 精确句柄(不接受 title/pid) |
| `width` / `height` | integer(≥40) | 目标尺寸(单边 ≤7680) |
| `x` / `y` | integer | 可选,目标屏幕坐标 |

有副作用的布局操作,受目标窗口自身最小尺寸约束。

### `window_activate`

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `hwnd` | string | — | 精确句柄,或 `@active` |
| `allow_alt_trick` | boolean | `false` | 激活最后手段:向前台窗口注入一次 Alt 按键解除焦点锁(有副作用) |

激活阶梯:直连 → AttachThreadInput → (可选)Alt 点击;响应 `activation_method` 标注哪一级生效。只请求置前,不发送鼠标键盘输入。

### `pixel_scan_line`

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `hwnd` | string | — | 精确句柄,或 `@active` |
| `x` | integer | — | 扫描列(客户区 X) |
| `y_from` / `y_to` | integer | — | 起始/结束行(客户区 Y,`y_to` 不含) |
| `tolerance` | integer | `2` | 相邻行每通道颜色差阈值(0–255) |
| `min_band` | integer | `2` | 短于该高度的颜色带被忽略 |
| `max_bands` | integer | `256` | 最多返回颜色带数(1–256) |
| `method` | string | `auto` | `auto`=屏幕采样(实时,遮挡时结果不可信,响应 `occluded` 标注);`wgc`=DWM 合成帧(遮挡准确但帧异步) |

每带返回 `y_from`/`y_to`(客户区行号,含)、`height` 与平均颜色 `#RRGGBB`。

### `pixel_find_color`

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `hwnd` | string | — | 精确句柄,或 `@active` |
| `x` / `y` | integer | — | 区域左上角(客户区) |
| `width` / `height` | integer(≥1) | — | 区域宽高 |
| `color` | string | — | 目标颜色 `#RRGGBB`(`#` 可省略) |
| `tolerance` | integer | `0` | 每通道差(0–255) |
| `method` | string | `auto` | 同 `pixel_scan_line` |

返回 `match_count` 与最小包围盒(`bbox`,无匹配时 null)。

### `pixel_match_multi`

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `hwnd` | string | — | 精确句柄或 `@active`(与 `monitor` 二选一,客户区坐标) |
| `monitor` | integer(≥1) | — | 显示器序号(与 `hwnd` 二选一,显示器坐标,走 DDAPI 直扫) |
| `anchor` | object | — | `{x, y, color, tolerance}`:锚点颜色 `#RRGGBB` 与期望位置(结果按距离排序用) |
| `offsets` | array | `[]` | 校验点链 `{offset_x, offset_y, color, tolerance}`(≤64 项,偏移缺省 0) |
| `search_rect` | object | 全区域 | `{x, y, width, height}` 查找范围 |
| `method` | string | `auto` | hwnd 模式采样来源:`auto`=屏幕(遮挡即错采,`occluded` 标注);`wgc`=DWM 合成帧 |
| `max_matches` | integer | `32` | 最多返回匹配数(1–4096) |

匹配语义:在 `search_rect` 内找锚点色全部命中 → 逐候选校验偏移链(偏移点必须落在 `search_rect` 内,各自独立颜色/容差)→ 按与期望锚点欧氏距离升序返回。响应含 `matches`(目标坐标系坐标)、`candidate_count`、`match_count`(截断前总数)、`truncated`、`engine`(avx2/scalar)与 `capture_elapsed_ms`/`scan_elapsed_ms`。

性能:x86_64 AVX2 下 4K(3840×2160)全区域锚点扫描实测 **~1 ms**(`cargo test --release -- --ignored` 可复现);全屏同色这类病态输入(每像素命中)约 20 ms,由结果物化主导。monitor 模式经 DDAPI staging 纹理 Map 后直接指针扫描,免整图连续拷贝;DDAPI 不可用(锁定/安全桌面/静态桌面无帧)回退屏幕采样并标注 `fallback`。

### `send_input`

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `hwnd` | string | — | 精确句柄,或 `@active` |
| `activate` | boolean | `false` | 先请求置前再注入(激活阶梯同 `window_activate`) |
| `allow_alt_trick` | boolean | `false` | 激活最后手段(有副作用,仅 `activate:true` 有效) |
| `delay_ms` | integer | `50` | 动作间延迟(0–5000) |
| `actions` | array | — | 有序输入动作(1–128 项,见下) |

| kind | 相关字段 | 效果 |
|---|---|---|
| `move` | `x`, `y`, `steps` | 移动指针;`steps`(1–200)控制插值,按住按钮时即为拖拽 |
| `click` / `down` / `up` | `x`, `y`, `button` | 按下+释放 / 按下 / 释放(`left`/`right`/`middle`) |
| `scroll` | `x`, `y`, `delta` | 滚轮格数(120/格,正=上滚) |
| `key_tap` / `key_down` / `key_up` | `vkey` | 虚拟键码:整数 0–255 或按键名(`Enter`、`A`、`F5`、`Left`、`Ctrl` 等) |

v0.8.0 起 `move` 走**拟人化轨迹**:三阶贝塞尔随机弧线(弯曲 σ=距离×0.08)+ 缓入缓出速度分布 + 每步 ±2px 高斯抖动 + 步间延迟高斯扰动(钳制 [0.5,1.2]×均值,计划延迟最多超出预算 20%),终点精确落地。

**安全**:目标窗口必须在整个批次中保持前台,鼠标坐标必须位于当前客户区内;整批先校验再执行,`down`/`key_down` 必须同批 `up`/`key_up` 配平,失败路径自动释放已按下的键;每批计划延迟 ≤60 秒;目标提权且本服务未提权时返回 `UIPI_BLOCKED`。

### `send_input_background`

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `hwnd` | string | — | 父窗口精确句柄,或 `@active` |
| `client_x` / `client_y` | integer | — | 批次基准点(客户区),各动作缺省继承 |
| `delay_ms` | integer | `50` | 动作间延迟(0–5000) |
| `actions` | array | — | 有序动作(1–64 项,见下) |

| kind | 效果 |
|---|---|
| `click` / `down` / `up` | 按下+释放 / 按下 / 释放;`button` 可选 `left`/`right`/`middle`,默认 left |
| `right_click` | 右键按下+释放 |
| `move` | 投递 WM_MOUSEMOVE |

实现:`RealChildWindowFromPoint` 命中最深子控件 → 子控件局部坐标 → `PostMessageW` 直投 `WM_MOUSEMOVE`/`WM_LBUTTONDOWN`/`WM_LBUTTONUP` 等。响应逐动作报告命中子控件句柄、类名与局部坐标。**不抢焦点、不动物理光标、不改变前台窗口**;`down` 必须同批 `up` 配平。

**诚实边界**:消息只是投递(posted),使用 RawInput/DirectInput 或消息泵不健全的应用可能不响应,请以截图或像素断言验证实际效果;目标提权且本服务未提权时返回 `UIPI_BLOCKED`。

### `send_text_background`

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `hwnd` | string | — | 目标输入框(编辑控件)句柄,可为 `@active` |
| `text` | string | — | 写入文本(≤4096 字符) |
| `clear_first` | boolean | `false` | 先清空再写入(仅 EDIT 控件有效) |

标准 EDIT/RichEdit 控件走 `EM_SETSEL`(可选全选)+ `EM_REPLACESEL` 系统封送整段置换——瞬间完成、免击键漏字;非编辑控件走 `WM_CHAR` 字符流投递(UTF-16 码元逐一投递,代理对分两条,换行统一 CR)。目标线程挂起时 5s 超时放弃;全程不抢焦点、不动键盘状态。建议先用 `inspect_element_at` 定位输入框精确句柄。

### `wait_for_window`

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `title_regex` | string | — | 窗口标题正则(Rust regex 语法,默认大小写不敏感) |
| `class` | string | — | 窗口类名子串(忽略大小写) |
| `pid` | integer | — | 进程 PID(精确) |
| `timeout_ms` | integer | `5000` | 超时(1–60000) |
| `case_insensitive` | boolean | `true` | `title_regex` 大小写不敏感 |

三者至少给一个(AND 关系)。进程级单例 WinEvent hook 监听 `EVENT_OBJECT_CREATE`/`EVENT_OBJECT_SHOW`(顶层窗口)/`EVENT_SYSTEM_FOREGROUND`,命中即返回窗口 hwnd、屏幕矩形、标题、类名、PID 与事件来源;已存在的匹配窗口立即返回(`source=existing`)。超时返回正常响应 `found=false`(非错误)。这是 `list_windows` 轮询的替代品:操作系统事件直接唤醒,零轮询延迟、零 token 开销。

### `inspect_element_at` / `inspect_focused_element`

| 参数 | 类型 | 说明 |
|---|---|---|
| `hwnd` | string | 精确句柄,或 `@active`(仅 `inspect_element_at`) |
| `x` / `y` | integer | 检查点(客户区;仅 `inspect_element_at`) |

UIA 无障碍树查询,返回控件类型(名称+类型码)、名称、类名、AutomationId、屏幕包围矩形、是否可键盘聚焦。Chromium 类应用的无障碍树可能惰性激活,首次查询可能只剩 RenderWidgetHostHWND 层;目标未实现 UIA 提供程序时返回 `UIA_UNAVAILABLE`。

### `get_diagnostics`

无参数。返回版本、运行时长、请求/错误统计(按工具与错误码分组)、当前 GDI/USER 句柄数与最近 20 条错误记录。排障时配合 `WT_LOG_LEVEL=info` 重启拿逐请求轨迹。

## HWND 格式

hwnd 默认按十进制解析;带 `0x`/`0X` 前缀按十六进制解析。`@active` 只可作为 `hwnd` 的值使用(`{"hwnd":"@active"}`),读取 Windows 当前前台 HWND。

## 错误契约

工具错误保留 MCP 惯用形状(`content[text]` + `isError: true`),同时返回 `error_code` 与 `retryable` 供调用方按码分支。错误码稳定(只增不改):

| 错误码 | 含义 | 可重试 |
|---|---|---|
| `INVALID_ARGUMENT` | 参数缺失/越界/格式错误/批次未配平 | 否 |
| `LIMIT_EXCEEDED` | 尺寸/像素/批次规模/计划延迟超出预算 | 否 |
| `WGC_UNAVAILABLE` | WGC 不可用(设备/DRM/无合成) | 否 |
| `DDAPI_UNAVAILABLE` | DDAPI 不可用(锁定/安全桌面/无帧且无缓存) | 否 |
| `UIPI_BLOCKED` | 目标提权且本服务未提权,注入会被静默拦截 | 否 |
| `INTERNAL_ERROR` | 不可预期内部错误(panic 路径) | 否 |
| `WINDOW_NOT_FOUND` | 窗口不存在/句柄失效 | 是 |
| `WINDOW_NOT_VISIBLE` | 窗口存在但不可见/已最小化 | 是 |
| `WINDOW_HUNG` | 目标窗口无响应 | 是 |
| `NOT_FOREGROUND` | 目标不是前台窗口 | 是 |
| `ACTIVATION_DENIED` | Windows 焦点保护拒绝激活 | 是 |
| `CAPTURE_FAILED` | GDI 采集/编码失败 | 是 |
| `INPUT_FAILED` | 输入执行期失败 | 是 |
| `UIA_UNAVAILABLE` | UIA 服务不可用或元素属性读取失败 | 是 |

## 截图预算

- `screenshot_window` 保留原尺寸,单边 ≤7680 px、总计 ≤1200 万像素;超限请先 `resize_window`。
- `screenshot_all` 最多三台 UHD 4K(3840×2160)原尺寸输出;超出按相同比例缩放并报告原始/实际尺寸。
- 截图与 UIA 查询都在 worker 线程执行 + 15s 硬超时,挂起窗口绝不卡死服务。

## 构建与测试

```bash
cd mcp/window-tools
cargo build --release
# 产物: target/x86_64-pc-windows-msvc/release/mcp-window-tools.exe
```

```bash
cargo test --locked                       # 91 项单元测试
cargo test --release -- --ignored         # 4K 扫描性能冒烟(输出实测毫秒数)
pwsh -File tests/run-e2e.ps1              # E2E 回归(87 项主套件 + 7 项挂起,需交互桌面)
cargo fmt --check && cargo clippy --all-targets -- -D warnings
```

## 部署

```powershell
pwsh -File mcp/window-tools/deploy.ps1
# 流程:构建 release → 冒烟验证 → 停止运行中的 MCP 进程 → 备份 → 覆盖
# 目标: C:\Program Files\ClaudeCodeBridge\claude-mcp-window-tools.exe
# 注册(~/.claude.json 的 mcpServers):
# "window-tools": {"type": "stdio", "command": "C:\\Program Files\\ClaudeCodeBridge\\claude-mcp-window-tools.exe", "args": []}
```

部署后 Claude Code 会在下一次工具调用时自动重连 MCP,新二进制即刻生效,无需重启会话(新工具 schema 在重连后生效)。

## 协议

换行分隔 JSON-RPC 2.0:`initialize` → `notifications/initialized` → `tools/list` → `tools/call`;协议版本 `2025-11-25`(兼容 `2025-06-18` / `2025-03-26`)。每个请求做 Rust panic unwind 隔离:可展开 panic 返回 `-32603` 错误并继续服务。stdout 是唯一协议通道,诊断走 stderr 或 `WT_LOG_FILE` 日志文件。

## 使用边界

`window_activate`、`resize_window`、`send_input`、`send_input_background`、`send_text_background` 是改变桌面状态的操作;其余为只读观察。桌面内容会作为 MCP 工具结果进入当前会话,应只在可信会话中使用。
