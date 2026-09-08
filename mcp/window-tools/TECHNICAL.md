# mcp-window-tools 技术文档

> 面向维护者:架构、内部机制、Windows API 细节与已知陷阱。
> 面向使用者的参数说明见 [README.md](README.md) / [README.zh-CN.md](README.zh-CN.md)。

## 1. 定位与总体架构

`mcp-window-tools` 是**独立 crate**(不挂桥接器 workspace,与桥接器共用同名依赖版本),
以 **stdio + 换行分隔 JSON-RPC 2.0** 提供窗口观察、布局验证与 UI 驱动工具。
进程为 **PerMonitorV2 DPI 感知**,与 claude-computer-host 的协议约定一致。

代码分层(自 v0.2.0 起为 lib + 薄 bin):

```
protocol.rs    handle_request → handle_request_isolated     initialize / ping / tools/list / tools/call
               └─ catch_unwind(AssertUnwindSafe(...))       ← 单请求 panic unwind 隔离
               └─ serve_stdio:每请求统计埋点 + 分级日志(stdout 绝不写诊断)
tools.rs       工具目录/调度/选择器解析 + 工具 schema(tools_list)
parse.rs       纯解析原语:json_i32 / json_u32 / parse_hwnd_str / parse_hex_color
error.rs       稳定错误契约:ToolError { code, message, retryable },14 个稳定码
log.rs         分级日志(stderr / WT_LOG_FILE,WT_LOG_LEVEL 控级)
diagnostics.rs 运行统计/错误环形缓冲/GDI 自检(get_diagnostics 工具)
backend/
  window.rs     窗口枚举/矩形/标题(超时读取)/resize/activate(升级阶梯)/遮挡走查
  capture.rs    GDI 采集:PrintWindow/BitBlt、预算、RAII 句柄守卫、worker+硬超时
  wgc.rs        WGC 合成帧:设备缓存 + 会话 LRU 缓存 + 增量差分 + 显示器路径
  ddapi.rs      DXGI Desktop Duplication:会话按显示器矩形缓存 + staging Map 直扫
  input.rs      前台输入:批次预校验、配平、RAII 释放守卫、贝塞尔插值执行
  bezier.rs     拟人轨迹纯数学:xorshift64*、三阶贝塞尔、缓入缓出、高斯抖动
  background.rs 后台输入:PostMessageW 直投(鼠标) + EM_REPLACESEL/WM_CHAR(文本)
  winevent.rs   WinEvent hook 线程 + 等待者表(wait_for_window)
  pixelsimd.rs  找色引擎:AVX2(8 像素/32 字节)+ 标量回退 + 偏移链校验
  pixelmatch.rs pixel_match_multi 编排:参数解析、采集源选择、排序截断
  uia.rs        UIA 查询(COM 限定 worker 线程)
  elevation.rs  UIPI 提权探测(OpenProcessToken ERROR_ACCESS_DENIED 技巧)
```

依赖(windows 0.61.3 与桥接器同版本):`png` / `jpeg-encoder` / `base64` / `serde_json` /
`regex`(v0.8.0 起,wait_for_window 标题正则)。无 tokio、无 rand —— 异步打通用
`std::sync::mpsc`,随机源为自研 xorshift64*(§7.3)。

运行模型:**stdio 主循环单线程顺序处理**;可能阻塞的操作(截图、UIA)在
**worker 线程**执行 + 硬超时;WinEvent 回调在**独立 hook 线程**的泵循环内
(§5.4)。同一时刻只有一个工具请求在执行。

## 2. 坐标与 DPI 模型

| 类别 | 语义 |
|---|---|
| 屏幕坐标 | 虚拟桌面**物理像素**(PerMonitorV2 下 GetWindowRect/ClientToScreen 均返回物理值) |
| `window_rect` | 窗口外框屏幕矩形(含标题栏/边框/阴影) |
| `client_rect` | 客户区屏幕矩形(`client_rect_of` = GetClientRect + 两角 ClientToScreen,返回屏幕坐标矩形) |
| 像素/输入工具的 `x`/`y` | **客户区相对物理像素**;`pixel_match_multi` 的 monitor 模式为**显示器相对** |
| 后台输入的命中 | 客户区坐标 → 屏幕坐标 → `RealChildWindowFromPoint` → 子控件局部坐标 |

