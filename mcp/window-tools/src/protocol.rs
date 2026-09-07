//! MCP JSON-RPC (stdio,换行分隔):请求分发、panic 隔离与主循环。
//! stdout 是唯一协议通道,任何诊断输出必须走 stderr 或日志文件。

use std::io::{BufRead, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};

use serde_json::{json, Value};

use crate::tools;

pub const MCP_PROTOCOL_VERSION: &str = "2025-11-25";

fn jsonrpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn jsonrpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

fn mcp_tool_error(message: impl Into<String>) -> Value {
    json!({
        "content": [{"type": "text", "text": message.into()}],
        "isError": true
    })
}

fn handle_request(request: &Value) -> Option<Value> {
    let id = request.get("id").cloned();
    if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Some(jsonrpc_error(
            id.unwrap_or(Value::Null),
            -32600,
            "Invalid JSON-RPC request",
        ));
    }
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let id = id?;
    let response = match method {
        "initialize" => {
            let requested = request
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(MCP_PROTOCOL_VERSION);
            let protocol_version = match requested {
                "2025-11-25" | "2025-06-18" | "2025-03-26" => requested,
                _ => MCP_PROTOCOL_VERSION,
            };
            jsonrpc_result(
                id,
                json!({
                    "protocolVersion": protocol_version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "claude-code-window-tools", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": "本地窗口工具集:枚举窗口、窗口截图、全屏截图、调整窗口尺寸、像素扫描(布局验证)、鼠标键盘输入。进程为 PerMonitorV2 DPI 感知,坐标均为物理像素。"
                }),
            )
        }
        "ping" => jsonrpc_result(id, json!({})),
        "tools/list" => jsonrpc_result(id, tools::tools_list()),
        "tools/call" => {
            let name = request
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let args = request
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match tools::handle_tool_call(name, &args) {
                Ok(result) => jsonrpc_result(id, result),
                Err(message) => jsonrpc_result(id, mcp_tool_error(message)),
            }
        }
        _ => jsonrpc_error(id, -32601, "Method not found"),
    };
    Some(response)
}

/// 提取 panic 负载的文本；非字符串负载给出占位说明。
/// 注意接收 `&Box<dyn Any>` 而不是 `&dyn Any`：前者经 Deref 强制转换后
/// downcast_ref 会静默失败（实测），后者与 std panic hook 的形态一致但暴露
/// 不出 Box 的自带方法，这里直接对 Box 调 downcast_ref 最稳。
fn panic_payload_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "未知 panic 负载".to_string()
}

/// 单请求隔离：任何 panic 都转成 JSON-RPC error 并继续服务，
/// 避免一次未预期的崩溃把整个 stdio 会话带走（此前表现为 Connection closed）。
fn handle_request_isolated(request: &Value) -> Option<Value> {
    match catch_unwind(AssertUnwindSafe(|| handle_request(request))) {
        Ok(response) => response,
        Err(payload) => Some(jsonrpc_error(
            request.get("id").cloned().unwrap_or(Value::Null),
            -32603,
            format!(
                "请求发生可展开的 Rust panic，已隔离（进程继续运行）: {}",
                panic_payload_message(&payload)
            ),
        )),
    }
}

/// stdio 主循环:逐行读取 JSON-RPC 请求并写回响应,直到 stdin 关闭或 stdout 写入失败。
pub fn serve_stdio() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Cannot read MCP stdin: {e}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let response = jsonrpc_error(Value::Null, -32700, format!("Parse error: {e}"));
                let _ = writeln!(stdout, "{response}");
                let _ = stdout.flush();
                continue;
            }
        };
        if let Some(response) = handle_request_isolated(&request) {
            if writeln!(stdout, "{response}")
                .and_then(|_| stdout.flush())
                .is_err()
            {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolates_panic_into_jsonrpc_error() {
        // 模拟 handle_request 的崩溃路径：隔离后返回 error 并保留请求 id。
        let panicking = json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": {"name": "does_not_exist_panic", "arguments": {}}
        });
        // 未知工具是普通 Err（isError result），不 panic：验证正常路径仍返回 result。
        let response = handle_request_isolated(&panicking).unwrap();
        assert_eq!(response["id"], 7);
        assert!(response.get("result").is_some());
        // 与 catch_unwind 一致的负载形态：Box<dyn Any + Send>，内在类型保持 panic! 的字面量
        let as_str: Box<dyn std::any::Any + Send> = Box::new("boom");
        assert_eq!(panic_payload_message(&as_str), "boom");
        let as_string: Box<dyn std::any::Any + Send> = Box::new(String::from("暴雷"));
        assert_eq!(panic_payload_message(&as_string), "暴雷");
        let as_int: Box<dyn std::any::Any + Send> = Box::new(42_i64);
        assert_eq!(panic_payload_message(&as_int), "未知 panic 负载");
    }
}
