// `upstream_identity` is a free-form display label, so strip userinfo only
// when the value actually is a URL: non-URL labels must stay untouched.
fn redact_url_credentials_if_url(value: &str) -> String {
    let Ok(mut url) = url::Url::parse(value) else {
        return value.to_string();
    };
    if !url.username().is_empty() || url.password().is_some() {
        let _ = url.set_username("");
        let _ = url.set_password(None);
        url.to_string()
    } else {
        value.to_string()
    }
}

// Profile connectivity probe. The bound and staleness policy is deliberately
// small: a probe sends a one-token non-streaming request and reports the
// round-trip result, so 30 seconds is far beyond any healthy upstream answer.
const PROFILE_PROBE_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
const PROFILE_PROBE_BODY_LIMIT: usize = 1024 * 1024;
const PROFILE_PROBE_MAX_TOKENS: u64 = 1;
const PROFILE_PROBE_RESPONSE_CHARS: usize = 600;

/// Minimal Anthropic-style request shared by every probe transport. Thinking
/// is disabled so the probe stays neutral and cheap: DeepSeek/Qwen reasoning
/// policies and Gemini thinking levels relax to their cheapest form when the
/// client explicitly disables thinking, and the request stays valid for every
/// upstream.
fn profile_probe_request(model: &str) -> Value {
    json!({
        "model": model,
        "max_tokens": PROFILE_PROBE_MAX_TOKENS,
        "stream": false,
        "thinking": {"type": "disabled"},
        "messages": [{"role": "user", "content": "ping"}]
    })
}

/// Scan free-form text for `scheme://` URL fragments and remove the userinfo
/// from the authority portion, so probe error text can never echo embedded
/// credentials from a credential-bearing URL (proxy, upstream, or base URL
/// with user:pass). Works for any scheme (http, https, socks5, ...) without
/// relying on the URL parser accepting the scheme.
fn redact_url_credentials_in_text(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(scheme_end) = rest.find("://") {
        // Only treat markers as URLs when a plausible scheme precedes them
        // directly: [a-z][a-z0-9+-.]*.
        let before = &rest[..scheme_end];
        let start = before
            .rfind(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '+' || ch == '-' || ch == '.'))
            .map_or(0, |index| index + 1);
        let scheme = &before[start..];
        let plausible = !scheme.is_empty()
            && scheme.chars().next().unwrap().is_ascii_alphabetic()
            && scheme
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '+' || ch == '-' || ch == '.');
        if !plausible {
            // Not a URL: keep the marker verbatim and continue past it.
            let marker_end = scheme_end + 3;
            result.push_str(&rest[..marker_end]);
            rest = &rest[marker_end..];
            continue;
        }
        let authority_start = scheme_end + 3;
        let authority_end = rest[authority_start..]
            .find(|ch: char| {
                ch.is_whitespace()
                    || ch.is_ascii_control()
                    || matches!(ch, '/' | '?' | '#' | '"' | '\'' | ',' | ')' | ']')
            })
            .map_or(rest.len(), |offset| authority_start + offset);
        let authority = &rest[authority_start..authority_end];
        let redacted_authority = authority
            .rfind('@')
            .map(|at| &authority[at + 1..])
            .unwrap_or(authority);
        result.push_str(&rest[..authority_start]);
        result.push_str(redacted_authority);
        rest = &rest[authority_end..];
    }
    result.push_str(rest);
    result
}

fn probe_safe_error_message(message: &str, sensitive_values: &[String]) -> String {
    let mut redacted = redact_url_credentials_in_text(message);
    for sensitive in sensitive_values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        redacted = redacted.replace(sensitive, "[REDACTED]");
    }
    let mut chars = redacted.chars();
    let truncated = chars
        .by_ref()
        .take(PROFILE_PROBE_RESPONSE_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn probe_error_stage(status: u16, error_type: Option<&str>) -> &'static str {
    match error_type.unwrap_or("") {
        "authentication_error" | "permission_error" => return "auth",
        "invalid_request_error" | "not_found_error" => return "request",
        _ => {}
    }
    // Fall back to the preserved upstream status when the error type is
    // generic (api_error covers both upstream failures and bridge-side
    // configuration gaps such as a missing fallback key).
    match status {
        401 | 403 => "auth",
        400 | 404 => "request",
        _ => "upstream",
    }
}

