#[test]
fn gemini_review_idle_timeout_covers_family_and_preserves_tiers() {
    for model in ["gemini-4-hydrogen", "gemini-4-carbon", "gemini-4-argon"] {
        assert_eq!(
            gemini_stream_idle_timeout(model, false, false),
            Duration::from_secs(900)
        );
        assert_eq!(
            gemini_stream_idle_timeout(model, false, true),
            GEMINI_FLEX_STREAM_IDLE_TIMEOUT
        );
        assert_eq!(
            gemini_stream_idle_timeout(model, true, true),
            GEMINI_BACKGROUND_LIFETIME
        );
    }
    assert_eq!(
        gemini_stream_idle_timeout("gemini-2.5-pro", false, false),
        UPSTREAM_STREAM_IDLE_TIMEOUT
    );
}

#[test]
fn gemini_review_loaded_profiles_use_model_defaults_and_explicit_overrides() {
    let root = env::temp_dir().join(format!("gemini-review-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    for (model, expected) in [
        ("gemini-4-hydrogen", "high"),
        ("gemini-4-carbon", "medium"),
        ("gemini-4-argon", "medium"),
    ] {
        for explicit in [false, true] {
            let path = root.join("profile.json");
            let mut raw = json!({"protocol":"gemini-interactions","model":model,"base_url":"https://generativelanguage.googleapis.com/v1beta","api_key":"test-key"});
            if explicit {
                raw["capabilities"] = json!({"default_reasoning_effort":"low"});
            }
            fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
            let profile = load_native_provider_profiles(vec![path], "http://127.0.0.1:18787")
                .unwrap()
                .remove(0);
            let body = translate_gemini_interactions_request(
                &json!({"max_tokens":1024,"messages":[{"role":"user","content":"hello"}]}),
                &profile,
                &RwLock::new(InteractionContinuationCache::default()),
            )
            .unwrap();
            let level = if explicit { "low" } else { expected };
            assert_eq!(body["generation_config"]["thinking_level"], level);
            let state = probe_state(
                vec![profile],
                "profile.json".into(),
                None,
                "http://unused.invalid".into(),
            );
            assert_eq!(
                effective_gemini_thinking_level(&state).unwrap().as_deref(),
                Some(level)
            );
        }
    }
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn gemini_review_stream_reports_actual_input_usage() {
    let body = argon_sse(vec![
        json!({"event_type":"interaction.completed","interaction":{"id":"job","status":"completed","usage":{"total_input_tokens":9123,"total_cached_tokens":4096,"total_output_tokens":11}}}),
    ]);
    let (base, server) = argon_mock_server(Router::new().route(
        "/interactions",
        post(move || {
            let body = body.clone();
            async { body }
        }),
    ))
    .await;
    let mut profile = argon_test_profile();
    profile.upstream_url = format!("{base}/interactions");
    let response = forward_gemini_interactions_profile(
        profile,
        json!({"stream":true,"max_tokens":1024,"messages":[{"role":"user","content":"hello"}]}),
        Arc::new(RwLock::new(InteractionContinuationCache::default())),
    )
    .await;
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let delta: Value = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| event["type"] == "message_delta")
        .unwrap();
    assert_eq!(delta["usage"]["input_tokens"], 9123);
    assert_eq!(delta["usage"]["cache_read_input_tokens"], 4096);
    assert_eq!(delta["usage"]["output_tokens"], 11);
    server.abort();
}

#[tokio::test]
async fn gemini_review_background_terminal_events_without_status_do_not_cancel() {
    for event_type in ["interaction.completed", "interaction.requires_action"] {
        let cancelled = Arc::new(tokio::sync::Notify::new());
        let notice = cancelled.clone();
        let body = argon_sse(vec![
            json!({"event_type":event_type,"event_id":"end","interaction_id":"job"}),
        ]);
        let router = Router::new()
            .route(
                "/interactions",
                post(move || {
                    let body = body.clone();
                    async { body }
                }),
            )
            .route(
                "/interactions/job/cancel",
                post(move || {
                    let notice = notice.clone();
                    async move {
                        notice.notify_one();
                        StatusCode::OK
                    }
                }),
            );
        let (base, server) = argon_mock_server(router).await;
        let mut profile = argon_test_profile();
        profile.upstream_url = format!("{base}/interactions");
        let response = profile
            .client
            .post(&profile.upstream_url)
            .send()
            .await
            .unwrap();
        let mut bytes =
            resumable_gemini_background_stream(response, GeminiBackgroundTransport::new(&profile));
        assert!(bytes.next().await.unwrap().is_ok());
        // Consumer may drop immediately after the terminal event, without reading EOF.
        drop(bytes);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), cancelled.notified())
                .await
                .is_err()
        );
        server.abort();
    }
}

