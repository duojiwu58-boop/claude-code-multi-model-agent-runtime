// Developer API background Interactions. This is not Cloud continuation_token.
const GEMINI_BACKGROUND_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);
const GEMINI_BACKGROUND_RETRIES: usize = 5;
const GEMINI_BACKGROUND_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

async fn read_gemini_background_json(response: reqwest::Response) -> Result<Value, String> {
    let bytes = read_response_bytes_limited(response, GEMINI_BACKGROUND_RESPONSE_BYTES).await?;
    serde_json::from_slice(&bytes).map_err(|e| format!("Invalid Gemini background JSON: {e}"))
}

fn gemini_background_context_usage(usage: &mut Value, input_tokens: u64) {
    // Preserve billing separately: per-hop input/cache totals cannot describe
    // the current context, and summing them triggers premature compaction.
    if let Some(object) = usage.as_object_mut() {
        for field in ["input_tokens", "cache_read_input_tokens"] {
            if let Some(value) = object.remove(field) {
                object.insert(format!("billed_{field}"), value);
            }
        }
        object.insert("input_tokens".to_string(), json!(input_tokens));
    }
}

#[derive(Clone)]
struct GeminiBackgroundTransport {
    client: Client,
    endpoint: String,
    api_key: String,
    poll_interval: Duration,
}

impl GeminiBackgroundTransport {
    fn new(profile: &ProviderProfile) -> Self {
        Self {
            client: profile.client.clone(),
            endpoint: profile.upstream_url.clone(),
            api_key: profile.api_key.clone().unwrap_or_default(),
            poll_interval: Duration::from_secs(2),
        }
    }

    fn resource_url(&self, id: &str, cancel: bool) -> Result<url::Url, String> {
        let mut url = url::Url::parse(&self.endpoint).map_err(|e| e.to_string())?;
        url.set_query(None);
        url.path_segments_mut()
            .map_err(|_| "Invalid Gemini interaction URL".to_string())?
            .pop_if_empty()
            .push(id);
        if cancel {
            url.path_segments_mut().unwrap().push("cancel");
        }
        Ok(url)
    }

    async fn get(&self, id: &str, cursor: Option<&str>) -> Result<reqwest::Response, String> {
        let mut url = self.resource_url(id, false)?;
        if let Some(cursor) = cursor {
            url.query_pairs_mut()
                .append_pair("stream", "true")
                .append_pair("last_event_id", cursor);
        }
        let mut request = self.client.get(url).header("x-goog-api-key", &self.api_key);
        if cursor.is_none() {
            request = request.timeout(GEMINI_ARGON_REASONING_TIMEOUT);
        }
        let response = tokio::time::timeout(GEMINI_ARGON_REASONING_TIMEOUT, request.send())
            .await
            .map_err(|_| "Gemini background retrieval timed out".to_string())?
            .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!(
                "Gemini background retrieval returned HTTP {}",
                response.status()
            ));
        }
        Ok(response)
    }
}

fn gemini_terminal_status(status: &str) -> bool {
    matches!(
        status,
        "completed" | "requires_action" | "incomplete" | "failed" | "cancelled"
    )
}

// The guard is owned by the response stream / polling future. Dropping a client
// connection cancels only this operation's still-running upstream interaction.
struct GeminiBackgroundGuard {
    transport: GeminiBackgroundTransport,
    id: Option<String>,
    terminal: bool,
}

impl Drop for GeminiBackgroundGuard {
    fn drop(&mut self) {
        if self.terminal {
            return;
        }
        let Some(id) = self.id.clone() else {
            return;
        };
        let transport = self.transport.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let Ok(url) = transport.resource_url(&id, true) else { return; };
                match transport.client.post(url).header("x-goog-api-key", &transport.api_key)
                    .timeout(Duration::from_secs(10)).send().await {
                    Ok(response) if response.status().is_success() => {},
                    Ok(response) => warn!(%id, status = %response.status(), "Gemini background cancellation failed"),
                    Err(error) => warn!(%id, %error, "Gemini background cancellation failed"),
                }
            });
        }
    }
}

async fn poll_gemini_background(
    transport: GeminiBackgroundTransport,
    mut body: Value,
) -> Result<Value, String> {
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "Gemini background create returned no interaction ID".to_string())?
        .to_string();
    let mut guard = GeminiBackgroundGuard {
        transport,
        id: Some(id.clone()),
        terminal: false,
    };
    let deadline = Instant::now() + GEMINI_BACKGROUND_LIFETIME;
    let mut failures = 0;
    loop {
        let status = body
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if gemini_terminal_status(status) {
            guard.terminal = true;
            return Ok(body);
        }
        if !matches!(status, "queued" | "in_progress") {
            return Err(format!("Unexpected Gemini background status '{status}'"));
        }
        if Instant::now() >= deadline {
            return Err("Gemini background operation exceeded 24 hours".to_string());
        }
        tokio::time::sleep(guard.transport.poll_interval).await;
        let result = async {
            let response = guard.transport.get(&id, None).await?;
            read_gemini_background_json(response).await
        }
        .await;
        match result {
            Ok(next) => {
                if next.get("id").and_then(Value::as_str) != Some(id.as_str()) {
                    return Err("Gemini background retrieval changed interaction ID".to_string());
                }
                body = next;
                failures = 0;
            }
            Err(error) => {
                failures += 1;
                if failures >= GEMINI_BACKGROUND_RETRIES {
                    return Err(error);
                }
                tokio::time::sleep(Duration::from_millis(250 * (1 << failures))).await;
            }
        }
    }
}