## 3. 目标窗口解析

`resolve_target`(观察类工具):`hwnd` / `title` / `pid` 三选一,selector 计数强制互斥;
`@active` 是 `hwnd` 值的快捷方式(读前台窗口,不做模糊匹配)。模糊选择时可见优先、
前台优先、面积最大优先;唯一匹配但不可见返回 `WINDOW_NOT_VISIBLE`。

`resolve_explicit_hwnd`(副作用与单查工具):只接受 `hwnd`(可为 `@active`),
title/pid 直接拒绝 —— 多窗口场景下防误操作。

## 4. 采集管线

### 4.1 窗口截图 `screenshot_window`

```
method=auto(默认):
  IsHungAppWindow 预检 → WINDOW_HUNG
  IsIconic 预检 → WINDOW_NOT_VISIBLE(确定性拒绝,不挂超时)
  PrintWindow(PW_RENDERFULLCONTENT / PW_CLIENTONLY) → GetDIBits 32bpp BGRA
    ├─ 可用(近纯黑检测:亮度<8 占比>98% 判黑,只按 RGB,alpha 不可靠)→ 编码输出
    └─ 近纯黑/失败 → WGC 缓存会话(§5.1) → 成功标注 fallback=wgc (...)
                     └─ WGC 失败 → 屏幕 BitBlt 采样(遮挡按屏幕现状)标注 fallback=screen (...)
method=wgc: 直接 WGC 缓存会话(被遮挡窗口也产出完整内容;DRM 拒绝/黑区;
            最小化确定性 WINDOW_NOT_VISIBLE;Win10 无去边框)
incremental=true(仅 wgc): 基线全帧 → 逐行像素差分聚带(≤8 区,超则合并外接盒)
             → 变化区域裁剪 PNG / 纯文本零图像。不信任原生 DirtyRegions:
             帧池缓冲有限,长间隔脏区相对的是中间帧,会漏报。
```

来源标注是**契约的一部分**(`fallback=wgc|screen (...)`,带原因),E2E 有断言。

### 4.2 全屏截图 `screenshot_all`

| method | 来源 | 光标 | 备注 |
|---|---|---|---|
| `auto` | 屏幕 BitBlt(StretchBlt 预算缩放) | 不含 | 默认;免疫 PrintWindow 类黑屏 |
| `wgc` | 每屏一次性 `CreateForMonitor` 会话(不落缓存) | 含 | 首帧预算 3s;无合成场景单屏报错 |
| `ddapi` | DXGI Desktop Duplication(§5.2) | 不含 | 单屏失败回退 BitBlt,`source=bitblt` 标注 |

总预算:三台 UHD 4K(3840×2160)不缩放;超出按统一比例 `resize_bgra` 双线性缩放
(与 StretchBlt 语义对齐),`max_dim` 再套一层。

### 4.3 像素采样源

| 来源 | 用途 | 语义 |
|---|---|---|
| `capture_client_region_bgra` | auto 像素工具 | 屏幕 BitBlt,实时;被遮挡区域采到遮挡方像素(响应 `occluded`) |
| `capture_screen_rect_bgra` | DDAPI 回退 | 虚拟桌面矩形 BitBlt(v0.8.0 从 client 版拆出) |
| `wgc::capture_client_region_wgc` | wgc 像素工具 | DWM 合成帧裁剪,遮挡准确但帧异步 |
| `ddapi::DupFrame` | monitor 模式 | staging Map 后**按行距直扫**,零整图拷贝;`DupMappedFrame` Drop 时 Unmap |

### 4.4 采样降级链的架构事实

DDAPI(DXGI Desktop Duplication)是**显示器级**复制 —— DXGI 不存在按窗口的
Duplication,因此它服务全屏截图与显示器像素扫描,不参与窗口级截图;
窗口链保持 `PrintWindow → WGC → 屏幕采样`。

## 5. 缓存与并发结构

### 5.1 WGC 会话缓存(`wgc.rs`)