fn probe_error_details<'a>(
    body: &'a Value,
    sensitive_values: &[String],
) -> (Option<&'a str>, String) {
    let object = body.get("error").and_then(Value::as_object);
    let error_type = object
        .and_then(|object| object.get("type"))
        .and_then(Value::as_str);
    let message = object
        .and_then(|object| object.get("message"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| "Profile probe failed without a structured error message".to_string());
    (
        error_type,
        probe_safe_error_message(&message, sensitive_values),
    )
}

fn is_valid_profile_probe_message(body: &Value) -> bool {
    body.get("type").and_then(Value::as_str) == Some("message")
        && body.get("role").and_then(Value::as_str) == Some("assistant")
        && body.get("content").and_then(Value::as_array).is_some()
}

fn profile_probe_network_json(
    proxy_url: Option<&str>,
    timing: ProfileProbeTimingSnapshot,
    total_ms: u64,
) -> Value {
    json!({
        "proxy_mode": if proxy_url.is_some() { "proxy" } else { "direct" },
        "proxy_url": proxy_url.map(redact_url_credentials),
        // reqwest exposes response-header readiness, not a portable standalone
        // DNS/TCP/TLS timing. This phase therefore intentionally includes DNS,
        // connect, proxy tunnel, TLS, retries, and upstream queue time.
        "response_headers_ms": timing.response_headers_ms,
        "first_byte_ms": timing.first_byte_ms,
        "response_body_ms": timing.response_body_ms,
        "total_ms": total_ms,
    })
}

async fn dispatch_profile_probe(
    state: &AppState,
    profile: ProviderProfile,
    request: Value,
) -> Response {
    match profile.transport {
        ProviderTransport::AntigravityLs => forward_ls_profile(state, profile, request, &HeaderMap::new()).await,
        ProviderTransport::Codex => forward_codex_profile(profile, request).await,
        ProviderTransport::OpenAiChat => {
            forward_openai_profile(profile, request, state.thought_signatures.clone()).await
        }
        ProviderTransport::OpenAiResponses => {
            forward_openai_responses_profile(
                profile,
                request,
                state.interaction_continuations.clone(),
            )
            .await
        }
        ProviderTransport::Anthropic => {
            forward_anthropic_profile(profile, &HeaderMap::new(), request).await
        }
        ProviderTransport::GeminiInteractions => {
            forward_gemini_interactions_profile(
                profile,
                request,
                state.interaction_continuations.clone(),
            )
            .await
        }
        ProviderTransport::LocalGemini => {
            forward_local_gemini_profile(state, &profile, request).await
        }
    }
}

/// Resolve a profile by file name, clone it under a short read lock, and run a
/// real minimal non-streaming inference through the profile's own transport,
/// upstream URL, model, credentials, proxy-configured client, and the normal
/// bridge conversion/response-parsing semantics. The routing state is only
/// read: the active profile is never switched, mutated, or persisted, so
/// highlighted-but-not-active profiles can be tested without racing live
/// requests. `timeout` bounds the whole probe including response-body reads.
async fn run_profile_probe(
    state: &AppState,
    file_name: &str,
    timeout: Duration,
) -> (StatusCode, Value) {
    let profile = {
        let Ok(routing) = state.routing.read() else {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({
                    "status": "error",
                    "file": file_name,
                    "error": {"stage": "resolve", "message": "Cannot read provider routing state"}
                }),
            );
        };
        routing
            .profiles
            .iter()
            .find(|profile| profile.file_name == file_name)
            .cloned()
    };
    let Some(mut profile) = profile else {
        return (
            StatusCode::NOT_FOUND,
            json!({
                "status": "error",
                "file": file_name,
                "model": Value::Null,
                "transport": Value::Null,
                "error": {
                    "stage": "resolve",
                    "message": format!("Unknown provider profile '{file_name}'")
                }
            }),
        );
    };
    let profile_file = profile.file_name.clone();
    let profile_model = profile.model.clone();
    let transport_label = profile.transport.as_str();
    if profile.transport == ProviderTransport::GeminiInteractions && profile.api_key.is_none() {
        if let Err(message) = apply_bridge_managed_gemini_credentials(state, &mut profile) {
            return (
                StatusCode::OK,
                json!({
                    "status": "error",
                    "file": profile_file,
                    "model": profile_model,
                    "transport": transport_label,
                    "latency_ms": 0,
                    "error": {
                        "stage": "auth",
                        "message": probe_safe_error_message(&message, &[])
                    }
                }),
            );
        }
    }
    if profile.transport == ProviderTransport::LocalGemini
        && state
            .fallback_api_key
            .as_deref()
            .is_none_or(|key| key.trim().is_empty())
    {
        return (
            StatusCode::OK,
            json!({
                "status": "error",
                "file": profile_file,
                "model": profile_model,
                "transport": transport_label,
                "latency_ms": 0,
                "error": {
                    "stage": "auth",
                    "message": "LocalGemini requires GEMINI_API_KEY or GEMINI_BRIDGE_API_KEY_PROFILE"
                }
            }),
        );
    }
    let effective_proxy_url = if profile.transport == ProviderTransport::LocalGemini {
        current_gemini_transport(state)
            .ok()
            .and_then(|transport| transport.proxy_url)
    } else {
        profile.proxy_url.clone()
    };
    // Keep the probe side-effect free: Responses store and Gemini interaction
    // store feed shared in-memory continuation caches that live requests may
    // follow, so a connectivity test must not create storable interactions.
    profile.openai_capabilities.responses_stateful = false;
    profile.openai_capabilities.gemini_store = false;

    // Upstream error payloads are untrusted and can reflect request metadata.
    // Strip every credential value known to this probe in addition to URL
    // userinfo before returning any message to the GUI.
    let sensitive_values = [
        profile.api_key.clone(),
        profile.auth_token.clone(),
        state.fallback_api_key.clone(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();

    let request = if profile.transport == ProviderTransport::AntigravityLs {
        json!({"model":profile.model,"stream":false,"messages":[{"role":"user","content":"ping"}]})
    } else { profile_probe_request(&profile.model) };
    let started = Instant::now();
    let probe_timing = Arc::new(RwLock::new(ProfileProbeTiming::new(started)));
    let outcome = tokio::time::timeout(
        timeout,
        PROFILE_PROBE_TIMING.scope(probe_timing.clone(), async {
            let response = dispatch_profile_probe(state, profile, request).await;
            let status = response.status().as_u16();
            let body = axum::body::to_bytes(response.into_body(), PROFILE_PROBE_BODY_LIMIT).await;
            (status, body)
        }),
    )
    .await;
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let timing = probe_timing
        .read()
        .map(|timing| timing.snapshot())
        .unwrap_or(ProfileProbeTimingSnapshot {
            response_headers_ms: None,
            first_byte_ms: None,
            response_body_ms: None,
        });
    let (status, body) = match outcome {
        Err(_) => {
            info!(
                provider = %profile_file,
                transport = transport_label,
                latency_ms,
                "Profile connectivity probe timed out"
            );
            return (
                StatusCode::OK,
                json!({
                    "status": "error",
                    "file": profile_file,
                    "model": profile_model,
                    "transport": transport_label,
                    "latency_ms": latency_ms,
                    "network": profile_probe_network_json(
                        effective_proxy_url.as_deref(), timing, latency_ms
                    ),
                    "error": {
                        "stage": "timeout",
                        "message": format!(
                            "Profile probe timed out after {} ms",
                            timeout.as_millis()
                        )
                    }
                }),
            );
        }
        Ok(outcome) => outcome,
    };
    let body_value = match body {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        Err(err) => {
            info!(
                provider = %profile_file,
                status,
                error = %err,
                "Profile connectivity probe response could not be read"
            );
            return (
                StatusCode::OK,
                json!({
                    "status": "error",
                    "file": profile_file,
                    "model": profile_model,
                    "transport": transport_label,
                    "latency_ms": latency_ms,
                    "network": profile_probe_network_json(
                        effective_proxy_url.as_deref(), timing, latency_ms
                    ),
                    "error": {
                        "stage": "protocol",
                        "status": status,
                        "message": probe_safe_error_message(
                            &format!("Cannot read probe response: {err}"),
                            &sensitive_values
                        )
                    }
                }),
            );
        }
    };
    // Inference transports return an Anthropic message at this boundary. Validate
    // that envelope explicitly because the native Anthropic path is otherwise
    // a byte-for-byte passthrough and a bare JSON object is not proof that the
    // selected model completed a request.
    if (200..300).contains(&status) && is_valid_profile_probe_message(&body_value) {
        info!(
            provider = %profile_file,
            transport = transport_label,
            status,
            latency_ms,
            "Profile connectivity probe succeeded"
        );
        return (
            StatusCode::OK,
            json!({
                "status": "ok",
                "file": profile_file,
                "model": profile_model,
                "transport": transport_label,
                "upstream_status": status,
                "latency_ms": latency_ms,
                "network": profile_probe_network_json(
                    effective_proxy_url.as_deref(), timing, latency_ms
                ),
                "message": format!("Valid {transport_label} response received from the upstream")
            }),
        );
    }
    if (200..300).contains(&status) {
        return (
            StatusCode::OK,
            json!({
                "status": "error",
                "file": profile_file,
                "model": profile_model,
                "transport": transport_label,
                "latency_ms": latency_ms,
                "network": profile_probe_network_json(
                    effective_proxy_url.as_deref(), timing, latency_ms
                ),
                "error": {
                    "stage": "protocol",
                    "status": status,
                    "message": "Profile probe received an invalid Anthropic message response"
                }
            }),
        );
    }
    let (error_type, message) = probe_error_details(&body_value, &sensitive_values);
    let stage = probe_error_stage(status, error_type);
    info!(
        provider = %profile_file,
        status,
        error_type = error_type.unwrap_or(""),
        error = %message,
        "Profile connectivity probe failed"
    );
    let mut error_body = json!({"stage": stage, "message": message});
    error_body["status"] = json!(status);
    (
        StatusCode::OK,
        json!({
            "status": "error",
            "file": profile_file,
            "model": profile_model,
            "transport": transport_label,
            "latency_ms": latency_ms,
            "network": profile_probe_network_json(
                effective_proxy_url.as_deref(), timing, latency_ms
            ),
            "error": error_body
        }),
    )
}

async fn admin_profile_test(
    State(state): State<Arc<AppState>>,
    Json(request): Json<Value>,
) -> Response {
    let Some(file_name) = request.get("file").and_then(Value::as_str) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Missing string field 'file'"})),
        )
            .into_response();
    };
    let (status, body) = run_profile_probe(&state, file_name, PROFILE_PROBE_TOTAL_TIMEOUT).await;
    (status, Json(body)).into_response()
}

