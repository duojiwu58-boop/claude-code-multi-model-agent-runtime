//! 运行统计与自检:请求/工具/错误码计数、错误环形缓冲、GDI 句柄自检。
//! `get_diagnostics` 工具把这些状态暴露给调用方,用于远程排障。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use serde_json::{json, Value};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetGuiResources, GET_GUI_RESOURCES_FLAGS,
};

use crate::error::ToolError;

/// 错误环形缓冲容量:保留最近 20 条,足够定位"发生了什么"而不吃内存。
const ERROR_RING_CAPACITY: usize = 20;

#[derive(Clone)]
struct ErrorRecord {
    /// 相对启动时刻的毫秒偏移。
    offset_ms: u64,
    tool: String,
    code: String,
    message: String,
}

struct DiagnosticsState {
    started_at: Instant,
    total_requests: AtomicU64,
    total_tool_calls: AtomicU64,
    total_errors: AtomicU64,
    methods: Mutex<HashMap<String, u64>>,
    tool_calls: Mutex<HashMap<String, u64>>,
    error_codes: Mutex<HashMap<String, u64>>,
    last_errors: Mutex<VecDeque<ErrorRecord>>,
}

fn state() -> &'static DiagnosticsState {
    static STATE: OnceLock<DiagnosticsState> = OnceLock::new();
    STATE.get_or_init(|| DiagnosticsState {
        started_at: Instant::now(),
        total_requests: AtomicU64::new(0),
        total_tool_calls: AtomicU64::new(0),
        total_errors: AtomicU64::new(0),
        methods: Mutex::new(HashMap::new()),
        tool_calls: Mutex::new(HashMap::new()),
        error_codes: Mutex::new(HashMap::new()),
        last_errors: Mutex::new(VecDeque::new()),
    })
}

/// 记录一次完整请求(协议方法 + 工具名 + 结果码)。协议层每个请求调用一次。
pub fn record_request(method: &str, tool: Option<&str>, result_code: Option<&str>) {
    let state = state();
    state.total_requests.fetch_add(1, Ordering::Relaxed);
    *state
        .methods
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(method.to_string())
        .or_insert(0) += 1;
    if let Some(tool) = tool {
        state.total_tool_calls.fetch_add(1, Ordering::Relaxed);
        *state
            .tool_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(tool.to_string())
            .or_insert(0) += 1;
    }
    if let Some(code) = result_code {
        state.total_errors.fetch_add(1, Ordering::Relaxed);
        *state
            .error_codes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(code.to_string())
            .or_insert(0) += 1;
    }
}

/// 记录一次工具错误明细(环形缓冲)。
pub fn record_tool_error(tool: &str, code: &str, message: &str) {
    let state = state();
    let mut ring = state.last_errors.lock().unwrap_or_else(|e| e.into_inner());
    if ring.len() >= ERROR_RING_CAPACITY {
        ring.pop_front();
    }
    ring.push_back(ErrorRecord {
        offset_ms: state.started_at.elapsed().as_millis() as u64,
        tool: tool.to_string(),
        code: code.to_string(),
        message: message.to_string(),
    });
}

/// GDI 对象数自检(泄漏的早期信号:正常采集后应回落为 0)。
fn gdi_objects() -> u32 {
    unsafe { GetGuiResources(GetCurrentProcess(), GET_GUI_RESOURCES_FLAGS(0)) }
}

/// USER 对象数自检。
fn user_objects() -> u32 {
    unsafe { GetGuiResources(GetCurrentProcess(), GET_GUI_RESOURCES_FLAGS(1)) }
}

/// `get_diagnostics` 工具载荷:自检 + 统计 + 最近错误。
pub fn diagnostics_payload() -> Result<Value, ToolError> {
    let state = state();
    let methods = state
        .methods
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let tool_calls = state
        .tool_calls
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let error_codes = state
        .error_codes
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let last_errors: Vec<Value> = state
        .last_errors
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|e| {
            json!({
                "offset_ms": e.offset_ms,
                "tool": e.tool,
                "code": e.code,
                "message": e.message
            })
        })
        .collect();
    let payload = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": state.started_at.elapsed().as_secs(),
        "total_requests": state.total_requests.load(Ordering::Relaxed),
        "total_tool_calls": state.total_tool_calls.load(Ordering::Relaxed),
        "total_errors": state.total_errors.load(Ordering::Relaxed),
        "methods": methods,
        "tool_calls": tool_calls,
        "error_codes": error_codes,
        "gdi_objects": gdi_objects(),
        "user_objects": user_objects(),
        "last_errors": last_errors
    });
    Ok(json!({"content": [{"type": "text", "text": payload.to_string()}]}))
}

/// 仅供测试:清空全部统计,避免全局状态在并行测试间互相污染。
#[cfg(test)]
pub fn reset_for_test() {
    let state = state();
    state.total_requests.store(0, Ordering::Relaxed);
    state.total_tool_calls.store(0, Ordering::Relaxed);
    state.total_errors.store(0, Ordering::Relaxed);
    state
        .tool_calls
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    state
        .methods
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    state
        .error_codes
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    state
        .last_errors
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_shape_and_error_ring_behavior() {
        reset_for_test();
        record_request("tools/call", Some("list_windows"), None);
        record_request(
            "tools/call",
            Some("get_window_rect"),
            Some("WINDOW_NOT_FOUND"),
        );
        record_tool_error("get_window_rect", "WINDOW_NOT_FOUND", "无效句柄");
        let payload = diagnostics_payload().unwrap();
        let text = payload["content"][0]["text"].as_str().unwrap();
        let obj: Value = serde_json::from_str(text).unwrap();
        for key in [
            "version",
            "uptime_seconds",
            "total_requests",
            "total_tool_calls",
            "total_errors",
            "methods",
            "tool_calls",
            "error_codes",
            "gdi_objects",
            "user_objects",
            "last_errors",
        ] {
            assert!(obj.get(key).is_some(), "缺少诊断字段 {key}");
        }
        assert_eq!(obj["total_requests"], 2);
        assert_eq!(obj["total_tool_calls"], 2);
        assert_eq!(obj["total_errors"], 1);
        assert_eq!(obj["methods"]["tools/call"], 2);
        assert_eq!(obj["tool_calls"]["list_windows"], 1);
        assert_eq!(obj["error_codes"]["WINDOW_NOT_FOUND"], 1);
        assert_eq!(obj["last_errors"].as_array().unwrap().len(), 1);

        // 环形缓冲:超容量丢弃最旧,最新在末尾
        reset_for_test();
        for i in 0..(ERROR_RING_CAPACITY + 5) {
            record_tool_error("t", "E", &format!("err {i}"));
        }
        let payload = diagnostics_payload().unwrap();
        let text = payload["content"][0]["text"].as_str().unwrap();
        let obj: Value = serde_json::from_str(text).unwrap();
        let ring = obj["last_errors"].as_array().unwrap();
        assert_eq!(ring.len(), ERROR_RING_CAPACITY);
        assert_eq!(ring[0]["message"], format!("err {}", 5));
        assert_eq!(
            ring[ring.len() - 1]["message"],
            format!("err {}", ERROR_RING_CAPACITY + 4)
        );
    }
}
