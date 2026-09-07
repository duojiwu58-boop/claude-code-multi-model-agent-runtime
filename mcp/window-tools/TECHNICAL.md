# mcp-window-tools 技术文档

> 面向维护者:架构、内部机制、Windows API 细节与已知陷阱。
> 面向使用者的参数说明见 [README.md](README.md) / [README.zh-CN.md](README.zh-CN.md)。

## 1. 定位与总体架构

`mcp-window-tools` 是**独立 crate**(不挂桥接器 workspace,与桥接器共用同名依赖版本),
以 **stdio + 换行分隔 JSON-RPC 2.0** 提供窗口观察、布局验证与 UI 驱动工具。
进程为 **PerMonitorV2 DPI 感知**,与 claude-computer-host 的协议约定一致。

代码分四层(`src/main.rs` 单文件,约 2250 行):

```
协议层    main → handle_request_isolated        initialize / ping / tools/list / tools/call
          └─ catch_unwind(AssertUnwindSafe(handle_request))   ← 单请求 panic unwind 隔离
          └─ jsonrpc_result / jsonrpc_error / mcp_tool_error

分发层    handle_tool_call(name, args)          按工具名分发;统一 isError 文本块约定

工具层    list_windows / get_window_rect / screenshot_window / screenshot_all /
          resize_window / window_activate / pixel_scan_line / pixel_find_color / send_input

解析层    resolve_target / resolve_explicit_hwnd  目标窗口统一定位
          parse_hwnd_str / json_i32 / json_u32   输入校验(防静默截断)
```

运行模型:**单线程、无内部会话状态、顺序处理**。每个 stdin 行是一个完整请求,处理完再写一行响应。
窗口布局与全局鼠标/键盘状态属于进程外部状态,不在“无内部会话状态”的含义内。

## 2. 坐标与 DPI 模型

| 类别 | 语义 |
|---|---|
| 屏幕坐标 | 虚拟桌面**物理像素**(PerMonitorV2 下 GetWindowRect / ClientToScreen 均返回物理值,不受单显示器缩放影响) |
| `window_rect` | 窗口外框屏幕矩形(含标题栏/边框/阴影) |
| `client_rect` | 客户区屏幕矩形(不含标题栏、边框、菜单) |
| `pixel_scan_line` / `pixel_find_color` / `send_input` | **客户区相对坐标**,工具内部用 `client_rect_of` 换算；输入坐标还必须位于当前客户区内 |

理解:所有输入坐标都是"相对客户区"的物理像素;多点屏上不同缩放率不影响一致性,
因为进程 DPI 感知下一切 API 都按物理像素工作。

## 3. 目标窗口解析(所有工具的前置)

### 3.1 定位方式

`resolve_target` 支持三种**互斥**的定位方式,一次调用必须且只能提供一种
(selector 计数校验,防止模糊标题与 PID/HWND 叠加掩盖调用者意图):

| 方式 | 语义 |
|---|---|
| `hwnd` | 十进制字符串/整数 / `0x` 前缀十六进制 / `@active`(取当前前台窗口,要求可见) |
| `title` | 标题子串(忽略大小写)模糊匹配 |
| `pid` | 进程 PID,取该进程**面积最大的可见窗口** |

`@active` **只**在 `hwnd` 槽位有效,不参与模糊匹配 —— 保证截图前后对比的目标稳定。

### 3.2 模糊定位的优先级

可见窗口池(无可见候选时才回退全量)→ **前台窗口优先** → **面积从大到小**。
匹配到但不可见时明确报错。

### 3.3 副作用工具的强制约束

`resize_window` / `window_activate` / `pixel_scan_line` / `pixel_find_color` / `send_input`
走 `resolve_explicit_hwnd`:**必须且只能给 hwnd**,不接受 title/pid 模糊定位 ——
避免在多窗口场景对同名窗口误操作。

### 3.4 历史 bug 与修复

`parse_hwnd_str`(commit 22bf17e):旧实现剥掉 `0x` 前缀后仍按十进制解析,
含 A-F 的十六进制句柄(如 `0x12E05FE`)必然失败,与错误提示承诺的 hex 支持矛盾。
现实现:带 `0x`/`0X` 前缀按 16 进制解析,否则十进制;`u64 → i64` 范围检查。

