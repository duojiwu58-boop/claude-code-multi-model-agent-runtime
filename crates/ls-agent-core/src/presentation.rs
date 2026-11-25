//! Allowlisted display projection. Never changes execution or approval semantics.
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn text(v: &Value) -> String {
    v.as_str().unwrap_or("").chars().take(16_000).collect()
}

fn tool_name(step: &Value) -> &str {
    step.pointer("/metadata/toolCall/name").and_then(Value::as_str)
        .or_else(|| step.pointer("/generic/toolName").and_then(Value::as_str)).unwrap_or("")
}

fn render(step: &Value) -> &Value { &step["generic"]["result"]["stepRenderInfo"] }

fn successful(step: &Value) -> bool {
    !step.get("error").is_some_and(|v| !v.is_null())
        && step["errorMessage"].as_str().unwrap_or("").is_empty()
        && matches!(step["status"].as_str(), Some("CORTEX_STEP_STATUS_DONE" | "CORTEX_STEP_STATUS_UNSPECIFIED") | None)
}

fn plan(step: &Value) -> Option<Vec<Value>> {
    if tool_name(step) != "update_steps" || !successful(step) { return None; }
    let markdown = render(step)["markdown"].as_str().unwrap_or("");
    let rows: Vec<Value> = markdown.lines().filter_map(|line| {
        let line = line.trim().strip_prefix("- ").or_else(|| line.trim().strip_prefix("* "))?;
        let bytes = line.as_bytes();
        if bytes.len() < 4 || bytes[0] != b'[' || bytes[2] != b']' || bytes[3] != b' ' { return None; }
        let status = match bytes[1] { b' ' => "pending", b'>' => "in_progress", b'x' | b'X' => "done", b'-' => "skipped", b'!' => "failed", _ => "unknown" };
        Some(json!({"title":line[4..].chars().take(2000).collect::<String>(),"status":status}))
    }).take(100).collect();
    if !rows.is_empty() { return Some(rows); }
    let arguments = step.pointer("/metadata/toolCall/argumentsJson").and_then(Value::as_str)
        .filter(|s| s.len() <= 100_000).and_then(|s| serde_json::from_str::<Value>(s).ok());
    let args = step["generic"].get("args").or(arguments.as_ref())?;
    let rows = args["steps"].as_array()?;
    Some(rows.iter().take(100).filter_map(|r| {
        let title = r["title"].as_str()?;
        let status = match r["status"].as_str().unwrap_or("") {
            s @ ("pending" | "in_progress" | "done" | "skipped" | "failed") => s, _ => "unknown"
        };
        Some(json!({"title":title.chars().take(2000).collect::<String>(),"status":status}))
    }).collect())
}

#[derive(Debug, Clone, Default)]
pub struct TaskProgress {
    floor: u64,
    plan_index: Option<u64>,
    focus_index: Option<u64>,
    plan: Vec<Value>,
    focus: String,
}

impl TaskProgress {
    pub fn begin_turn(&mut self, floor: u64) { *self = Self { floor, ..Self::default() }; }

    pub fn observe(&mut self, steps: &BTreeMap<u64, Value>) {
        if let Some((&index, _)) = steps.iter().rev().find(|(_, s)| s.get("userInput").is_some())
            && index > self.floor { self.begin_turn(index); }
        for (&index, step) in steps.range(self.floor..) {
            match tool_name(step) {
                "update_steps" if self.plan_index.is_none_or(|old| index >= old) => {
                    if let Some(rows) = plan(step) { self.plan = rows; self.plan_index = Some(index); }
                    else if self.plan_index == Some(index) { self.plan.clear(); }
                }
                "update_focus" if self.focus_index.is_none_or(|old| index >= old) => {
                    if successful(step) {
                        self.focus = text(&render(step)["title"]);
                        if self.focus.is_empty() { self.focus = text(&step["generic"]["args"]["focus"]); }
                        self.focus_index = Some(index);
                    } else if self.focus_index == Some(index) { self.focus.clear(); }
                }
                _ => {}
            }
        }
    }

    pub fn snapshot(&self) -> Value {
        json!({"version":1,"turnStartIndex":self.floor,"planStepIndex":self.plan_index,
            "focusStepIndex":self.focus_index,"plan":self.plan,"focus":self.focus})
    }
}