#[tokio::test]
async fn gemini_review_background_retry_budget_requires_new_progress() {
    for progress in [true, false] {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = attempts.clone();
        let initial = argon_sse(vec![
            json!({"event_type":"interaction.created","event_id":"e0","interaction":{"id":"job","status":"in_progress"}}),
        ]);
        let router = Router::new()
            .route("/interactions", post(move || {let body = initial.clone(); async {body}}))
            .route("/interactions/job", get(move || {let count=count.clone(); async move {
                let n = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                argon_sse(vec![if progress {
                    json!({"event_type":"interaction.status_update","event_id":format!("e{n}"),"status":if n > GEMINI_BACKGROUND_RETRIES {"completed"} else {"in_progress"}})
                } else {
                    json!({"event_type":"interaction.created","event_id":"e0","interaction":{"id":"job","status":"in_progress"}})
                }])
            }}));
        let (base, server) = argon_mock_server(router).await;
        let mut profile = argon_test_profile();
        profile.upstream_url = format!("{base}/interactions");
        let response = profile
            .client
            .post(&profile.upstream_url)
            .send()
            .await
            .unwrap();
        let mut bytes =
            resumable_gemini_background_stream(response, GeminiBackgroundTransport::new(&profile));
        let mut error = None;
        while let Some(result) = bytes.next().await {
            if let Err(e) = result {
                error = Some(e);
                break;
            }
        }
        if progress {
            assert!(error.is_none(), "{error:?}");
            assert_eq!(
                attempts.load(std::sync::atomic::Ordering::SeqCst),
                GEMINI_BACKGROUND_RETRIES + 1
            );
        } else {
            assert!(error.unwrap().contains("reconnect limit"));
            assert_eq!(
                attempts.load(std::sync::atomic::Ordering::SeqCst),
                GEMINI_BACKGROUND_RETRIES
            );
        }
        drop(bytes);
        server.abort();
    }
}

fn signed_gemini_tool_transcript() -> Value {
    json!({"max_tokens":1024,"messages":[
        {"role":"user","content":"Read both files"},
        {"role":"assistant","content":[
            {"type":"thinking","thinking":"","signature":"opaque-original-signature"},
            {"type":"tool_use","id":"call-a","name":"read_file","input":{"path":"a"}},
            {"type":"tool_use","id":"call-b","name":"read_file","input":{"path":"b"}}
        ]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-b","is_error":true,"content":"missing"}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-a","content":"file contents"}]}
    ]})
}

#[test]
fn gemini_stateless_signed_parallel_tools_preserve_native_history() {
    for store in [false, true] {
        let mut profile = argon_test_profile();
        profile.openai_capabilities.gemini_store = store;
        let request = signed_gemini_tool_transcript();
        let body = translate_gemini_interactions_request(
            &request,
            &profile,
            &RwLock::new(InteractionContinuationCache::default()),
        )
        .unwrap();
        assert_eq!(body["store"], store);
        assert!(body.get("previous_interaction_id").is_none());
        let steps = body["input"].as_array().unwrap();
        assert_eq!(
            steps[1],
            json!({"type":"thought","summary":[],"signature":"opaque-original-signature"})
        );
        assert_eq!(
            steps[2],
            json!({"type":"function_call","id":"call-a","name":"read_file","arguments":{"path":"a"}})
        );
        assert_eq!(steps[3]["id"], "call-b");
        assert_eq!(steps[4]["type"], "function_result");
        assert_eq!(steps[4]["call_id"], "call-b");
        assert_eq!(steps[4]["is_error"], true);
        assert_eq!(steps[4]["result"], "missing");
        assert_eq!(steps[5]["call_id"], "call-a");
        assert_eq!(steps[5]["result"], "file contents");
        assert!(!body.to_string().contains("An earlier"));
    }
}