- **D3D11 设备**进程级缓存(硬件→WARP,`create_device` 与 DDAPI 共用);
  WinRT `IDirect3DDevice` 封装不缓存(未实现 Send,转换实测 ~0ms)。
- **会话**按 `(hwnd, client_only, incremental)` 键缓存,LRU 上限 4(与系统
  CaptureSession 配额保持距离);每次调用顺手清扫已销毁窗口。
- 帧语义:窗口静态时 DWM 不产帧,`TryGetNextFrame` 返回 null+S_OK;既有会话
  立即取帧,空则等一个 DWM 交付周期(~100ms)再取,仍无帧复用上次 BGRA 重编码。
- 尺寸变化以 `frame.ContentSize()` 为准(帧纹理固定于建池尺寸),不符即重建,
  恢复动画期瞬时值由 2 次尝试上限自愈。
- 增量会话 `SetIsCursorCaptureEnabled(false)`:光标移动不产帧,差分确定。
- 两处 WinRT 坑:首帧需轮询(100ms 间隔/10s 预算);`IDirect3DSurface` 对 COM QI
  一律 E_NOINTERFACE,必须 `cast::<IDirect3DDxgiInterfaceAccess>()` 再 `GetInterface`。
- COM 限定 worker 线程 MTA(`with_mta`),接口指针随闭包返回释放后才
  `CoUninitialize`。

### 5.2 DDAPI 会话缓存(`ddapi.rs`)

- 会话按**显示器矩形**(`DXGI_OUTPUT_DESC.DesktopCoordinates` 匹配)缓存,
  键由 `EnumDisplayMonitors` 的矩形给出 —— 与用户可见的显示器序号一致。
- 取帧:`AcquireNextFrame(0)` 非阻塞 + 10ms 空转,新帧预算 500ms;
  桌面静止无帧 → 复用 `last_bgra`(语义等价);首帧无缓存 → `DDAPI_UNAVAILABLE`。
- 数据通路:`CopyResource`(GPU,入队)→ `ReleaseFrame`(桌面资源立即归还)→
  调用级 staging 纹理 `Map`(CPU 同步等待)→ 扫描/编码 → Drop 时 Unmap。
  映射仅在调用内有效,指针绝不过夜。
- 会话失效(ACCESS_LOST/INVALID_CALL/DEVICE_REMOVED:锁屏、UAC 安全桌面、
  驱动重置)自动销毁重建一次,再失败上抛。

### 5.3 提权探测(`elevation.rs`)

提权进程的 token 拒绝非提权调用方 `OpenProcessToken(TOKEN_QUERY)` ——
ERROR_ACCESS_DENIED 本身即提权信号。`send_input`/`window_activate`/
`send_input_background`/`send_text_background` 前置 `ensure_uipi_injectable`,
提权目标 + 未提权服务 → 结构化 `UIPI_BLOCKED`(UIPI 静默丢弃输入,无错误码,
不提前拒绝会让模型陷入"点了没反应"重试死循环)。

### 5.4 WinEvent hook 线程(`winevent.rs`)

- 进程级单例 hook 线程:`SetWinEventHook(EVENT_MIN..EVENT_MAX, OUTOFCONTEXT)`
  + 独立 `GetMessageW` 泵循环,回调在该线程执行。
- 等待者表 `Mutex<Vec<WaiterEntry>>`(上限 8),std mpsc 通道打通回调与等待调用;
  `HOOK_READY: OnceLock<Result<(), &'static str>>` 是 Copy,免克隆同步。
- 竞态封口:注册等待者 → hook 就绪 → 枚举现有窗口 —— 注册前已存在与 hook
  安装间隙内出现的窗口都被枚举兜住。
- 回调快速过滤链:无等待者 → 非目标事件 → 本进程窗口 → 不可见 → WS_CHILD
  (仅 CREATE/SHOW,控件级创建事件是噪声)→ 匹配。标题读取(SendMessageTimeoutW
  500ms)只在确有标题正则等待者时执行,且克隆等待者表后**在锁外**读。

## 6. 输入子系统

### 6.1 前台注入(`input.rs`)

