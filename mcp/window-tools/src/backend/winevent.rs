//! WinEvent 事件驱动窗口出现监听:`wait_for_window` 的后端。
//!
//! 与轮询 list_windows 相比,窗口出现由操作系统事件**直接唤醒**,零轮询
//! 延迟、零 token 开销。架构:
//! - 进程级单例 hook 线程(独立消息泵):`SetWinEventHook(EVENT_MIN..EVENT_MAX,
//!   WINEVENT_OUTOFCONTEXT)` 监听全桌面窗口事件,回调在该线程执行;
//! - 等待者表:`std::sync::mpsc` 通道打通 hook 线程与等待调用 —— 项目无
//!   tokio 依赖,标准库 channel 即等价物(设计书里的 tokio oneshot 属实现
//!   细节,语义不变);
//! - 竞态封口:注册等待者 → hook 就绪 → 枚举现有窗口一次 —— 注册前已存在
//!   的窗口与 hook 安装间隙内出现的窗口都被这次枚举兜住;
//! - 回调快速路径:无等待者/非目标事件/本进程窗口/不可见窗口立即返回,
//!   标题读取(SendMessageTimeoutW 500ms)只在确有标题正则等待者时执行。
//!
//! 监听事件:EVENT_OBJECT_CREATE / EVENT_OBJECT_SHOW(顶层窗口,
//! WS_CHILD 过滤)与 EVENT_SYSTEM_FOREGROUND(新前台窗口)。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use regex::{Regex, RegexBuilder};
use serde_json::{json, Value};
use windows::Win32::{
    Foundation::{HWND, RECT},
    System::Threading::GetCurrentProcessId,
    UI::{
        Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK},
        WindowsAndMessaging::{
            DispatchMessageW, GetMessageW, GetWindowLongPtrW, GetWindowRect,
            GetWindowThreadProcessId, IsWindow, IsWindowVisible, CHILDID_SELF, EVENT_MAX,
            EVENT_MIN, EVENT_OBJECT_CREATE, EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND, GWL_STYLE,
            MSG, OBJID_WINDOW, WINEVENT_OUTOFCONTEXT, WS_CHILD,
        },
    },
};

use crate::backend::window::{collect_windows, hwnd_num, rect_json, window_class, window_title};
use crate::error::{ToolError, INTERNAL_ERROR, INVALID_ARGUMENT, LIMIT_EXCEEDED};
use crate::parse::{json_i32, json_u32};

/// 并发等待者上限:每个事件回调都要过一遍等待者表,给回调时延上界。
const MAX_WAITERS: usize = 8;
/// 超时上限:单请求占用 stdio 主循环的时间护栏。
const MAX_TIMEOUT_MS: i32 = 60_000;
/// hook 线程就绪等待上限(线程启动失败/死锁护栏)。
const HOOK_READY_TIMEOUT_MS: u64 = 5_000;

/// 等待者匹配条件。全部条件 AND;至少一个由调用方保证。
#[derive(Clone)]
struct WindowMatcher {
    title_regex: Option<Regex>,
    class: Option<String>,
    pid: Option<u32>,
}

impl WindowMatcher {
    fn matches(&self, pid: u32, class: &str, title: &str) -> bool {
        if let Some(p) = self.pid {
            if pid != p {
                return false;
            }
        }
        if let Some(needle) = &self.class {
            if !class.to_lowercase().contains(needle) {
                return false;
            }
        }
        if let Some(re) = &self.title_regex {
            if !re.is_match(title) {
                return false;
            }
        }
        true
    }
}

/// 命中窗口快照(现有枚举或事件回调,同构)。
#[derive(Clone)]
struct FoundWindow {
    hwnd: isize,
    pid: u32,
    title: String,
    class: String,
    rect: RECT,
    source: &'static str,
}

struct WaiterEntry {
    id: u64,
    matcher: WindowMatcher,
    tx: Sender<FoundWindow>,
}

