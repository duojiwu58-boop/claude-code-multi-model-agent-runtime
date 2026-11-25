//! Pure projections from LS state; no prompt generation or client tool calls.
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};

/// Ordered Anthropic content, projected only from native LS display data.
/// A single block is open at a time. Late updates create a fresh block rather
/// than sending deltas to an already closed index.
#[derive(Default)]
pub struct Timeline {
    previous: BTreeMap<(u64, bool), String>,
    tools: BTreeMap<u64, Value>,
    active: Option<(u64, bool, usize)>,
    pub content: Vec<Value>,
    bytes: usize,
}

impl Timeline {
    pub fn close(&mut self) -> Vec<Value> {
        self.active.take().map(|(_, _, index)| vec![json!({"type":"content_block_stop","index":index})]).unwrap_or_default()
    }

    fn delta(&mut self, step: u64, thinking: bool, text: &str, events: &mut Vec<Value>) -> Result<(), String> {
        if text.is_empty() { return Ok(()); }
        self.bytes = self.bytes.saturating_add(text.len());
        if self.bytes > 8 * 1024 * 1024 { return Err("LS response exceeded the 8 MiB text limit".into()); }
        let index = match self.active {
            Some((old_step, old_thinking, index)) if old_step == step && old_thinking == thinking => index,
            _ => {
                events.extend(self.close());
                let index = self.content.len();
                // LS has no Anthropic signature. Empty means unauthenticated,
                // matching the bridge's Codex reasoning adapter; never fabricate one.
                let block = if thinking { json!({"type":"thinking","thinking":"","signature":""}) }
                    else { json!({"type":"text","text":""}) };
                events.push(json!({"type":"content_block_start","index":index,"content_block":block}));
                self.content.push(block);
                self.active = Some((step, thinking, index));
                index
            }
        };
        let field = if thinking { "thinking" } else { "text" };
        let existing = self.content[index][field].as_str().unwrap_or("");
        self.content[index][field] = json!(format!("{existing}{text}"));
        let delta = if thinking { json!({"type":"thinking_delta","thinking":text}) }
            else { json!({"type":"text_delta","text":text}) };
        events.push(json!({"type":"content_block_delta","index":index,"delta":delta}));
        Ok(())
    }

    pub fn observe(&mut self, steps: &BTreeMap<u64, Value>, floor: u64) -> Result<Vec<Value>, String> {
        let mut events = Vec::new();
        for (&index, step) in steps.range(floor..) {
            if step.get("userInput").is_some() || step["type"] == "CORTEX_STEP_TYPE_USER_INPUT" { continue; }
            if let Some(planner) = step.get("plannerResponse") {
                for (thinking, field) in [(true, "thinking"), (false, "response")] {
                    let text = planner[field].as_str().filter(|s| !s.is_empty()).or_else(|| {
                        if thinking { None } else { planner["modifiedResponse"].as_str() }
                    }).unwrap_or("");
                    if text.is_empty() { continue; }
                    let key = (index, thinking);
                    let old = self.previous.get(&key).map(String::as_str).unwrap_or("");
                    if !text.starts_with(old) { return Err("Non-append LS timeline update".into()); }
                    let added = text[old.len()..].to_owned();
                    self.previous.insert(key, text.to_owned());
                    self.delta(index, thinking, &added, &mut events)?;
                }
                continue;
            }
            let mut display = ls_agent_core::presentation::tool_presentation(step);
            display["status"] = step["status"].clone();
            display["error"] = step.get("errorMessage").filter(|v| v.is_string())
                .or_else(|| step.pointer("/error/userErrorMessage"))
                .or_else(|| step.pointer("/error/message")).cloned().unwrap_or(Value::Null);
            if display["name"].as_str().unwrap_or("").is_empty() {
                display["name"] = json!(step["type"].as_str().unwrap_or("tool").trim_start_matches("CORTEX_STEP_TYPE_"));
            }
            if display["control"] == true {
                // Reuse the reference plan/focus projection, not raw tool args.
                let mut progress = ls_agent_core::presentation::TaskProgress::default();
                progress.begin_turn(floor);
                progress.observe(&BTreeMap::from([(index, step.clone())]));
                display["progress"] = progress.snapshot();
            }
            if self.tools.get(&index) == Some(&display) { continue; }
            let previous = self.tools.insert(index, display.clone());
            let text = tool_progress(index, &display, previous.as_ref());
            self.delta(index, false, &text, &mut events)?;
        }
        Ok(events)
    }

    /// Matches the text-only history projection on inbound assistant messages;
    /// thinking is never replayed as a user instruction or answer text.
    pub fn history_text(&self) -> String {
        self.content.iter().filter_map(|b| b["text"].as_str()).collect()
    }
}

fn bounded(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let mut result: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() { result.push_str("…（已截断）"); }
    result
}