批次先整体校验再执行:`down/key_down` 必须同批 `up/key_up` 配平,坐标必须位于
客户区,批 ≤128 动作、计划延迟 ≤60s。`InputReleaseGuard` 在错误路径 Drop 时
按逆序自动释放已按下键(RAII)。执行中每步复检前台状态(`ensure_input_foreground`),
丢失前台即中止 —— 输入绝不落到其他窗口。

### 6.2 拟人轨迹(`bezier.rs`,v0.8.0)

- 随机源:xorshift64*(零依赖),线程局部,时间 + 进程原子计数播种;
  `ThreadId::as_u64` 不稳定,勿用。高斯 = Box-Muller 极坐标法,钳制 ±4σ。
- 轨迹:三阶贝塞尔,控制点在起终连线 1/3、2/3 处 + 垂直高斯偏移
  (σ=距离×0.08,钳 ±0.4×距离);采样参数经缓入缓出 `3t²-2t³` 向两端聚拢;
  每点 ±2px 高斯抖动;**终点强制精确落地**(纯 move 动作必须确定性落点)。
- 步间延迟:高斯 σ=0.15×均值,钳 [0.5,1.2]×均值 —— 60s 预算护栏最多被
  超出 20%,有界。
- 全部函数以显式 `&mut XorShift64` 为参,单测固定种子完全复现。

### 6.3 后台投递(`background.rs`)

- 鼠标:`RealChildWindowFromPoint`(屏幕坐标)命中最深子控件 → `ScreenToClient`
  子控件局部坐标 → `PostMessageW`(WM_MOUSEMOVE/LBUTTONDOWN/UP、RBUTTON…,
  wParam 按批次按住状态维护 MK_* 位)。不抢焦点、不动物理光标、不改前台。
- 文本:类名含 "EDIT"(忽略大小写,兼容 RichEdit20W/WindowsForms10.Edit 等)
  → `EM_SETSEL`(clear_first 时全选)+ `EM_REPLACESEL`(wParam=TRUE,lParam 指向
  NUL 结尾 UTF-16;EM_REPLACESEL 在系统跨进程封送消息列表内);其他控件 →
  `WM_CHAR` UTF-16 码元逐一投递(代理对分两条,换行统一 CR)。发送走
  `SendMessageTimeoutW` 5s + SMTO_ABORTIFHUNG,挂起线程绝不拖死服务。
- **诚实边界是特性**:投递 ≠ 生效。RawInput/DirectInput 应用可能无视 posted
  消息;EM_REPLACESEL 只对处理该消息的编辑控件有效。响应带 `delivery` 字段
  与提示,验证手段是截图/像素断言(README 同款措辞)。

## 7. 找色引擎(`pixelsimd.rs` / `pixelmatch.rs`)

- 锚点扫描:每 32 字节(8 像素)一次 `_mm256_loadu_si256`;逐字节 |差| =
  `or(subs_epu8(a,b), subs_epu8(b,a))`(饱和减,天然无符号);阈值判定 =
  `cmpeq_epi8(min_epu8(diff, tol), diff)`(避免有符号比较在 |差|>127 时翻车);
  movemask 取 32 位逐字节标志,`m & (m>>1) & (m>>2) & 0x1111_1111` 聚合出
  逐像素命中(忽略 alpha 字节)。行首 8 对齐补扫 + 行尾标量补扫。
- 运行时 `is_x86_feature_detected!("avx2")` 探测,标量回退语义一致 ——
  单测对多种行宽(1/3/7/8/9/16/17/31/40/64)互证两条路径。
- 偏移链校验:候选 ± 偏移点必须落在 search_rect 内(i64 运算防溢出),
  各偏移点独立颜色/容差;锚点自身扫描阶段已确认,不复检。
- 实测(--release):4K 全区域零命中 0.99ms、全命中(病态,830 万结果物化)21.5ms。
- 像素读取器 `PixelReader::{Contiguous, Strided}`:Strided 供 DDAPI 映射内存
  按行距直读;`read` 是 unsafe fn(有 `# Safety` 契约文档)。

## 8. 错误契约(`error.rs`)

