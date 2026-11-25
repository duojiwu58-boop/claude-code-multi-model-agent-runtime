fn codex_effective_tools(request: &Value) -> Result<Vec<Value>, String> {
    let mut definitions = IndexMap::<String, Value>::new();
    let mut enabled = HashSet::new();
    if let Some(tools) = request.get("tools") {
        for tool in tools.as_array().ok_or("Codex tools must be an array")? {
            let name = tool["name"].as_str().ok_or("Codex tool has no name")?;
            if definitions.insert(name.to_string(), tool.clone()).is_some() {
                return Err(format!("Duplicate Codex tool '{name}'"));
            }
            if tool["defer_loading"] != true {
                enabled.insert(name.to_string());
            }
        }
    }
    if let Some(messages) = request["messages"].as_array() {
        for message in messages {
            if message["role"] != "system" {
                continue;
            }
            for block in message["content"].as_array().into_iter().flatten() {
                let kind = block["type"].as_str().unwrap_or("");
                if !matches!(kind, "tool_addition" | "tool_removal") {
                    continue;
                }
                if message["clear_at"] == "next_user_message" {
                    return Err("Turn-scoped system messages cannot change tools".to_string());
                }
                let reference = &block["tool"];
                let name = match reference["type"].as_str() {
                    Some("tool_reference") => reference["name"].as_str().ok_or("Tool reference has no name")?.to_string(),
                    Some("tool_definition") if kind == "tool_addition" => {
                        let definition = &reference["definition"];
                        let name = definition["name"].as_str().ok_or("Inline tool definition has no name")?.to_string();
                        definitions.insert(name.clone(), definition.clone());
                        name
                    }
                    _ => return Err("Codex supports client tool references and inline definitions, not server MCP toolsets".to_string()),
                };
                if !definitions.contains_key(&name) {
                    return Err(format!("Unresolved Codex tool reference '{name}'"));
                }
                if kind == "tool_addition" {
                    enabled.insert(name);
                } else {
                    enabled.remove(&name);
                }
            }
        }
    }
    definitions.into_iter().filter(|(name, _)| enabled.contains(name)).map(|(_, tool)| {
        if let Some(native) = anthropic_client_tool_as_function(&tool)? {
            Ok(json!({"name":native["name"],"description":native["description"],"input_schema":native["parameters"]}))
        } else { Ok(tool) }
    }).collect()
}

fn codex_request_diagnostics(request: &Value) -> Vec<String> {
    let mut warnings = Vec::new();
    if request
        .pointer("/bridge_controls/max_output_chars")
        .is_some()
        || request["stop_sequences"]
            .as_array()
            .is_some_and(|s| !s.is_empty())
    {
        warnings.push("Local text termination interrupts the native turn; final usage may be unavailable (bridge_usage_complete=false)".into());
    }
    if request["messages"]
        .as_array()
        .is_some_and(|messages| messages.iter().any(|m| m["role"] == "system"))
    {
        warnings.push("Conversation system instructions are mapped to GPT developer messages, preserving instruction content and order".into());
    }
    for key in ["max_tokens", "temperature", "top_p", "top_k"] {
        if request.get(key).is_some_and(|value| !value.is_null()) {
            warnings.push(format!(
                "Codex App Server manages {key}; the Anthropic value is not enforced"
            ));
        }
    }
    if request
        .pointer("/bridge_controls/max_output_chars")
        .is_some()
    {
        warnings.push("bridge_controls.max_output_chars limits visible text locally, not model tokens or reasoning cost; length termination uses max_tokens and discards native continuation".into());
    }
    if request.pointer("/thinking/budget_tokens").is_some() {
        warnings.push(
            "Codex uses reasoning effort; Anthropic thinking.budget_tokens has no exact equivalent"
                .into(),
        );
    }
    if request
        .get("context_management")
        .is_some_and(|value| !value.is_null())
    {
        warnings.push("Context keep-all is accepted; native reasoning survives exact tool continuations, but is unavailable after visible-history replay".into());
    }
    if request.get("cache_control").is_some() || request.to_string().contains("\"cache_control\"") {
        warnings.push(
            "Anthropic cache breakpoints/TTL are not forwarded; Codex controls prompt caching"
                .into(),
        );
    }
    if request["tools"]
        .as_array()
        .is_some_and(|tools| tools.iter().any(|tool| tool["strict"] == true))
    {
        warnings.push("Strict tool JSON Schema is validated locally before calls are delivered; native constrained decoding is not exposed".into());
    }
    warnings
}