#[test]
fn gemini_stateless_incomplete_history_uses_explicit_recovery() {
    let base = signed_gemini_tool_transcript();
    let mut variants = Vec::new();
    let mut missing = base.clone();
    missing["messages"][1]["content"][0]
        .as_object_mut()
        .unwrap()
        .remove("signature");
    variants.push(missing);
    let mut orphan = base.clone();
    orphan["messages"][2]["content"][0]["tool_use_id"] = json!("unknown");
    variants.push(orphan);
    let mut duplicate = base.clone();
    duplicate["messages"][3]["content"][0]["tool_use_id"] = json!("call-b");
    variants.push(duplicate);
    let mut partial = base.clone();
    partial["messages"].as_array_mut().unwrap().pop();
    variants.push(partial);
    for request in variants {
        let body = translate_gemini_interactions_request(
            &request,
            &argon_test_profile(),
            &RwLock::new(InteractionContinuationCache::default()),
        )
        .unwrap();
        assert!(body["input"].as_array().unwrap().iter().all(|s| !matches!(
            s["type"].as_str(),
            Some("function_call" | "function_result" | "thought")
        )));
        assert!(body["system_instruction"]
            .as_str()
            .unwrap()
            .contains("invoke one of the provided function tools"));
    }
}

#[tokio::test]
async fn gemini_hydrogen_xhigh_reaches_wire_with_native_stateless_history() {
    let (base, captured, server) = spawn_probe_mock("/interactions", StatusCode::OK,
        json!({"status":"completed","steps":[{"type":"model_output","content":[{"type":"text","text":"OK"}]}]})).await;
    let mut profile = argon_test_profile();
    profile.model = "gemini-4-hydrogen".into();
    profile.upstream_url = format!("{base}/interactions");
    profile.openai_capabilities.gemini_store = false;
    let mut request = signed_gemini_tool_transcript();
    request["output_config"] = json!({"effort":"xhigh"});
    let response = forward_gemini_interactions_profile(
        profile,
        request,
        Arc::new(RwLock::new(InteractionContinuationCache::default())),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response
        .headers()
        .values()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.contains("textual observations")));
    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].0["generation_config"]["thinking_level"],
        "xhigh"
    );
    assert_eq!(
        requests[0].0["input"][1]["signature"],
        "opaque-original-signature"
    );
    assert_eq!(requests[0].0["input"][4]["call_id"], "call-b");
    server.abort();
}