`ToolError { code: &'static str, message: String, retryable: bool }`,retryable 由
**单一码表**推导(14 个码,只增不改)。协议层响应保留 MCP 惯用形状
(`content[text]` + `isError`)并追加 `error_code`/`retryable`。

不可重试:`INVALID_ARGUMENT` `LIMIT_EXCEEDED` `WGC_UNAVAILABLE`
`DDAPI_UNAVAILABLE` `UIPI_BLOCKED` `INTERNAL_ERROR`;
可重试:`WINDOW_NOT_FOUND` `WINDOW_NOT_VISIBLE` `WINDOW_HUNG` `NOT_FOREGROUND`
`ACTIVATION_DENIED` `CAPTURE_FAILED` `INPUT_FAILED` `UIA_UNAVAILABLE`。

## 9. 预算与限制一览

| 常量 | 位置 | 值 |
|---|---|---|
| MAX_CAPTURE_DIM / MAX_WINDOW_CAPTURE_PIXELS | capture.rs | 7680 px / 1200 万像素 |
| MAX_ALL_CAPTURE_PIXELS | capture.rs | 3×3840×2160(三台 UHD 4K 不缩放) |
| CAPTURE_TIMEOUT_MS | capture.rs | 15s(worker 硬超时;超时线程泄漏是记录在案的边界) |
| 近纯黑判定 | capture.rs | 亮度<8 占比>98%(只按 RGB,alpha 不可靠) |
| MAX_INPUT_ACTIONS / 计划延迟 | input.rs | 128 动作 / 60s |
| MAX_BG_ACTIONS / MAX_TEXT_CHARS | background.rs | 64 动作 / 4096 字符 / 5s 文本超时 |
| MAX_OFFSETS / MAX_MATCHES | pixelmatch.rs | 64 偏移 / 4096 匹配(默认 32) |
| MAX_CACHED_SESSIONS(WGC LRU) | wgc.rs | 4(增量脏区上限 8) |
| WGC 首帧轮询 | wgc.rs | 100ms 间隔 / 10s 预算(显示器 3s) |
| DDAPI 新帧预算 | ddapi.rs | 500ms(10ms 空转) |
| MAX_WAITERS / 等待超时 | winevent.rs | 8 / 60s(默认 5s;hook 就绪 5s) |
| 标题读取超时 | window.rs | SendMessageTimeoutW 500ms + SMTO_ABORTIFHUNG |
| 贝塞尔默认 | bezier.rs | curvature 0.08 / 抖动 σ 0.9px 钳 ±2px / 延迟钳 [0.5,1.2]×均值 |

## 10. 线程、超时与资源模型

- stdio 主循环:`handle_request_isolated` 对每个请求 `catch_unwind`,可展开
  panic → `-32603` 并继续服务;access violation/abort 不在保证内。
- 阻塞操作(截图/UIA)经 `run_with_timeout` 在 worker 线程执行;超时后 worker
  无法终止(Rust 无强制杀线程),可能阻塞并泄漏当次资源 —— 挂起预检
  (SMTO_ABORTIFHUNG 标题 + IsHungAppWindow)已把概率压到趋近于零,剩余是
  驱动级卡死的极端边界。
- GDI 句柄 RAII:`GdiHandle`(ScreenDc/MemDc/Bitmap)配对释放,panic 展开路径
  同样不泄漏;WGC `FrameGuard`/`OneShotSessionGuard` 同策略。
- COM:UIA 限定每次查询的 worker 线程(APARTMENTTHREADED,接口先释放再
  CoUninitialize);WGC 限定 worker 线程 MTA(进程内 MTA 唯一);DDAPI 纯原生
  D3D11/DXGI,无需 CoInitialize。
- WinEvent hook 线程进程生命周期内不退出;回调绝不 panic(全部取返回值)。

## 11. 可观测性

- stdout 是唯一协议通道;诊断走 stderr(`WT_LOG_LEVEL`,默认 warn)或
  `WT_LOG_FILE` 落盘。`WT_LOG_LEVEL=info` 拿逐请求轨迹。
- `get_diagnostics`:版本、uptime、请求/工具/错误码统计、GDI/USER 句柄数
  (GetGuiResources)、最近 20 条错误环形缓冲。protocol 层每请求自动埋点。