## 4. 工具实现详解

### 4.1 list_windows

- `EnumWindows` 回调收集 `WindowEntry {hwnd, pid, title, class, visible, minimized, rect}`;
- 过滤:`title` / `class`(子串,忽略大小写)/ `pid` / `visible_only`;
- 排序:可见优先 → 标题非空优先 → 面积从大到小;截断 `limit`(上限 500);
- 每行额外输出 `class`(GetClassNameW)与 `minimized`(IsIconic)。

### 4.2 screenshot_window(PrintWindow 路径)

```
GetWindowRect/ClientToScreen → CreateCompatibleBitmap(原尺寸) → CreateCompatibleDC
→ PrintWindow(hwnd, hdc, client_only ? PW_CLIENTONLY : PW_RENDERFULLCONTENT)
→ GetDIBits(32bpp DIB, biHeight=-h 自上而下) → BGRA → RGBA → PNG
```

关键点:

- **PW_RENDERFULLCONTENT**:捕获窗口自身渲染内容,**可截被遮挡窗口**;部分 GPU 加速/
  受保护/无响应窗口可能失败 —— 失败即报错,**不退化为全屏截图**;
- 位图按原尺寸分配,超大窗口在分配前被预算拒绝(见 §6);
- 单窗口预算:单边 7680 px、总计 1200 万像素。

### 4.3 screenshot_all

- `EnumDisplayMonitors` 枚举逐屏;
- 总预算 **3×UHD 4K(24,883,200 px)**:不超时保持 1.0 缩放;超过时所有屏用**同一比例**
  缩放(保持相对视觉布局一致);
- 每屏再套单图预算;尺寸不同走 `StretchBlt`(缩放发生在采集阶段,不额外分配原图缓冲);
- 单屏失败不失败整体:错误以文本块随 PNG 一并返回。

### 4.4 resize_window / window_activate

- `SetWindowPos(SWP_NOZORDER|SWP_NOOWNERZORDER|SWP_NOACTIVATE)`,返回 requested/applied 双矩形;
- 最小 40×40(窗口自身最小尺寸约束会进一步收紧);
- `window_activate` 用 `SetForegroundWindow` + 前后台比对,**Windows 焦点保护拒绝时明确报错**,
  不假装成功。

### 4.5 pixel_scan_line(布局断言基元)

```
capture_client_region_bgra(hwnd, x, y_from, 1, y_to-y_from)
  → 客户区矩形 + 屏幕 BitBlt(1px 列)→ GetDIBits
→ 逐行 (r,g,b) → merge_color_bands
```

- 采样的是**屏幕当前显示**(窗口被遮挡/最小化的区域按屏幕上实际内容采样),不是
  PrintWindow —— 布局验证针对"用户看到的",且 1px 窗口渲染成本过低、屏幕复制成本可忽略;
- **合并算法**:相邻行每通道差 ≤ `tolerance`(默认 2)归入同一带;只保留高度 ≥ `min_band`
  的带(默认 2,过滤抗锯齿/渲染噪声行);带颜色取带内平均值(四舍五入);
- 返回带区间(客户区行号,含)、高度、平均色 `#RRGGBB`;`max_bands`(默认 256)截断并标 `truncated`。

### 4.6 pixel_find_color

区域扫描(块匹配,非逐行):统计与目标色每通道差 ≤ `tolerance` 的像素数,同时维护
最小包围盒;无匹配时 `bbox: null`。与 `pixel_scan_line` 配合即可做
"该面板在 y=773 处恰好高 74px" 一类断言(实测用于校验 GUI 日志面板高度钳制)。

### 4.7 send_input(唯一有副作用的注入工具)

**动作模型**(有序数组,坐标一律客户区物理像素):

