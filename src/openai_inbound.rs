// OpenAI HTTP inputs -> the existing Codex model backend. Client tools still
// execute outside this service. This is independent of the external-provider
// adapters in openai_responses.rs and openai_chat_forward.rs.
fn api_error(status: StatusCode, message: impl AsRef<str>) -> Response {
    (status, Json(json!({"error":{"message":message.as_ref(),"type":if status.is_client_error(){"invalid_request_error"}else{"api_error"},"param":null,"code":status.as_u16().to_string()}}))).into_response()
}

fn api_string<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    v[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("'{key}' must be a nonempty string"))
}

fn api_content(content: &Value) -> Result<Value, String> {
    if content.is_string() {
        return Ok(content.clone());
    }
    let parts = content
        .as_array()
        .ok_or("content must be text or an array")?;
    let mut result = Vec::new();
    for p in parts {
        result.push(match p["type"].as_str() {
            Some("input_text" | "output_text" | "text") => json!({"type":"text","text":p["text"].as_str().ok_or("Text content requires text")?}),
            Some("input_image" | "image_url") => {
                let url = p["image_url"].as_str().or_else(|| p.pointer("/image_url/url").and_then(Value::as_str)).ok_or("Image requires image_url; upload file IDs are not supported by Codex")?;
                json!({"type":"image","source":{"type":"url","url":url}})
            }
            Some("input_file" | "file") => {
                let name = p["filename"].as_str().unwrap_or("document.txt");
                let data = p["file_data"].as_str().or_else(||p.pointer("/file/file_data").and_then(Value::as_str)).ok_or("Codex file inputs require inline text file_data; remote files and upload IDs require a native API provider")?;
                let text = if let Some((meta, encoded)) = data.split_once(",").filter(|(m,_)|m.starts_with("data:")) {
                    if !meta.starts_with("data:text/plain;") || !meta.ends_with(";base64") { return Err("Codex inline files currently support UTF-8 text/plain, not PDF or binary files".into()); }
                    String::from_utf8(BASE64_STANDARD.decode(encoded).map_err(|_|"Invalid base64 text file")?).map_err(|_|"Text file is not UTF-8")?
                } else { data.to_string() };
                json!({"type":"document","title":name,"source":{"type":"text","media_type":"text/plain","data":text}})
            }
            Some(other) => return Err(format!("Codex does not support OpenAI content type '{other}'")),
            None => return Err("Content part has no type".into()),
        });
    }
    Ok(json!(result))
}

fn api_input_items(input: &Value) -> Result<Vec<Value>, String> {
    if let Some(text) = input.as_str() {
        return Ok(vec![json!({"type":"message","role":"user","content":text})]);
    }
    let mut items = input
        .as_array()
        .ok_or("input must be a string or array")?
        .clone();
    for item in &mut items {
        if !item.is_object() {
            return Err("Every input item must be a JSON object".into());
        }
        if item.get("type").is_none() && item.get("role").is_some() {
            item["type"] = json!("message");
        }
    }
    Ok(items)
}

fn api_items_to_messages(items: &[Value]) -> Result<Vec<Value>, String> {
    let mut messages = Vec::new();
    for item in items {
        match item["type"].as_str() {
            Some("message") => {
                let role = api_string(item,"role")?;
                if !matches!(role,"user"|"assistant"|"system"|"developer") { return Err(format!("Unsupported message role '{role}'")); }
                messages.push(json!({"role":role,"content":api_content(&item["content"])?}));
            }
            Some("function_call") => {
                let arguments: Value = serde_json::from_str(api_string(item,"arguments")?).map_err(|_|"Function arguments must be valid JSON")?;
                if !arguments.is_object() { return Err("Function arguments must be an object".into()); }
                messages.push(json!({"role":"assistant","content":[{"type":"tool_use","id":api_string(item,"call_id")?,"name":api_string(item,"name")?,"input":arguments}]}));
            }
            Some("function_call_output") => messages.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":api_string(item,"call_id")?,"content":api_content(&item["output"])?}]})),
            Some("custom_tool_call") => messages.push(json!({"role":"assistant","content":[{"type":"tool_use","id":api_string(item,"call_id")?,"name":api_string(item,"name")?,"input":{"input":item["input"].as_str().ok_or("Custom tool input must be a string")?}}]})),
            Some("custom_tool_call_output") => messages.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":api_string(item,"call_id")?,"content":api_content(&item["output"])?}]})),
            Some("reasoning") if item.get("encrypted_content").is_none_or(Value::is_null) => {},
            Some(kind) => return Err(format!("Unsupported input item '{kind}'; use previous_response_id for this bridge's retained reasoning state")),
            None => return Err("Input item has no type or role".into()),
        }
    }
    // Native visible history groups consecutive assistant text blocks around
    // tool calls, even when public reasoning summaries separate those blocks.
    let mut merged: Vec<Value> = Vec::new();
    for mut message in messages {
        if message["role"] == "assistant" && message["content"].is_string() {
            message["content"] = json!([{"type":"text","text":message["content"]}]);
        }
        if message["role"] == "assistant" && merged.last().is_some_and(|m| m["role"] == "assistant")
        {
            merged.last_mut().unwrap()["content"]
                .as_array_mut()
                .unwrap()
                .extend(message["content"].as_array().unwrap().clone());
        } else {
            merged.push(message);
        }
    }
    Ok(merged)
}

