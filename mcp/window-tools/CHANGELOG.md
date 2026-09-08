# Changelog

本项目遵循向后兼容演进:工具 schema 只增不删,错误响应保留 MCP 惯用形状
(content[text] + isError) 并新增机器可读字段。错误码一旦发布保持稳定(只增不改)。

## [0.8.0-experimental] - 2026-09-08

### 新增

- **`pixel_match_multi` 多点特征比色**(backend/pixelsimd.rs + pixelmatch.rs):
  锚点色全区域搜索 + 相对偏移链校验(各偏移点独立颜色/容差,ROI 约束),
  按与期望锚点欧氏距离升序返回。x86_64 运行时探测 AVX2 向量化分支
  (8 像素/32 字节,饱和减 |差| + min 阈值 + movemask 位聚合),标量回退语义
  一致(单测互证)。目标二选一:hwnd 客户区(auto 屏幕/wgc 合成帧)或
  monitor(DDAPI staging Map 直扫,零整图拷贝,失败回退屏幕采样)。
  响应带 engine 与 capture/scan 分段时间。实测 4K(3840x2160)全区域锚点
  扫描 ~1 ms;全屏同色病态输入(每像素命中)~20 ms(结果物化主导)。
- **`screenshot_all method=ddapi`**(backend/ddapi.rs):DXGI Desktop
  Duplication 显存复制 — DuplicateOutput 会话缓存(按显示器矩形),新帧
  AcquireNextFrame → CopyResource 到调用级 staging → ReleaseFrame → Map;
  静态桌面复用缓存帧,会话失效(锁屏/安全桌面/驱动重置)自动重建一次。
  单屏不可用回退 BitBlt,MonitorShot 增 `source` 字段(screen/wgc/ddapi/bitblt)。
  D3D11 设备与 WGC 共用创建路径(create_device pub(crate))。新错误码
  `DDAPI_UNAVAILABLE`(不可重试)。
- **`send_input_background` 后台静默鼠标**(backend/background.rs):
  RealChildWindowFromPoint 命中子控件 + 子控件局部坐标 + PostMessageW 直投
  WM_MOUSEMOVE/LBUTTONDOWN/… — 不抢焦点、不动物理光标、不改前台窗口。
  click/down/up/right_click/move,批次预校验 + 配平 + UIPI 前置拒绝。
- **`send_text_background` 后台文本**(同模块):EDIT/RichEdit 走
  EM_SETSEL+EM_REPLACESEL 系统封送整段置换(clear_first 一步清空+写入),
  非编辑控件走 WM_CHAR 字符流(UTF-16 码元逐条,代理对分两条,换行转 CR)。
- **`wait_for_window` 事件驱动等待**(backend/winevent.rs):进程级单例
  SetWinEventHook(OUTOFCONTEXT)+ 专用泵消息线程,监听 OBJECT_CREATE/
  OBJECT_SHOW(WS_CHILD 过滤)/SYSTEM_FOREGROUND;std mpsc 打通回调与等待
  调用(项目无 tokio,等价物),注册→hook 就绪→枚举现有窗口封竞态;超时返回
  正常响应 found=false。新增依赖 regex 1.11.1。
- **`send_input` move 拟人化轨迹**(backend/bezier.rs):三阶贝塞尔随机弧线 +
  缓入缓出速度分布 + 每步 ±2px 高斯抖动 + 步间延迟高斯扰动
  (钳制 [0.5,1.2]×均值);xorshift64* + Box-Muller 零依赖随机源,线程局部
  时间+原子计数播种。终点精确落地,固定种子可复现(单测)。

### 变更

- 工具目录 12 → 16;`#![recursion_limit = "256"]`(json! 宏单函数体超默认
  递归上限);windows features 增 SystemServices/UI_Controls(MK_*/EM_*);
  版本 0.8.0-experimental。
- `capture_client_region_bgra` 拆出公共 `capture_screen_rect_bgra`(屏幕矩形
  采样,DDAPI 回退与客户端路径共用);`window_title`/`window_class`/
  `create_device` pub(crate) 供新后端复用。

