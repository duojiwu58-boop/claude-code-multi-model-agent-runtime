# Changelog

本项目遵循向后兼容演进:工具 schema 只增不删,错误响应保留 MCP 惯用形状
(content[text] + isError) 并新增机器可读字段。错误码一旦发布保持稳定(只增不改)。

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