type GeminiByteStream = std::pin::Pin<Box<dyn Stream<Item = Result<Bytes, String>> + Send>>;

struct GeminiResumableStream {
    bytes: GeminiByteStream,
    decoder: SseDataDecoder,
    pending: VecDeque<String>,
    guard: GeminiBackgroundGuard,
    cursor: Option<String>,
    seen: IndexMap<String, ()>,
    uncheckpointed: bool,
    retries: usize,
    deadline: Instant,
    ended: bool,
}

fn resumable_gemini_background_stream(
    upstream: reqwest::Response,
    transport: GeminiBackgroundTransport,
) -> GeminiByteStream {
    let state = GeminiResumableStream {
        bytes: Box::pin(
            upstream
                .bytes_stream()
                .map(|r| r.map_err(|e| e.to_string())),
        ),
        decoder: SseDataDecoder::default(),
        pending: VecDeque::new(),
        guard: GeminiBackgroundGuard {
            transport,
            id: None,
            terminal: false,
        },
        cursor: None,
        seen: IndexMap::new(),
        uncheckpointed: false,
        retries: 0,
        deadline: Instant::now() + GEMINI_BACKGROUND_LIFETIME,
        ended: false,
    };
    Box::pin(stream::unfold(state, |mut state| async move {
        loop {
            if state.ended {
                return None;
            }
            if Instant::now() >= state.deadline {
                state.ended = true;
                return Some((
                    Err("Gemini background operation exceeded 24 hours".to_string()),
                    state,
                ));
            }
            if let Some(payload) = state.pending.pop_front() {
                if payload.trim() == "[DONE]" {
                    if state.guard.terminal {
                        state.ended = true;
                        return Some((Ok(Bytes::from_static(b"data: [DONE]\n\n")), state));
                    }
                    continue;
                }
                let event: Value = match serde_json::from_str(&payload) {
                    Ok(event) => event,
                    Err(error) => {
                        state.ended = true;
                        return Some((Err(error.to_string()), state));
                    }
                };
                if let Some(id) = event
                    .pointer("/interaction/id")
                    .or_else(|| event.get("interaction_id"))
                    .and_then(Value::as_str)
                {
                    if state.guard.id.as_deref().is_some_and(|old| old != id) {
                        state.ended = true;
                        return Some((
                            Err("Gemini stream changed interaction ID".to_string()),
                            state,
                        ));
                    }
                    state.guard.id = Some(id.to_string());
                }
                if let Some(id) = event
                    .get("event_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                {
                    if state.seen.contains_key(id) {
                        continue;
                    }
                    state.seen.insert(id.to_string(), ());
                    if state.seen.len() > 8192 {
                        state.seen.shift_remove_index(0);
                    }
                    state.cursor = Some(id.to_string());
                    state.uncheckpointed = false;
                } else {
                    state.uncheckpointed = true;
                }
                // Only new, parsed events demonstrate progress; replayed checkpoints
                // above must not reset the consecutive reconnect failure budget.
                state.retries = 0;
                let status = event
                    .pointer("/interaction/status")
                    .or_else(|| event.get("status"))
                    .and_then(Value::as_str);
                if status.is_some_and(gemini_terminal_status)
                    || matches!(
                        event.get("event_type").and_then(Value::as_str),
                        Some("interaction.completed" | "interaction.requires_action")
                    )
                {
                    state.guard.terminal = true;
                }
                return Some((Ok(Bytes::from(format!("data: {payload}\n\n"))), state));
            }
            let read =
                tokio::time::timeout(GEMINI_ARGON_REASONING_TIMEOUT, state.bytes.next()).await;
            match read {
                Ok(Some(Ok(bytes))) => match state.decoder.push_bytes(&bytes) {
                    Ok(payloads) => {
                        state.pending.extend(payloads);
                        continue;
                    }
                    Err(error) => {
                        state.ended = true;
                        return Some((Err(error), state));
                    }
                },
                Ok(None) => {
                    match state.decoder.finish() {
                        Ok(payloads) if !payloads.is_empty() => {
                            // EOF may cut an SSE JSON record in half. It was
                            // never emitted and must be replayed from the last
                            // complete checkpoint, not treated as a model error.
                            state.pending.extend(payloads.into_iter().filter(|payload| {
                                payload.trim() == "[DONE]"
                                    || serde_json::from_str::<Value>(payload).is_ok()
                            }));
                            if !state.pending.is_empty() {
                                continue;
                            }
                        }
                        _ => {}
                    }
                    if state.guard.terminal {
                        state.ended = true;
                        return Some((Ok(Bytes::from_static(b"data: [DONE]\n\n")), state));
                    }
                }
                _ => {}
            }
            // Never replay a create or guess a cursor. Partial SSE bytes are
            // discarded; only fully decoded and emitted events are checkpoints.
            if state.uncheckpointed || state.cursor.is_none() || state.guard.id.is_none() {
                state.ended = true;
                return Some((
                    Err(
                        "Gemini background stream interrupted without a safe event checkpoint"
                            .to_string(),
                    ),
                    state,
                ));
            }
            loop {
                state.retries += 1;
                if state.retries > GEMINI_BACKGROUND_RETRIES {
                    state.ended = true;
                    return Some((
                        Err("Gemini background stream reconnect limit exceeded".to_string()),
                        state,
                    ));
                }
                tokio::time::sleep(Duration::from_millis(250 * (1 << (state.retries - 1)))).await;
                match state
                    .guard
                    .transport
                    .get(state.guard.id.as_deref().unwrap(), state.cursor.as_deref())
                    .await
                {
                    Ok(response) => {
                        state.bytes = Box::pin(
                            response
                                .bytes_stream()
                                .map(|r| r.map_err(|e| e.to_string())),
                        );
                        state.decoder = SseDataDecoder::default();
                        break;
                    }
                    Err(_) => continue,
                }
            }
        }
    }))
}