/// Only text and typed values are projected. No arbitrary HTML or executable links.
pub fn tool_presentation(step: &Value) -> Value {
    let name = tool_name(step);
    let info = render(step);
    let nested = &step["generic"]["result"]["payload"];
    let source = if nested.is_object() { nested } else { step };
    let args = &step["generic"]["args"];
    let blocks = info["contentBlocks"].as_array();
    let terminal = blocks.and_then(|b| b.iter().find_map(|x| x.get("terminal"))).unwrap_or(&Value::Null);
    let file = info["titleParts"].as_array().and_then(|b| b.iter().find_map(|x| x.get("file"))).unwrap_or(&Value::Null);
    let first = |values: &[&Value]| values.iter().map(|v| text(v)).find(|s| !s.is_empty()).unwrap_or_default();
    let command = first(&[&source["runCommand"]["commandLine"], &terminal["command"], &args["CommandLine"], &args["commandLine"], &args["command"]]);
    let target = first(&[&source["runCommand"]["cwd"], &terminal["cwd"], &source["viewFile"]["absolutePathUri"], &file["uri"], &args["AbsolutePath"], &args["TargetFile"], &args["cwd"]]);
    let output = first(&[&terminal["output"], &source["runCommand"]["combinedOutput"]["full"]]);
    let exit_code = source["runCommand"]["exitCode"].as_i64().or_else(|| terminal["exitCode"].as_i64());
    let control = match name {
        "update_steps" => plan(step).is_some(),
        "update_focus" => successful(step) && (!text(&info["title"]).is_empty() || !text(&args["focus"]).is_empty()),
        _ => false,
    };
    let title = first(&[&info["title"]]);
    let parts = info["titleParts"].as_array().map(|p| p.iter().filter_map(|x| x["text"].as_str()).collect::<Vec<_>>().join(" ")).unwrap_or_default();
    json!({"version":1,"name":name,"title":if title.is_empty(){parts}else{title},
        "command":command,"target":target,"output":output,"exitCode":exit_code,
        "startLine":file["startLine"].as_u64(),"endLine":file["endLine"].as_u64(),
        "control":control && step.get("requestedInteraction").is_none(),
        "kind":source["type"].as_str().unwrap_or("")})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn captured_official_plans_replay_after_args_disappear_and_reset() {
        let evidence: Value = serde_json::from_str(include_str!("../tests/fixtures/presentation.json")).unwrap();
        let mut store = crate::state_store::AgentStateStore::default();
        let mut records: Vec<_> = evidence["sessions"][0]["plans"].as_object().unwrap().iter().collect();
        records.sort_by_key(|(k,_)| k.parse::<u64>().unwrap());
        for (index, row) in records {
            let index = index.parse::<u64>().unwrap();
            let step = json!({"type":"CORTEX_STEP_TYPE_GENERIC","status":row["finalStatus"],
                "metadata":{"toolCall":{"name":"update_steps"}},"generic":{"result":{"stepRenderInfo":row["render"]}}});
            store.apply_update(0,&json!({"trajectoryId":"a","mainTrajectoryUpdate":{"stepsUpdate":{
                "totalLength":index+1,"indices":[index],"steps":[step],"pageBounds":{"startIndex":index}}}})).unwrap();
            assert_eq!(store.task_progress.snapshot()["plan"],row["args"]["steps"]);
        }
        let before=store.task_progress.snapshot();
        assert!(store.apply_update(0,&json!({"mainTrajectoryUpdate":{"stepsUpdate":{"totalLength":1,"indices":[2],"steps":[{}]}}})).is_err());
        assert_eq!(store.task_progress.snapshot(),before);
        store.reset_for_subscription(1,"new".into());
        assert_eq!(store.task_progress.snapshot()["plan"],json!([]));
        assert!(!store.apply_update(0,&json!({"fullyIdle":true})).unwrap());
    }
    fn control(name: &str, title: &str, markdown: &str) -> Value {
        json!({"status":"CORTEX_STEP_STATUS_DONE","metadata":{"toolCall":{"name":name}},
            "generic":{"result":{"stepRenderInfo":{"title":title,"markdown":markdown,"hidden":true}}}})
    }
    #[test]
    fn final_render_survives_window_but_not_new_turn_or_failed_replacement() {
        let mut p = TaskProgress::default();
        p.observe(&BTreeMap::from([(11,control("update_steps","","- [x] inspect\n- [>] audit"))]));
        assert_eq!(p.snapshot()["plan"][1]["status"],"in_progress");
        p.observe(&BTreeMap::from([(80,control("update_focus","testing",""))]));
        assert_eq!(p.snapshot()["plan"].as_array().unwrap().len(),2);
        p.observe(&BTreeMap::from([(2,control("update_focus","old page",""))]));
        assert_eq!(p.snapshot()["focus"],"testing");
        let mut failed = control("update_steps","", "- [x] fake"); failed["status"]=json!("CORTEX_STEP_STATUS_ERROR");
        p.observe(&BTreeMap::from([(11,failed)])); assert_eq!(p.snapshot()["plan"],json!([]));
        p.observe(&BTreeMap::from([(90,json!({"userInput":{}}))]));
        assert_eq!(p.snapshot()["focus"],"");
    }
    #[test]
    fn nested_command_and_unknown_hidden_step_stay_visible() {
        let s=json!({"type":"CORTEX_STEP_TYPE_GENERIC","metadata":{"toolCall":{"name":"run_command"}},"generic":{"result":{"payload":{"runCommand":{"commandLine":"cargo test","exitCode":1}},"stepRenderInfo":{"hidden":true,"contentBlocks":[{"terminal":{"output":"34 passed, 7 failed"}}]}}}});
        let p=tool_presentation(&s);assert_eq!(p["exitCode"],1);assert_eq!(p["command"],"cargo test");assert_eq!(p["control"],false);
        assert_eq!(p["output"],"34 passed, 7 failed");
    }
}
