fn api_test_state(p: ProviderProfile) -> Arc<AppState> {
    let (shutdown_tx, _) = tokio::sync::watch::channel(false);
    Arc::new(AppState {
        ls_lifecycle: Default::default(),
        gemini_transport: Arc::new(RwLock::new(GeminiTransport {
            client: Client::new(),
            proxy_url: None,
        })),
        gemini_thinking_level: Arc::new(RwLock::new(None)),
        reasoning_efforts: Arc::new(RwLock::new(HashMap::new())),
        fallback_api_key: None,
        local_auth_token: "test-local-token-that-is-long-enough-1234567890".into(),
        upstream_url: "https://example.invalid".into(),
        model: "test-model".into(),
        thought_signatures: Arc::new(RwLock::new(IndexMap::new())),
        interaction_continuations: Arc::new(RwLock::new(InteractionContinuationCache::default())),
        computer: Arc::new(ComputerBroker),
        vision_cache: Arc::new(tokio::sync::Mutex::new(IndexMap::new())),
        gemini_file_cache: Arc::new(tokio::sync::Mutex::new(IndexMap::new())),
        routing: Arc::new(RwLock::new(ProviderRoutingState {
            active_file: p.file_name.clone(),
            profiles: vec![p],
            source: ProviderProfileSource::Native,
        })),
        shutdown_tx,
        settings_dir: PathBuf::new(),
        providers_dir: PathBuf::new(),
        bridge_state_path: env::temp_dir().join(format!("bridge-api-test-{}.json", Uuid::new_v4())),
        image_output_dir: PathBuf::new(),
        image_model: String::new(),
        image_upstream_url: String::new(),
        local_bridge_base_url: String::new(),
        admin_state_lock: Arc::new(tokio::sync::Mutex::new(())),
    })
}
fn api_cleanup(state: &AppState) {
    let path = api_store_path(state);
    if path.exists() {
        fs::remove_file(path).unwrap();
    }
}
async fn api_json(response: Response) -> Value {
    assert!(response.status().is_success(), "{}", response.status());
    body(response).await
}