| kind | 字段 | 实现 |
|---|---|---|
| `move` | x,y,steps | `SetCursorPos` + 按 `steps` 插值(每步 ≥10ms);按住按钮时即为拖拽 |
| `click` | x,y,button | SetCursorPos → mouse_event(down) → mouse_event(up) |
| `down` / `up` | x,y,button | 单一按下/释放;必须在**同一批次**配平 |
| `scroll` | x,y,delta | SetCursorPos 后 `mouse_event(MOUSEEVENTF_WHEEL, delta)`,120/格,正=上滚 |
| `key_tap` / `key_down` / `key_up` | vkey | `keybd_event`;tap = down+up |

- **整批预校验**:产生真实输入前解析所有动作,校验坐标、动作参数与 down/up 配平;
- **坐标守卫**:每个鼠标动作执行前重新取得客户区,拒绝负数或越界坐标;
- **前景守卫**:批次开始及每个动作执行前都确认目标仍是前台窗口(防止延迟期间焦点切换后误投);
  `activate:true` 先 `SetForegroundWindow`,失败(焦点保护)则报错;
- **失败清理**:执行途中发生错误或 unwind panic 时,RAII guard 释放本批仍按下的鼠标键/键盘键;
- 按键名解析表:ASCII 字母/数字直通、Enter/Space/Tab/Esc/Backspace/Delete/Insert/
  Home/End/PageUp/PageDown/方向键/Ctrl/Shift/Alt/Win/F1-F24;
- `delay_ms`(0–5000,默认 50)作用于动作之间,也是插值步进间隔;
- 每批最多 128 个动作,按动作间隔与 move 步进估算的计划延迟最多 60 秒,避免阻塞单线程服务。

## 5. 协议层与 panic 隔离

### 5.1 JSON-RPC 骨架

- `initialize`:协议协商,接受 `2025-11-25` / `2025-06-18` / `2025-03-26`,其余回落到 `2025-11-25`;
- `tools/call` 的**错误约定**:工具参数错误、未知工具名及执行失败返回
  `result: {content:[…], isError:true}`(文本块);JSON 解析错误、非法 JSON-RPC 请求与未知
  JSON-RPC method 才返回 JSON-RPC `error`,客户端可区分“工具失败”与“协议调用不可用”;
- 每请求输出一行,写完 flush;stdin 读失败退出循环。

### 5.2 catch_unwind 隔离(2026-09-07 新增)

此前单次可展开的 Rust panic 会带崩整个 stdio 进程,客户端表现为 **Connection closed**
(实测复现)。现实现提供的是 **panic unwind 隔离**,不是任意原生崩溃隔离:
访问违规、进程 abort、OOM abort 或不可跨 FFI 展开的 panic 仍可能终止进程。
现结构:

```rust
fn handle_request_isolated(request: &Value) -> Option<Value> {
    match catch_unwind(AssertUnwindSafe(|| handle_request(request))) {
        Ok(response) => response,
        Err(payload) => Some(jsonrpc_error(id, -32603,
            format!("请求执行崩溃，已隔离（进程继续运行）: {}", panic_payload_message(&payload)))),
    }
}
```

坑位(已写入单元测试与注释,**实测验证**):

- `panic_payload_message` 必须接收 **`&Box<dyn Any + Send>`** 并对 Box 直接调
  `downcast_ref`。接收 `&(dyn Any + Send)` 时,`&Box<dyn>` 经 Deref 强转后的 fat pointer
  downcast 会**静默失败**;这与 std panic hook 通过 `PanicInfo::payload()` 拿 `&dyn` 的路径不同;
- panic 展开不会释放已取得的 GDI 句柄(hdc/hbitmap 泄漏)—— 泄漏有界且只在崩溃路径发生,
  通过上述隔离避免级联;正常错误路径全部走 `?` + 手动释放。

## 6. 预算与校验

| 常量 | 值 | 用途 |
|---|---|---|
| `MAX_CAPTURE_DIM` | 7680 px | 单边硬上限 |
| `MAX_WINDOW_CAPTURE_PIXELS` | 12,000,000 | 单窗口/单屏/单扫描区域像素上限 |
| `MAX_ALL_CAPTURE_PIXELS` | 3×3840×2160 = 24,883,200 | 多屏截图总预算 |
| `MAX_WINDOWS_LIMIT` | 500 | list_windows 条目上限 |
| `MAX_SCAN_BANDS` | 256 | 颜色带条数上限 |
| `MAX_INPUT_ACTIONS` | 128 | 单个输入批次动作上限 |
| `MAX_INPUT_SCHEDULED_DELAY_MS` | 60,000 ms | 动作间隔与 move 步进的计划延迟上限 |

