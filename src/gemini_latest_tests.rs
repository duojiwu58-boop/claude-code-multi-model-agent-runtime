#[test]
fn gemini_review_idle_timeout_covers_family_and_preserves_tiers() {
    for model in [
        "gemini-4-neon",
        "gemini-4-hydrogen",
        "gemini-4-carbon",
        "gemini-4-argon",
    ] {
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
        ("gemini-4-neon", "high"),
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
        "gemini-4-neon",
        " MODELS/Gemini-4-Neon-preview ",
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
                if model.to_ascii_lowercase().contains("hydrogen")
                    || model.to_ascii_lowercase().contains("neon")
                {
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
        "gemini-4-neonish",
        "gemini-4-hydrogenish",
        "gemini-4-argonish",
    ] {
        assert!(!is_gemini_4_model(model));
    }
}

#[tokio::test]
async fn gemini_4_carbon_hydrogen_preflight_reserves_actual_context() {
    for model in ["gemini-4-carbon", "gemini-4-hydrogen", "gemini-4-neon"] {
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
    state.routing.write().unwrap().profiles[0].model = "gemini-4-neon".into();
    assert_eq!(
        effective_gemini_thinking_level(&state).unwrap().as_deref(),
        Some("xhigh")
    );
    let response =
        admin_set_gemini_thinking_level(State(state.clone()), Json(json!({"level":"xhigh"}))).await;
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

#[test]
fn gemini_tool_retrieval_forwards_defer_loading_tool_search_and_fallbacks() {
    let mut profile = argon_test_profile();
    profile.model = "gemini-4-neon".into();
    let cache = RwLock::new(InteractionContinuationCache::default());
    let request = json!({
        "messages": [{"role": "user", "content": "Inspect issue 42"}],
        "tools": [
            {"type": "tool_search_tool_bm25_20251119", "name": "tool_search"},
            {
                "name": "read_file",
                "description": "Read a file",
                "short_description": "Should not be forwarded without defer_loading",
                "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}
            },
            {
                "name": "read_issue",
                "description": "Read one issue in detail",
                "short_description": "Read issue",
                "defer_loading": true,
                "input_schema": {"type": "object", "properties": {"id": {"type": "integer"}}}
            }
        ]
    });

    // 1. Supported model + gemini_store=true -> emits defer_loading, short_description, and one server-side tool_search
    let wire = translate_gemini_interactions_request(&request, &profile, &cache).unwrap();
    let tools = wire["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 3);
    assert_eq!(tools[0]["name"], "read_file");
    assert!(tools[0].get("defer_loading").is_none());
    assert!(tools[0].get("short_description").is_none());
    assert_eq!(tools[1]["name"], "read_issue");
    assert_eq!(tools[1]["defer_loading"], true);
    assert_eq!(tools[1]["short_description"], "Read issue");
    assert_eq!(tools[2], json!({"type": "tool_search"}));

    // Diagnostics report native mapping and do not claim defer_loading was ignored.
    let diag = gemini_interaction_request_diagnostics_for_profile(&request, &profile).join("\n");
    assert!(diag.contains("native server-side 'tool_search'"));
    assert!(!diag.contains("Ignored Anthropic extension 'defer_loading'"));

    // 2. Forced named tool_choice clears defer_loading on that tool (and omits unused tool_search).
    let mut forced = request.clone();
    forced["tool_choice"] = json!({"type": "tool", "name": "read_issue"});
    let forced_wire = translate_gemini_interactions_request(&forced, &profile, &cache).unwrap();
    let forced_tools = forced_wire["tools"].as_array().unwrap();
    assert_eq!(forced_tools.len(), 2);
    assert!(forced_tools[1].get("defer_loading").is_none());
    assert!(forced_tools[1].get("short_description").is_none());

    // 3. Stateless mode (gemini_store=false) strips tool_search and defer_loading.
    let mut stateless = profile.clone();
    stateless.openai_capabilities.gemini_store = false;
    let stateless_wire = translate_gemini_interactions_request(&request, &stateless, &cache).unwrap();
    let stateless_tools = stateless_wire["tools"].as_array().unwrap();
    assert_eq!(stateless_tools.len(), 2);
    assert!(stateless_tools.iter().all(|t| t["type"] == "function" && t.get("defer_loading").is_none()));

    // 4. Profile opt-out (gemini_tool_retrieval=false) disables deferred loading.
    let mut disabled = profile.clone();
    disabled.openai_capabilities.gemini_tool_retrieval = false;
    let disabled_wire = translate_gemini_interactions_request(&request, &disabled, &cache).unwrap();
    assert_eq!(disabled_wire["tools"].as_array().unwrap().len(), 2);
    assert!(disabled_wire["tools"][1].get("defer_loading").is_none());

    // 5. Unsupported model (gemini-2.5-pro / gemini-3.5-flash) disables deferred loading.
    for old_model in ["gemini-2.5-pro", "gemini-3.5-flash"] {
        let mut old = profile.clone();
        old.model = old_model.into();
        let old_wire = translate_gemini_interactions_request(&request, &old, &cache).unwrap();
        assert_eq!(old_wire["tools"].as_array().unwrap().len(), 2);
        assert!(old_wire["tools"][1].get("defer_loading").is_none());
    }

    // 6. Follow-up turn with cached continuation keeps tool_search + defer_loading;
    //    continuation fallback (allow_continuation=false) or uncached history strips them.
    remember_interaction_continuation(
        &cache,
        &profile.file_name,
        &request,
        "interaction-tr-1",
        &[json!({"type": "tool_use", "id": "call-tr-1", "name": "read_issue", "input": {"id": 42}})],
        &[("call-tr-1".to_string(), "read_issue".to_string())],
    );
    let mut followup = request.clone();
    followup["messages"] = json!([
        {"role": "user", "content": "Inspect issue 42"},
        {"role": "assistant", "content": [{"type": "tool_use", "id": "call-tr-1", "name": "read_issue", "input": {"id": 42}}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call-tr-1", "content": "Issue body"}]}
    ]);
    let resumed = translate_gemini_interactions_request(&followup, &profile, &cache).unwrap();
    assert_eq!(resumed["previous_interaction_id"], "interaction-tr-1");
    assert_eq!(resumed["tools"][1]["defer_loading"], true);
    assert_eq!(resumed["tools"][2], json!({"type": "tool_search"}));

    let fallback = translate_gemini_interactions_request_with_continuation(
        &followup, &profile, &cache, false,
    )
    .unwrap();
    assert!(fallback.get("previous_interaction_id").is_none());
    assert_eq!(fallback["tools"].as_array().unwrap().len(), 2);
    assert!(fallback["tools"][1].get("defer_loading").is_none());

    let uncached = translate_gemini_interactions_request(
        &followup,
        &profile,
        &RwLock::new(InteractionContinuationCache::default()),
    )
    .unwrap();
    assert_eq!(uncached["tools"].as_array().unwrap().len(), 2);
    assert!(uncached["tools"][1].get("defer_loading").is_none());
}

#[test]
fn gemini_tool_retrieval_remote_mcp_and_mixed_tools_safety() {
    let mut profile = argon_test_profile();
    profile.model = "gemini-4-neon".into();
    let cache = RwLock::new(InteractionContinuationCache::default());

    // Uniform server-wide defer_loading on mcp_toolset -> forwarded to mcp_server + tool_search added.
    let uniform_request = json!({
        "messages": [{"role": "user", "content": "Check repo"}],
        "mcp_servers": [{
            "type": "url",
            "name": "repo-tools",
            "url": "https://mcp.example.com/mcp"
        }],
        "tools": [{
            "type": "mcp_toolset",
            "mcp_server_name": "repo-tools",
            "default_config": {"enabled": true, "defer_loading": true}
        }]
    });
    let wire = translate_gemini_interactions_request(&uniform_request, &profile, &cache).unwrap();
    let tools = wire["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["type"], "mcp_server");
    assert_eq!(tools[0]["defer_loading"], true);
    assert_eq!(tools[1], json!({"type": "tool_search"}));
    let diag = gemini_interaction_request_diagnostics_for_profile(&uniform_request, &profile).join("\n");
    assert!(!diag.contains("defer_loading"));

    // Mixed per-tool defer_loading on mcp_toolset -> eager + explicit granularity diagnostic.
    let mixed_mcp_request = json!({
        "messages": [{"role": "user", "content": "Check repo"}],
        "mcp_servers": [{
            "type": "url",
            "name": "repo-tools",
            "url": "https://mcp.example.com/mcp"
        }],
        "tools": [{
            "type": "mcp_toolset",
            "mcp_server_name": "repo-tools",
            "default_config": {"enabled": false, "defer_loading": true},
            "configs": {
                "read_issue": {"enabled": true, "defer_loading": false}
            }
        }]
    });
    let wire_mixed =
        translate_gemini_interactions_request(&mixed_mcp_request, &profile, &cache).unwrap();
    assert_eq!(wire_mixed["tools"].as_array().unwrap().len(), 1);
    assert!(wire_mixed["tools"][0].get("defer_loading").is_none());
    let mixed_diag =
        gemini_interaction_request_diagnostics_for_profile(&mixed_mcp_request, &profile).join("\n");
    assert!(mixed_diag.contains("server-wide defer_loading"));

    // Profile remote MCP defer_loading parsing and mixed-tools stripping.
    let raw = json!({
        "capabilities": {
            "gemini_tool_retrieval": true,
            "gemini_remote_mcp_servers": [{
                "name": "catalog_mcp",
                "url": "https://mcp.example.com/catalog",
                "defer_loading": true
            }]
        }
    });
    let caps = parse_openai_capabilities_with_defaults(
        raw.as_object().unwrap(),
        "neon.json",
        OpenAiCapabilities::gemini_interactions(),
    )
    .unwrap();
    assert!(caps.gemini_tool_retrieval);
    assert_eq!(caps.gemini_remote_mcp_servers[0]["defer_loading"], true);

    // tool_search + function alone is NOT treated as mixed server tools.
    let mut only_tr = json!({
        "tools": [
            {"type": "function", "name": "f1", "defer_loading": true},
            {"type": "tool_search"}
        ]
    });
    assert!(!interaction_request_has_mixed_tools(&only_tr));
    // Removing server tools when only mcp_server was deferred also cleans up orphan tool_search.
    let mut mcp_only_deferred = json!({
        "tools": [
            {"type": "function", "name": "f1"},
            {"type": "mcp_server", "name": "m1", "defer_loading": true},
            {"type": "tool_search"}
        ]
    });
    assert!(interaction_request_has_mixed_tools(&mcp_only_deferred));
    assert!(remove_interaction_server_tools(&mut mcp_only_deferred));
    assert_eq!(mcp_only_deferred["tools"], json!([{"type": "function", "name": "f1"}]));
    // Stripping tool retrieval removes tool_search, defer_loading, and short_description.
    assert!(strip_interaction_tool_retrieval(&mut only_tr));
    assert_eq!(only_tr["tools"], json!([{"type": "function", "name": "f1"}]));
}

#[tokio::test]
async fn gemini_tool_retrieval_captures_steps_and_retries_eagerly_on_400() {
    // 1. Response translation & streaming capture tool_search_call / tool_search_result cleanly.
    let upstream = json!({
        "id": "interaction-ts-1",
        "status": "requires_action",
        "steps": [
            {
                "type": "tool_search_call",
                "id": "tsc-1",
                "arguments": {"function_names": ["read_issue"]},
                "signature": "sig-ts-1"
            },
            {
                "type": "tool_search_result",
                "call_id": "tsc-1",
                "result": [{"type": "function", "name": "read_issue"}]
            },
            {
                "type": "function_call",
                "id": "fc-1",
                "name": "read_issue",
                "arguments": {"id": 7}
            }
        ],
        "usage": {
            "total_input_tokens": 200,
            "total_output_tokens": 25,
            "total_tool_use_tokens": 40
        }
    });
    let translated = translate_gemini_interactions_response_for_request(
        &upstream,
        "gemini-4-neon",
        None,
        None,
        true,
    )
    .unwrap();
    assert_eq!(translated.message["stop_reason"], "tool_use");
    assert_eq!(translated.message["content"].as_array().unwrap().len(), 1);
    assert_eq!(translated.message["content"][0]["type"], "tool_use");
    assert_eq!(translated.message["content"][0]["name"], "read_issue");
    assert_eq!(translated.message["usage"]["tool_use_tokens"], 40);
    assert_eq!(
        translated.message["provider_metadata"]["google"]["tool_search_requests"],
        1
    );
    assert_eq!(
        translated.message["provider_metadata"]["google"]["interaction_server_tools"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // 1b. Streaming SSE also captures tool_search_call / tool_search_result cleanly without breaking function_call.
    let sse_body = argon_sse(vec![
        json!({"event_type":"interaction.start","interaction":{"id":"interaction-ts-sse","status":"in_progress"}}),
        json!({"event_type":"step.start","index":0,"step":{"type":"tool_search_call","id":"tsc-sse-1"}}),
        json!({"event_type":"step.delta","index":0,"delta":{"type":"tool_search_call","arguments":{"function_names":["read_issue"]},"signature":"sig-sse-1"}}),
        json!({"event_type":"step.stop","index":0}),
        json!({"event_type":"step.start","index":1,"step":{"type":"tool_search_result","call_id":"tsc-sse-1"}}),
        json!({"event_type":"step.delta","index":1,"delta":{"type":"tool_search_result","result":[{"type":"function","name":"read_issue"}]}}),
        json!({"event_type":"step.stop","index":1}),
        json!({"event_type":"step.start","index":2,"step":{"type":"function_call","id":"fc-sse-1","name":"read_issue","arguments":{"id":9}}}),
        json!({"event_type":"step.stop","index":2}),
        json!({"event_type":"interaction.requires_action","interaction":{"id":"interaction-ts-sse","status":"requires_action","usage":{"total_input_tokens":150,"total_output_tokens":15,"total_tool_use_tokens":30}}}),
    ]);
    let (sse_base, sse_server) = argon_mock_server(Router::new().route(
        "/interactions",
        post(move || {
            let body = sse_body.clone();
            async { body }
        }),
    ))
    .await;
    let mut sse_profile = argon_test_profile();
    sse_profile.model = "gemini-4-neon".into();
    sse_profile.upstream_url = format!("{sse_base}/interactions");
    let sse_cache = Arc::new(RwLock::new(InteractionContinuationCache::default()));
    let sse_resp = forward_gemini_interactions_profile(
        sse_profile,
        json!({"stream":true,"max_tokens":1024,"messages":[{"role":"user","content":"Check issue 9"}]}),
        sse_cache.clone(),
    )
    .await;
    assert_eq!(sse_resp.status(), StatusCode::OK);
    let sse_text = String::from_utf8(
        axum::body::to_bytes(sse_resp.into_body(), 1_000_000)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    let sse_events: Vec<Value> = sse_text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();
    assert!(sse_events.iter().any(|e| {
        e["type"] == "content_block_start"
            && e["content_block"]["type"] == "tool_use"
            && e["content_block"]["name"] == "read_issue"
    }));
    let msg_delta = sse_events
        .iter()
        .find(|e| e["type"] == "message_delta")
        .unwrap();
    assert_eq!(msg_delta["delta"]["stop_reason"], "tool_use");
    assert_eq!(
        msg_delta["delta"]["provider_metadata"]["google"]["tool_search_requests"],
        1
    );
    assert_eq!(msg_delta["usage"]["tool_use_tokens"], 30);
    sse_server.abort();

    // 2. HTTP 400 mentioning defer_loading triggers one eager retry without tool_search/defer_loading.
    let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let seen_handler = seen.clone();
    let router = Router::new().route(
        "/interactions",
        post(move |Json(body): Json<Value>| {
            let seen = seen_handler.clone();
            async move {
                let attempt = {
                    let mut guard = seen.lock().unwrap();
                    guard.push(body);
                    guard.len()
                };
                if attempt == 1 {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": {"message": "defer_loading is not supported in this context"}})),
                    )
                        .into_response()
                } else {
                    Json(json!({
                        "id": "interaction-eager-retry",
                        "status": "completed",
                        "steps": [{"type": "model_output", "content": [{"type": "text", "text": "ok"}]}]
                    }))
                    .into_response()
                }
            }
        }),
    );
    let (base, server) = argon_mock_server(router).await;
    let mut profile = argon_test_profile();
    profile.model = "gemini-4-neon".into();
    profile.upstream_url = format!("{base}/interactions");
    let request = json!({
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{
            "name": "read_issue",
            "description": "Read issue",
            "short_description": "Read",
            "defer_loading": true,
            "input_schema": {"type": "object", "properties": {}}
        }]
    });
    let response = forward_gemini_interactions_profile(
        profile,
        request,
        Arc::new(RwLock::new(InteractionContinuationCache::default())),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let warnings: Vec<&str> = response
        .headers()
        .get_all(BRIDGE_WARNING_HEADER)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    assert!(
        warnings.iter().any(|w| w.contains("eager function exposure")),
        "expected eager fallback warning in {warnings:?}"
    );
    let captured = seen.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0]["tools"][0]["defer_loading"], true);
    assert_eq!(captured[0]["tools"][1], json!({"type": "tool_search"}));
    assert_eq!(captured[1]["tools"].as_array().unwrap().len(), 1);
    assert!(captured[1]["tools"][0].get("defer_loading").is_none());
    assert!(captured[1]["tools"][0].get("short_description").is_none());
    server.abort();
}

#[test]
fn gemini_document_office_mime_slicing_validation_and_inline_tool_results() {
    // 1. All 11 Office/OpenDocument formats + PDF MIME inference.
    for (name, expected_mime) in [
        ("a.pdf", "application/pdf"),
        ("a.docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
        ("a.doc", "application/msword"),
        ("a.dotx", "application/vnd.openxmlformats-officedocument.wordprocessingml.template"),
        ("a.odt", "application/vnd.oasis.opendocument.text"),
        ("a.pptx", "application/vnd.openxmlformats-officedocument.presentationml.presentation"),
        ("a.ppt", "application/vnd.ms-powerpoint"),
        ("a.odp", "application/vnd.oasis.opendocument.presentation"),
        ("a.xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
        ("a.xls", "application/vnd.ms-excel"),
        ("a.ods", "application/vnd.oasis.opendocument.spreadsheet"),
    ] {
        assert_eq!(
            gemini_document_mime_from_path_or_url(&format!("https://example.com/files/{name}?v=1")),
            Some(expected_mime),
            "{name}"
        );
    }

    // 2. Paginated & spreadsheet processing + resolution + file source mapping on Gemini 4 Neon.
    let mut profile = argon_test_profile();
    profile.model = "gemini-4-neon".into();
    let cache = RwLock::new(InteractionContinuationCache::default());
    let request = json!({
        "max_tokens": 1024,
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "document",
                    "resolution": "high",
                    "processing": {
                        "type": "static",
                        "page_options": {
                            "start_page": 2,
                            "end_page": 5,
                            "content_layers": ["page_text", "page_image"]
                        }
                    },
                    "source": {"type": "url", "url": "https://example.com/deck.pptx"}
                },
                {
                    "type": "document",
                    "title": "budget.xlsx",
                    "resolution": "medium",
                    "processing": {
                        "type": "static",
                        "spreadsheet_options": {
                            "sheet_index": 2,
                            "visible_cells": "A1:G40",
                            "content_layers": ["data", "formulas", "visual"]
                        }
                    },
                    "source": {"type": "file", "file_id": "files/sheet123"}
                }
            ]
        }]
    });
    let wire = translate_gemini_interactions_request(&request, &profile, &cache).unwrap();
    let parts = wire["input"][0]["content"].as_array().unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(
        parts[0]["mime_type"],
        "application/vnd.openxmlformats-officedocument.presentationml.presentation"
    );
    assert_eq!(parts[0]["resolution"], "high");
    assert_eq!(
        parts[0]["processing"],
        json!({
            "type": "static",
            "page_options": {
                "start_page": 2,
                "end_page": 5,
                "content_layers": ["page_text", "page_image"]
            }
        })
    );
    assert_eq!(
        parts[1]["uri"],
        "https://generativelanguage.googleapis.com/v1beta/files/sheet123"
    );
    assert_eq!(
        parts[1]["mime_type"],
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    );
    assert_eq!(parts[1]["resolution"], "medium");
    assert_eq!(
        parts[1]["processing"],
        json!({
            "type": "static",
            "spreadsheet_options": {
                "sheet_index": 2,
                "visible_cells": "A1:G40",
                "content_layers": ["data", "formulas", "visual"]
            }
        })
    );

    // 3. Invalid slicing options return clean errors.
    for invalid_doc in [
        json!({"type": "document", "processing": {"type": "static", "page_options": {"start_page": 0}}, "source": {"type": "url", "url": "https://example.com/a.pdf"}}),
        json!({"type": "document", "processing": {"type": "static", "page_options": {"start_page": 5, "end_page": 2}}, "source": {"type": "url", "url": "https://example.com/a.pdf"}}),
        json!({"type": "document", "processing": {"type": "static", "page_options": {"start_page": 1}}, "source": {"type": "url", "url": "https://example.com/a.xlsx"}}),
        json!({"type": "document", "processing": {"type": "static", "spreadsheet_options": {"sheet_index": 1}}, "source": {"type": "url", "url": "https://example.com/a.pdf"}}),
        json!({"type": "document", "processing": {"type": "static", "spreadsheet_options": {"visible_cells": "INVALID!!"}}, "source": {"type": "url", "url": "https://example.com/a.xlsx"}}),
        json!({"type": "document", "resolution": "giant", "source": {"type": "url", "url": "https://example.com/a.pdf"}}),
    ] {
        let bad = json!({"max_tokens": 1024, "messages": [{"role": "user", "content": [invalid_doc]}]});
        assert!(translate_gemini_interactions_request(&bad, &profile, &cache).is_err());
    }

    // 4. Inline tool_result documents stay inside function_result.result on Gemini 4 Neon,
    //    while legacy Gemini 2.5 keeps moving them to a trailing user_input step.
    let tool_doc_request = json!({
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": "Check workbook"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "check", "signature": "sig-1"},
                {"type": "tool_use", "id": "call-doc-1", "name": "read_doc", "input": {}}
            ]},
            {"role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": "call-doc-1",
                "content": [
                    {"type": "text", "text": "Workbook slice"},
                    {
                        "type": "document",
                        "processing": {"type": "static", "spreadsheet_options": {"sheet_index": 1, "visible_cells": "A1:C10"}},
                        "source": {
                            "type": "base64",
                            "media_type": "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                            "data": "eGxzeA=="
                        }
                    }
                ]
            }]}
        ]
    });
    let neon_wire = translate_gemini_interactions_request(&tool_doc_request, &profile, &cache).unwrap();
    let fn_step = neon_wire["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["type"] == "function_result")
        .unwrap();
    assert_eq!(fn_step["result"][0]["type"], "text");
    assert_eq!(fn_step["result"][1]["type"], "document");
    assert_eq!(
        fn_step["result"][1]["mime_type"],
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    );
    assert_eq!(
        fn_step["result"][1]["processing"]["spreadsheet_options"]["visible_cells"],
        "A1:C10"
    );

    let mut legacy = profile.clone();
    legacy.model = "gemini-2.5-pro".into();
    let legacy_wire = translate_gemini_interactions_request(&tool_doc_request, &legacy, &cache).unwrap();
    let legacy_steps = legacy_wire["input"].as_array().unwrap();
    let legacy_fn_pos = legacy_steps
        .iter()
        .position(|s| s["type"] == "function_result")
        .unwrap();
    assert_eq!(legacy_steps[legacy_fn_pos]["result"].as_array().unwrap().len(), 1);
    assert_eq!(legacy_steps[legacy_fn_pos + 1]["type"], "user_input");
    assert_eq!(legacy_steps[legacy_fn_pos + 1]["content"][0]["type"], "document");
}

#[tokio::test]
async fn mcp_inspect_document_uploads_caches_and_expands_into_gemini_function_result() {
    let uploads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let uploads_start = uploads.clone();
    let polls_get = polls.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let base_url = format!("http://{address}/v1beta");
    let upload_session_url = format!("http://{address}/upload-session/1");
    let file_uri = format!("http://{address}/v1beta/files/mockfile1");
    let file_uri_finalize = file_uri.clone();
    let file_uri_poll = file_uri.clone();

    let mock = Router::new()
        .route(
            "/upload/v1beta/files",
            post(move |headers: HeaderMap| {
                let uploads = uploads_start.clone();
                let session_url = upload_session_url.clone();
                async move {
                    assert_eq!(
                        headers.get("x-goog-api-key").and_then(|v| v.to_str().ok()),
                        Some("doc-secret")
                    );
                    assert_eq!(
                        headers
                            .get("x-goog-upload-protocol")
                            .and_then(|v| v.to_str().ok()),
                        Some("resumable")
                    );
                    assert_eq!(
                        headers
                            .get("x-goog-upload-command")
                            .and_then(|v| v.to_str().ok()),
                        Some("start")
                    );
                    assert_eq!(
                        headers
                            .get("x-goog-upload-header-content-type")
                            .and_then(|v| v.to_str().ok()),
                        Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet")
                    );
                    uploads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let mut resp = StatusCode::OK.into_response();
                    resp.headers_mut().insert(
                        "x-goog-upload-url",
                        HeaderValue::from_str(&session_url).unwrap(),
                    );
                    resp
                }
            }),
        )
        .route(
            "/upload-session/1",
            post(move |headers: HeaderMap, body: Bytes| {
                let uri = file_uri_finalize.clone();
                async move {
                    assert_eq!(
                        headers
                            .get("x-goog-upload-command")
                            .and_then(|v| v.to_str().ok()),
                        Some("upload, finalize")
                    );
                    assert_eq!(body.as_ref(), b"PK\x03\x04mock-xlsx");
                    Json(json!({
                        "file": {
                            "name": "files/mockfile1",
                            "uri": uri,
                            "mimeType": "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                            "state": "PROCESSING"
                        }
                    }))
                }
            }),
        )
        .route(
            "/v1beta/files/mockfile1",
            get(move || {
                let polls = polls_get.clone();
                let uri = file_uri_poll.clone();
                async move {
                    polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Json(json!({
                        "name": "files/mockfile1",
                        "uri": uri,
                        "mimeType": "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                        "state": "ACTIVE"
                    }))
                }
            }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

    let dir = env::temp_dir().join(format!("claude-bridge-doc-{}", Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    let xlsx_path = dir.join("quarterly.xlsx");
    fs::write(&xlsx_path, b"PK\x03\x04mock-xlsx").unwrap();

    let mut profile = argon_test_profile();
    profile.model = "gemini-4-neon".into();
    profile.base_url = base_url.clone();
    profile.upstream_url = format!("{base_url}/interactions");
    profile.api_key = Some("doc-secret".into());
    let state = probe_state(
        vec![profile.clone()],
        profile.file_name.clone(),
        None,
        "http://unused.invalid".into(),
    );

    // tools/list includes inspect_document
    let list_resp = mcp(
        State(state.clone()),
        HeaderMap::new(),
        Json(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})),
    )
    .await;
    let list_body: Value = serde_json::from_slice(
        &axum::body::to_bytes(list_resp.into_body(), 1_000_000)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(list_body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == "inspect_document"));

    // First call uploads + polls PROCESSING->ACTIVE
    let call_1 = mcp(
        State(state.clone()),
        HeaderMap::new(),
        Json(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "inspect_document",
                "arguments": {
                    "path": xlsx_path.to_string_lossy(),
                    "sheet_index": 1,
                    "content_layers": ["data", "formulas"]
                }
            }
        })),
    )
    .await;
    let body_1: Value = serde_json::from_slice(
        &axum::body::to_bytes(call_1.into_body(), 1_000_000)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body_1["result"]["isError"], false);
    assert_eq!(body_1["result"]["structuredContent"]["cached"], false);
    assert_eq!(
        body_1["result"]["structuredContent"]["document"]["uri"],
        file_uri
    );
    assert_eq!(uploads.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(polls.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Second call with narrower cell range & visual layer hits SHA-256 cache without re-uploading!
    let call_2 = mcp(
        State(state.clone()),
        HeaderMap::new(),
        Json(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "inspect_document",
                "arguments": {
                    "path": xlsx_path.to_string_lossy(),
                    "sheet_index": 2,
                    "visible_cells": "A1:F25",
                    "content_layers": ["data", "visual"],
                    "resolution": "high"
                }
            }
        })),
    )
    .await;
    let body_2: Value = serde_json::from_slice(
        &axum::body::to_bytes(call_2.into_body(), 1_000_000)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body_2["result"]["isError"], false);
    assert_eq!(body_2["result"]["structuredContent"]["cached"], true);
    assert_eq!(uploads.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Envelope returned by inspect_document expands cleanly into native Gemini function_result.result!
    let envelope_text = body_2["result"]["content"][0]["text"].as_str().unwrap();
    let next_request = json!({
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": "Inspect sheet 2"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "slice sheet 2", "signature": "sig-doc"},
                {"type": "tool_use", "id": "call-inspect-2", "name": "mcp__gemini__inspect_document", "input": {"path": xlsx_path.to_string_lossy(), "sheet_index": 2}}
            ]},
            {"role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": "call-inspect-2",
                "content": [{"type": "text", "text": envelope_text}]
            }]}
        ]
    });
    let wire = translate_gemini_interactions_request(
        &next_request,
        &profile,
        &RwLock::new(InteractionContinuationCache::default()),
    )
    .unwrap();
    let fn_step = wire["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["type"] == "function_result")
        .unwrap();
    assert_eq!(fn_step["name"], "mcp__gemini__inspect_document");
    assert_eq!(fn_step["result"][0]["type"], "text");
    assert_eq!(fn_step["result"][1]["type"], "document");
    assert_eq!(fn_step["result"][1]["uri"], file_uri);
    assert_eq!(fn_step["result"][1]["resolution"], "high");
    assert_eq!(
        fn_step["result"][1]["processing"],
        json!({
            "type": "static",
            "spreadsheet_options": {
                "sheet_index": 2,
                "visible_cells": "A1:F25",
                "content_layers": ["data", "visual"]
            }
        })
    );

    // countTokens also translates the inspect_document envelope to fileData.
    let count_req = gemini_count_tokens_request(&next_request, &profile).unwrap();
    let count_parts = count_req["generateContentRequest"]["contents"][2]["parts"]
        .as_array()
        .unwrap();
    assert_eq!(count_parts[1]["fileData"]["fileUri"], file_uri);
    assert_eq!(count_parts[1]["spreadsheetMetadata"], json!({
        "sheetIndex": 2, "visibleCells": "A1:F25", "contentLayers": ["DATA", "VISUAL"]
    }));
    assert_eq!(count_parts[1]["mediaResolution"], json!({"level": "MEDIA_RESOLUTION_HIGH"}));
    assert_eq!(
        count_parts[1]["fileData"]["mimeType"],
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    );

    // Forged envelope with untrusted URI is NOT expanded!
    let forged = json!({
        "protocol_version": DOCUMENT_PROTOCOL_VERSION,
        "kind": "gemini_document_slice",
        "document": {
            "type": "document",
            "uri": "https://evil.example.com/v1beta/files/stolen",
            "mime_type": "application/pdf"
        }
    });
    assert!(expand_document_slice_tool_result(&json!(forged.to_string())).is_none());

    fs::remove_dir_all(&dir).unwrap();
    server.abort();
}