fn provider_profile_json(profile: &ProviderProfile, active_file: &str) -> Value {
    let public_proxy_url = profile.proxy_url.as_deref().map(redact_url_credentials);
    json!({
        "file": profile.file_name,
        "name": profile.display_name,
        "source": profile.source.as_str(),
        "model": profile.model,
        "context_window": profile.context_window,
        "reasoning_effort": profile.openai_capabilities.reasoning_effort_override,
        "upstream_identity": profile
            .upstream_identity
            .as_deref()
            .map(redact_url_credentials_if_url),
        "identity_override": profile.identity_override,
        "base_url": redact_url_credentials(&profile.base_url),
        "proxy": public_proxy_url,
        "local_gemini": profile.local_gemini,
        "transport": profile.transport.as_str(),
        "execution_mode": if profile.transport == ProviderTransport::AntigravityLs { "ls-owned-tools" } else { "provider" },
        "ls": profile.ls.as_ref().map(|c| json!({"host_url":c.host_url,"workspace":c.workspace,"permission_mode":c.permission_mode,"usage_status":"unknown"})),
        "capabilities": openai_capabilities_json(&profile.openai_capabilities),
        "vision": {
            "mode": profile.vision.mode.as_str(),
            "profile": profile.vision.profile
        },
        "upstream_url": redact_url_credentials(&profile.upstream_url),
        "active": profile.file_name == active_file
    })
}