fn api_responses_request(request: &Value, items: &[Value], session: &str) -> Result<Value, String> {
    let object = request.as_object().ok_or("Request must be a JSON object")?;
    let allowed = [
        "model",
        "input",
        "instructions",
        "tools",
        "tool_choice",
        "parallel_tool_calls",
        "reasoning",
        "text",
        "stream",
        "stream_options",
        "store",
        "metadata",
        "previous_response_id",
        "conversation",
        "max_output_tokens",
        "temperature",
        "top_p",
        "service_tier",
        "bridge_controls",
        "include",
        "background",
        "truncation",
        "user",
        "safety_identifier",
        "prompt_cache_key",
    ];
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("Unsupported Responses parameter '{key}' on Codex"));
        }
    }
    if request.pointer("/bridge_controls/mode") == Some(&json!("strict")) {
        for key in ["user", "safety_identifier", "prompt_cache_key"] {
            if request.get(key).is_some_and(|v| !v.is_null()) {
                return Err(format!("Codex cannot enforce '{key}' in strict mode"));
            }
        }
        if request["parallel_tool_calls"] == false
            || request.pointer("/stream_options/include_obfuscation") == Some(&json!(true))
        {
            return Err(
                "Strict mode cannot enforce parallel planning or stream obfuscation".into(),
            );
        }
    }
    for key in ["stream", "store", "parallel_tool_calls", "background"] {
        if object.get(key).is_some_and(|v| !v.is_boolean()) {
            return Err(format!("{key} must be boolean"));
        }
    }
    if request.get("truncation").is_some_and(|v| v != "disabled") {
        return Err(
            "Automatic truncation is not available; explicitly compact or edit history".into(),
        );
    }
    if request["include"].as_array().is_some_and(|a| !a.is_empty()) {
        return Err("Additional include fields are not available from Codex App Server".into());
    }
    if let Some(include) = request.get("include") {
        if !include.is_array() {
            return Err("include must be an array".into());
        }
    }
    if let Some(options) = request.get("stream_options") {
        if !options.is_object() {
            return Err("stream_options must be an object".into());
        }
        if options
            .as_object()
            .unwrap()
            .keys()
            .any(|k| k != "include_obfuscation")
        {
            return Err("Unsupported Responses stream_options".into());
        }
        if options
            .get("include_obfuscation")
            .is_some_and(|v| !v.is_boolean())
        {
            return Err("include_obfuscation must be boolean".into());
        }
    }
    let mut result = json!({"model":request["model"],"messages":api_items_to_messages(items)?,"metadata":{"session_id":session},"stream":request["stream"]==true});
    if let Some(instructions) = request.get("instructions").filter(|v| !v.is_null()) {
        if !instructions.is_string() {
            return Err("instructions must be a string".into());
        }
        result["system"] = instructions.clone();
    }
    for key in ["temperature", "top_p", "service_tier", "bridge_controls"] {
        if let Some(v) = request.get(key) {
            result[key] = v.clone();
        }
    }
    if let Some(n) = request.get("max_output_tokens") {
        if n.as_u64().is_none_or(|v| v == 0) {
            return Err("max_output_tokens must be positive".into());
        }
        result["max_tokens"] = n.clone();
    }
    if let Some(r) = request.get("reasoning") {
        let r = r.as_object().ok_or("reasoning must be an object")?;
        if r.keys()
            .any(|k| !matches!(k.as_str(), "effort" | "summary"))
        {
            return Err("Unsupported reasoning option".into());
        }
        if let Some(effort) = r.get("effort") {
            result["output_config"]["effort"] = effort.clone();
        }
        if let Some(summary) = r.get("summary").filter(|v| !v.is_null()) {
            if !matches!(summary.as_str(), Some("auto" | "concise" | "detailed")) {
                return Err("Unsupported reasoning summary".into());
            }
            result["thinking"] = json!({"type":"adaptive","summary":summary});
        }
    }
    if let Some(text) = request.get("text") {
        let text = text.as_object().ok_or("text must be an object")?;
        if text
            .keys()
            .any(|k| !matches!(k.as_str(), "format" | "verbosity"))
        {
            return Err("Only text.format and text.verbosity are available through Codex".into());
        }
        if let Some(v) = text.get("verbosity") {
            if !matches!(v.as_str(), Some("low" | "medium" | "high")) {
                return Err("text.verbosity must be low, medium or high".into());
            }
            result["output_config"]["verbosity"] = v.clone();
        }
        if let Some(format) = text.get("format") {
            result["output_config"]["format"] = format.clone();
        }
    }
    let mut tools = Vec::new();
    if let Some(defs) = request.get("tools") {
        for tool in defs.as_array().ok_or("tools must be an array")? {
            if tool["type"] == "custom" {
                if tool.get("format").is_some_and(|f| f["type"] != "text") {
                    return Err("Codex custom tools support free-form text; grammar-constrained decoding requires a native Responses API provider".into());
                }
                tools.push(json!({"name":api_string(tool,"name")?,"description":format!("{}\nReturn the exact raw tool input in the input string property.",tool["description"].as_str().unwrap_or("")),"input_schema":{"type":"object","properties":{"input":{"type":"string"}},"required":["input"],"additionalProperties":false}}));
                continue;
            }
            if tool["type"] != "function" {
                return Err(format!("Codex exposes client function tools, not hosted '{}'; select a native Responses API profile for hosted tools",tool["type"].as_str().unwrap_or("unknown")));
            }
            let mut t = json!({"name":api_string(tool,"name")?,"description":tool["description"].as_str().unwrap_or(""),"input_schema":tool.get("parameters").cloned().unwrap_or(json!({"type":"object","properties":{}}))});
            if let Some(strict) = tool.get("strict") {
                t["strict"] = strict.clone();
            }
            tools.push(t);
        }
    }
    result["tools"] = json!(tools);
    result["tool_choice"] = match request.get("tool_choice") {
        None => json!({"type":"auto"}),
        Some(Value::String(s)) if matches!(s.as_str(), "auto" | "none" | "required") => {
            json!({"type":if s=="required"{"any"}else{s}})
        }
        Some(v) if v["type"] == "function" || v["type"] == "custom" => {
            json!({"type":"tool","name":api_string(v,"name")?})
        }
        _ => return Err("Unsupported tool_choice".into()),
    };
    Ok(result)
}