### 验证

- 单元测试 56 → 91(新增:贝塞尔端点/包络/复现/延迟钳制、AVX2↔标量路径
  互证、偏移链 ROI 约束、步进读取器行距、DDAPI 错误码、WinEvent 匹配器与
  等待者守卫、后台批次校验与 MAKELPARAM)+ 1 项性能冒烟(ignore,
  --release 实测 4K 零命中 0.99ms / 全命中 21.5ms);fmt/clippy -D warnings
  全绿。

## [0.6.1-experimental] - 2026-09-08

### 修复

- **最小化窗口确定性拒绝**(GDI 与 WGC 两条采集入口):此前 `GetWindowRect` 对
  最小化窗口返回位置 (-32000,-32000) 但尺寸仍为正,尺寸预检拦不住;WGC 缓存
  会话还会复用最小化前的过期旧帧。现显式 `IsIconic` → `WINDOW_NOT_VISIBLE`
  (1ms 结构化拒绝,不挂 15s),恢复窗口后采集正常。
- **尺寸变化重建会话失效**:WGC 帧纹理固定于建池时的 item 尺寸,窗口调整大小后
  纹理不变,只有 `ContentSize()` 跟随变化(实测 640x480 窗口:tex 恒 422x292、
  ContentSize 626x473);原重建逻辑比较纹理尺寸,永远不触发。现以 ContentSize
  为准检测(纹理与 ContentSize 不符同样触发),恢复/最小化动画期的瞬时
  ContentSize 由 2 次尝试上限自愈。

### 变更

- `CachedSession` 新增 `content_size` 字段(与帧纹理尺寸 `size` 分离);
  `screenshot_window` 工具描述同步最小化契约措辞。

### 验证

- **使用期验证(合并前最后一项)5 个可自动化项全部 E2E 收口**(新增 12 项断言):
  ① 最小化中途(缓存会话/无会话/auto/incremental 四路径 + 恢复);② 会话缓存
  压力 50 次(句柄 886→886、内存 +0MB、复用标记);③ 5 会话 LRU 淘汰级联
  (日志证据,在用会话不破坏);④ 窗口销毁中途(WINDOW_NOT_FOUND + sweep 日志 +
  服务存活);⑤ 尺寸变化(PNG 头尺寸断言 422x292→626x473)。日志证据经
  `WT_LOG_FILE` 落盘断言。
- 单元测试 42 项;E2E 主套件 69 项 + 挂起 7 项全绿;clippy -D warnings 零告警
  (顺手清理 4 个遗留告警)。

## [0.6.0-experimental] - 2026-09-08

### 新增

- **screenshot_all 的 WGC 显示器路径**:新增 `method` 参数(`auto`=屏幕 BitBlt,
  默认,不含光标;`wgc`=Windows Graphics Capture 合成帧,实验,含光标)。
  每台显示器一个一次性 `CreateForMonitor` 捕获会话(首帧预算 3s,不落缓存,
  不挤占系统 CaptureSession 配额),单屏失败不中断其余;超预算时与 BitBlt
  路径同比例 CPU 双线性缩放(纯函数 `resize_bgra`,与 StretchBlt 语义对齐)。

### 变更

- `capture_all_monitors` 拆出公共总预算/标签助手,两条路径共用;
- `wait_first_frame` 预算参数化(窗口 10s / 显示器 3s——显示器无合成场景
  应快速单屏报错,而不是把整次多屏调用拖到 15s 硬超时);
- 响应文本每屏带 `method=` 标注。

### 验证

- 实测(3×2560x1440):644ms 三张全分辨率 PNG;单元测试 42 项(新增
  resize_bgra 恒等/均值/通道对齐/非法尺寸);E2E 49+7 绿(新增
  screenshot_all method=wgc 与未知 method 拒绝)。

## [0.5.0-experimental] - 2026-09-08

### 新增

