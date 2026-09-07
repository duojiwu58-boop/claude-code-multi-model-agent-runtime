# Changelog

本项目遵循向后兼容演进:工具 schema 只增不删,错误响应保留 MCP 惯用形状
(content[text] + isError) 并新增机器可读字段。错误码一旦发布保持稳定(只增不改)。

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
