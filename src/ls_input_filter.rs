//! Claude Code HTTP boundary. Never concatenate client scaffolding with input.
//! Structural rules are backed by captured 2.1.251 requests and the webview
//! composer. Literal user text is never searched-and-replaced.
use serde_json::{json, Value};

const CONTEXT: &str =
    "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n";
const CONTEXT_END: &str = "      IMPORTANT: this context may or may not be relevant to your tasks. You should not respond to this context unless it is highly relevant to your task.\n</system-reminder>";
const SYNTHETIC: &str = "[MESSAGE FROM NON-USER SOURCE - NOT USER INPUT]\n";

pub struct Filtered {
    pub messages: Vec<Value>,
    pub workspace: Option<String>,
}

pub fn generated_context(text: &str) -> bool {
    text.starts_with(CONTEXT) && text.trim_end().ends_with(CONTEXT_END)
}

fn environment(text: &str) -> Option<String> {
    // Do not take paths from ordinary user text or arbitrary prose mentioning cwd.
    let (_, rest) =
        text.split_once("# Environment\nYou have been invoked in the following environment:")?;
    let line = rest
        .lines()
        .find(|l| l.starts_with(" - Primary working directory: "))?;
    Some(
        line.strip_prefix(" - Primary working directory: ")?
            .to_owned(),
    )
}

fn set_workspace(target: &mut Option<String>, path: Option<String>) -> Result<(), String> {
    if let Some(path) = path {
        if path.is_empty() {
            return Err("Empty Claude workspace".into());
        }
        if target.as_ref().is_some_and(|old| old != &path) {
            return Err("Conflicting Claude workspace contexts".into());
        }
        *target = Some(path);
    }
    Ok(())
}

fn text_blocks(content: &Value) -> Vec<&str> {
    match content {
        Value::String(s) => vec![s],
        Value::Array(a) => a
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect(),
        _ => vec![],
    }
}

