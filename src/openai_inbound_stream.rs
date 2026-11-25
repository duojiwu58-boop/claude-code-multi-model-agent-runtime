// Native OpenAI HTTP surface. Streaming consumes the same bounded, cancellable
// Codex event stream as Messages; it never buffers a whole turn before replying.
async fn api_error_contract(request: Request, next: Next) -> Response {
    let path = request.uri().path();
    let openai = path.starts_with("/v1/responses")
        || path.starts_with("/v1/conversations")
        || path.starts_with("/v1/chat/completions")
        || path.starts_with("/v1/files");
    let response = next.run(request).await;
    if openai
        && !response.status().is_success()
        && !response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|s| s.starts_with("application/json"))
    {
        let status = if response.status() == StatusCode::UNPROCESSABLE_ENTITY {
            StatusCode::BAD_REQUEST
        } else {
            response.status()
        };
        let message = match axum::body::to_bytes(response.into_body(), 64 * 1024).await {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(_) => "Invalid API request".into(),
        };
        return api_error(status, message);
    }
    response
}
async fn responses(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    let Some(profile) = active_provider_profile(&state) else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "No active provider profile",
        );
    };
    if profile.transport == ProviderTransport::Codex {
        return api_codex(state, profile, request, None).await;
    }
    if profile.transport == ProviderTransport::OpenAiResponses {
        return api_native(profile, request, "").await;
    }
    if profile.local_gemini {
        return legacy_responses(State(state), headers, Json(request)).await;
    }
    api_error(
        StatusCode::NOT_IMPLEMENTED,
        "Select a Codex or native OpenAI Responses profile for this endpoint",
    )
}

async fn api_chat(State(state): State<Arc<AppState>>, Json(request): Json<Value>) -> Response {
    let Some(profile) = active_provider_profile(&state) else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "No active provider profile",
        );
    };
    if profile.transport == ProviderTransport::OpenAiChat {
        return api_native(profile, request, "").await;
    }
    if profile.transport != ProviderTransport::Codex {
        return api_error(
            StatusCode::NOT_IMPLEMENTED,
            "Select a Codex or native Chat Completions profile for this endpoint",
        );
    }
    match api_chat_request(&request) {
        Ok(r) if request["n"].as_u64().unwrap_or(1) > 1 => {
            api_chat_multiple(state, profile, r, request).await
        }
        Ok(r) => api_codex(state, profile, r, Some(request)).await,
        Err(e) => api_error(StatusCode::BAD_REQUEST, e),
    }
}

async fn api_native_send(builder: reqwest::RequestBuilder, streaming: bool) -> Result<reqwest::Response, String> {
    if streaming {
        tokio::time::timeout(UPSTREAM_STREAM_IDLE_TIMEOUT, builder.send()).await
            .map_err(|_| "Native upstream timed out waiting for response headers".to_string())?
            .map_err(|e| e.to_string())
    } else {
        builder.send().await.map_err(|e| e.to_string())
    }
}

fn api_native_body(upstream: reqwest::Response, streaming: bool, idle: Duration) -> Body {
    if !streaming { return Body::from_stream(upstream.bytes_stream()); }
    Body::from_stream(stream::unfold((upstream.bytes_stream(), false), move |(mut source, ended)| async move {
        if ended { return None; }
        match tokio::time::timeout(idle, source.next()).await {
            Ok(Some(Ok(bytes))) => Some((Ok(bytes), (source, false))),
            Ok(None) => None,
            Ok(Some(Err(error))) => Some((Err(std::io::Error::other(redact_url_credentials_in_text(&error.to_string()))), (source, true))),
            Err(_) => Some((Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "Native upstream stream idle timeout")), (source, true))),
        }
    }))
}