fn tool_progress(index: u64, display: &Value, previous: Option<&Value>) -> String {
    let raw_status = display["status"].as_str().unwrap_or("UNKNOWN");
    let error = display["error"].as_str().unwrap_or("");
    let failed = !error.is_empty() || display["exitCode"].as_i64().is_some_and(|c| c != 0);
    let status = if failed { "失败" } else { match raw_status.trim_start_matches("CORTEX_STEP_STATUS_") {
        "DONE" => "完成", "RUNNING" => "执行中", "PENDING" => "等待执行",
        "WAITING" | "WAITING_FOR_USER" => "等待确认", "ERROR" | "FAILED" => "失败",
        "CANCELED" | "CANCELLED" => "已取消", other => other,
    }};
    let name = bounded(display["name"].as_str().unwrap_or("tool"), 120);
    let mut text = format!("\n\n[LS 工具 #{index} · {name} · {status}]\n");
    for (field, label) in [("title", "说明"), ("target", "目标"), ("command", "命令")] {
        let value = display[field].as_str().unwrap_or("");
        if !value.is_empty() && previous.is_none_or(|p| p[field] != display[field]) {
            text.push_str(&format!("{label}：{}\n", bounded(value, 2000)));
        }
    }
    let output = display["output"].as_str().unwrap_or("");
    let old = previous.and_then(|p| p["output"].as_str()).unwrap_or("");
    let output = output.strip_prefix(old).unwrap_or(output);
    if !output.is_empty() {
        text.push_str("输出：\n");
        for line in bounded(output, 6000).lines() { text.push_str(&format!("    {line}\n")); }
    }
    if let Some(code) = display["exitCode"].as_i64() { text.push_str(&format!("退出码：{code}\n")); }
    if !error.is_empty() { text.push_str(&format!("错误：{}\n", bounded(error, 2000))); }
    if let Some(progress) = display.get("progress") {
        if let Some(focus) = progress["focus"].as_str().filter(|s| !s.is_empty()) { text.push_str(&format!("当前任务：{}\n", bounded(focus, 2000))); }
        for row in progress["plan"].as_array().into_iter().flatten() {
            text.push_str(&format!("- [{}] {}\n", row["status"].as_str().unwrap_or("unknown"), row["title"].as_str().unwrap_or("")));
        }
    }
    text
}

#[cfg(test)]
pub fn append_texts(
    previous: &mut BTreeMap<u64, String>,
    updates: Vec<(u64, String)>,
) -> Result<Vec<(u64, String, bool)>, String> {
    let mut deltas = vec![];
    for (index, text) in updates {
        let old = previous.get(&index).map(String::as_str).unwrap_or("");
        if !text.starts_with(old) {
            return Err("Non-append LS text update".into());
        }
        if text.len() > old.len() {
            deltas.push((index, text[old.len()..].to_owned(), old.is_empty()));
        }
        previous.insert(index, text);
        if previous.values().map(String::len).sum::<usize>() > 8 * 1024 * 1024 {
            return Err("LS response exceeded the 8 MiB text limit".into());
        }
    }
    Ok(deltas)
}