fn is_gemini_37_or_newer_flash_model(model: &str) -> bool {
    let model = normalized_gemini_model_name(model);
    let Some(rest) = model.strip_prefix("gemini-") else {
        return false;
    };
    let Some((version, variant)) = rest.split_once('-') else {
        return false;
    };
    if variant != "flash" && !variant.starts_with("flash-") {
        return false;
    }

    let mut version_parts = version.split('.');
    let (Some(major), Some(minor), None) = (
        version_parts
            .next()
            .and_then(|part| part.parse::<u64>().ok()),
        version_parts
            .next()
            .and_then(|part| part.parse::<u64>().ok()),
        version_parts.next(),
    ) else {
        return false;
    };
    major > 3 || (major == 3 && minor >= 7)
}

fn supports_gemini_thinking_levels(profile: &ProviderProfile) -> bool {
    profile.transport == ProviderTransport::GeminiInteractions
        && (is_gemini_37_or_newer_flash_model(&profile.model) || is_gemini_4_model(&profile.model))
}

fn gemini_thinking_level_locked(state: &AppState) -> bool {
    state.routing.read().ok().is_some_and(|routing| {
        routing
            .profiles
            .iter()
            .find(|profile| profile.file_name == routing.active_file)
            .is_some_and(|profile| {
                supports_gemini_thinking_levels(profile)
                    && profile
                        .openai_capabilities
                        .reasoning_effort_override
                        .is_some()
            })
    })
}

fn effective_gemini_thinking_level(state: &AppState) -> Result<Option<String>, String> {
    let routing = state
        .routing
        .read()
        .map_err(|_| "Cannot read provider routing state".to_string())?;
    let Some(profile) = routing
        .profiles
        .iter()
        .find(|profile| profile.file_name == routing.active_file)
        .filter(|profile| supports_gemini_thinking_levels(profile))
    else {
        return Ok(None);
    };
    if let Some(level) = profile
        .openai_capabilities
        .reasoning_effort_override
        .as_deref()
    {
        return Ok(normalize_gemini_thinking_level(level, &profile.model).map(str::to_owned));
    }
    if let Some(level) = current_gemini_thinking_level(state)? {
        return Ok(normalize_gemini_thinking_level(&level, &profile.model).map(str::to_owned));
    }
    Ok(profile
        .openai_capabilities
        .default_reasoning_effort
        .as_deref()
        .and_then(|level| normalize_gemini_thinking_level(level, &profile.model))
        .or(Some(gemini_default_thinking_level(&profile.model)))
        .map(str::to_owned))
}

fn reasoning_effort_control_json(state: &AppState) -> Result<Value, String> {
    let routing = state
        .routing
        .read()
        .map_err(|_| "Cannot read provider routing state".to_string())?;
    let Some(profile) = routing
        .profiles
        .iter()
        .find(|profile| profile.file_name == routing.active_file)
    else {
        return Ok(json!({"supported": false, "choices": [], "current": null}));
    };
    let Some(choices) = profile_effort_choices(profile) else {
        return Ok(json!({"supported": false, "choices": [], "current": null}));
    };
    // No runtime entry and no profile static value means there is no fixed
    // current tier; the status reports null instead of inventing one.
    let current = runtime_reasoning_efforts(state)?
        .get(&profile.file_name)
        .cloned()
        .or_else(|| {
            profile
                .openai_capabilities
                .reasoning_effort_override
                .clone()
        })
        .as_deref()
        .and_then(|effort| normalized_profile_effort(profile, effort).map(str::to_owned));
    Ok(json!({
        "supported": true,
        "choices": choices,
        "current": current
    }))
}