async fn api_native(profile: ProviderProfile, mut request: Value, suffix: &str) -> Response {
    if !request.is_object() {
        return api_error(StatusCode::BAD_REQUEST, "Request must be a JSON object");
    }
    if request
        .get("model")
        .is_some_and(|v| v.as_str() != Some(display_model_name(&profile.model).as_str()))
    {
        return api_error(
            StatusCode::NOT_FOUND,
            "Requested model differs from the active provider profile",
        );
    }
    request["model"] = json!(display_model_name(&profile.model));
    let streaming = request["stream"] == true;
    let mut builder = profile
        .client
        .post(format!(
            "{}{suffix}",
            profile.upstream_url.trim_end_matches('/')
        ))
        .json(&request);
    builder = apply_upstream_total_timeout(builder, streaming);
    if let Some(key) = profile.auth_token.as_ref().or(profile.api_key.as_ref()) {
        builder = builder.bearer_auth(key);
    }
    match api_native_send(builder, streaming).await {
        Ok(upstream) => {
            let status = upstream.status();
            let headers = upstream.headers().clone();
            let mut response = Response::new(api_native_body(upstream, streaming, UPSTREAM_STREAM_IDLE_TIMEOUT));
            *response.status_mut() = status;
            for key in ["content-type", "x-request-id", "retry-after"] {
                if let Some(v) = headers.get(key) {
                    response.headers_mut().insert(key, v.clone());
                }
            }
            response
        }
        Err(e) => api_error(
            StatusCode::BAD_GATEWAY,
            redact_url_credentials_in_text(&e.to_string()),
        ),
    }
}

async fn api_native_resources(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    let resource = path.starts_with("/v1/files")
        || path.starts_with("/v1/chat/completions/")
        || (path == "/v1/chat/completions" && request.method() == reqwest::Method::GET)
        || path.starts_with("/v1/conversations")
        || (path.starts_with("/v1/responses/")
            && !matches!(path, "/v1/responses/compact" | "/v1/responses/input_tokens"));
    if !resource {
        return next.run(request).await;
    }
    let Some(profile) = active_provider_profile(&state).filter(|p| {
        p.transport == ProviderTransport::OpenAiResponses
            || (p.transport == ProviderTransport::OpenAiChat
                && (path.starts_with("/v1/files") || path.starts_with("/v1/chat/completions")))
    }) else {
        return next.run(request).await;
    };
    let Some(base) = profile.upstream_url.trim_end_matches('/').strip_suffix(
        if profile.transport == ProviderTransport::OpenAiChat {
            "/chat/completions"
        } else {
            "/responses"
        },
    ) else {
        return api_error(
            StatusCode::BAD_GATEWAY,
            "Native resource routing requires an upstream URL ending in /responses",
        );
    };
    let relative = request
        .uri()
        .path_and_query()
        .unwrap()
        .as_str()
        .strip_prefix("/v1")
        .unwrap();
    if path
        .split('/')
        .any(|p| p == "." || p == ".." || p.contains('%'))
    {
        return api_error(StatusCode::BAD_REQUEST, "Invalid resource path");
    }
    let url = format!("{base}{relative}");
    let streaming = request.uri().query().is_some_and(|query|
        url::form_urlencoded::parse(query.as_bytes()).any(|(key,value)| key == "stream" && value == "true"));
    let method = request.method().clone();
    let content_type = request.headers().get("content-type").cloned();
    let bytes = match axum::body::to_bytes(request.into_body(), MAX_UPSTREAM_RESPONSE_BYTES).await {
        Ok(b) => b,
        Err(_) => {
            return api_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Resource body exceeds local 8 MiB limit",
            )
        }
    };
    let mut builder = apply_upstream_total_timeout(profile.client.request(method, url), streaming);
    if !bytes.is_empty() {
        builder = builder
            .header(
                "content-type",
                content_type.unwrap_or_else(|| HeaderValue::from_static("application/json")),
            )
            .body(bytes);
    }
    if let Some(key) = profile.auth_token.as_ref().or(profile.api_key.as_ref()) {
        builder = builder.bearer_auth(key);
    }
    match api_native_send(builder, streaming).await {
        Ok(upstream) => {
            let status = upstream.status();
            let headers = upstream.headers().clone();
            let mut response = Response::new(api_native_body(upstream, streaming, UPSTREAM_STREAM_IDLE_TIMEOUT));
            *response.status_mut() = status;
            for key in ["content-type", "x-request-id", "retry-after"] {
                if let Some(v) = headers.get(key) {
                    response.headers_mut().insert(key, v.clone());
                }
            }
            response
        }
        Err(e) => api_error(
            StatusCode::BAD_GATEWAY,
            redact_url_credentials_in_text(&e.to_string()),
        ),
    }
}