#[test]
fn gemini_4_family_background_storage_and_default_contract() {
    for model in [
        "gemini-4-argon",
        "gemini-4-carbon",
        "gemini-4-hydrogen",
        " MODELS/Gemini-4-Carbon-preview ",
    ] {
        let mut profile = argon_test_profile();
        profile.model = model.into();
        profile.openai_capabilities.default_reasoning_effort = None;
        let cache = RwLock::new(InteractionContinuationCache::default());
        let request = json!({"max_tokens":262144,"messages":[{"role":"user","content":"hello"}]});
        for stream in [false, true] {
            let mut request = request.clone();
            request["stream"] = json!(stream);
            let body = translate_gemini_interactions_request(&request, &profile, &cache).unwrap();
            assert_eq!(body["background"], true, "{model}");
            assert_eq!(body["generation_config"]["max_output_tokens"], 262144);
            assert_eq!(
                body["generation_config"]["thinking_level"],
                if is_gemini_4_hydrogen_model(model) {
                    "high"
                } else {
                    "medium"
                }
            );
        }
        profile.openai_capabilities.gemini_store = false;
        assert!(
            translate_gemini_interactions_request(&request, &profile, &cache)
                .unwrap_err()
                .contains("gemini_store=true")
        );
        profile.openai_capabilities.max_output_tokens = Some(32768);
        let body = translate_gemini_interactions_request(&request, &profile, &cache).unwrap();
        assert!(body.get("background").is_none());
        assert_eq!(body["generation_config"]["max_output_tokens"], 32768);
        let prefill = json!({"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"hello"}]});
        assert!(
            translate_gemini_interactions_request(&prefill, &profile, &cache)
                .unwrap_err()
                .contains("prefill")
        );
    }
    for model in [
        "gemini-4-carbonish",
        "gemini-4-hydrogenish",
        "gemini-4-argonish",
    ] {
        assert!(!is_gemini_4_model(model));
    }
}

#[tokio::test]
async fn gemini_4_carbon_hydrogen_preflight_reserves_actual_context() {
    for model in ["gemini-4-carbon", "gemini-4-hydrogen"] {
        let router = Router::new().route(
            &format!("/models/{model}:countTokens"),
            post(|| async { Json(json!({"totalTokens":1_000_000})) }),
        );
        let (base, server) = argon_mock_server(router).await;
        let mut profile = argon_test_profile();
        profile.model = model.into();
        profile.base_url = base.clone();
        profile.upstream_url = format!("{base}/interactions");
        let request = json!({"max_tokens":1048576,"messages":[{"role":"user","content":"short"}]});
        let mut body = translate_gemini_interactions_request(
            &request,
            &profile,
            &RwLock::new(InteractionContinuationCache::default()),
        )
        .unwrap();
        prepare_gemini_4_context_budget(&profile, &request, &mut body, &mut vec![])
            .await
            .unwrap();
        assert_eq!(body["generation_config"]["max_output_tokens"], 48576);
        assert!(body.get("background").is_none());
        assert!(apply_gemini_4_context_budget(&profile, 1048576, &mut body)
            .unwrap_err()
            .contains("context exhausted"));
        server.abort();
    }
}

#[tokio::test]
async fn gemini_admin_hydrogen_xhigh_persists_and_carbon_rejects_it() {
    let root = env::temp_dir().join(format!("gemini-latest-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let mut profile = argon_test_profile();
    profile.model = "gemini-4-hydrogen".into();
    profile.openai_capabilities.default_reasoning_effort = None;
    let mut state = probe_state(
        vec![profile],
        "argon-background.json".into(),
        None,
        "http://unused.invalid".into(),
    );
    Arc::get_mut(&mut state).unwrap().bridge_state_path = root.join("state.json");
    Arc::get_mut(&mut state).unwrap().providers_dir = root.clone();
    Arc::get_mut(&mut state).unwrap().settings_dir = root.clone();
    assert_eq!(
        effective_gemini_thinking_level(&state).unwrap().as_deref(),
        Some("high")
    );
    let response =
        admin_set_gemini_thinking_level(State(state.clone()), Json(json!({"level":"xhigh"}))).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        load_persisted_gemini_thinking_level(&state.bridge_state_path).as_deref(),
        Some("xhigh")
    );
    let response = admin_status(State(state.clone())).await;
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        body["gemini_thinking_choices"],
        json!(["low", "medium", "high", "xhigh"])
    );
    assert_eq!(body["gemini_thinking_level"], "xhigh");
    state.routing.write().unwrap().profiles[0].model = "gemini-4-carbon".into();
    assert_eq!(
        effective_gemini_thinking_level(&state).unwrap().as_deref(),
        Some("high")
    );
    let response =
        admin_set_gemini_thinking_level(State(state.clone()), Json(json!({"level":"xhigh"}))).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        load_persisted_gemini_thinking_level(&state.bridge_state_path).as_deref(),
        Some("xhigh")
    );
    let response =
        admin_set_gemini_thinking_level(State(state.clone()), Json(json!({"level":"medium"})))
            .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = admin_status(State(state.clone())).await;
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        body["gemini_thinking_choices"],
        json!(["low", "medium", "high"])
    );
    assert_eq!(body["gemini_thinking_level"], "medium");
    fs::remove_dir_all(root).unwrap();
}
