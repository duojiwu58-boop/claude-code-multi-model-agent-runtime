// Background jobs own their Codex stream independently of HTTP subscribers.
struct ApiBackgroundData {
    response: Value,
    events: Vec<Value>,
    bytes: usize,
    terminal_at: Option<Instant>,
}
struct ApiBackgroundJob {
    state: Arc<AppState>,
    record: Value,
    persist: bool,
    conversation: Option<String>,
    stream_enabled: bool,
    data: std::sync::Mutex<ApiBackgroundData>,
    cancel: tokio::sync::watch::Sender<bool>,
    changed: tokio::sync::watch::Sender<u64>,
}
fn api_background_jobs() -> &'static std::sync::Mutex<HashMap<String, Arc<ApiBackgroundJob>>> {
    static JOBS: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Arc<ApiBackgroundJob>>>> =
        std::sync::OnceLock::new();
    JOBS.get_or_init(Default::default)
}
fn api_background_key(state: &AppState, id: &str) -> String {
    format!("{}:{id}", api_store_path(state).display())
}
fn api_background_find(state: &AppState, id: &str) -> Option<Arc<ApiBackgroundJob>> {
    let mut jobs = api_background_jobs().lock().ok()?;
    jobs.retain(|_, job| {
        job.data.lock().ok().is_some_and(|d| {
            d.terminal_at
                .is_none_or(|t| t.elapsed() < Duration::from_secs(600))
        })
    });
    jobs.get(&api_background_key(state, id)).cloned()
}
fn api_background_persist(job: &ApiBackgroundJob, response: &Value) -> Result<(), String> {
    let completed = matches!(response["status"].as_str(), Some("completed" | "incomplete"));
    if !job.persist && !(completed && job.conversation.is_some()) {
        return Ok(());
    }
    api_store_transaction(&job.state, true, |store| {
        let mut record = job.record.clone();
        record["response"] = response.clone();
        if completed {
            if let Some(id) = &job.conversation {
                let conversation = store["conversations"].get_mut(id)
                    .ok_or("Conversation changed during generation")?;
                let mut items = job.record["input"].as_array().ok_or("Invalid background input")?.clone();
                items.extend(response["output"].as_array().ok_or("Invalid background output")?.clone());
                conversation["items"] = json!(items);
                conversation["profile"] = job.record["profile"].clone();
            }
        }
        if job.persist {
            store["responses"][response["id"].as_str().ok_or("Missing response ID")?] = record;
        }
        Ok(())
    })
}
fn api_background_push(job: &ApiBackgroundJob, mut event: Value) -> Result<(), String> {
    let mut data = job.data.lock().map_err(|_| "Background job lock failed")?;
    if data.terminal_at.is_some() { return Ok(()); }
    let bytes = event.to_string().len();
    if data.bytes + bytes > MAX_UPSTREAM_SSE_BUFFER_BYTES {
        return Err("Background event replay exceeds the local 8 MiB limit".into());
    }
    event["sequence_number"] = json!(data.events.len());
    if let Some(response) = event.get("response") {
        let mut response = response.clone();
        response["background"] = json!(true);
        event["response"] = response.clone();
        if !matches!(
            response["status"].as_str(),
            Some("queued" | "in_progress")
        ) {
            api_background_persist(job, &response)?;
            data.terminal_at = Some(Instant::now());
        }
        data.response = response;
    }
    data.bytes += bytes;
    data.events.push(event);
    job.changed.send_replace(data.events.len() as u64);
    Ok(())
}
fn api_background_end(job: &ApiBackgroundJob, status: &str, error: Option<&str>) {
    if let Ok(mut data) = job.data.lock() {
        if data.terminal_at.is_some() {
            return;
        }
        data.response["status"] = json!(status);
        data.response["completed_at"] = Value::Null;
        data.response["usage"] = Value::Null;
        data.response["error"] = error
            .map(|s| json!({"code":"server_error","message":s}))
            .unwrap_or(Value::Null);
        if let Err(e) = api_background_persist(job, &data.response) {
            data.response["status"] = json!("failed");
            data.response["error"] = json!({"code":"storage_error","message":e});
        }
        let event = json!({"type":format!("response.{}",data.response["status"].as_str().unwrap_or("failed")),"sequence_number":data.events.len(),"response":data.response});
        data.events.push(event);
        data.terminal_at = Some(Instant::now());
        job.changed.send_replace(data.events.len() as u64);
    }
}
async fn api_background_start(
    profile: ProviderProfile,
    mut native: Value,
    mut context: ApiContext,
    diagnostics: Vec<String>,
) -> Response {
    let stream_enabled = context.request["stream"] == true;
    let mut response = api_response_object(
        &context.response_id,
        context.created,
        &context.request,
        &json!({"model":profile.model,"content":[],"usage":{"bridge_usage_complete":false}}),
    );
    response["status"] = json!("queued");
    response["background"] = json!(true);
    response["completed_at"] = Value::Null;
    response["usage"] = Value::Null;
    let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
    let (changed, _) = tokio::sync::watch::channel(0);
    let job = Arc::new(ApiBackgroundJob {
        state: context.state.clone(),
        record: json!({"input":context.input,"profile":context.profile_key,"session":context.session}),
        persist: context.request["store"] != false,
        conversation: context.conversation.clone(),
        stream_enabled,
        data: std::sync::Mutex::new(ApiBackgroundData {
            response: response.clone(),
            events: Vec::new(),
            bytes: 0,
            terminal_at: None,
        }),
        cancel,
        changed,
    });
    let key = api_background_key(&context.state, &context.response_id);
    {
        let mut jobs = match api_background_jobs().lock() {
            Ok(j) => j,
            Err(_) => {
                return api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Background registry lock failed",
                )
            }
        };
        jobs.retain(|_, j| {
            j.data.lock().ok().is_some_and(|d| {
                d.terminal_at
                    .is_none_or(|t| t.elapsed() < Duration::from_secs(600))
            })
        });
        if jobs.len() >= 128
            || jobs
                .values()
                .filter(|j| j.data.lock().ok().is_some_and(|d| d.terminal_at.is_none()))
                .count()
                >= 16
        {
            return api_error(StatusCode::TOO_MANY_REQUESTS,"Background capacity reached (16 active, 128 retained); retry later or delete completed responses");
        }
        if let Err(e) = api_background_persist(&job, &response) {
            return api_resource_error(e);
        }
        jobs.insert(key, job.clone());
    }
    // Every job uses the genuine incremental backend; subscribers only observe it.
    native["stream"] = json!(true);
    context.request["stream"] = json!(true);
    let worker = job.clone();
    let mut shutdown = context.state.shutdown_tx.subscribe();
    let conversation_lease = context._lease.take();
    tokio::spawn(async move {
        // Keep the conversation reserved until the worker commits a terminal state.
        let _conversation_lease = conversation_lease;
        let response = api_codex_execute(profile, native, context, None, diagnostics).await;
        if !response.status().is_success() {
            let bytes = axum::body::to_bytes(response.into_body(), MAX_UPSTREAM_RESPONSE_BYTES)
                .await
                .unwrap_or_default();
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            api_background_end(
                &worker,
                "failed",
                Some(
                    body["error"]["message"]
                        .as_str()
                        .unwrap_or("Background execution failed"),
                ),
            );
            return;
        }
        let mut upstream = response.into_body().into_data_stream();
        let mut decoder = SseDataDecoder::default();
        loop {
            let next = tokio::select! {
                biased;
                _ = cancelled.changed() => { drop(upstream); api_background_end(&worker,"cancelled",None); return; }
                _ = shutdown.changed() => { drop(upstream); api_background_end(&worker,"failed",Some("Bridge is shutting down")); return; }
                next = upstream.next() => next,
            };
            let result = match next {
                Some(Ok(bytes)) => decoder.push_bytes(&bytes),
                Some(Err(_)) => Err("Background transport failed".into()),
                None => {
                    api_background_end(
                        &worker,
                        "failed",
                        Some("Background stream ended without a terminal event"),
                    );
                    return;
                }
            };
            let result = result.and_then(|payloads| {
                for payload in payloads {
                    let event: Value = serde_json::from_str(&payload)
                        .map_err(|_| "Invalid background SSE payload")?;
                    api_background_push(&worker, event)?;
                }
                Ok(())
            });
            if let Err(e) = result {
                drop(upstream);
                api_background_end(&worker, "failed", Some(&e));
                return;
            }
            if worker.data.lock().is_ok_and(|d| d.terminal_at.is_some()) {
                return;
            }
        }
    });
    if stream_enabled {
        api_background_stream(job, None)
    } else {
        Json(response).into_response()
    }
}
fn api_background_stream(job: Arc<ApiBackgroundJob>, after: Option<u64>) -> Response {
    let changed = job.changed.subscribe();
    let output = stream::unfold(
        (
            job,
            changed,
            after.map_or(0, |n| n.saturating_add(1) as usize),
        ),
        |(job, mut changed, mut index)| async move {
            loop {
                let (event, terminal) = {
                    let data = job.data.lock().ok()?;
                    (data.events.get(index).cloned(), data.terminal_at.is_some())
                };
                if let Some(event) = event {
                    index += 1;
                    let out = Event::default()
                        .event(event["type"].as_str().unwrap_or("error"))
                        .data(event.to_string());
                    return Some((Ok::<Event, Infallible>(out), (job, changed, index)));
                }
                if terminal || changed.changed().await.is_err() {
                    return None;
                }
            }
        },
    );
    Sse::new(output)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}