fn api_chat_request(request: &Value) -> Result<Value, String> {
    let obj = request.as_object().ok_or("Request must be a JSON object")?;
    for k in obj.keys() {
        if ![
            "model",
            "messages",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "stream",
            "stream_options",
            "response_format",
            "reasoning_effort",
            "max_tokens",
            "max_completion_tokens",
            "temperature",
            "top_p",
            "service_tier",
            "stop",
            "bridge_controls",
            "metadata",
            "user",
            "n",
            "store",
        ]
        .contains(&k.as_str())
        {
            return Err(format!("Unsupported Chat parameter '{k}'"));
        }
    }
    if request
        .get("n")
        .is_some_and(|v| v.as_u64().is_none_or(|n| !(1..=4).contains(&n)))
    {
        return Err("Local Codex supports n between 1 and 4".into());
    }
    for key in ["stream", "store", "parallel_tool_calls"] {
        if request.get(key).is_some_and(|v| !v.is_boolean()) {
            return Err(format!("{key} must be boolean"));
        }
    }
    if let Some(options) = request.get("stream_options") {
        let options = options
            .as_object()
            .ok_or("stream_options must be an object")?;
        if options.iter().any(|(k, v)| {
            !matches!(k.as_str(), "include_usage" | "include_obfuscation") || !v.is_boolean()
        }) {
            return Err("Unsupported Chat stream_options".into());
        }
    }
    let mut items = Vec::new();
    for m in request["messages"]
        .as_array()
        .ok_or("messages must be an array")?
    {
        let role = api_string(m, "role")?;
        if role == "tool" {
            items.push(json!({"type":"function_call_output","call_id":api_string(m,"tool_call_id")?,"output":m["content"]}));
            continue;
        }
        if !m["content"].is_null() {
            items.push(json!({"type":"message","role":role,"content":m["content"]}));
        }
        if let Some(calls) = m.get("tool_calls") {
            if role != "assistant" {
                return Err("tool_calls require assistant role".into());
            }
            for call in calls.as_array().ok_or("tool_calls must be an array")? {
                if call["type"] != "function" {
                    return Err("Only function tool calls supported".into());
                }
                items.push(json!({"type":"function_call","call_id":api_string(call,"id")?,"name":api_string(&call["function"],"name")?,"arguments":api_string(&call["function"],"arguments")?}));
            }
        }
    }
    let mut out = json!({"model":request["model"],"input":items,"store":false,"stream":request["stream"]==true});
    for key in [
        "temperature",
        "top_p",
        "service_tier",
        "bridge_controls",
        "parallel_tool_calls",
        "user",
        "metadata",
    ] {
        if let Some(v) = request.get(key) {
            out[key] = v.clone();
        }
    }
    if let Some(n) = request
        .get("max_completion_tokens")
        .or_else(|| request.get("max_tokens"))
    {
        out["max_output_tokens"] = n.clone();
    }
    if let Some(e) = request.get("reasoning_effort") {
        out["reasoning"] = json!({"effort":e});
    }
    if let Some(f) = request.get("response_format") {
        out["text"]["format"] = if f["type"] == "json_schema" {
            let mut format = f["json_schema"].clone();
            format["type"] = json!("json_schema");
            format
        } else {
            f.clone()
        };
    }
    if let Some(tools) = request.get("tools") {
        let mut defs = Vec::new();
        for t in tools.as_array().ok_or("tools must be an array")? {
            if t["type"] != "function" {
                return Err("Only function tools supported".into());
            }
            let mut f = t["function"].clone();
            f["type"] = json!("function");
            defs.push(f);
        }
        out["tools"] = json!(defs);
    }
    if let Some(choice) = request.get("tool_choice") {
        out["tool_choice"] = if choice.is_string() {
            choice.clone()
        } else {
            json!({"type":"function","name":choice["function"]["name"]})
        };
    }
    Ok(out)
}