fn validate_codex_request_controls(request: &Value) -> Result<(), String> {
    if request
        .pointer("/thinking/summary")
        .is_some_and(|v| !matches!(v.as_str(), Some("auto" | "concise" | "detailed")))
    {
        return Err("Unsupported reasoning summary style".into());
    }
    if let Some(controls) = request.get("bridge_controls") {
        let controls = controls
            .as_object()
            .ok_or("bridge_controls must be an object")?;
        if controls
            .keys()
            .any(|k| !matches!(k.as_str(), "mode" | "max_output_chars"))
        {
            return Err("Unknown bridge_controls field".into());
        }
        if controls
            .get("mode")
            .is_some_and(|v| !matches!(v.as_str(), Some("compatible" | "strict")))
        {
            return Err("bridge_controls.mode must be compatible or strict".into());
        }
        if controls.get("max_output_chars").is_some_and(|v| {
            v.as_u64()
                .is_none_or(|n| n == 0 || n > MAX_UPSTREAM_RESPONSE_BYTES as u64)
        }) {
            return Err("bridge_controls.max_output_chars must be a positive integer within the response limit".into());
        }
        if controls.get("mode").and_then(Value::as_str) == Some("strict") {
            for key in [
                "max_tokens",
                "temperature",
                "top_p",
                "top_k",
                "cache_control",
            ] {
                if request.get(key).is_some_and(|v| !v.is_null()) {
                    return Err(format!("Codex App Server cannot enforce '{key}'; use compatible mode with explicit warnings, or a native API provider that supports this control"));
                }
            }
            if request.pointer("/thinking/budget_tokens").is_some()
                || request.to_string().contains("\"cache_control\"")
            {
                return Err(
                    "Codex strict mode cannot enforce thinking budgets or cache breakpoints".into(),
                );
            }
        }
    }
    if request.get("service_tier").is_some_and(|v| {
        !v.is_null()
            && !matches!(
                v.as_str(),
                Some("auto" | "default" | "priority" | "fast" | "flex" | "ultrafast")
            )
    }) {
        return Err("Invalid Codex service_tier".into());
    }
    if let Some(system) = request.get("system").filter(|value| !value.is_null()) {
        let valid = system.is_string()
            || system.as_array().is_some_and(|blocks| {
                blocks
                    .iter()
                    .all(|block| block["type"] == "text" && block["text"].is_string())
            });
        if !valid {
            return Err("Codex system instructions must be text or text blocks".into());
        }
    }
    if let Some(stops) = request.get("stop_sequences").filter(|v| !v.is_null()) {
        let stops = stops.as_array().ok_or("stop_sequences must be an array")?;
        if stops.len() > 16
            || stops
                .iter()
                .any(|v| v.as_str().is_none_or(|s| s.is_empty() || s.len() > 1024))
        {
            return Err(
                "stop_sequences requires at most 16 nonempty strings of at most 1024 bytes".into(),
            );
        }
        if !stops.is_empty() && responses_output_format(request)?.is_some() {
            return Err("stop_sequences cannot be combined with structured output".into());
        }
    }
    if request
        .pointer("/bridge_controls/max_output_chars")
        .is_some()
        && responses_output_format(request)?.is_some()
    {
        return Err("Local character limits cannot be combined with structured output".into());
    }
    if let Some(context) = request
        .get("context_management")
        .filter(|value| !value.is_null())
    {
        let object = context
            .as_object()
            .ok_or("Codex context_management must be an object")?;
        if object.keys().any(|key| key != "edits") {
            return Err("Unsupported Codex context_management field".into());
        }
        if let Some(edits) = object.get("edits") {
            for edit in edits
                .as_array()
                .ok_or("Codex context_management.edits must be an array")?
            {
                // Anthropic's documented keep-all strategy requests no deletion.
                // Native reasoning remains owned by Codex; active deletion policies
                // cannot be implemented by altering an opaque native turn.
                if edit["type"] != "clear_thinking_20251015"
                    || edit["keep"] != "all"
                    || edit.as_object().is_none_or(|object| {
                        object
                            .keys()
                            .any(|key| !matches!(key.as_str(), "type" | "keep"))
                    })
                {
                    return Err(format!("Codex cannot apply context edit '{}'; only clear_thinking_20251015 with keep=all is supported", edit["type"].as_str().unwrap_or("unknown")));
                }
            }
        }
    }
    for key in ["mcp_servers", "container"] {
        if request.get(key).is_some_and(|value| !value.is_null()) {
            return Err(format!("Codex cannot preserve Anthropic '{key}' semantics; use Claude Code client tools and context management"));
        }
    }
    Ok(())
}

fn codex_capability_report() -> Value {
    json!({"transport":"codex-app-server","text":true,"images":true,"client_tools":true,
        "http_apis":["messages","responses","chat_completions","response_resources","conversations","files","stored_chat_completions"],
        "tool_images":"native_thread_supplemental_media","text_documents":true,"binary_documents":"PDF via optional Poppler renderer; images native","audio":false,"hosted_tools":false,
        "files":{"capacity":64,"max_file_bytes":8388608,"pdf_max_pages":16,"pdf_renderer":"pdftoppm on PATH or CLAUDE_BRIDGE_PDFTOPPM"},
        "background":{"active_limit":16,"retention_seconds":600,"cancel":true,"stream_replay":true,"restart":"unfinished_jobs_fail"},
        "chat_storage":{"capacity":128,"choices_limit":4,"multiple_choices":"independent sequential generations; tool-result branching unsupported"},
        "json_object":"locally_validated; SSE buffered","custom_tools":"free_text_only","strict_tools":"locally_validated_before_delivery",
        "exact_input_tokens":"requires_native_responses_api","exportable_compaction":"requires_native_responses_api",
        "response_storage":{"capacity":128,"conversation_capacity":64,"max_bytes":33554432,"default_store":true,"retention":"until_deleted","encryption":"filesystem_only"},
        "service_tier":"native","reasoning_effort":"native; explicit OpenAI request overrides profile","verbosity":"native_thread_config","json_schema":"native",
        "stop_sequences":"local_text_filter","max_output_chars":"local_text_filter",
        "max_tokens":"compatible_warning_or_strict_error","sampling":"compatible_warning_or_strict_error",
        "cache_control":"compatible_warning_or_strict_error","thinking_budget":"compatible_warning_or_strict_error",
        "strict_mode":{"field":"bridge_controls.mode","value":"strict"},
        "local_limits_do_not_bound_model_cost":true})
}
