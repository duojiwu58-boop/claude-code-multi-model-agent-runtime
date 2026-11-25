#[path = "ls_host_protocol.rs"]
mod ls_host_protocol;
#[path = "ls_input_filter.rs"]
mod ls_input_filter;
include!("ls_lifecycle.rs");

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
struct LsLaunchConfig {
    install_root: PathBuf,
    data_root: PathBuf,
}

#[derive(Clone)]
struct LsProviderConfig {
    launch: Option<LsLaunchConfig>,
    host_url: String,
    token_file: PathBuf,
    workspace: String,
    permission_mode: String,
    timeout_seconds: u64,
    claude_projects_dir: Option<PathBuf>,
}

fn parse_ls_provider_config(
    object: &Map<String, Value>,
    name: &str,
) -> Result<LsProviderConfig, String> {
    let config = object
        .get("ls")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{name}: missing ls configuration"))?;
    let required = |key: &str| {
        config
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| format!("{name}: ls.{key} is required"))
    };
    let host_url = required("host_url")?;
    let url = url::Url::parse(&host_url).map_err(|_| format!("{name}: invalid LS Host URL"))?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port().is_none()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(format!(
            "{name}: LS Host must be http://127.0.0.1:<port> without credentials or path"
        ));
    }
    let token_file = PathBuf::from(required("token_file")?);
    let workspace = required("workspace")?;
    if !token_file.is_absolute()
        || !Path::new(&workspace).is_absolute()
        || !Path::new(&workspace).is_dir()
    {
        return Err(format!(
            "{name}: absolute token_file and existing absolute workspace required"
        ));
    }
    let permission_mode = config
        .get("permission_mode")
        .and_then(Value::as_str)
        .unwrap_or("turbo")
        .to_owned();
    if permission_mode != "turbo" {
        return Err("LS permission_mode is fixed to turbo".into());
    }
    let timeout_seconds = config
        .get("timeout_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(600);
    if !(5..=3600).contains(&timeout_seconds) {
        return Err("LS timeout_seconds must be 5..3600".into());
    }
    let claude_projects_dir = config.get("claude_projects_dir").map(|v| {
        let path = PathBuf::from(v.as_str().ok_or("ls.claude_projects_dir must be a path string")?);
        if !path.is_absolute() || !path.is_dir() { return Err("ls.claude_projects_dir must be an existing absolute directory"); }
        Ok(path)
    }).transpose()?;
    let launch: Option<LsLaunchConfig> = config.get("launch")
        .map(|v| serde_json::from_value(v.clone()).map_err(|e| format!("Invalid ls.launch: {e}"))).transpose()?;
    if let Some(launch) = &launch {
        if !launch.install_root.is_absolute() || !launch.data_root.is_absolute() {
            return Err("ls.launch install_root and data_root must be absolute".into());
        }
        if config.get("launch").and_then(|v| v.get("proxy_url")).is_some() {
            return Err("Remove ls.launch.proxy_url: LS uses the interactive user's environment variables".into());
        }
    }
    Ok(LsProviderConfig {
        launch,
        host_url: host_url.trim_end_matches('/').to_owned(),
        token_file,
        workspace,
        permission_mode,
        timeout_seconds,
        claude_projects_dir,
    })
}

#[cfg(test)]
fn ls_turn_request(
    profile: &ProviderProfile,
    request: &Value,
) -> Result<ls_host_protocol::TurnRequest, String> {
    ls_turn_request_with_originals(profile, request, None)
}

