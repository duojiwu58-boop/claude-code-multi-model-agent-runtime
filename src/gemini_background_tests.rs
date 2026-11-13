fn argon_test_profile() -> ProviderProfile {
    let mut profile = interactions_test_profile("argon-background.json");
    profile.model = "gemini-4-argon".to_string();
    profile.openai_capabilities.max_output_tokens = None;
    profile.openai_capabilities.gemini_builtin_tools.clear();
    profile
}

async fn argon_mock_server(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (base, task)
}

fn argon_sse(events: Vec<Value>) -> String {
    events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

#[test]
fn gemini_argon_background_budget_and_storage_contract() {
    let mut profile = argon_test_profile();
    let cache = RwLock::new(InteractionContinuationCache::default());
    let request = json!({"max_tokens":262144,"messages":[{"role":"user","content":"Prove it"}]});
    for stream in [false, true] {
        let mut request = request.clone();
        request["stream"] = json!(stream);
        let body = translate_gemini_interactions_request(&request, &profile, &cache).unwrap();
        assert_eq!(body["background"], true);
        assert_eq!(body["generation_config"]["max_output_tokens"], 262144);
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
}

#[test]
fn gemini_argon_unified_context_reserves_output_and_rejects_exhaustion() {
    let mut profile = argon_test_profile();
    let mut body = json!({"background":true,"generation_config":{"max_output_tokens":262144}});
    assert!(
        apply_gemini_4_context_budget(&profile, 1_000_000, &mut body)
            .unwrap()
            .is_some()
    );
    assert_eq!(body["generation_config"]["max_output_tokens"], 48576);
    assert!(body.get("background").is_none());
    assert!(apply_gemini_4_context_budget(&profile, 1_048_576, &mut body).is_err());
    profile.context_window = Some(1000);
    assert!(apply_gemini_4_context_budget(&profile, 1000, &mut body).is_err());
    body["generation_config"]["max_output_tokens"] = json!(0);
    assert!(apply_gemini_4_context_budget(&profile, 10, &mut body).is_err());
}

#[tokio::test]
async fn gemini_argon_native_context_preflight_uses_actual_count() {
    let router = Router::new().route(
        "/models/gemini-4-argon:countTokens",
        post(|| async { Json(json!({"totalTokens":1_000_000})) }),
    );
    let (base, server) = argon_mock_server(router).await;
    let mut profile = argon_test_profile();
    profile.base_url = base.clone();
    profile.upstream_url = format!("{base}/interactions");
    let request = json!({"max_tokens":1_048_576,"messages":[{"role":"user","content":"short"}]});
    let mut body = translate_gemini_interactions_request(
        &request,
        &profile,
        &RwLock::new(InteractionContinuationCache::default()),
    )
    .unwrap();
    let mut diagnostics = Vec::new();
    prepare_gemini_4_context_budget(&profile, &request, &mut body, &mut diagnostics)
        .await
        .unwrap();
    assert_eq!(body["generation_config"]["max_output_tokens"], 48576);
    assert!(diagnostics.join(" ").contains("1000000 input"));
    server.abort();
}

#[tokio::test]
async fn gemini_argon_background_polling_joins_hops_and_separates_billed_input() {
    let router = Router::new()
        .route("/interactions", post(|Json(body): Json<Value>| async move {
            assert_eq!(body["background"], true);
            assert_eq!(body["generation_config"]["max_output_tokens"], 262144);
            Json(json!({"id":"job","status":"queued"}))
        }))
        .route("/interactions/job", get(|| async { Json(json!({
            "id":"job","status":"completed","steps":[
                {"type":"user_input","content":[{"type":"text","text":"must not echo"}]},
                {"type":"model_output","content":[{"type":"text","text":"first"}]},
                {"type":"model_output","content":[{"type":"text","text":"second"}]}
            ],"usage":{"total_input_tokens":200000,"total_cached_tokens":150000,"total_output_tokens":200,"total_thought_tokens":100}
        })) }));
    let (base, server) = argon_mock_server(router).await;
    let mut profile = argon_test_profile();
    profile.upstream_url = format!("{base}/interactions");
    let response = forward_gemini_interactions_profile(
        profile,
        json!({"max_tokens":262144,"messages":[{"role":"user","content":"prove"}]}),
        Arc::new(RwLock::new(InteractionContinuationCache::default())),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["content"][0]["text"], "first");
    assert_eq!(value["content"][1]["text"], "second");
    assert_eq!(value["usage"]["billed_input_tokens"], 200000);
    assert_eq!(value["usage"]["billed_cache_read_input_tokens"], 150000);
    assert!(value["usage"].get("cache_read_input_tokens").is_none());
    assert!(value["usage"]["input_tokens"].as_u64().unwrap() < 100);
    assert_eq!(value["usage"]["output_tokens"], 300);
    server.abort();
}

#[tokio::test]
async fn gemini_argon_background_resume_preserves_cursor_and_deduplicates() {
    let replayed = json!({"event_type":"step.delta","event_id":"e3","index":0,"delta":{"type":"text","text":"first"}});
    let first = argon_sse(vec![
        json!({"event_type":"interaction.created","event_id":"e0","interaction":{"id":"job","status":"in_progress"}}),
        json!({"event_type":"step.start","event_id":"e1","index":0,"step":{"type":"model_output"}}),
        replayed.clone(),
        // Checkpoint can be carried by status, not just text.
        json!({"event_type":"interaction.status_update","event_id":"e4","status":"in_progress"}),
    ]);
    let second = argon_sse(vec![
        replayed,
        json!({"event_type":"interaction.status_update","event_id":"e4","status":"in_progress"}),
        json!({"event_type":"step.delta","event_id":"e5","index":0,"delta":{"type":"text","text":"second"}}),
        json!({"event_type":"step.stop","event_id":"e6","index":0,"usage":{"total_input_tokens":100000,"total_output_tokens":10}}),
        json!({"event_type":"interaction.status_update","event_id":"e7","status":"completed","usage":{"total_input_tokens":200000,"total_output_tokens":20,"total_thought_tokens":5}}),
    ]);
    let router = Router::new()
        .route(
            "/interactions",
            post(move || {
                let body = first.clone();
                async { body }
            }),
        )
        .route(
            "/interactions/job",
            get(move |uri: Uri, headers: HeaderMap| {
                let body = second.clone();
                async move {
                    assert_eq!(headers.get("x-goog-api-key").unwrap(), "test-key");
                    assert!(uri.query().unwrap().contains("last_event_id=e4"));
                    assert!(uri.query().unwrap().contains("stream=true"));
                    body
                }
            }),
        );
    let (base, server) = argon_mock_server(router).await;
    let mut profile = argon_test_profile();
    profile.upstream_url = format!("{base}/interactions");
    let response = forward_gemini_interactions_profile(
        profile,
        json!({"stream":true,"max_tokens":262144,"messages":[{"role":"user","content":"prove"}]}),
        Arc::new(RwLock::new(InteractionContinuationCache::default())),
    )
    .await;
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert_eq!(text.matches("\"text\":\"first\"").count(), 1, "{text}");
    assert_eq!(text.matches("\"text\":\"second\"").count(), 1, "{text}");
    assert!(text.contains("message_stop"), "{text}");
    assert!(text.contains("\"output_tokens\":25"), "{text}");
    assert!(!text.contains("event: error"), "{text}");
    server.abort();
}

#[tokio::test]
async fn gemini_argon_background_drop_cancels_only_active_job() {
    let cancelled = Arc::new(tokio::sync::Notify::new());
    let notice = cancelled.clone();
    let router = Router::new().route(
        "/interactions/job/cancel",
        post(move || {
            let notice = notice.clone();
            async move {
                notice.notify_one();
                Json(json!({"status":"cancelled"}))
            }
        }),
    );
    let (base, server) = argon_mock_server(router).await;
    let mut profile = argon_test_profile();
    profile.upstream_url = format!("{base}/interactions");
    let transport = GeminiBackgroundTransport::new(&profile);
    drop(GeminiBackgroundGuard {
        transport: transport.clone(),
        id: Some("job".into()),
        terminal: true,
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(30), cancelled.notified())
            .await
            .is_err()
    );
    drop(GeminiBackgroundGuard {
        transport,
        id: Some("job".into()),
        terminal: false,
    });
    tokio::time::timeout(Duration::from_secs(2), cancelled.notified())
        .await
        .unwrap();
    server.abort();
}

#[test]
fn gemini_argon_terminal_status_preserves_tools_and_incomplete() {
    for (status, expected) in [
        ("requires_action", "tool_use"),
        ("incomplete", "max_tokens"),
    ] {
        let value = json!({"id":"job","status":status,"steps":[],"usage":{}});
        let translated = translate_gemini_interactions_response(&value, "gemini-4-argon").unwrap();
        assert_eq!(translated.message["stop_reason"], expected);
    }
    for status in ["failed", "cancelled"] {
        assert!(translate_gemini_interactions_response(
            &json!({"id":"job","status":status,"steps":[]}),
            "gemini-4-argon"
        )
        .is_err());
    }
}

#[tokio::test]
async fn gemini_argon_background_partial_record_replays_tool_once() {
    let mut first = argon_sse(vec![
        json!({"event_type":"interaction.created","event_id":"e0","interaction":{"id":"job","status":"in_progress"}}),
        json!({"event_type":"step.start","event_id":"e1","index":0,"step":{"type":"function_call","id":"call","name":"read_file"}}),
    ]);
    first.push_str("data: {\"event_type\":\"step.delta\",\"delta\":");
    let second = argon_sse(vec![
        json!({"event_type":"step.delta","event_id":"e2","index":0,"delta":{"type":"arguments_delta","arguments":"{\"path\":\"Cargo.toml\"}"}}),
        json!({"event_type":"step.stop","event_id":"e3","index":0}),
        json!({"event_type":"interaction.status_update","event_id":"e4","status":"requires_action"}),
    ]);
    let router = Router::new()
        .route(
            "/interactions",
            post(move || {
                let body = first.clone();
                async { body }
            }),
        )
        .route(
            "/interactions/job",
            get(move |uri: Uri| {
                let body = second.clone();
                async move {
                    assert!(uri.query().unwrap().contains("last_event_id=e1"));
                    body
                }
            }),
        );
    let (base, server) = argon_mock_server(router).await;
    let mut profile = argon_test_profile();
    profile.upstream_url = format!("{base}/interactions");
    let response = forward_gemini_interactions_profile(profile,
        json!({"stream":true,"max_tokens":262144,"messages":[{"role":"user","content":"read"}],
            "tools":[{"name":"read_file","input_schema":{"type":"object","properties":{"path":{"type":"string"}}}}]}),
        Arc::new(RwLock::new(InteractionContinuationCache::default()))).await;
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert_eq!(text.matches("\"name\":\"read_file\"").count(), 1, "{text}");
    assert_eq!(text.matches("Cargo.toml").count(), 1, "{text}");
    assert!(text.contains("\"stop_reason\":\"tool_use\""), "{text}");
    assert!(!text.contains("event: error"), "{text}");
    server.abort();
}

#[tokio::test]
async fn gemini_argon_background_missing_checkpoint_is_error_not_duplicate_create() {
    let cancelled = Arc::new(tokio::sync::Notify::new());
    let notice = cancelled.clone();
    let body = argon_sse(vec![
        json!({"event_type":"interaction.created","interaction":{"id":"job","status":"in_progress"}}),
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
    assert!(bytes
        .next()
        .await
        .unwrap()
        .unwrap_err()
        .contains("safe event checkpoint"));
    drop(bytes);
    tokio::time::timeout(Duration::from_secs(2), cancelled.notified())
        .await
        .unwrap();
    server.abort();
}

#[tokio::test]
async fn gemini_argon_background_reconnect_exhaustion_cancels_job() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = attempts.clone();
    let cancelled = Arc::new(tokio::sync::Notify::new());
    let notice = cancelled.clone();
    let body = argon_sse(vec![
        json!({"event_type":"interaction.created","event_id":"checkpoint","interaction":{"id":"job","status":"in_progress"}}),
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
            "/interactions/job",
            get(move || {
                let count = count.clone();
                async move {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    StatusCode::SERVICE_UNAVAILABLE
                }
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
    assert!(bytes
        .next()
        .await
        .unwrap()
        .unwrap_err()
        .contains("reconnect limit"));
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        GEMINI_BACKGROUND_RETRIES
    );
    drop(bytes);
    tokio::time::timeout(Duration::from_secs(2), cancelled.notified())
        .await
        .unwrap();
    server.abort();
}

#[tokio::test]
async fn gemini_argon_background_dropping_stream_cancels_upstream() {
    let cancelled = Arc::new(tokio::sync::Notify::new());
    let notice = cancelled.clone();
    let body = argon_sse(vec![
        json!({"event_type":"interaction.created","event_id":"e0","interaction":{"id":"job","status":"in_progress"}}),
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
    drop(bytes);
    tokio::time::timeout(Duration::from_secs(2), cancelled.notified())
        .await
        .unwrap();
    server.abort();
}