async fn api_compact(State(state): State<Arc<AppState>>, Json(request): Json<Value>) -> Response {
    if let Some(profile) = active_provider_profile(&state)
        .filter(|p| p.transport == ProviderTransport::OpenAiResponses)
    {
        return api_native(profile, request, "/compact").await;
    }
    api_error(StatusCode::NOT_IMPLEMENTED,"Codex App Server cannot export an OpenAI encrypted compaction item. Select a native Responses API profile for /responses/compact; retained Codex threads use native automatic context compaction.")
}
async fn api_input_tokens(
    State(state): State<Arc<AppState>>,
    Json(request): Json<Value>,
) -> Response {
    if let Some(profile) = active_provider_profile(&state)
        .filter(|p| p.transport == ProviderTransport::OpenAiResponses)
    {
        return api_native(profile, request, "/input_tokens").await;
    }
    api_error(StatusCode::NOT_IMPLEMENTED,"Codex App Server does not expose exact preflight tokenization. Select a native Responses API profile for /responses/input_tokens; Messages count_tokens is an explicitly marked estimate.")
}

async fn api_codex(
    state: Arc<AppState>,
    mut profile: ProviderProfile,
    request: Value,
    chat: Option<Value>,
) -> Response {
    let model = display_model_name(&profile.model);
    if request["model"].as_str() != Some(model.as_str()) {
        return api_error(
            StatusCode::NOT_FOUND,
            format!("Select the active model '{model}' in the request"),
        );
    }
    if let Err(e) = api_metadata(&request) {
        return api_error(StatusCode::BAD_REQUEST, e);
    }
    let mut context = match ApiContext::prepare(state.clone(), &profile, request) {
        Ok(c) => c,
        Err(e) => return api_resource_error(e),
    };
    context.chat_request = chat.clone();
    if let Err(e) = api_expand_files(&state, &mut context.input).await {
        return api_resource_error(e);
    }
    if context
        .input
        .iter()
        .map(|v| v.to_string().len())
        .sum::<usize>()
        > MAX_UPSTREAM_RESPONSE_BYTES
    {
        return api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Expanded file inputs exceed the local 8 MiB limit",
        );
    }
    let mut native = match api_responses_request(&context.request, &context.input, &context.session)
    {
        Ok(v) => v,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, e),
    };
    // Bridge-issued call IDs can identify stateless callbacks. Similar prompts
    // alone never become a session key.
    if chat.is_some()
        || (context.request["store"] == false
            && context.request["previous_response_id"].is_null()
            && context.conversation.is_none())
    {
        native.as_object_mut().unwrap().remove("metadata");
    }
    if let Some(stop) = chat
        .as_ref()
        .and_then(|v| v.get("stop"))
        .filter(|v| !v.is_null())
    {
        native["stop_sequences"] = if stop.is_string() {
            json!([stop])
        } else {
            stop.clone()
        };
    }
    if let Some(e) = state
        .reasoning_efforts
        .read()
        .ok()
        .and_then(|m| m.get(&profile.file_name).cloned())
    {
        if let Some(e) = normalized_profile_effort(&profile, &e) {
            profile.openai_capabilities.reasoning_effort_override = Some(e.to_owned());
        }
    }
    // OpenAI request-level effort wins over the bridge profile default. The
    // Messages entry retains its existing explicit profile override policy.
    if let Some(effort) = context
        .request
        .pointer("/reasoning/effort")
        .and_then(Value::as_str)
    {
        profile.openai_capabilities.reasoning_effort_override = Some(effort.to_owned());
    }
    let mut diagnostics = match apply_provider_request_overrides(&profile, &mut native) {
        Ok(v) => v,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, e),
    };
    diagnostics.extend(codex_request_diagnostics(&native));
    if let Some(effort) = native.pointer("/output_config/effort") {
        context.request["reasoning"]["effort"] = effort.clone();
    }
    for key in ["user", "safety_identifier", "prompt_cache_key"] {
        if context.request.get(key).is_some_and(|v| !v.is_null()) {
            diagnostics.push(format!(
                "Codex App Server does not expose '{key}'; this field is not forwarded"
            ));
        }
    }
    if context.request["parallel_tool_calls"] == false {
        diagnostics.push("Codex callbacks are returned one at a time; parallel_tool_calls=false cannot constrain upstream planning".into());
    }
    if context
        .request
        .pointer("/stream_options/include_obfuscation")
        == Some(&json!(true))
    {
        diagnostics.push("Local Codex streams do not add obfuscation padding".into());
    }
    if let Err(e) = prepare_codex_request(&native, &profile) {
        return api_error(StatusCode::BAD_REQUEST, e);
    }
    if context.request.pointer("/text/format/type") == Some(&json!("json_object")) {
        diagnostics.push("Codex JSON object mode validates the complete object before delivery; JSON-mode SSE is buffered until validation succeeds".into());
    }
    if context.request["background"] == true {
        return api_background_start(profile, native, context, diagnostics).await;
    }
    api_codex_execute(profile, native, context, chat, diagnostics).await
}