fn api_output_items(message: &Value) -> Vec<Value> {
    message["content"].as_array().into_iter().flatten().enumerate().filter_map(|(i,b)| {
        let id = format!("{}_{}",message["id"].as_str().unwrap_or("msg"),i);
        match b["type"].as_str() {
            Some("text") => Some(json!({"id":id,"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":b["text"],"annotations":[],"logprobs":[]}]})),
            Some("thinking") => Some(json!({"id":format!("rs_{id}"),"type":"reasoning","summary":[{"type":"summary_text","text":b["thinking"]}]})),
            Some("tool_use") => Some(json!({"id":format!("fc_{}",b["id"].as_str().unwrap_or("call")),"type":"function_call","status":"completed","call_id":b["id"],"name":b["name"],"arguments":b["input"].to_string()})),
            _ => None,
        }
    }).collect()
}

fn api_is_custom(request: &Value, name: &Value) -> bool {
    request["tools"].as_array().is_some_and(|tools| {
        tools
            .iter()
            .any(|t| t["type"] == "custom" && &t["name"] == name)
    })
}
fn api_output_items_for(message: &Value, request: &Value) -> Vec<Value> {
    api_output_items(message)
        .into_iter()
        .map(|mut item| {
            if item["type"] == "function_call" && api_is_custom(request, &item["name"]) {
                let args: Value = serde_json::from_str(item["arguments"].as_str().unwrap_or("{}"))
                    .unwrap_or(Value::Null);
                item["type"] = json!("custom_tool_call");
                item["input"] = json!(args["input"].as_str().unwrap_or(""));
                item.as_object_mut().unwrap().remove("arguments");
            }
            item
        })
        .collect()
}
fn api_validate_custom_output(message: &Value, request: &Value) -> Result<(), String> {
    for block in message["content"].as_array().into_iter().flatten() {
        if block["type"] == "tool_use"
            && api_is_custom(request, &block["name"])
            && !block["input"]["input"].is_string()
        {
            return Err("Codex custom tool did not produce the required raw input string".into());
        }
    }
    Ok(())
}