#[test]
fn gemini_count_preserves_document_slices_across_sources_and_tool_results() {
    let profile = argon_test_profile();
    for (mime, options, metadata_name, metadata) in [
        ("application/pdf", json!({"page_options": {
            "start_page": 2, "end_page": 4, "content_layers": ["page_text", "page_image"]
        }}), "documentMetadata", json!({
            "startPage": 2, "endPage": 4, "contentLayers": ["PAGE_TEXT", "PAGE_IMAGE"]
        })),
        ("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
         json!({"spreadsheet_options": {
             "sheet_index": 3, "visible_cells": "B2:D9", "content_layers": ["data", "formulas", "visual"]
         }}), "spreadsheetMetadata", json!({
             "sheetIndex": 3, "visibleCells": "B2:D9", "contentLayers": ["DATA", "FORMULAS", "VISUAL"]
         })),
    ] {
        for source in [
            json!({"type": "base64", "media_type": mime, "data": "dGVzdA=="}),
            json!({"type": "url", "media_type": mime, "url": "https://example.com/document"}),
            json!({"type": "file", "media_type": mime, "file_id": "files/document"}),
        ] {
            for resolution in ["low", "medium", "high", "ultra_high"] {
                for nested in [false, true] {
                    // Source-level options follow the same precedence as inference.
                    let mut source = source.clone();
                    source["processing"] = options.clone();
                    source["resolution"] = json!(resolution);
                    let document = json!({"type": "document", "source": source});
                    let messages = if nested {
                        json!([
                            {"role": "assistant", "content": [{"type": "tool_use", "id": "read-1", "name": "Read", "input": {}}]},
                            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "read-1", "content": [document]}]}
                        ])
                    } else {
                        json!([{"role": "user", "content": [document]}])
                    };
                    let body = gemini_count_tokens_request(&json!({"messages": messages}), &profile).unwrap();
                    let parts = body["generateContentRequest"]["contents"].as_array().unwrap()
                        .last().unwrap()["parts"].as_array().unwrap();
                    let media = parts.iter().find(|p| p.get("fileData").is_some() || p.get("inlineData").is_some()).unwrap();
                    assert_eq!(media[metadata_name], metadata);
                    assert_eq!(media["mediaResolution"]["level"], format!("MEDIA_RESOLUTION_{}", resolution.to_ascii_uppercase()));
                    assert!(media.get("processing").is_none());
                    assert!(media.get("resolution").is_none());
                }
            }
        }
    }
}