## 12. 测试布局

- 单元测试 91 项:解析原语、错误码表、颜色带合并、输入批次约束、贝塞尔
  (端点/包络/复现/延迟钳制)、AVX2↔标量互证、偏移链 ROI 约束、DDAPI 矩形
  匹配、WinEvent 匹配器与等待者守卫、后台批次与 MAKELPARAM、schema 契约
  (无 oneOf、数值字段 `["integer","string"]` 容错)。
- 性能冒烟(ignore):4K 全区域扫描,--release 下打印实测毫秒。
- E2E(`tests/run-e2e.ps1`):主套件 87 项 + 挂起窗口 7 项,需交互桌面;
  含正向标题断言、句柄/内存泄漏、LRU 淘汰日志证据、来源标注契约。
  探针为 C#(Add-Type):STA + Application.Run 泵消息;ps1 必须 UTF-8 BOM。
- 质量门禁:CI `cargo fmt --check` + `clippy --all-targets -- -D warnings` + test。

## 13. Windows API 陷阱速查(实测踩过的坑)

- **SendMessageTimeoutW 返回 LRESULT 只是"发送成功"**,WM_GETTEXT 实际拷贝长度
  在 `lpdwResult` 里;误用返回值会得到"所有标题只剩 1 个字符"。
- **HWND 是裸指针包装不满足 Send**,跨线程传 isize 重建。
- **`&Box<dyn Any>` 经 Deref 强转后 downcast_ref 静默失败**,panic 负载直接对
  Box 调。
- WGC:`TryGetNextFrame` 首帧到达前返回 null+S_OK,必须轮询;
  `IDirect3DSurface` 对 QI 一律 E_NOINTERFACE,走 `IDirect3DDxgiInterfaceAccess`。
- WGC 帧纹理固定于建池尺寸,**尺寸变化检测以 ContentSize() 为准**。
- windows-rs 0.61:`_mm256_subs_epu8/min/cmpeq/movemask` 是安全 intrinsic
  (仅 loadu/add 需 unsafe);`D3D11Texture2D` 等接口的 `cast` 需
  `use windows::core::Interface as _`;`Unmap` 返回 `()`(`let _ =` 会吃
  let_unit_value);`IsWindow` 收 `Option<HWND>`、`IsWindowVisible` 收 `HWND`。
- EM_REPLACESEL 的 lParam 是**指针**,跨进程生效依赖系统封送列表
  (EM_REPLACESEL 在内),文本必须 NUL 结尾且不含内嵌 NUL。
- UIA:`CUIAutomation` 是 GUID 常量,CoCreateInstance 由返回值类型推断接口;
  常量驼峰命名作 match pattern 会触发 rustc 原生 `non_upper_case_globals`
  (非 clippy 前缀版)。
- 提权探测:OpenProcessToken 返回 ERROR_ACCESS_DENIED 即提权信号,无需权限。
- 测试脚本:PS5.1 管道中文进原生进程按 ANSI 编码损坏(走 ProcessStartInfo
  字节流或全 ASCII);`ReadToEnd` 在 stdin 未关闭时永久阻塞(异步等待要
  ReadLine);`run-e2e.ps1 -Exe` 转发在 PS5.1 下坏掉,直接
  `& e2e-main.ps1 -Exe <路径>`。
- 最小化窗口 `GetWindowRect` 位置 (-32000,-32000) 但尺寸为正,尺寸预检拦不住,
  必须显式 `IsIconic`。
- e2e 探针 C# 内禁止中文注释(PS5.1 Add-Type 乱码编译失败)。

## 14. 版本演进(摘要)

v0.2.x 架构与活性 → v0.3 UIA → v0.4/0.5 WGC 与增量差分 → v0.6 显示器 WGC 与
使用期修复 → v0.7 多模型容错/UIPI/遮挡感知/JPEG+max_dim/激活阶梯 →
v0.8 后台静默输入、多点比色(AVX2)、DDAPI、WinEvent 事件驱动、贝塞尔拟人轨迹。
详见 [CHANGELOG.md](CHANGELOG.md)。