/// `originals` is an optional snapshot from the locally configured Claude
/// transcript, never supplied by the HTTP caller. It disambiguates synthetic
/// events from user text which happens to contain the same marker.
pub fn filter(request: &Value, originals: Option<&[Value]>) -> Result<Filtered, String> {
    if request.pointer("/metadata/ls_input").is_some() {
        return Err(
            "LS accepts ordinary Claude messages; the legacy ls_input bypass is removed".into(),
        );
    }
    let native_file_bytes = text_blocks(&request["system"]).iter().any(|s| {
        s.starts_with("x-anthropic-billing-header: cc_version=2.1.251.")
            && s.contains("cc_entrypoint=claude-vscode;")
    });
    let source = request["messages"]
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or("messages must not be empty")?;
    let mut result = Filtered {
        messages: vec![],
        workspace: None,
    };
    for text in text_blocks(&request["system"]) {
        set_workspace(&mut result.workspace, environment(text))?;
    }
    let mut first_user = true;
    let mut user_index = 0;
    for message in source {
        let role = message["role"].as_str().ok_or("Missing role")?;
        if matches!(role, "system" | "developer") {
            continue;
        }
        if role == "assistant" {
            result.messages.push(message.clone());
            continue;
        }
        if role != "user" {
            return Err("Unsupported client message role".into());
        }
        let mut blocks = match &message["content"] {
            Value::String(s) => vec![json!({"type":"text","text":s})],
            Value::Array(a) => a.clone(),
            _ => return Err("Invalid user content".into()),
        };
        // Exactly one generated prefix on the first user message. Preserve a
        // second, identical block when the user pasted the reminder literally.
        if first_user
            && blocks.len() == 1
            && blocks[0]["text"].as_str().is_some_and(generated_context)
        {
            return Err("Client context without original input was not sent to LS".into());
        }
        if first_user && blocks.len() > 1 && blocks[0]["type"] == "text" {
            if let Some(text) = blocks[0]["text"].as_str().filter(|s| generated_context(s)) {
                set_workspace(&mut result.workspace, environment(text))?;
                blocks.remove(0);
            }
        }
        first_user = false;
        let original = originals.and_then(|rows| rows.get(user_index));
        user_index += 1;
        if let Some(row) = original {
            if row.get("isMeta").and_then(Value::as_bool) == Some(true)
                || row.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
            {
                // Do not keep an answer to an automatic task as user history.
                return Err(
                    "Claude automatic/compacted turn is not original conversation input".into(),
                );
            }
            let raw = match &row["message"]["content"] {
                Value::String(s) => vec![json!({"type":"text","text":s})],
                Value::Array(a) => a.clone(),
                _ => return Err("Invalid original transcript content".into()),
            };
            fn without_cache(mut blocks: Vec<Value>) -> Vec<Value> {
                for block in &mut blocks {
                    if let Some(o) = block.as_object_mut() {
                        o.remove("cache_control");
                    }
                }
                blocks
            }
            if without_cache(raw) != without_cache(blocks.clone()) {
                return Err(
                    "Client content differs from the recorded original input; no task was sent"
                        .into(),
                );
            }
        }
        let has_synthetic = blocks
            .iter()
            .any(|b| b["text"].as_str().is_some_and(|s| s.starts_with(SYNTHETIC)));
        if has_synthetic {
            let literal = original.is_some_and(|row| {
                let c = &row["message"]["content"];
                let raw = if c.is_string() {
                    vec![json!({"type":"text","text":c})]
                } else {
                    c.as_array().cloned().unwrap_or_default()
                };
                raw == blocks
            });
            if !literal {
                return Err("Synthetic client message needs original-source verification; it was not sent to LS".into());
            }
        }
        // The installed webview puts context before the final composer text.
        // Classify only complete standalone prefix blocks; never modify the
        // user's final text or the contents of an image/document attachment.
        let last_text = blocks.iter().rposition(|b| b["type"] == "text");
        let mut clean = vec![];
        for (index, mut block) in blocks.into_iter().enumerate() {
            // The verified VS Code 2.1.251 composer uses atob(FileReader data)
            // for text attachments. Restore UTF-8 bytes once, only for that
            // producer's document blocks; never reinterpret user prompt text.
            if native_file_bytes && block["type"] == "document" && block["source"]["type"] == "text"
            {
                if let Some(data) = block["source"]["data"].as_str() {
                    if data.chars().all(|c| (c as u32) <= 255) {
                        let bytes: Vec<u8> = data.chars().map(|c| c as u8).collect();
                        let decoded = String::from_utf8(bytes).map_err(|_| "Native text attachment is not UTF-8; no guessed legacy-encoding conversion was performed")?;
                        block["source"]["data"] = Value::String(decoded);
                    }
                }
            }
            if Some(index) != last_text && block["type"] == "text" {
                let text = block["text"].as_str().ok_or("Invalid text block")?;
                if text.starts_with("<browser_instruction>")
                    && text.ends_with("</browser_instruction>")
                {
                    continue;
                }
                if let Some(value) = ide_context(text)? {
                    clean.push(value);
                    continue;
                }
                // Never forward unknown injected reminder variants on trust.
                // These are not normal composer blocks; require a captured
                // source rule instead of guessing at how much text to cut.
                if text.starts_with("<system-reminder>") {
                    return Err("Unrecognized client reminder block; filtering rule required before sending".into());
                }
            }
            clean.push(block);
        }
        if clean.is_empty() {
            return Err("No original user input remains".into());
        }
        result.messages.push(json!({"role":"user","content":clean}));
    }
    if result.messages.last().is_none_or(|m| m["role"] != "user") {
        return Err("A final user message is required".into());
    }
    Ok(result)
}

fn ide_context(text: &str) -> Result<Option<Value>, String> {
    const TAIL: &str = "\n\nThis may or may not be related to the current task.</ide_selection>";
    if let Some(selection) = text.strip_prefix("<ide_selection>The user selected the lines ") {
        let (header, code) = selection
            .split_once(":\n")
            .ok_or("Malformed IDE selection")?;
        let code = code
            .strip_suffix(TAIL)
            .ok_or("Malformed IDE selection trailer")?;
        let (lines, path) = header
            .split_once(" from ")
            .ok_or("Malformed IDE selection source")?;
        return Ok(Some(
            json!({"type":"document","title":format!("{path} (lines {lines})"),"source":{"type":"text","media_type":"text/plain","data":code}}),
        ));
    }
    if let Some(path) = text.strip_prefix("<ide_opened_file>The user opened the file ") {
        let path = path.strip_suffix(" in the IDE. This may or may not be related to the current task.</ide_opened_file>").ok_or("Malformed IDE opened-file context")?;
        return Ok(Some(json!({"type":"text","text":format!("File: {path}")})));
    }
    Ok(None)
}