- **WGC 会话缓存 + 增量差分**:
  - 进程级 D3D11 设备缓存(硬件→WARP,只建一次)+ 按 `(hwnd, client_only,
    incremental)` 的捕获会话缓存,LRU 上限 4,每次调用清理已销毁窗口的会话;
  - `screenshot_window` 新增 `incremental` 参数(仅 method=wgc 有效):首次调用
    返回完整基线帧,后续调用只返回相对上次的变化区域图像(文本列出帧坐标矩形),
    无变化时仅返回文本零图像;
  - 增量差分实现:逐行差分 + 变化行聚带(≤8 区,超过合并为外接盒),不信任原生
    DirtyRegions(帧池缓冲有限,长间隔后脏区相对中间帧而非客户端基线,会漏报);
  - 增量会话禁用光标捕获,差分与光标移动无关,判定确定。

### 变更

- method=wgc 与 auto 链的 WGC 分支都走缓存路径:既有会话立即取帧,空则等待
  一个 DWM 交付周期(实测 ≤100ms)再取一次,仍无帧即复用上次帧(语义等价)。
- `WindowCapture` 新增 `incremental`/`reused` 字段;全帧复用时响应文本标注
  `复用上次帧(无新帧)`。
- 等待首帧超预算的报错消息改为明确表述(原消息可能误报 S_OK)。

### 验证

- 探针实测(902x632 硬件设备):全新建链 210-230ms → 缓存命中且窗口有变化
  ~10-30ms、静态窗口 ~100ms(增量零图像)。
- 单元测试 38 项(差分聚带/裁剪/合并等纯函数);E2E 新增 6 项增量契约
  (参数校验/基线/无变化/点击触发变化/回稳)。

## [0.4.1-experimental] - 2026-09-08

### 变更

- **WGC 进 auto 链**:PrintWindow 近纯黑或失败后先试 WGC 合成帧,仍不可用
  再回退屏幕采样;WGC 失败只改变来源标注,不改变错误契约。
- 来源标注动态化:`fallback=wgc (PrintWindow 近纯黑|失败)` /
  `fallback=screen (…; WGC 不可用: <原因>)`;无标注 = PrintWindow 直出。

## [0.4.0-experimental] - 2026-09-08

### 新增

- **WGC 采集路径**:`screenshot_window` 新增 `method` 参数(`auto` 默认 /
  `wgc` = Windows Graphics Capture GPU 合成帧),免疫 PrintWindow 黑屏与被
  遮挡;边界:最小化窗口不产帧、DRM 内容被拒或黑区、CaptureSession 并发上限。
- 新错误码 `WGC_UNAVAILABLE`(不可重试)。

## [0.3.0] - 2026-09-08

### 新增

- **UIA 无障碍树查询**:
  - `inspect_element_at(hwnd, x, y)`:客户区坐标命中测试,返回该点无障碍元素的
    控件类型(名称 + UIA 类型码)/名称/类名/AutomationId/屏幕包围矩形/是否可键盘聚焦;
  - `inspect_focused_element()`:系统当前键盘聚焦元素的同套属性,无需指定窗口。
- 库内首个 COM 使用点:初始化限定在 worker 线程按调用完成,查询复用通用限时
  执行原语(`run_with_timeout`,与截图共用),目标进程挂起时限时返回 WINDOW_HUNG。
- 新错误码 `UIA_UNAVAILABLE`(COM 初始化失败/目标未实现 UIA 提供程序,可重试)。

### 变更

- `capture` 的超时原语泛化为 `run_with_timeout`,截图与 UIA 查询共用同一
  线程泄漏边界说明。
- 工具总数 10 → 12。

### 验证

- 单元测试 29 项;真实 GUI E2E 主套件 36 项(新增确定性 WinForms 按钮探针:
  独立进程 + STA 线程 + Application.Run 消息泵,按钮置于客户区几何中心,
  断言与 DPI 缩放无关)+ 挂起窗口 7 项;clippy -D warnings 零告警。