#[tokio::test]
async fn openai_api_responses_store_parent_delete_and_profile_isolation() {
    let mut p = profile("session");
    p.codex.as_mut().unwrap().native_continuation = true;
    let state = api_test_state(p.clone());
    let first = api_json(
        responses(
            State(state.clone()),
            HeaderMap::new(),
            Json(json!({"model":"test-model","input":"hello"})),
        )
        .await,
    )
    .await;
    assert_eq!(first["object"], "response");
    assert_eq!(first["status"], "completed");
    assert!(first["output"][0]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("turn 1"));
    let id = first["id"].as_str().unwrap().to_owned();
    assert_eq!(
        api_json(api_response_get(State(state.clone()), axum::extract::Path(id.clone())).await)
            .await,
        first
    );
    let second=api_json(responses(State(state.clone()),HeaderMap::new(),Json(json!({"model":"test-model","previous_response_id":id,"input":"next","store":false}))).await).await;
    assert!(second["output"][0]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("turn 2"));
    assert_eq!(
        api_response_get(
            State(state.clone()),
            axum::extract::Path(second["id"].as_str().unwrap().into())
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    let mut other = p.clone();
    other.file_name = "other.json".into();
    assert!(
        ApiContext::prepare(state.clone(), &other, json!({"previous_response_id":id}))
            .err()
            .unwrap()
            .contains("different provider")
    );
    let inputs = api_json(
        api_response_inputs(
            State(state.clone()),
            axum::extract::Path(id.clone()),
            axum::extract::Query(HashMap::new()),
        )
        .await,
    )
    .await;
    assert_eq!(inputs["data"][0]["content"], "hello");
    api_json(api_response_delete(State(state.clone()), axum::extract::Path(id.clone())).await)
        .await;
    assert_eq!(
        api_response_get(State(state.clone()), axum::extract::Path(id))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    api_cleanup(&state);
}

#[tokio::test]
async fn openai_api_streams_real_deltas_usage_tools_and_terminal_events() {
    let state = api_test_state(profile("normal"));
    let response = responses(
        State(state.clone()),
        HeaderMap::new(),
        Json(json!({"model":"test-model","input":"hello","stream":true})),
    )
    .await;
    let bytes = axum::body::to_bytes(response.into_body(), MAX_UPSTREAM_RESPONSE_BYTES)
        .await
        .unwrap();
    let payloads = SseDataDecoder::default().push_bytes(&bytes).unwrap();
    let events: Vec<Value> = payloads
        .iter()
        .map(|p| serde_json::from_str(p).unwrap())
        .collect();
    assert_eq!(events[0]["type"], "response.created");
    assert!(events
        .iter()
        .any(|e| e["type"] == "response.output_text.delta" && e["delta"] == "你好，Codex。"));
    for (i, e) in events.iter().enumerate() {
        assert_eq!(e["sequence_number"], i);
    }
    let final_response = &events.last().unwrap()["response"];
    assert_eq!(final_response["status"], "completed");
    assert_eq!(final_response["usage"]["input_tokens"], 42);
    assert_eq!(
        api_json(
            api_response_get(
                State(state.clone()),
                axum::extract::Path(final_response["id"].as_str().unwrap().into())
            )
            .await
        )
        .await,
        *final_response
    );
    let chat=api_chat(State(state.clone()),Json(json!({"model":"test-model","messages":[{"role":"user","content":"read"}],"stream":true,"stream_options":{"include_usage":true},"tools":[{"type":"function","function":{"name":"Read","parameters":{"type":"object","properties":{"file_path":{"type":"string"}}}}}]}))).await;
    let bytes = axum::body::to_bytes(chat.into_body(), MAX_UPSTREAM_RESPONSE_BYTES)
        .await
        .unwrap();
    let chunks = SseDataDecoder::default().push_bytes(&bytes).unwrap();
    assert_eq!(chunks.last().unwrap(), "[DONE]");
    let chunks: Vec<Value> = chunks[..chunks.len() - 1]
        .iter()
        .map(|p| serde_json::from_str(p).unwrap())
        .collect();
    assert!(chunks
        .iter()
        .any(|c| c.pointer("/choices/0/delta/tool_calls/0/function/name") == Some(&json!("Read"))));
    assert!(chunks
        .iter()
        .any(|c| c.pointer("/choices/0/finish_reason") == Some(&json!("tool_calls"))));
    assert!(chunks.last().unwrap()["usage"].is_object());
    api_cleanup(&state);
}

#[tokio::test]
async fn openai_api_function_roundtrip_preserves_native_callback() {
    let mut p = profile("continuation");
    p.codex.as_mut().unwrap().native_continuation = true;
    let state = api_test_state(p);
    let mut request = json!({"model":"test-model","input":"Read the file","tools":[{"type":"function","name":"Read","parameters":{"type":"object","properties":{"file_path":{"type":"string"}}}}]});
    let first = api_json(
        responses(
            State(state.clone()),
            HeaderMap::new(),
            Json(request.clone()),
        )
        .await,
    )
    .await;
    let call = first["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call")
        .unwrap();
    request["previous_response_id"] = first["id"].clone();
    request["input"] =
        json!([{"type":"function_call_output","call_id":call["call_id"],"output":"file contents"}]);
    let next =
        api_json(responses(State(state.clone()), HeaderMap::new(), Json(request)).await).await;
    assert_eq!(
        next["output"][0]["content"][0]["text"],
        "Native result: file contents"
    );
    api_cleanup(&state);
}

#[tokio::test]
async fn openai_api_conversation_lifecycle_conflicts_and_pagination() {
    let state = api_test_state(profile("normal"));
    let conversation=api_json(api_conversation_create(State(state.clone()),Json(json!({"metadata":{"name":"test"},"items":[{"role":"user","content":"first"},{"role":"assistant","content":"second"}]}))).await).await;
    let id = conversation["id"].as_str().unwrap().to_owned();
    let ctx = ApiContext::prepare(
        state.clone(),
        &profile("normal"),
        json!({"conversation":id,"input":"third","model":"test-model"}),
    )
    .unwrap();
    assert!(ApiContext::prepare(
        state.clone(),
        &profile("normal"),
        json!({"conversation":id})
    )
    .is_err());
    assert_eq!(
        api_conversation_delete(State(state.clone()), axum::extract::Path(id.clone()))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    drop(ctx);
    let items = api_json(
        api_conversation_items(
            State(state.clone()),
            axum::extract::Path(id.clone()),
            axum::extract::Query(HashMap::from([
                ("order".into(), "asc".into()),
                ("limit".into(), "1".into()),
            ])),
        )
        .await,
    )
    .await;
    assert_eq!(items["data"][0]["content"], "first");
    assert_eq!(items["has_more"], true);
    let first_id = items["first_id"].as_str().unwrap().to_owned();
    let next = api_json(
        api_conversation_items(
            State(state.clone()),
            axum::extract::Path(id.clone()),
            axum::extract::Query(HashMap::from([
                ("order".into(), "asc".into()),
                ("after".into(), first_id.clone()),
            ])),
        )
        .await,
    )
    .await;
    assert_eq!(next["data"][0]["content"], "second");
    api_json(
        api_conversation_item_delete(
            State(state.clone()),
            axum::extract::Path((id.clone(), first_id.clone())),
        )
        .await,
    )
    .await;
    assert_eq!(
        api_conversation_item_get(
            State(state.clone()),
            axum::extract::Path((id.clone(), first_id))
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    api_json(api_conversation_delete(State(state.clone()), axum::extract::Path(id.clone())).await)
        .await;
    assert_eq!(
        api_conversation_get(State(state.clone()), axum::extract::Path(id))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    api_cleanup(&state);
}

#[tokio::test]
async fn openai_api_errors_are_standard_and_do_not_report_completion() {
    let state = api_test_state(profile("failed"));
    let r = responses(
        State(state.clone()),
        HeaderMap::new(),
        Json(json!({"model":"test-model","input":"x","stream":true,"store":false})),
    )
    .await;
    let bytes = axum::body::to_bytes(r.into_body(), MAX_UPSTREAM_RESPONSE_BYTES)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("response.failed"));
    assert!(!text.contains("response.completed"));
    for req in [
        json!({"model":"wrong","input":"x"}),
        json!({"model":"test-model","input":"x","max_output_tokens":1,"bridge_controls":{"mode":"strict"}}),
        json!({"model":"test-model","input":"x","tools":[{"type":"web_search"}]}),
    ] {
        let response = responses(State(state.clone()), HeaderMap::new(), Json(req)).await;
        assert!(response.status().is_client_error());
        assert!(body(response).await["error"]["message"].is_string());
    }
    assert_eq!(
        api_compact(State(state.clone()), Json(json!({})))
            .await
            .status(),
        StatusCode::NOT_IMPLEMENTED
    );
    assert_eq!(
        api_input_tokens(State(state.clone()), Json(json!({})))
            .await
            .status(),
        StatusCode::NOT_IMPLEMENTED
    );
    assert!(!api_store_path(&state).exists());
}

#[test]
fn openai_api_documents_json_schema_and_unsupported_controls() {
    for value in [json!([false]), json!([3]), json!([null]), json!(["bad"])] {
        assert!(api_input_items(&value).is_err());
    }
    assert!(api_metadata(&json!([])).is_err());
    assert!(api_usage(&json!({"bridge_usage_complete":false})).is_null());
    let request = json!({"model":"test-model","input":[{"role":"user","content":[{"type":"input_file","filename":"note.txt","file_data":"data:text/plain;base64,YWJj"}]}],"text":{"format":{"type":"json_schema","name":"answer","strict":true,"schema":{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}}}});
    let items = api_input_items(&request["input"]).unwrap();
    let native = api_responses_request(&request, &items, "test").unwrap();
    let prepared = prepare_codex_request(&native, &profile("normal")).unwrap();
    assert!(prepared.history[0]["content"][0]["text"]
        .as_str()
        .unwrap()
        .ends_with("abc"));
    assert!(prepared.output_schema.is_some());
    assert!(api_content(
        &json!([{"type":"input_file","file_data":"data:application/pdf;base64,YWJj"}])
    )
    .is_err());
    assert!(api_chat_request(&json!({"model":"test-model","messages":[],"stream":"yes"})).is_err());
    assert!(api_responses_request(
        &json!({"model":"test-model","include":"bad"}),
        &items,
        "test"
    )
    .is_err());
}

#[tokio::test]
async fn openai_api_native_profiles_proxy_resources_and_keep_auth_boundary() {
    let mock = Router::new().fallback(|request: Request| async move {
        assert_eq!(request.headers()["authorization"], "Bearer provider-secret");
        let path = request.uri().to_string();
        let method = request.method().to_string();
        let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let input = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice::<Value>(&bytes).unwrap()
        };
        Json(json!({"path":path,"method":method,"input":input}))
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upstream = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let mut p = profile("normal");
    p.transport = ProviderTransport::OpenAiResponses;
    p.upstream_url = format!("http://{address}/v1/responses");
    p.auth_token = Some("provider-secret".into());
    let state = api_test_state(p);
    let app = Router::new()
        .route("/v1/responses", post(responses))
        .route("/v1/responses/compact", post(api_compact))
        .route("/v1/responses/input_tokens", post(api_input_tokens))
        .route(
            "/v1/responses/{id}",
            get(api_response_get).delete(api_response_delete),
        )
        .route("/v1/conversations", post(api_conversation_create))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            api_native_resources,
        ))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_local_auth,
        ))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let bridge = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = Client::new();
    assert_eq!(
        client
            .get(format!("http://{address}/v1/responses/resp_test"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    for (path, method) in [
        ("/v1/responses", reqwest::Method::POST),
        ("/v1/responses/compact", reqwest::Method::POST),
        ("/v1/responses/input_tokens", reqwest::Method::POST),
        ("/v1/responses/resp_test", reqwest::Method::GET),
        ("/v1/responses/resp_test", reqwest::Method::DELETE),
        ("/v1/conversations", reqwest::Method::POST),
    ] {
        let mut builder = client
            .request(method.clone(), format!("http://{address}{path}"))
            .bearer_auth(&state.local_auth_token);
        if method == reqwest::Method::POST {
            builder = builder.json(&json!({"model":"test-model","input":"x"}));
        }
        let response = builder.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let value: Value = response.json().await.unwrap();
        assert_eq!(value["path"], path);
        assert_eq!(value["method"], method.as_str());
    }
    assert!(!api_store_path(&state).exists());
    bridge.abort();
    upstream.abort();
}