static WAITERS: OnceLock<Mutex<Vec<WaiterEntry>>> = OnceLock::new();
static NEXT_WAITER_ID: AtomicU64 = AtomicU64::new(1);
/// hook 线程安装结果:Result<(), &'static str> 是 Copy,免克隆同步。
static HOOK_READY: OnceLock<Result<(), &'static str>> = OnceLock::new();
static HOOK_STARTED: OnceLock<()> = OnceLock::new();

fn waiters() -> &'static Mutex<Vec<WaiterEntry>> {
    WAITERS.get_or_init(|| Mutex::new(Vec::new()))
}

/// 确保 hook 线程已启动并安装完成(进程级一次性;首次调用等待就绪)。
fn ensure_hook() -> Result<(), ToolError> {
    HOOK_STARTED.get_or_init(|| {
        std::thread::Builder::new()
            .name("winevent-hook".to_string())
            .spawn(hook_thread_main)
            .expect("无法启动 WinEvent hook 线程");
    });
    let mut waited_ms = 0u64;
    loop {
        if let Some(result) = HOOK_READY.get() {
            return result.map_err(|e| {
                ToolError::new(INTERNAL_ERROR, format!("WinEvent hook 安装失败: {e}"))
            });
        }
        if waited_ms >= HOOK_READY_TIMEOUT_MS {
            return Err(ToolError::new(
                INTERNAL_ERROR,
                "WinEvent hook 线程未在限时内就绪",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
        waited_ms += 10;
    }
}

/// hook 线程:安装 → 泵消息直到 WM_QUIT → 卸载(进程生命周期内不退出)。
fn hook_thread_main() {
    let hook = unsafe {
        SetWinEventHook(
            EVENT_MIN,
            EVENT_MAX,
            None,
            Some(winevent_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };
    if hook.0.is_null() {
        let _ = HOOK_READY.set(Err("SetWinEventHook 返回 NULL"));
        return;
    }
    let _ = HOOK_READY.set(Ok(()));
    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        unsafe {
            let _ = DispatchMessageW(&msg);
        }
    }
    unsafe {
        let _ = UnhookWinEvent(hook);
    }
}

fn event_name(event: u32) -> &'static str {
    match event {
        EVENT_OBJECT_CREATE => "event_object_create",
        EVENT_OBJECT_SHOW => "event_object_show",
        EVENT_SYSTEM_FOREGROUND => "event_system_foreground",
        _ => "event_other",
    }
}

/// WinEvent 回调(hook 线程执行)。快速过滤链:无等待者 → 非目标事件 →
/// 本进程 → 不可见 → 子窗口(仅 CREATE/SHOW)→ 匹配。绝不 panic:
/// 全部 Win32 调用取返回值,失败按不命中处理。
unsafe extern "system" fn winevent_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _tid: u32,
    _time: u32,
) {
    if event != EVENT_OBJECT_CREATE
        && event != EVENT_OBJECT_SHOW
        && event != EVENT_SYSTEM_FOREGROUND
    {
        return;
    }
    if event != EVENT_SYSTEM_FOREGROUND
        && (id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32)
    {
        return;
    }
    if hwnd.0.is_null() {
        return;
    }
    // 克隆等待者表后释放锁:标题读取(≤500ms)不在锁内执行。
    let snapshot: Vec<WaiterEntry> = {
        let guard = waiters().lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_empty() {
            return;
        }
        guard
            .iter()
            .map(|w| WaiterEntry {
                id: w.id,
                matcher: w.matcher.clone(),
                tx: w.tx.clone(),
            })
            .collect()
    };

    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return;
    }
    let mut pid = 0u32;
    unsafe {
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    if pid == unsafe { GetCurrentProcessId() } {
        return;
    }
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return;
    }
    // CREATE/SHOW 只收顶层窗口:控件级创建事件(EVENT_OBJECT_CREATE 对每个
    // 子控件都会触发)对"窗口出现"语义是噪声。
    if event != EVENT_SYSTEM_FOREGROUND {
        let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
        if style & WS_CHILD.0 != 0 {
            return;
        }
    }
    let class = window_class(hwnd);
    let need_title = snapshot.iter().any(|w| w.matcher.title_regex.is_some());
    let title = if need_title {
        window_title(hwnd)
    } else {
        String::new()
    };
    let mut rect = RECT::default();
    let _ = unsafe { GetWindowRect(hwnd, &mut rect) };
    for waiter in &snapshot {
        if waiter.matcher.matches(pid, &class, &title) {
            let _ = waiter.tx.send(FoundWindow {
                hwnd: hwnd_num(hwnd),
                pid,
                title: title.clone(),
                class: class.clone(),
                rect,
                source: event_name(event),
            });
        }
    }
}

/// 枚举现有窗口找首个匹配(竞态封口的"已存在"快路径)。
fn find_existing(matcher: &WindowMatcher) -> Option<FoundWindow> {
    collect_windows().into_iter().find_map(|entry| {
        if matcher.matches(entry.pid, &entry.class, &entry.title) {
            Some(FoundWindow {
                hwnd: entry.hwnd,
                pid: entry.pid,
                title: entry.title,
                class: entry.class,
                rect: RECT {
                    left: entry.left,
                    top: entry.top,
                    right: entry.left + entry.width,
                    bottom: entry.top + entry.height,
                },
                source: "existing",
            })
        } else {
            None
        }
    })
}

fn register_waiter(matcher: &WindowMatcher, tx: Sender<FoundWindow>) -> Result<u64, ToolError> {
    let mut guard = waiters().lock().unwrap_or_else(|e| e.into_inner());
    if guard.len() >= MAX_WAITERS {
        return Err(ToolError::new(
            LIMIT_EXCEEDED,
            format!("并发等待者超过上限 {MAX_WAITERS}(前序 wait_for_window 尚未返回)"),
        ));
    }
    let id = NEXT_WAITER_ID.fetch_add(1, Ordering::SeqCst);
    guard.push(WaiterEntry {
        id,
        matcher: matcher.clone(),
        tx,
    });
    Ok(id)
}

/// 等待者守卫:任何返回路径(命中/超时/错误)都从表中移除。
struct WaiterGuard(u64);

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        let mut guard = waiters().lock().unwrap_or_else(|e| e.into_inner());
        guard.retain(|w| w.id != self.0);
    }
}