fn apply_gemini_4_context_budget(
    profile: &ProviderProfile,
    input_tokens: u64,
    body: &mut Value,
) -> Result<Option<String>, String> {
    let window = profile.context_window.unwrap_or(1_048_576).min(1_048_576);
    let remaining = window.saturating_sub(input_tokens);
    if remaining == 0 {
        return Err(format!(
            "Gemini 4 context exhausted: input {input_tokens}, unified limit {window}"
        ));
    }
    let requested = body
        .pointer("/generation_config/max_output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(65_536);
    if requested == 0 {
        return Err("Gemini 4 max_tokens must be greater than zero".to_string());
    }
    let effective = requested.min(remaining);
    body["generation_config"]["max_output_tokens"] = json!(effective);
    if effective > 65_536 {
        body["background"] = json!(true);
    } else {
        body.as_object_mut().unwrap().remove("background");
    }
    Ok((effective < requested).then(|| format!("Reduced Gemini 4 output budget from {requested} to {effective} to reserve space for {input_tokens} input tokens in the unified {window} context")))
}

async fn prepare_gemini_4_context_budget(
    profile: &ProviderProfile,
    request: &Value,
    body: &mut Value,
    diagnostics: &mut Vec<String>,
) -> Result<(), String> {
    if !is_gemini_4_model(&profile.model) {
        return Ok(());
    }
    // Include full visible history even when the wire request uses a previous ID.
    // Counts cannot reconstruct hidden server thoughts; the server remains the
    // authority on exact stored context. Use native counting near the boundary.
    let mut input = estimate_anthropic_input_tokens(request) as u64;
    let output = body
        .pointer("/generation_config/max_output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(65_536);
    let window = profile.context_window.unwrap_or(1_048_576).min(1_048_576);
    if input.saturating_add(output) > window * 3 / 4 {
        let count = async {
            let response = profile
                .client
                .post(gemini_count_tokens_url(profile)?)
                .header(
                    "x-goog-api-key",
                    profile.api_key.as_deref().unwrap_or_default(),
                )
                .json(&gemini_count_tokens_request(request, profile)?)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if !response.status().is_success() {
                return Err("Native Gemini context count failed".to_string());
            }
            let value = read_response_json_limited(response)
                .await
                .map_err(|e| e.to_string())?;
            value
                .get("totalTokens")
                .or_else(|| value.get("total_tokens"))
                .and_then(Value::as_u64)
                .ok_or_else(|| "Native Gemini context count missing totalTokens".to_string())
        };
        input = tokio::time::timeout(GEMINI_COUNT_TOKENS_TIMEOUT, count)
            .await
            .map_err(|_| {
                "Native Gemini context count timed out; cannot safely allocate near-limit output"
                    .to_string()
            })??;
    } else {
        // Avoid a count round trip on ordinary short turns. Never present this as
        // an exact tokenizer result, especially for images and stored thoughts.
        diagnostics.push("Gemini 4 context budget uses a visible-history estimate; near-limit requests use native countTokens, and upstream validates hidden stored context".to_string());
    }
    if let Some(message) = apply_gemini_4_context_budget(profile, input, body)? {
        diagnostics.push(message);
    }
    Ok(())
}