async fn admin_status(State(state): State<Arc<AppState>>) -> Response {
    let ls_host = if let Some(profile) = active_provider_profile(&state).filter(|p| p.transport == ProviderTransport::AntigravityLs) {
        let outcome=tokio::time::timeout(Duration::from_secs(2),async {
            let response=ls_host_post(profile.ls.as_ref().unwrap(),"/health",&json!({})).await?;
            if !response.status().is_success(){return Err("LS Host is unavailable".to_string());}
            let body=read_response_json_limited(response).await.map_err(|_|"Invalid Host status".to_string())?;
            Ok::<_,String>(json!({"status":"ok","engine_version":body.pointer("/instance/lsVersion"),"pid":body.pointer("/instance/pid"),"execution":"ls-owned-tools","model":profile.model,"mode":"turbo","auto_approve":true,"groups":ls_model_groups(&body["models"])}))
        }).await;
        Some(match outcome {Ok(Ok(value))=>value,_=>json!({"status":"unavailable","execution":"ls-owned-tools"})})
    } else {None};
    let transport = match current_gemini_transport(&state) {
        Ok(transport) => transport,
        Err(message) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": message})),
            )
                .into_response();
        }
    };
    let (active_profile, profile_count, profile_source) = {
        let Ok(routing) = state.routing.read() else {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Cannot read provider routing state"})),
            )
                .into_response();
        };
        let active_profile = routing
            .profiles
            .iter()
            .find(|profile| profile.file_name == routing.active_file)
            .map(|profile| provider_profile_json(profile, &profile.file_name));
        (
            active_profile,
            routing.profiles.len(),
            routing.source.as_str(),
        )
    };
    let config_stamp =
        match provider_config_stamp_async(state.providers_dir.clone(), state.settings_dir.clone())
            .await
        {
            Ok(stamp) => stamp,
            Err(message) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": message})),
                )
                    .into_response();
            }
        };
    let gemini_thinking_level = match effective_gemini_thinking_level(&state) {
        Ok(level) => level,
        Err(message) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": message})),
            )
                .into_response();
        }
    };
    let proxy_enabled = transport.proxy_url.is_some();
    let public_proxy_url = transport.proxy_url.as_deref().map(redact_url_credentials);
    let reasoning_effort_control = match reasoning_effort_control_json(&state) {
        Ok(control) => control,
        Err(message) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": message})),
            )
                .into_response();
        }
    };
    Json(json!({
        "status": "ok",
        "active_profile": active_profile,
        "profile_count": profile_count,
        "gemini_proxy": public_proxy_url,
        "gemini_proxy_mode": if proxy_enabled { "proxy" } else { "direct" },
        "listen_url": state.local_bridge_base_url,
        "providers_dir": state.providers_dir.to_string_lossy(),
        "profile_source": profile_source,
        "settings_dir": state.settings_dir.to_string_lossy(),
        "gemini_thinking_level": gemini_thinking_level,
        "gemini_thinking_choices": active_provider_profile(&state).filter(supports_gemini_thinking_levels)
            .map(|profile| gemini_thinking_choices(&profile.model)).unwrap_or(&[]),
        "gemini_thinking_locked": gemini_thinking_level_locked(&state),
        "reasoning_effort_control": reasoning_effort_control,
        "codex_continuity": codex_continuity_status(),
        "codex_capabilities": codex_capability_report(),
        "ls_host":ls_host,
        "config_stamp": config_stamp,
        "settings_stamp": config_stamp
    }))
    .into_response()
}

async fn admin_profiles(State(state): State<Arc<AppState>>) -> Response {
    let (profiles, profile_source) = {
        let Ok(routing) = state.routing.read() else {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Cannot read provider routing state"})),
            )
                .into_response();
        };
        (
            routing
                .profiles
                .iter()
                .map(|profile| provider_profile_json(profile, &routing.active_file))
                .collect::<Vec<_>>(),
            routing.source.as_str(),
        )
    };
    let config_stamp =
        match provider_config_stamp_async(state.providers_dir.clone(), state.settings_dir.clone())
            .await
        {
            Ok(stamp) => stamp,
            Err(message) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": message})),
                )
                    .into_response();
            }
        };
    Json(json!({
        "profiles": profiles,
        "profile_source": profile_source,
        "config_stamp": config_stamp,
        "settings_stamp": config_stamp
    }))
    .into_response()
}