fn found_response(
    found: FoundWindow,
    elapsed_ms: f64,
    timeout_ms: i32,
) -> Result<Value, ToolError> {
    Ok(json!({
        "content": [{"type": "text", "text": json!({
            "found": true,
            "source": found.source,
            "elapsed_ms": round_ms(elapsed_ms),
            "timeout_ms": timeout_ms,
            "window": {
                "hwnd": found.hwnd.to_string(),
                "pid": found.pid,
                "title": found.title,
                "class": found.class,
                "rect": rect_json(found.rect)
            }
        }).to_string()}]
    }))
}

/// 执行 wait_for_window 工具:等待匹配窗口出现(现有窗口立即返回)。
/// title_regex 为 Rust 正则(默认大小写不敏感);class 子串忽略大小写;
/// pid 精确。三者至少给一个。超时返回正常响应(found=false),不是错误。
pub fn wait_for_window(args: &Value) -> Result<Value, ToolError> {
    let title_pattern = args
        .get("title_regex")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let class = args
        .get("class")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase);
    let pid = json_u32(args, "pid")?;
    if title_pattern.is_none() && class.is_none() && pid.is_none() {
        return Err(ToolError::new(
            INVALID_ARGUMENT,
            "至少需要一个过滤条件(title_regex/class/pid),否则任意窗口都会立刻命中",
        ));
    }
    let timeout_ms = json_i32(args, "timeout_ms", false)?
        .unwrap_or(5000)
        .clamp(1, MAX_TIMEOUT_MS);
    let case_insensitive = args
        .get("case_insensitive")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let title_regex = match title_pattern {
        Some(pattern) => Some(
            RegexBuilder::new(pattern)
                .case_insensitive(case_insensitive)
                .build()
                .map_err(|e| {
                    ToolError::new(INVALID_ARGUMENT, format!("title_regex 正则无效: {e}"))
                })?,
        ),
        None => None,
    };
    let matcher = WindowMatcher {
        title_regex,
        class,
        pid,
    };

    let started = Instant::now();
    ensure_hook()?;
    // 竞态封口:先注册(此时起事件必达)→ hook 就绪 → 枚举现有窗口
    // (兜住注册前与 hook 安装间隙内已出现的窗口)。
    let (tx, rx) = mpsc::channel();
    let waiter_id = register_waiter(&matcher, tx)?;
    let _guard = WaiterGuard(waiter_id);
    if let Some(found) = find_existing(&matcher) {
        return found_response(found, elapsed_s(started), timeout_ms);
    }
    match rx.recv_timeout(Duration::from_millis(timeout_ms as u64)) {
        Ok(found) => found_response(found, elapsed_s(started), timeout_ms),
        Err(RecvTimeoutError::Timeout) => Ok(json!({
            "content": [{"type": "text", "text": json!({
                "found": false,
                "source": "timeout",
                "elapsed_ms": round_ms(elapsed_s(started)),
                "timeout_ms": timeout_ms,
                "note": "超时未出现匹配窗口;可加大 timeout_ms 或先 list_windows 确认过滤条件"
            }).to_string()}]
        })),
        Err(RecvTimeoutError::Disconnected) => {
            Err(ToolError::new(INTERNAL_ERROR, "等待者通道异常断开"))
        }
    }
}