fn ls_turn_request_with_originals(
    profile: &ProviderProfile,
    request: &Value,
    originals: Option<&[Value]>,
) -> Result<ls_host_protocol::TurnRequest, String> {
    let config = profile
        .ls
        .as_ref()
        .ok_or("Missing LS profile configuration")?;
    for key in [
        "temperature",
        "top_p",
        "top_k",
        "stop_sequences",
    ] {
        if request.get(key).is_some() {
            return Err(format!(
                "LS does not support '{key}'; no prompt emulation is performed"
            ));
        }
    }
    // Claude Code sends effort/thinking controls even for ordinary text turns.
    // LS reasoning is selected by the GUI's native model ID, not these fields.
    if let Some(output) = request.get("output_config") {
        let Some(output) = output.as_object() else { return Err("output_config must be an object".into()); };
        if output.keys().any(|key| key != "effort") {
            return Err("LS does not support structured output_config; only effort is overridden by the selected LS model".into());
        }
        if output.get("effort").is_some_and(|v| !v.as_str().is_some_and(is_reasoning_effort)) {
            return Err("Invalid output_config.effort".into());
        }
    }
    if let Some(thinking) = request.get("thinking") {
        let Some(thinking) = thinking.as_object() else { return Err("thinking must be an object".into()); };
        if !thinking.get("type").and_then(Value::as_str).is_some_and(|t| matches!(t, "enabled" | "disabled" | "adaptive"))
            || thinking.keys().any(|k| !matches!(k.as_str(), "type" | "budget_tokens" | "display"))
            || thinking.get("budget_tokens").is_some_and(|v| v.as_u64().is_none()) {
            return Err("Unsupported thinking control".into());
        }
    }
    if let Some(choice) = request.get("tool_choice") {
        if choice.get("type").and_then(Value::as_str) != Some("auto") {
            return Err("LS owns tools; forced client tool_choice is unsupported".into());
        }
    }
    let filtered = ls_input_filter::filter(request, originals)?;
    let messages = filtered.messages;
    let payload = ls_host_protocol::user_payload(&messages.last().unwrap()["content"])?;
    let text_items = payload.text_items;
    let text = text_items.concat();
    let history = ls_host_protocol::normalize_history(&messages[..messages.len() - 1])?;
    let workspace = filtered.workspace.unwrap_or_else(|| config.workspace.clone());
    if !Path::new(&workspace).is_absolute() || !Path::new(&workspace).is_dir() {
        return Err("Claude current working directory must be an existing absolute directory".into());
    }
    let session = match request.pointer("/metadata/session_id") {
        Some(value) => value
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 200)
            .ok_or("metadata.session_id must be a nonempty string of at most 200 bytes")?
            .to_owned(),
        None => codex_session_key(request).unwrap_or_else(|| Uuid::new_v4().to_string()),
    };
    Ok(ls_host_protocol::TurnRequest {
        session,
        profile: profile.file_name.clone(),
        model: profile.model.clone(),
        workspace,
        permission_mode: config.permission_mode.clone(),
        timeout_seconds: config.timeout_seconds,
        history,
        text,
        text_items,
        media: payload.media,
        user_content: payload.content,
        stream: request
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

async fn ls_host_post(
    config: &LsProviderConfig,
    path: &str,
    body: &Value,
) -> Result<reqwest::Response, String> {
    let token =
        fs::read_to_string(&config.token_file).map_err(|_| "Cannot read LS Host token file")?;
    if token.trim().len() < 32 {
        return Err("LS Host token is invalid".into());
    }
    let client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(5))
        .build()
        .map_err(|_| "Cannot build LS Host client")?;
    client
        .post(format!("{}{path}", config.host_url))
        .bearer_auth(token.trim())
        .json(body)
        .send()
        .await
        .map_err(|_| "LS Host transport failed; outcome unknown, request was not retried".into())
}

async fn forward_ls_profile(state: &AppState, profile: ProviderProfile, request: Value, _headers: &HeaderMap) -> Response {
    let lease = match state.ls_lifecycle.clone().try_read_owned() {
        Ok(lease) => lease,
        Err(_) => return anthropic_error(StatusCode::SERVICE_UNAVAILABLE, "api_error", "LS is starting or switching; retry after it is ready"),
    };
    if active_provider_profile(state).as_ref().map(|p| &p.file_name) != Some(&profile.file_name) || lease.child.is_none() {
        return anthropic_error(StatusCode::CONFLICT, "api_error", "Select the LS provider and wait for its Host to start before sending a task");
    }
    let originals = match ls_recorded_originals(profile.ls.as_ref().unwrap(), &request).await {
        Ok(rows) => rows,
        Err(e) => return anthropic_error(StatusCode::CONFLICT, "invalid_request_error", &e),
    };
    let turn = match ls_turn_request_with_originals(&profile, &request, originals.as_deref()) {
        Ok(t) => t,
        Err(e) => return anthropic_error(StatusCode::BAD_REQUEST, "invalid_request_error", &e),
    };
    let response = match ls_host_post(
        profile.ls.as_ref().unwrap(),
        "/turn",
        &serde_json::to_value(turn).unwrap(),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return anthropic_error(StatusCode::BAD_GATEWAY, "api_error", &e),
    };
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .cloned()
        .unwrap_or(HeaderValue::from_static("application/json"));
    let usage_status = response.headers().get("x-ls-usage-status").cloned();
    // Hold the lease through the entire response, including streaming bodies.
    let stream = futures_util::stream::unfold((response.bytes_stream(), lease), |(mut stream, lease)| async move {
        use futures_util::StreamExt;
        stream.next().await.map(|item| (item, (stream, lease)))
    });
    let mut result = (status, Body::from_stream(stream)).into_response();
    result.headers_mut().insert("content-type", content_type);
    if let Some(status) = usage_status {
        result.headers_mut().insert("x-ls-usage-status", status);
    }
    attach_bridge_diagnostics(result, &profile.file_name, &["LS native input filter active. Client system/tools and generated context are removed; user text and supported attachments are preserved. Inline PDFs use native LS artifacts (10 MiB per PDF). max_tokens is not enforced; see x-ls-usage-status.".into()])
}

async fn ls_recorded_originals(config: &LsProviderConfig, request: &Value) -> Result<Option<Vec<Value>>, String> {
    let Some(root) = config.claude_projects_dir.clone() else { return Ok(None) };
    let metadata = request.pointer("/metadata/user_id").and_then(Value::as_str)
        .and_then(|s| serde_json::from_str::<Value>(s).ok()).ok_or("Claude session metadata required for original-source verification")?;
    let session = metadata["session_id"].as_str().filter(|s| Uuid::parse_str(s).is_ok())
        .ok_or("Invalid Claude session ID")?.to_owned();
    let count = request["messages"].as_array().ok_or("messages must be an array")?.iter().filter(|m| m["role"] == "user").count();
    // Transcript writes are asynchronous. Bounded waiting is outside Tokio's
    // blocking workers; roots come only from trusted provider configuration.
    for _ in 0..20 {
        let root = root.clone();
        let session = session.clone();
        let rows = tokio::task::spawn_blocking(move || -> Result<Option<Vec<Value>>, String> {
            let mut found = None;
            for entry in fs::read_dir(&root).map_err(|_| "Cannot read configured Claude transcript directory")? {
                let entry = entry.map_err(|_| "Cannot enumerate Claude transcript directory")?;
                if !entry.file_type().map_err(|_| "Cannot inspect transcript directory")?.is_dir() { continue; }
                let path = entry.path().join(format!("{session}.jsonl"));
                if !path.is_file() { continue; }
                let canonical = fs::canonicalize(&path).map_err(|_| "Cannot resolve transcript path")?;
                let canonical_root = fs::canonicalize(&root).map_err(|_| "Cannot resolve transcript root")?;
                if !canonical.starts_with(&canonical_root) { return Err("Transcript path escaped configured root".into()); }
                if fs::metadata(&path).map_err(|_| "Cannot read transcript metadata")?.len() > 128 * 1024 * 1024 {
                    return Err("Claude transcript exceeds verification size limit".into());
                }
                if found.is_some() { return Err("Ambiguous Claude session transcript".into()); }
                let text = fs::read_to_string(path).map_err(|_| "Cannot read Claude session transcript")?;
                let mut rows = vec![];
                for line in text.lines() {
                    let Ok(row) = serde_json::from_str::<Value>(line) else { continue };
                    if row["type"] == "user" && row["isSidechain"] != true { rows.push(row); }
                }
                found = Some(rows);
            }
            Ok(found)
        }).await.map_err(|_| "Transcript reader failed")??;
        if let Some(rows) = rows {
            if rows.len() == count { return Ok(Some(rows)); }
            if rows.len() > count { return Err("Claude transcript branch/compaction differs from incoming original history".into()); }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err("Latest original Claude input has not reached its transcript; no task was sent".into())
}

async fn admin_ls(
    State(state): State<Arc<AppState>>,
    uri: Uri,
    Json(mut body): Json<Value>,
) -> Response {
    let _lease = match state.ls_lifecycle.try_read() {
        Ok(lease) => lease,
        Err(_) => return anthropic_error(StatusCode::CONFLICT, "api_error", "LS is starting or switching"),
    };
    let profile = if let Some(name) = body.get("profile") {
        name.as_str().and_then(|name| {
            state
                .routing
                .read()
                .ok()?
                .profiles
                .iter()
                .find(|p| p.file_name == name)
                .cloned()
        })
    } else {
        active_provider_profile(&state)
    };
    let Some(profile) = profile.filter(|p| p.transport == ProviderTransport::AntigravityLs) else {
        return anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Select an LS profile",
        );
    };
    if let Some(object) = body.as_object_mut() {
        object.remove("profile");
    }
    let path = match uri.path() {
        "/admin/ls/interactions" => "/interactions",
        "/admin/ls/respond" => "/respond",
        "/admin/ls/cancel" => "/cancel",
        "/admin/ls/models" => "/models",
        "/admin/ls/resume" => "/resume",
        _ => "/health",
    };
    match ls_host_post(profile.ls.as_ref().unwrap(), path, &body).await {
        Ok(response) => {
            let status = response.status();
            match read_response_json_limited(response).await {
                Ok(v) => (status, Json(v)).into_response(),
                Err(_) => anthropic_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    "Invalid Host response",
                ),
            }
        }
        Err(e) => anthropic_error(StatusCode::BAD_GATEWAY, "api_error", &e),
    }
}

// Mirror AntigravityCN/web/src/model-controls.js: only explicit catalog variants
// become reasoning choices. Never invent a model ID or emulate effort in text.
fn ls_model_groups(catalog: &Value) -> Vec<Value> {
    let mut groups: Vec<Value> = Vec::new();
    for model in catalog["models"].as_array().into_iter().flatten() {
        let Some(id) = model["id"].as_str().filter(|s| !s.is_empty()) else { continue };
        let label = model["label"].as_str().filter(|s| !s.is_empty()).unwrap_or(id);
        let levels = ["xhigh", "high", "medium", "low", "minimal"];
        let suffix = id.rsplit_once('-').filter(|(_, level)| levels.contains(&level.to_ascii_lowercase().as_str()));
        let label_variant = label.rsplit_once(" (").and_then(|(base, tail)| {
            let level = tail.strip_suffix(')')?;
            if levels.contains(&level.to_ascii_lowercase().as_str()) { Some((base, level)) } else {
                // Optional final annotation: "Gemini (High) (preview)".
                base.rsplit_once(" (").and_then(|(base, tail)| {
                    let level = tail.strip_suffix(')')?;
                    levels.contains(&level.to_ascii_lowercase().as_str()).then_some((base, level))
                })
            }
        });
        let level = label_variant.map(|v| v.1).or(suffix.map(|v| v.1)).unwrap_or("默认").to_ascii_lowercase();
        let key = suffix.map(|v| v.0).or(label_variant.map(|v| v.0)).unwrap_or(id);
        let name = label_variant.map(|v| v.0).unwrap_or_else(|| {
            if suffix.is_some() && label.eq_ignore_ascii_case(id) { key } else { label }
        });
        let index = groups.iter().position(|g| g["key"] == key).unwrap_or_else(|| {
            groups.push(json!({"key":key,"label":name,"variants":[]}));
            groups.len() - 1
        });
        groups[index]["variants"].as_array_mut().unwrap().push(json!({"id":id,"level":level}));
    }
    groups
}

fn apply_ls_model_preferences(profiles: &mut [ProviderProfile], state_path: &Path) {
    let state = read_state_object(state_path);
    let Some(models) = state.get("ls_models").and_then(Value::as_object) else { return };
    for profile in profiles.iter_mut().filter(|p| p.transport == ProviderTransport::AntigravityLs) {
        if let Some(model) = models.get(&profile.file_name).and_then(Value::as_str).filter(|m| !m.is_empty()) {
            profile.model = model.to_owned();
        }
    }
}

async fn admin_set_ls_model(State(state): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    let _transition = state.admin_state_lock.lock().await;
    let Some(profile) = active_provider_profile(&state).filter(|p| p.transport == ProviderTransport::AntigravityLs) else {
        return anthropic_error(StatusCode::BAD_REQUEST, "invalid_request_error", "Select an LS profile");
    };
    // A delayed GUI action must not edit a newly selected profile.
    if body["profile"] != profile.file_name || body["previous_model"] != profile.model {
        return anthropic_error(StatusCode::CONFLICT, "invalid_request_error", "LS selection changed; refresh controls");
    }
    let Some(model) = body["model"].as_str() else {
        return anthropic_error(StatusCode::BAD_REQUEST, "invalid_request_error", "Missing model");
    };
    let outcome = tokio::time::timeout(Duration::from_secs(5), async {
        let response = ls_host_post(profile.ls.as_ref().unwrap(), "/models", &json!({})).await?;
        if !response.status().is_success() { return Err("LS Host unavailable".to_string()); }
        read_response_json_limited(response).await.map_err(|_| "Invalid model catalog".to_string())
    }).await;
    let catalog = match outcome {
        Ok(Ok(v)) => v,
        _ => return anthropic_error(StatusCode::BAD_GATEWAY, "api_error", "Cannot validate LS model catalog"),
    };
    if !catalog["models"]["models"].as_array().is_some_and(|models| models.iter().any(|m| m["id"] == model)) {
        return anthropic_error(StatusCode::BAD_REQUEST, "invalid_request_error", "Unknown LS model ID");
    }
    if profile.source != ProviderProfileSource::Native || !valid_reasoning_effort_profile_key(&profile.file_name) {
        return anthropic_error(StatusCode::BAD_REQUEST, "invalid_request_error", "LS controls require a native profile");
    }
    // The virtual service account can persist bridge state, but native provider
    // files are intentionally read-only. Do not broaden provider-directory ACLs.
    let saved = (|| -> Result<(), String> {
        let mut value = read_state_object(&state.bridge_state_path);
        let models = value.entry("ls_models".to_string()).or_insert_with(|| json!({}));
        let models = models.as_object_mut().ok_or("Invalid persisted LS model preferences")?;
        models.insert(profile.file_name.clone(), json!(model));
        write_state_atomically(&state.bridge_state_path, &serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?)
    })();
    if let Err(e) = saved { return anthropic_error(StatusCode::CONFLICT, "api_error", &e); }
    let Ok(mut routing) = state.routing.write() else {
        return anthropic_error(StatusCode::INTERNAL_SERVER_ERROR, "api_error", "Model saved; reload required");
    };
    if let Some(p) = routing.profiles.iter_mut().find(|p| p.file_name == profile.file_name) { p.model = model.into(); }
    Json(json!({"status":"ok","model":model,"mode":"turbo","auto_approve":true})).into_response()
}