async fn admin_set_active_profile(
    State(state): State<Arc<AppState>>,
    Json(request): Json<Value>,
) -> Response {
    let Some(file_name) = request.get("file").and_then(Value::as_str) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Missing string field 'file'"})),
        )
            .into_response();
    };
    let _transition = state.admin_state_lock.lock().await;
    let selected = match state.routing.read() {
        Ok(routing) => routing
            .profiles
            .iter()
            .find(|profile| profile.file_name == file_name)
            .cloned(),
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Cannot read provider routing state"})),
            )
                .into_response();
        }
    };
    let Some(selected) = selected else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Unknown provider profile"})),
        )
            .into_response();
    };
    let proxy_url = match current_gemini_transport(&state) {
        Ok(transport) => transport.proxy_url,
        Err(message) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": message})),
            )
                .into_response();
        }
    };
    let mut lifecycle = match state.ls_lifecycle.try_write() {
        Ok(guard) => guard,
        Err(_) => return (StatusCode::CONFLICT, Json(json!({"error":"LS is busy or starting; finish/cancel the task before switching"}))).into_response(),
    };
    let previous = active_provider_profile(&state);
    if let Err(err) = lifecycle.select(selected.ls.as_ref()).await {
        return (StatusCode::CONFLICT, Json(json!({"error":err}))).into_response();
    }
    if let Err(err) = persist_bridge_state_async(
        state.bridge_state_path.clone(),
        selected.file_name.clone(),
        proxy_url,
    )
    .await
    {
        let _ = lifecycle.select(previous.as_ref().and_then(|p| p.ls.as_ref())).await;
        error!("Cannot persist active profile and proxy state: {err}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("Provider was not switched because state persistence failed: {err}")})),
        )
            .into_response();
    }
    let Ok(mut routing) = state.routing.write() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "Cannot update provider routing state"})),
        )
            .into_response();
    };
    routing.active_file = selected.file_name.clone();
    Json(json!({
        "status": "ok",
        "active_profile": provider_profile_json(&selected, &selected.file_name)
    }))
    .into_response()
}

async fn admin_reload_profiles(State(state): State<Arc<AppState>>) -> Response {
    let providers_dir = state.providers_dir.clone();
    let settings_dir = state.settings_dir.clone();
    let local_bridge_base_url = state.local_bridge_base_url.clone();
    let mut loaded_profiles = match tokio::task::spawn_blocking(move || {
        load_provider_profiles(&providers_dir, &settings_dir, &local_bridge_base_url)
    })
    .await
    {
        Ok(Ok(profiles)) => profiles,
        Ok(Err(message)) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": message}))).into_response();
        }
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Cannot join profile loader: {err}")})),
            )
                .into_response();
        }
    };
    let _transition = state.admin_state_lock.lock().await;
    apply_ls_model_preferences(&mut loaded_profiles.profiles, &state.bridge_state_path);
    let active_file = match state.routing.read() {
        Ok(routing) => routing.active_file.clone(),
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Cannot read provider routing state"})),
            )
                .into_response();
        }
    };
    let selected = if loaded_profiles
        .profiles
        .iter()
        .any(|profile| profile.file_name == active_file)
    {
        active_file
    } else {
        loaded_profiles
            .profiles
            .iter()
            .find(|profile| profile.local_gemini)
            .or_else(|| loaded_profiles.profiles.first())
            .map(|profile| profile.file_name.clone())
            .unwrap_or_default()
    };
    let count = loaded_profiles.profiles.len();
    let proxy_url = match current_gemini_transport(&state) {
        Ok(transport) => transport.proxy_url,
        Err(message) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": message})),
            )
                .into_response();
        }
    };
    let mut lifecycle = match state.ls_lifecycle.try_write() {
        Ok(guard) => guard,
        Err(_) => return (StatusCode::CONFLICT, Json(json!({"error":"LS is busy or starting; finish/cancel the task before reloading"}))).into_response(),
    };
    let previous = active_provider_profile(&state);
    let next = loaded_profiles.profiles.iter().find(|p| p.file_name == selected);
    if let Err(err) = lifecycle.select(next.and_then(|p| p.ls.as_ref())).await {
        return (StatusCode::CONFLICT, Json(json!({"error":err}))).into_response();
    }
    if let Err(err) =
        persist_bridge_state_async(state.bridge_state_path.clone(), selected.clone(), proxy_url)
            .await
    {
        let _ = lifecycle.select(previous.as_ref().and_then(|p| p.ls.as_ref())).await;
        error!("Cannot persist profiles and proxy state: {err}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("Profiles were not reloaded because state persistence failed: {err}")})),
        )
            .into_response();
    }
    let Ok(mut routing) = state.routing.write() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "Cannot update provider routing state"})),
        )
            .into_response();
    };
    routing.profiles = loaded_profiles.profiles;
    routing.source = loaded_profiles.source;
    routing.active_file = selected;
    Json(json!({"status": "ok", "profile_count": count})).into_response()
}

fn proxy_from_admin_request(request: &Value) -> Result<Option<String>, String> {
    match request.get("proxy") {
        Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => {
            let value = value.trim();
            if value.is_empty() {
                Ok(None)
            } else {
                Ok(Some(value.to_string()))
            }
        }
        Some(_) => Err("Field 'proxy' must be a string or null".to_string()),
        None => Err("Missing field 'proxy'".to_string()),
    }
}

fn gemini_thinking_level_from_admin_request(request: &Value) -> Result<String, String> {
    match request.get("level").and_then(Value::as_str) {
        Some(level @ ("low" | "medium" | "high" | "xhigh")) => Ok(level.to_string()),
        Some(_) => Err("Field 'level' must be low, medium, high, or xhigh".to_string()),
        None => Err("Missing string field 'level'".to_string()),
    }
}