fn elapsed_s(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

fn round_ms(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(
        title: Option<&str>,
        class: Option<&str>,
        pid: Option<u32>,
        case_insensitive: bool,
    ) -> WindowMatcher {
        WindowMatcher {
            title_regex: title.map(|p| {
                RegexBuilder::new(p)
                    .case_insensitive(case_insensitive)
                    .build()
                    .unwrap()
            }),
            class: class.map(str::to_lowercase),
            pid,
        }
    }

    #[test]
    fn matcher_ands_all_conditions() {
        let m = matcher(Some(r"登录|login"), Some("Dialog"), Some(1234), true);
        assert!(m.matches(1234, "MyDialog", "用户登录"));
        assert!(!m.matches(1234, "MyDialog", "欢迎"));
        assert!(!m.matches(1234, "Editor", "用户登录"));
        assert!(!m.matches(99, "MyDialog", "用户登录"));
    }

    #[test]
    fn matcher_case_sensitivity_follows_flag() {
        let sensitive = matcher(Some("Login"), None, None, false);
        assert!(sensitive.matches(1, "", "Login"));
        assert!(!sensitive.matches(1, "", "login"));
        let insensitive = matcher(Some("Login"), None, None, true);
        assert!(insensitive.matches(1, "", "LOGIN"));
    }

    #[test]
    fn class_filter_is_substring_case_insensitive() {
        let m = matcher(None, Some("EDIT"), None, true);
        assert!(m.matches(1, "RichEdit20W", ""));
        assert!(!m.matches(1, "Button", ""));
    }

    #[test]
    fn event_names_are_stable() {
        assert_eq!(event_name(EVENT_OBJECT_CREATE), "event_object_create");
        assert_eq!(event_name(EVENT_OBJECT_SHOW), "event_object_show");
        assert_eq!(
            event_name(EVENT_SYSTEM_FOREGROUND),
            "event_system_foreground"
        );
        assert_eq!(event_name(9999), "event_other");
    }

    #[test]
    fn waiter_guard_removes_only_itself() {
        let (tx1, _rx1) = mpsc::channel();
        let (tx2, _rx2) = mpsc::channel();
        let id1 = register_waiter(&matcher(None, Some("x"), None, true), tx1).unwrap();
        let id2 = register_waiter(&matcher(None, Some("y"), None, true), tx2).unwrap();
        {
            let _guard = WaiterGuard(id1);
        }
        let guard = waiters().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(guard.len(), 1);
        assert_eq!(guard[0].id, id2);
        drop(guard);
        {
            let _guard = WaiterGuard(id2);
        }
        assert!(waiters()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
    }
}