fn integer(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

/// ModelUsage outputTokens already includes thinkingOutputTokens; never add it twice.
/// inputTokens excludes cacheReadTokens in the observed LS 2.35.1 metadata.
pub fn native_usage(trajectory: &Value, floor: u64) -> Option<Value> {
    let steps = trajectory.pointer("/trajectory/steps")?.as_array()?;
    let mut seen = HashSet::new();
    let (mut input, mut output, mut cache) = (0u64, 0u64, 0u64);
    let mut count = 0;
    for step in steps.iter().skip(usize::try_from(floor).ok()?) {
        if step["type"] != "CORTEX_STEP_TYPE_PLANNER_RESPONSE" {
            continue;
        }
        let usage = step.pointer("/metadata/modelUsage")?;
        let id = usage.get("messageId").and_then(Value::as_str)?;
        if !seen.insert(id) {
            continue;
        }
        input = input.checked_add(integer(&usage["inputTokens"])?)?;
        output = output.checked_add(integer(&usage["outputTokens"])?)?;
        cache = cache.checked_add(usage.get("cacheReadTokens").and_then(integer).unwrap_or(0))?;
        count += 1;
    }
    (count > 0).then(
        || json!({"input_tokens":input,"output_tokens":output,"cache_read_input_tokens":cache}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timeline_streams_thinking_tools_and_answer_in_order_without_tool_callbacks() {
        let mut t = Timeline::default();
        let mut steps = BTreeMap::from([
            (0, json!({"userInput":{"text":"never echo user"}})),
            (1, json!({"plannerResponse":{"thinking":"先检查文件"}})),
        ]);
        let mut events = t.observe(&steps, 1).unwrap();
        assert_eq!(events[0]["content_block"]["type"], "thinking");
        assert_eq!(events[1]["delta"]["thinking"], "先检查文件");
        assert!(t.observe(&steps, 1).unwrap().is_empty());
        steps.insert(1, json!({"plannerResponse":{"thinking":"先检查文件，再执行"}}));
        events.extend(t.observe(&steps, 1).unwrap());
        steps.insert(2, json!({"type":"CORTEX_STEP_TYPE_RUN_COMMAND","status":"CORTEX_STEP_STATUS_RUNNING","runCommand":{"commandLine":"echo hello","cwd":"C:/work"}}));
        events.extend(t.observe(&steps, 1).unwrap());
        steps.insert(2, json!({"type":"CORTEX_STEP_TYPE_RUN_COMMAND","status":"CORTEX_STEP_STATUS_DONE","runCommand":{"commandLine":"echo hello","cwd":"C:/work","exitCode":0,"combinedOutput":{"full":"hello"}}}));
        events.extend(t.observe(&steps, 1).unwrap());
        steps.insert(3, json!({"plannerResponse":{"thinking":"已核对","response":"最终回答"}}));
        events.extend(t.observe(&steps, 1).unwrap());
        events.extend(t.close());
        let mut active = None;
        let mut count = 0;
        for e in &events {
            let index = e["index"].as_u64().unwrap();
            match e["type"].as_str().unwrap() {
                "content_block_start" => { assert!(active.is_none()); assert_eq!(index, count); count += 1; active = Some(index); }
                "content_block_delta" => assert_eq!(active, Some(index)),
                "content_block_stop" => { assert_eq!(active.take(), Some(index)); }
                _ => panic!("unexpected event"),
            }
        }
        assert!(active.is_none());
        let wire = serde_json::to_string(&events).unwrap();
        assert!(!wire.contains("tool_use") && !wire.contains("never echo user"));
        let history = t.history_text();
        assert!(history.contains("执行中") && history.contains("完成") && history.contains("hello") && history.ends_with("最终回答"));
        assert!(!history.contains("先检查") && !history.contains("已核对"));
        assert!(t.observe(&steps, 1).unwrap().is_empty());
    }

    #[test]
    fn timeline_keeps_errors_and_late_updates_honest() {
        let mut t = Timeline::default();
        let mut steps = BTreeMap::from([(1, json!({"plannerResponse":{"thinking":"a","response":"b"}}))]);
        t.observe(&steps, 1).unwrap();
        steps.insert(1, json!({"plannerResponse":{"thinking":"ac","response":"bd"}}));
        let events = t.observe(&steps, 1).unwrap();
        assert_eq!(events[0]["type"], "content_block_stop");
        assert_eq!(events[1]["index"], 2);
        steps.insert(2, json!({"type":"CORTEX_STEP_TYPE_RUN_COMMAND","status":"CORTEX_STEP_STATUS_DONE","runCommand":{"exitCode":5}}));
        t.observe(&steps, 1).unwrap();
        assert!(t.history_text().contains("失败") && t.history_text().contains("退出码：5"));
        steps.insert(1, json!({"plannerResponse":{"thinking":"rewritten"}}));
        assert!(t.observe(&steps, 1).is_err());
    }
    #[test]
    fn snapshot_replay_is_not_duplicate_text_and_rewrites_are_errors() {
        let mut previous = BTreeMap::new();
        assert_eq!(
            append_texts(&mut previous, vec![(3, "你好".into())]).unwrap(),
            vec![(3, "你好".into(), true)]
        );
        assert!(append_texts(&mut previous, vec![(3, "你好".into())])
            .unwrap()
            .is_empty());
        assert_eq!(
            append_texts(&mut previous, vec![(3, "你好世界".into())]).unwrap(),
            vec![(3, "世界".into(), false)]
        );
        assert!(append_texts(&mut previous, vec![(3, "改写".into())]).is_err());
    }
    #[test]
    fn native_usage_deduplicates_generations_and_preserves_unknown() {
        let step = json!({"type":"CORTEX_STEP_TYPE_PLANNER_RESPONSE","metadata":{"modelUsage":{"messageId":"one","inputTokens":"5205","outputTokens":"620","thinkingOutputTokens":"500","responseOutputTokens":"120","cacheReadTokens":"8192"}}});
        let full = json!({"trajectory":{"steps":[step.clone(),step]}});
        assert_eq!(
            native_usage(&full, 0),
            Some(json!({"input_tokens":5205,"output_tokens":620,"cache_read_input_tokens":8192}))
        );
        assert_eq!(
            native_usage(
                &json!({"trajectory":{"steps":[{"type":"CORTEX_STEP_TYPE_PLANNER_RESPONSE"}]}}),
                0
            ),
            None
        );
    }
}