async fn api_codex_execute(
    profile: ProviderProfile,
    native: Value,
    context: ApiContext,
    chat: Option<Value>,
    diagnostics: Vec<String>,
) -> Response {
    let provider = profile.file_name.clone();
    let response = forward_codex_profile(profile, native).await;
    let status = response.status();
    if !status.is_success() || context.request["stream"] != true {
        let bytes =
            match axum::body::to_bytes(response.into_body(), MAX_UPSTREAM_RESPONSE_BYTES).await {
                Ok(v) => v,
                Err(_) => return api_error(StatusCode::BAD_GATEWAY, "Invalid Codex response body"),
            };
        let message: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => return api_error(StatusCode::BAD_GATEWAY, "Invalid Codex JSON"),
        };
        if !status.is_success() {
            return api_error(
                status,
                message["error"]["message"]
                    .as_str()
                    .unwrap_or("Codex request failed"),
            );
        }
        let result = if chat.is_some() {
            context.finish_chat(&message)
        } else {
            context.finish(&message)
        };
        return attach_bridge_diagnostics(
            match result {
                Ok(v) => Json(v).into_response(),
                Err(e) => api_resource_error(e),
            },
            &provider,
            &diagnostics,
        );
    }
    let converter = ApiStream::new(
        context,
        chat.is_some(),
        chat.as_ref()
            .is_some_and(|v| v.pointer("/stream_options/include_usage") == Some(&json!(true))),
    );
    let upstream = response.into_body().into_data_stream();
    let output = stream::unfold(
        (
            upstream,
            SseDataDecoder::default(),
            converter,
            VecDeque::<Event>::new(),
        ),
        |(mut upstream, mut decoder, mut converter, mut queue)| async move {
            loop {
                if let Some(event) = queue.pop_front() {
                    return Some((
                        Ok::<Event, Infallible>(event),
                        (upstream, decoder, converter, queue),
                    ));
                }
                if converter.ended {
                    return None;
                }
                let result = match upstream.next().await {
                    Some(Ok(bytes)) => decoder.push_bytes(&bytes),
                    Some(Err(_)) => Err("Codex stream transport failed".into()),
                    None => Err("Codex stream ended before message_stop".into()),
                };
                match result {
                    Ok(payloads) => {
                        for data in payloads {
                            if converter.ended {
                                break;
                            }
                            match serde_json::from_str::<Value>(&data)
                                .map_err(|e| e.to_string())
                                .and_then(|v| converter.accept(v))
                            {
                                Ok(events) => queue.extend(events),
                                Err(e) => queue.extend(converter.fail(&e)),
                            }
                        }
                    }
                    Err(e) => queue.extend(converter.fail(&e)),
                }
            }
        },
    );
    attach_bridge_diagnostics(
        Sse::new(output)
            .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
            .into_response(),
        &provider,
        &diagnostics,
    )
}