## [0.2.1] - 2026-09-08

### 新增

- **list_windows 降噪**:新增 `interactive_only` 参数(默认 true)启发式过滤——
  可见、非最小化、≥100×100 物理像素、无 WS_EX_TOOLWINDOW、标题非空;
  `false` 逃生口恢复全量(实测 20 个可见窗口 → 4 个交互窗口)。
- **截图黑屏回退**:PrintWindow 产物近纯黑(亮度<8 占比>98%,只按 RGB 判定,
  兼容位图 alpha 不可靠)或直接失败时,自动回退屏幕采样;响应文本带
  `fallback=` 标注,被遮挡区域按屏幕当前显示内容。像素扫描工具
  (pixel_scan_line / pixel_find_color)本就走屏幕采样,天然免疫。

### 修复

- **窗口标题只剩 1 字符**:SendMessageTimeoutW 的返回 LRESULT 只是"发送成功"
  标志,WM_GETTEXT 实际拷贝字符数在 lpdwResult 里;此前误把返回值当长度。
- **移除 screenshot_window schema 的 oneOf**:Claude Code 客户端会静默丢弃
  inputSchema 含 oneOf 的工具(实测部署端 10 个工具会话仅注册 9 个);
  三个定位字段改为全可选,排他性由 resolve_target 运行时强制。

### 验证

- 单元测试 27 项;真实 GUI E2E 主套件 32 项 + 挂起窗口 7 项
  (`tests/run-e2e.ps1`,需交互桌面与前台进程权限);clippy -D warnings 零告警。

## [0.2.0] - 2026-09-08

### 新增

- **稳定错误契约**:所有工具错误携带 `error_code` + `retryable` 双轨字段
  (旧字段保留)。10 个稳定错误码:INVALID_ARGUMENT / WINDOW_NOT_FOUND /
  WINDOW_NOT_VISIBLE / WINDOW_HUNG / NOT_FOREGROUND / ACTIVATION_DENIED /
  CAPTURE_FAILED / LIMIT_EXCEEDED / INPUT_FAILED / INTERNAL_ERROR。
- **活性保障**(挂起窗口不再卡死服务):
  - 标题读取改用 `SendMessageTimeoutW(WM_GETTEXT, 500ms, SMTO_ABORTIFHUNG)`,
    桌面上存在挂起窗口时 `list_windows` 照常限时完成;
  - 截图前 `IsHungAppWindow` 预检,挂起窗口直接返回 WINDOW_HUNG,不进入 PrintWindow;
  - 截图在 worker 线程执行并设硬超时(15s),驱动级卡死也不阻塞主循环。
- **可观测性**:
  - 分级日志(stderr / 可选 `WT_LOG_FILE`),级别由 `WT_LOG_LEVEL` 控制
    (默认 warn,info 输出每请求一行:工具/耗时/结果码);stdout 保持纯协议通道;
  - 新工具 `get_diagnostics`:版本、运行时长、请求/方法/工具/错误码统计、
    GDI/USER 句柄自检、最近 20 条错误环形缓冲。

### 变更

- 架构:2258 行单文件 main.rs 拆为 lib/bin 分层
  (parse / backend{window,capture,input} / tools / protocol / error / log / diagnostics)。
- GDI 句柄 RAII 守卫:正常与 panic 展开路径均零句柄泄漏。
- 修复 send_input 前置校验(可见性 / activate:true 激活 / 非前台拒绝)。

### 验证

- 单元测试 25 项;真实 GUI E2E 29 项 + 真实挂起窗口 E2E 7 项
  (`tests/run-e2e.ps1`);clippy -D warnings 零告警;GDI 泄漏循环(50 次截图)0→0。

## [0.1.0] - 2026-09-06

- 初始版本:窗口枚举/矩形查询/窗口截图/全屏截图/窗口缩放/窗口激活/
  像素扫描/颜色查找/输入注入 9 工具,DPI PerMonitorV2 物理坐标,
  截图与输入批次预算体系,panic 单请求隔离。