#[test]
fn gemini_count_document_defaults_precedence_and_validation_match_inference() {
    let profile = argon_test_profile();
    let source = json!({"type": "url", "url": "https://example.com/report.pdf"});
    let request = |document: Value| json!({"messages": [{"role": "user", "content": [document]}]});
    let body = gemini_count_tokens_request(&request(json!({"type": "document", "source": source})), &profile).unwrap();
    assert_eq!(body["generateContentRequest"]["contents"][0]["parts"][0], json!({
        "fileData": {"mimeType": "application/pdf", "fileUri": "https://example.com/report.pdf"}
    }));
    let body = gemini_count_tokens_request(&request(json!({
        "type": "document", "source": {"type": "url", "url": "https://example.com/report.pdf", "resolution": "low"},
        "resolution": "HIGH", "page_options": {"start_page": 5, "content_layers": ["PAGE_TEXT"]}
    })), &profile).unwrap();
    let part = &body["generateContentRequest"]["contents"][0]["parts"][0];
    assert_eq!(part["mediaResolution"]["level"], "MEDIA_RESOLUTION_HIGH");
    assert_eq!(part["documentMetadata"], json!({"startPage": 5, "contentLayers": ["PAGE_TEXT"]}));
    for invalid in [
        json!({"type": "document", "source": source, "resolution": "invalid"}),
        json!({"type": "document", "source": source, "page_options": {"start_page": 0}}),
        json!({"type": "document", "source": source, "spreadsheet_options": {"sheet_index": 1}}),
    ] {
        assert!(gemini_count_tokens_request(&request(invalid.clone()), &profile).is_err());
        let nested = json!({"messages": [
            {"role": "assistant", "content": [{"type": "tool_use", "id": "read-1", "name": "Read", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "read-1", "content": [invalid]}]}
        ]});
        assert!(gemini_count_tokens_request(&nested, &profile).is_err());
    }
}