fn api_usage(u: &Value) -> Value {
    if u["bridge_usage_complete"] == false {
        return Value::Null;
    }
    let cached = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
    let writes = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
    let input = u["input_tokens"].as_u64().unwrap_or(0) + cached + writes;
    let output = u["output_tokens"].as_u64().unwrap_or(0);
    json!({"input_tokens":input,"input_tokens_details":{"cached_tokens":cached,"cache_write_tokens":writes},"output_tokens":output,"output_tokens_details":{"reasoning_tokens":u["reasoning_tokens"].as_u64().unwrap_or(0)},"total_tokens":input+output})
}

fn api_response_object(id: &str, created: u64, request: &Value, message: &Value) -> Value {
    let limited = message["stop_reason"] == "max_tokens";
    json!({"id":id,"object":"response","created_at":created,"completed_at":api_now(),"model":message["model"],
        "status":if limited{"incomplete"}else{"completed"},"error":null,"incomplete_details":if limited{json!({"reason":"max_output_tokens"})}else{Value::Null},
        "output":api_output_items_for(message,request),"usage":api_usage(&message["usage"]),"instructions":request["instructions"],
        "tools":request.get("tools").cloned().unwrap_or(json!([])),"tool_choice":request.get("tool_choice").cloned().unwrap_or(json!("auto")),
        "parallel_tool_calls":request.get("parallel_tool_calls").cloned().unwrap_or(json!(true)),"reasoning":request.get("reasoning").cloned().unwrap_or(json!({"effort":null,"summary":null})),
        "text":request.get("text").cloned().unwrap_or(json!({"format":{"type":"text"}})),"metadata":request.get("metadata").cloned().unwrap_or(json!({})),
        "previous_response_id":request["previous_response_id"],"conversation":request["conversation"],"store":request["store"]!=false,
        "max_output_tokens":null,"temperature":null,"top_p":null,"service_tier":null,"truncation":"disabled",
        "bridge_details":{"service_tier_requested":request["service_tier"],"service_tier_actual":"not_reported_by_codex","local_termination":if message["usage"]["bridge_usage_complete"]==false{message["stop_reason"].clone()}else{Value::Null},"max_output_chars":request.pointer("/bridge_controls/max_output_chars").cloned().unwrap_or(Value::Null)}})
}

fn api_chat_object(id: &str, created: u64, message: &Value) -> Value {
    let mut text = String::new();
    let mut calls = Vec::new();
    for item in api_output_items(message) {
        if item["type"] == "message" {
            text.push_str(item["content"][0]["text"].as_str().unwrap_or(""));
        }
        if item["type"] == "function_call" {
            calls.push(json!({"id":item["call_id"],"type":"function","function":{"name":item["name"],"arguments":item["arguments"]}}));
        }
    }
    let mut m = json!({"role":"assistant","content":if text.is_empty(){Value::Null}else{json!(text)},"refusal":null});
    if !calls.is_empty() {
        m["tool_calls"] = json!(calls);
    }
    let u = api_usage(&message["usage"]);
    let usage = if u.is_null() {
        Value::Null
    } else {
        json!({"prompt_tokens":u["input_tokens"],"completion_tokens":u["output_tokens"],"total_tokens":u["total_tokens"],"prompt_tokens_details":u["input_tokens_details"],"completion_tokens_details":u["output_tokens_details"]})
    };
    json!({"id":id,"object":"chat.completion","created":created,"model":message["model"],"choices":[{"index":0,"message":m,"logprobs":null,"finish_reason":api_chat_finish(message)}],"usage":usage})
}

fn api_chat_finish(m: &Value) -> &'static str {
    match m["stop_reason"].as_str() {
        Some("tool_use") => "tool_calls",
        Some("max_tokens") => "length",
        _ => "stop",
    }
}

fn api_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