- `checked_pixels`:所有位图分配前先以 i64 计算面积,防 i32 乘法溢出(小分配/越界);
- `json_i32` / `json_u32`:JSON 数字先做类型与范围校验再转,防 PID/坐标被静默截断指向其他进程;
- 三个新工具返回统一 `content` 包装(与既有工具一致,符合 MCP `CallToolResult` 规范)。

## 7. 构建、测试、部署

### 7.1 构建

仓库根目录 `.cargo/config.toml` 有全局 `[build] target = "x86_64-pc-windows-msvc"`,
所以**产物不在** `target/release/`,而在:

```
target/x86_64-pc-windows-msvc/release/mcp-window-tools.exe
```

### 7.2 测试(14 项)

| 组 | 覆盖 |
|---|---|
| hwnd 解析 | 十进制 / 0x 十六进制 / 空白 / 负数 / 溢出 / 非法 |
| 颜色解析 | 带/不带 `#`、大小写、长度错误、非 ASCII |
| 颜色带合并 | 相邻相似合并、平均色、min_band 过滤、tolerance 边界 |
| 按键名 | 字母/数字/F 键/方向键/非法值/越界整数 |
| 输入批次 | 客户区坐标、down/up 配平、动作数与计划时长限制、schema 发布 |
| panic 隔离 | 负载提取(三种形态)、隔离后保留请求 id、正常路径不受影响 |
| 工具清单 | 9 工具齐全、新工具 schema 约束(必填 hwnd、annotations) |

### 7.3 部署

```powershell
# 1) 杀旧进程(文件占用:Device or resource busy)
taskkill /IM claude-mcp-window-tools.exe /F
# 2) 备份 + 覆盖
Copy-Item ...\claude-mcp-window-tools.exe "...\claude-mcp-window-tools.exe.backup-yyyyMMdd-HHmmss"
Copy-Item target\x86_64-pc-windows-msvc\release\mcp-window-tools.exe `
  "C:\Program Files\ClaudeCodeBridge\claude-mcp-window-tools.exe"
```

注册在 `~/.claude.json` 的 `mcpServers.window-tools`(stdio,直接指向上述路径);
**重开会话才加载**(Claude Code 会话启动时连 MCP)。

## 8. windows-rs 0.61 签名陷阱(写代码前先 grep 源码确认)

| API | 0.61.3 签名 | 备注 |
|---|---|---|
| `BitBlt` | `-> windows_core::Result<()>` | 不是 BOOL,错误用 `.map_err`/`.is_ok()` |
| `StretchBlt` | `-> BOOL` | 与 BitBlt 不对称,一眼容易被带偏 |
| `mouse_event` / `keybd_event` | `Win32::UI::Input::KeyboardAndMouse` | **需要 feature `Win32_UI_Input_KeyboardAndMouse`**(并不在 `Win32_UI_Input` 下直接导出) |
| `MOUSE_EVENT_FLAGS` | 常量本身是带类型新类型 | 函数参数不认裸 `u32`,需用 `MOUSEEVENTF_*` 常量 |
| `GetClassNameW` | `(&mut [u16]) -> i32` | 直接传 buffer slice |
| `IsIconic` / `GetCursorPos` / `SetCursorPos` | WindowsAndMessaging | SetCursorPos 返回 `Result<()>` |

## 9. 设计约定与扩展点

- **只读与破坏性泾渭分明**:annotations 矩阵中 `readOnlyHint` / `destructiveHint` 与
  工具语义严格对应(仅 `resize_window`、`send_input` 为破坏性操作);
- 幂等性:观察类工具 `idempotentHint: true`;注入类为 false;
- 扩展新动作:在 `send_input` 的 `kind` 匹配中加分支 + README 表 + 单测,无需改动协议层;
- 扩展新观察维度:在 `pixel_scan_line` 的合并算法上叠加(如按列/区域聚合),保持返回带结构兼容。