async fn admin_set_gemini_thinking_level(
    State(state): State<Arc<AppState>>,
    Json(request): Json<Value>,
) -> Response {
    let level = match gemini_thinking_level_from_admin_request(&request) {
        Ok(level) => level,
        Err(message) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": message}))).into_response();
        }
    };
    let _transition = state.admin_state_lock.lock().await;
    let supported = match state.routing.read() {
        Ok(routing) => routing
            .profiles
            .iter()
            .find(|profile| profile.file_name == routing.active_file)
            .is_some_and(|profile| {
                supports_gemini_thinking_levels(profile)
                    && gemini_thinking_choices(&profile.model).contains(&level.as_str())
            }),
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Cannot read provider routing state"})),
            )
                .into_response();
        }
    };
    if !supported {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Thinking level is unsupported by the active profile; Gemini 4 Carbon/Argon and Gemini 3.7-or-newer Flash support low/medium/high, Hydrogen also supports xhigh"})),
        )
            .into_response();
    }
    if gemini_thinking_level_locked(&state) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Gemini Thinking is locked by the profile's top-level reasoning_effort; remove that setting to enable GUI changes"})),
        ).into_response();
    }
    if let Err(message) =
        persist_gemini_thinking_level_async(state.bridge_state_path.clone(), level.clone()).await
    {
        error!("{message}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": message})),
        )
            .into_response();
    }
    let Ok(mut current_level) = state.gemini_thinking_level.write() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "Cannot update Gemini thinking-level state"})),
        )
            .into_response();
    };
    *current_level = Some(level.clone());
    info!(level = %level, "Gemini thinking level changed");
    Json(json!({
        "status": "ok",
        "gemini_thinking_level": level
    }))
    .into_response()
}

fn reasoning_effort_from_admin_request(
    request: &Value,
    choices: &[&str],
) -> Result<String, String> {
    match request.get("level").and_then(Value::as_str) {
        Some(level) if choices.contains(&level) => Ok(level.to_string()),
        Some(_) => Err(format!(
            "Field 'level' must be one of {} for the active provider profile",
            choices.join(", ")
        )),
        None => Err("Missing string field 'level'".to_string()),
    }
}

async fn admin_set_reasoning_effort(
    State(state): State<Arc<AppState>>,
    Json(request): Json<Value>,
) -> Response {
    let _transition = state.admin_state_lock.lock().await;
    let active_profile = match state.routing.read() {
        Ok(routing) => routing
            .profiles
            .iter()
            .find(|profile| profile.file_name == routing.active_file)
            .cloned(),
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Cannot read provider routing state"})),
            )
                .into_response();
        }
    };
    let Some(active_profile) = active_profile else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "The active provider profile does not support a reasoning-effort control"})),
        )
            .into_response();
    };
    let Some(choices) = profile_effort_choices(&active_profile) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "The active provider profile does not support a reasoning-effort control"})),
        )
            .into_response();
    };
    let level = match reasoning_effort_from_admin_request(&request, choices) {
        Ok(level) => level,
        Err(message) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": message}))).into_response();
        }
    };
    // Build the candidate map under the admin state lock, persist it first,
    // and only then swap the in-memory map. A failed write leaves the
    // previous in-memory values untouched.
    let mut candidate = match runtime_reasoning_efforts(&state) {
        Ok(efforts) => efforts,
        Err(message) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": message})),
            )
                .into_response();
        }
    };
    candidate.insert(active_profile.file_name.clone(), level.clone());
    if let Err(message) =
        persist_reasoning_efforts_async(state.bridge_state_path.clone(), candidate.clone()).await
    {
        error!("{message}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": message})),
        )
            .into_response();
    }
    let Ok(mut efforts) = state.reasoning_efforts.write() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "Cannot update provider reasoning-effort state"})),
        )
            .into_response();
    };
    *efforts = candidate;
    info!(
        provider = %active_profile.file_name,
        level = %level,
        "Provider reasoning effort changed"
    );
    Json(json!({
        "status": "ok",
        "reasoning_effort_control": {
            "supported": true,
            "choices": choices,
            "current": level
        }
    }))
    .into_response()
}

async fn admin_set_gemini_proxy(
    State(state): State<Arc<AppState>>,
    Json(request): Json<Value>,
) -> Response {
    let proxy_url = match proxy_from_admin_request(&request) {
        Ok(proxy_url) => proxy_url,
        Err(message) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": message}))).into_response();
        }
    };
    let client = match build_gemini_client(proxy_url.as_deref(), None) {
        Ok(client) => client,
        Err(message) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": message}))).into_response();
        }
    };
    let _transition = state.admin_state_lock.lock().await;
    let active_profile = match state.routing.read() {
        Ok(routing) => routing.active_file.clone(),
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Cannot read provider routing state"})),
            )
                .into_response();
        }
    };
    if let Err(message) = persist_bridge_state_async(
        state.bridge_state_path.clone(),
        active_profile,
        proxy_url.clone(),
    )
    .await
    {
        error!("{message}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": message})),
        )
            .into_response();
    }
    let Ok(mut transport) = state.gemini_transport.write() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "Cannot update Gemini proxy state"})),
        )
            .into_response();
    };
    *transport = GeminiTransport {
        client,
        proxy_url: proxy_url.clone(),
    };
    drop(transport);
    let public_proxy_url = proxy_url.as_deref().map(redact_url_credentials);
    info!(
        "Gemini network route changed to {}",
        public_proxy_url.as_deref().unwrap_or("direct connection")
    );
    Json(json!({
        "status": "ok",
        "gemini_proxy": public_proxy_url,
        "gemini_proxy_mode": if proxy_url.is_some() { "proxy" } else { "direct" }
    }))
    .into_response()
}