async fn api_response_retrieve(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    if q.get("stream").is_some_and(|s| s != "true" && s != "false") {
        return api_error(StatusCode::BAD_REQUEST, "stream must be true or false");
    }
    if q.get("stream").is_some_and(|s| s == "true") {
        let after = match q
            .get("starting_after")
            .map(|s| s.parse::<u64>())
            .transpose()
        {
            Ok(v) => v,
            Err(_) => {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "starting_after must be a nonnegative integer",
                )
            }
        };
        return match api_background_find(&state,&id) {
            Some(job) if job.stream_enabled => api_background_stream(job,after),
            _ => api_error(StatusCode::BAD_REQUEST,"Stream replay requires a background response created with stream=true and retained in this process (10 minutes after completion)"),
        };
    }
    api_response_get(State(state), axum::extract::Path(id)).await
}
async fn api_response_cancel(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if let Some(job) = api_background_find(&state, &id) {
        let mut changed = job.changed.subscribe();
        job.cancel.send_replace(true);
        loop {
            if let Ok(data) = job.data.lock() {
                if data.terminal_at.is_some() {
                    return Json(data.response.clone()).into_response();
                }
            }
            if changed.changed().await.is_err() {
                return api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Background worker ended unexpectedly",
                );
            }
        }
    }
    match api_store_transaction(&state, false, |s| {
        s["responses"]
            .get(&id)
            .map(|r| r["response"].clone())
            .ok_or_else(|| format!("Not found: response '{id}'"))
    }) {
        Ok(v) if v["background"] == true => Json(v).into_response(),
        Ok(_) => api_error(
            StatusCode::BAD_REQUEST,
            "Only background responses can be cancelled",
        ),
        Err(e) => api_resource_error(e),
    }
}
fn api_background_recover(state: &AppState) -> Result<(), String> {
    if !api_store_path(state).exists() {
        return Ok(());
    }
    api_store_transaction(state, true, |s| {
        for record in s["responses"].as_object_mut().unwrap().values_mut() {
            let r = &mut record["response"];
            if r["background"] == true
                && matches!(r["status"].as_str(), Some("queued" | "in_progress"))
            {
                r["status"] = json!("failed");
                r["error"] = json!({"code":"bridge_restarted","message":"Bridge restarted before this background response completed; submit a new request"});
                r["usage"] = Value::Null;
            }
        }
        Ok(())
    })
}