struct ApiStream {
    context: ApiContext,
    chat: bool,
    include_usage: bool,
    sequence: u64,
    message: Value,
    arguments: HashMap<usize, String>,
    tool_indices: HashMap<usize, usize>,
    ended: bool,
}
impl ApiStream {
    fn new(context: ApiContext, chat: bool, include_usage: bool) -> Self {
        Self {
            context,
            chat,
            include_usage,
            sequence: 0,
            message: Value::Null,
            arguments: HashMap::new(),
            tool_indices: HashMap::new(),
            ended: false,
        }
    }
    fn event(&mut self, kind: &str, mut data: Value) -> Event {
        data["type"] = json!(kind);
        data["sequence_number"] = json!(self.sequence);
        self.sequence += 1;
        Event::default().event(kind).data(data.to_string())
    }
    fn chunk(&self, delta: Value, finish: Value) -> Event {
        Event::default().data(json!({"id":self.context.response_id,"object":"chat.completion.chunk","created":self.context.created,"model":self.message["model"],"choices":[{"index":0,"delta":delta,"logprobs":null,"finish_reason":finish}]}).to_string())
    }
    fn item(&self, index: usize) -> Result<Value, String> {
        api_output_items_for(&self.message, &self.context.request)
            .get(index)
            .cloned()
            .ok_or_else(|| "Invalid output block index".into())
    }
    fn accept(&mut self, v: Value) -> Result<Vec<Event>, String> {
        let mut events = Vec::new();
        match v["type"].as_str() {
            Some("message_start") => {
                if !self.message.is_null() {
                    return Err("Duplicate message_start".into());
                }
                self.message = v["message"].clone();
                self.message["content"] = json!([]);
                if self.chat {
                    events.push(self.chunk(json!({"role":"assistant","content":""}), Value::Null));
                } else {
                    let mut r = api_response_object(
                        &self.context.response_id,
                        self.context.created,
                        &self.context.request,
                        &self.message,
                    );
                    r["status"] = json!("in_progress");
                    r["completed_at"] = Value::Null;
                    r["usage"] = Value::Null;
                    events.push(self.event("response.created", json!({"response":r})));
                    events.push(self.event("response.in_progress", json!({"response":r})));
                }
            }
            Some("content_block_start") => {
                let index = v["index"].as_u64().ok_or("Missing block index")? as usize;
                let blocks = self.message["content"]
                    .as_array_mut()
                    .ok_or("Missing message_start")?;
                if index != blocks.len() {
                    return Err("Out-of-order content block".into());
                }
                let block = v["content_block"].clone();
                blocks.push(block.clone());
                if block["type"] == "tool_use" {
                    let next = self.tool_indices.len();
                    self.tool_indices.insert(index, next);
                    self.arguments.insert(index, String::new());
                }
                if self.chat {
                    if block["type"] == "tool_use" {
                        events.push(self.chunk(json!({"tool_calls":[{"index":self.tool_indices[&index],"id":block["id"],"type":"function","function":{"name":block["name"],"arguments":""}}]}),Value::Null));
                    }
                } else {
                    let mut item = self.item(index)?;
                    if item["type"] == "function_call" {
                        item["arguments"] = json!("");
                        item["status"] = json!("in_progress");
                    }
                    if item["type"] == "custom_tool_call" {
                        item["input"] = json!("");
                        item["status"] = json!("in_progress");
                    }
                    if item["type"] == "message" {
                        item["content"] = json!([]);
                        item["status"] = json!("in_progress");
                    }
                    if item["type"] == "reasoning" {
                        item["summary"] = json!([]);
                    }
                    events.push(self.event(
                        "response.output_item.added",
                        json!({"output_index":index,"item":item}),
                    ));
                    match block["type"].as_str(){
                        Some("text")=>events.push(self.event("response.content_part.added",json!({"output_index":index,"item_id":item["id"],"content_index":0,"part":{"type":"output_text","text":"","annotations":[],"logprobs":[]}}))),
                        Some("thinking")=>events.push(self.event("response.reasoning_summary_part.added",json!({"output_index":index,"item_id":item["id"],"summary_index":0,"part":{"type":"summary_text","text":""}}))),_=>{}
                    }
                }
            }
            Some("content_block_delta") => {
                let index = v["index"].as_u64().ok_or("Missing block index")? as usize;
                let block = self.message["content"]
                    .get_mut(index)
                    .ok_or("Unknown block")?;
                let kind = v["delta"]["type"].as_str().ok_or("Missing delta type")?;
                let (event, key, delta) = match kind {
                    "text_delta" => (
                        "response.output_text.delta",
                        "text",
                        v["delta"]["text"].as_str().ok_or("Missing text")?,
                    ),
                    "thinking_delta" => (
                        "response.reasoning_summary_text.delta",
                        "thinking",
                        v["delta"]["thinking"].as_str().ok_or("Missing thinking")?,
                    ),
                    "input_json_delta" => (
                        "response.function_call_arguments.delta",
                        "input",
                        v["delta"]["partial_json"]
                            .as_str()
                            .ok_or("Missing arguments")?,
                    ),
                    "signature_delta" => return Ok(events),
                    _ => return Err("Unknown Codex delta".into()),
                };
                let current = if key == "input" {
                    self.arguments.get_mut(&index).ok_or("Missing tool")?
                } else {
                    let mut text = block[key].as_str().unwrap_or("").to_string();
                    text.push_str(delta);
                    if text.len() > MAX_UPSTREAM_RESPONSE_BYTES {
                        return Err("Output exceeds local limit".into());
                    }
                    block[key] = json!(text);
                    // Arguments are accumulated separately; other block types are done here.
                    if self.chat {
                        if key == "text" {
                            events.push(self.chunk(json!({"content":delta}), Value::Null));
                        }
                    } else {
                        let item = self.item(index)?;
                        let mut data =
                            json!({"output_index":index,"item_id":item["id"],"delta":delta});
                        data[if key == "text" {
                            "content_index"
                        } else {
                            "summary_index"
                        }] = json!(0);
                        if key == "text" {
                            data["logprobs"] = json!([]);
                        }
                        events.push(self.event(event, data));
                    }
                    return Ok(events);
                };
                if current.len() + delta.len() > MAX_STREAMED_TOOL_ARGUMENT_BYTES {
                    return Err("Arguments exceed local limit".into());
                }
                current.push_str(delta);
                if !self.chat
                    && api_is_custom(
                        &self.context.request,
                        &self.message["content"][index]["name"],
                    )
                {
                    return Ok(events);
                }
                if self.chat {
                    events.push(self.chunk(json!({"tool_calls":[{"index":self.tool_indices[&index],"function":{"arguments":delta}}]}),Value::Null));
                } else {
                    let item = self.item(index)?;
                    events.push(self.event(
                        event,
                        json!({"output_index":index,"item_id":item["id"],"delta":delta}),
                    ));
                }
            }
            Some("content_block_stop") => {
                let index = v["index"].as_u64().ok_or("Missing block index")? as usize;
                if let Some(args) = self.arguments.remove(&index) {
                    self.message["content"][index]["input"] =
                        serde_json::from_str(if args.is_empty() { "{}" } else { &args })
                            .map_err(|_| "Invalid tool argument JSON")?;
                    api_validate_custom_output(&self.message, &self.context.request)?;
                }
                if !self.chat {
                    let item = self.item(index)?;
                    let common = json!({"output_index":index,"item_id":item["id"]});
                    match item["type"].as_str() {
                        Some("custom_tool_call") => {
                            let mut d = common.clone();
                            d["delta"] = item["input"].clone();
                            events.push(self.event("response.custom_tool_call_input.delta", d));
                            let mut d = common.clone();
                            d["input"] = item["input"].clone();
                            events.push(self.event("response.custom_tool_call_input.done", d));
                        }
                        Some("function_call") => {
                            let mut d = common.clone();
                            d["arguments"] = item["arguments"].clone();
                            events.push(self.event("response.function_call_arguments.done", d));
                        }
                        Some("message") => {
                            let mut d = common.clone();
                            d["content_index"] = json!(0);
                            d["text"] = item["content"][0]["text"].clone();
                            d["logprobs"] = json!([]);
                            events.push(self.event("response.output_text.done", d.clone()));
                            d.as_object_mut().unwrap().remove("text");
                            d["part"] = item["content"][0].clone();
                            events.push(self.event("response.content_part.done", d));
                        }
                        Some("reasoning") => {
                            let mut d = common.clone();
                            d["summary_index"] = json!(0);
                            d["text"] = item["summary"][0]["text"].clone();
                            events.push(
                                self.event("response.reasoning_summary_text.done", d.clone()),
                            );
                            d.as_object_mut().unwrap().remove("text");
                            d["part"] = item["summary"][0].clone();
                            events.push(self.event("response.reasoning_summary_part.done", d));
                        }
                        _ => {}
                    }
                    events.push(self.event(
                        "response.output_item.done",
                        json!({"output_index":index,"item":item}),
                    ));
                }
            }
            Some("message_delta") => {
                self.message["stop_reason"] = v["delta"]["stop_reason"].clone();
                self.message["stop_sequence"] = v["delta"]["stop_sequence"].clone();
                if let Some(u) = v["usage"].as_object() {
                    for (k, value) in u {
                        self.message["usage"][k] = value.clone();
                    }
                }
            }
            Some("message_stop") => {
                if self.message.is_null() {
                    return Err("Missing message_start".into());
                }
                if self.chat {
                    let full = self.context.finish_chat(&self.message)?;
                    events.push(self.chunk(json!({}), json!(api_chat_finish(&self.message))));
                    if self.include_usage {
                        events.push(Event::default().data(json!({"id":self.context.response_id,"object":"chat.completion.chunk","created":self.context.created,"model":self.message["model"],"choices":[],"usage":full["usage"]}).to_string()));
                    }
                    events.push(Event::default().data("[DONE]"));
                } else {
                    let response = self.context.finish(&self.message)?;
                    let kind = if response["status"] == "incomplete" {
                        "response.incomplete"
                    } else {
                        "response.completed"
                    };
                    events.push(self.event(kind, json!({"response":response})));
                }
                self.ended = true;
            }
            Some("error") => {
                return Err(v["error"]["message"]
                    .as_str()
                    .unwrap_or("Codex stream failed")
                    .into())
            }
            Some("ping") => {}
            _ => return Err("Unexpected Codex stream event".into()),
        }
        Ok(events)
    }
    fn fail(&mut self, message: &str) -> Vec<Event> {
        self.ended = true;
        let error = json!({"type":"api_error","code":"server_error","message":message});
        if self.chat {
            return vec![
                Event::default().data(json!({"error":error}).to_string()),
                Event::default().data("[DONE]"),
            ];
        }
        let mut response = api_response_object(
            &self.context.response_id,
            self.context.created,
            &self.context.request,
            &self.message,
        );
        response["status"] = json!("failed");
        response["error"] = error.clone();
        response["usage"] = Value::Null;
        vec![
            self.event("error", error),
            self.event("response.failed", json!({"response":response})),
        ]
    }
}