async fn admin_test_gemini_proxy(
    State(state): State<Arc<AppState>>,
    Json(request): Json<Value>,
) -> Response {
    let proxy_url = match proxy_from_admin_request(&request) {
        Ok(proxy_url) => proxy_url,
        Err(message) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": message}))).into_response();
        }
    };
    let client = match build_gemini_client(proxy_url.as_deref(), Some(Duration::from_secs(25))) {
        Ok(client) => client,
        Err(message) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": message}))).into_response();
        }
    };
    let Some(api_key) = state
        .fallback_api_key
        .clone()
        .filter(|value| !value.trim().is_empty())
    else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "Gemini API key is not configured"})),
        )
            .into_response();
    };
    let Some(models_base_url) = state.upstream_url.strip_suffix("/chat/completions") else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Gemini upstream URL does not use the OpenAI chat/completions path"})),
        )
            .into_response();
    };
    let model_url = format!("{models_base_url}/models/{}", state.model);
    let upstream = match client.get(&model_url).bearer_auth(api_key).send().await {
        Ok(response) => response,
        Err(err) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("Gemini connection test failed: {err}")})),
            )
                .into_response();
        }
    };
    let status = upstream.status();
    let body = read_response_json_limited(upstream)
        .await
        .unwrap_or_else(|_| json!({}));
    if !status.is_success() {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": format!("Gemini returned HTTP {status}: {}", safe_error_message(&body))
            })),
        )
            .into_response();
    }
    let public_proxy_url = proxy_url.as_deref().map(redact_url_credentials);
    Json(json!({
        "status": "ok",
        "model": body.get("id").and_then(Value::as_str).unwrap_or(&state.model),
        "gemini_proxy": public_proxy_url,
        "gemini_proxy_mode": if proxy_url.is_some() { "proxy" } else { "direct" }
    }))
    .into_response()
}

async fn admin_shutdown(State(state): State<Arc<AppState>>) -> Response {
    if state.shutdown_tx.send(true).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "Graceful shutdown channel is unavailable"})),
        )
            .into_response();
    }
    Json(json!({"status": "shutting_down"})).into_response()
}

async fn health(State(state): State<Arc<AppState>>) -> Json<Value> {
    let active_profile = active_provider_profile(&state);
    let proxy_url = current_gemini_transport(&state)
        .ok()
        .and_then(|transport| transport.proxy_url);
    Json(json!({
        "status": "ok",
        "model": state.model,
        "upstream": redact_url_credentials(&state.upstream_url),
        "gemini_proxy": proxy_url.as_deref().map(redact_url_credentials),
        "gemini_thinking_level": effective_gemini_thinking_level(&state).ok().flatten(),
        "active_profile": active_profile.as_ref().map(|profile| provider_profile_json(profile, &profile.file_name))
    }))
}

async fn models(State(state): State<Arc<AppState>>) -> Json<Value> {
    let active_profile = active_provider_profile(&state);
    Json(models_response_value(&state.model, active_profile.as_ref()))
}

fn models_response_value(local_model: &str, active_profile: Option<&ProviderProfile>) -> Value {
    let model = active_profile
        .map(|profile| profile.model.as_str())
        .unwrap_or(local_model);
    let known_limits = claude_5_model(model).map(|_| (1_000_000_u64, 128_000_u64));
    let context_window = active_profile
        .and_then(|profile| profile.context_window)
        .or_else(|| known_limits.map(|limits| limits.0));
    let max_output_tokens = known_limits.map(|limits| limits.1);
    json!({
        "object": "list",
        "models": [],
        "data": [{
            "id": model,
            "object": "model",
            "created": 0,
            "owned_by": "claude-bridge",
            "upstream_model": active_profile.as_ref().map(|profile| profile.model.clone()),
            "context_window": context_window,
            "max_output_tokens": max_output_tokens
        }]
    })
}

async fn model_by_id(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(model_id): axum::extract::Path<String>,
) -> Response {
    let active_profile = active_provider_profile(&state);
    let listed = models_response_value(&state.model, active_profile.as_ref());
    let Some(model) = listed
        .get("data")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .cloned()
    else {
        return anthropic_error(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "No model is currently available",
        );
    };
    // Accept every name the bridge itself routes to /v1/messages: the listed
    // model id, the upstream model id, and the local fallback model name.
    let known = [
        model.get("id").and_then(Value::as_str),
        model.get("upstream_model").and_then(Value::as_str),
        Some(state.model.as_str()),
    ]
    .into_iter()
    .flatten()
    .any(|candidate| candidate == model_id);
    if known {
        Json(model).into_response()
    } else {
        anthropic_error(
            StatusCode::NOT_FOUND,
            "not_found_error",
            &format!("Model '{model_id}' not found"),
        )
    }
}
