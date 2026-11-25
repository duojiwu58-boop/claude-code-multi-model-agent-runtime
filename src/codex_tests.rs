use super::*;
include!("openai_inbound_tests.rs");
include!("openai_capability_tests.rs");

#[test]
fn codex_local_controls_are_unicode_safe_and_hold_stop_prefixes() {
    let mut output = CodexOutput::new("test".into());
    output.stops = vec!["STOP".into()];
    output.text_delta("a", "你好ST").unwrap();
    assert_eq!(output.content[0]["text"], "你好");
    output.text_delta("a", "OPsecret").unwrap();
    assert_eq!(output.content[0]["text"], "你好");
    assert_eq!(output.local_stop, Some("stop_sequence"));
    assert_eq!(
        output.message("stop_sequence", &json!({}))["stop_sequence"],
        "STOP"
    );
    let mut output = CodexOutput::new("test".into());
    output.max_chars = Some(3);
    output.text_delta("a", "蓝😀AB").unwrap();
    assert_eq!(output.content[0]["text"], "蓝😀A");
    assert_eq!(output.local_stop, Some("max_tokens"));
    let mut output = CodexOutput::new("test".into());
    output.stops = vec!["STOP".into()];
    output.text_delta("a", "hello ST").unwrap();
    output.text_complete("a", "hello ST").unwrap();
    assert_eq!(output.content[0]["text"], "hello ST");
    assert!(output.local_stop.is_none());
}

#[test]
fn codex_strict_controls_and_native_tier_are_explicit() {
    let mut r = request();
    r["bridge_controls"] = json!({"mode":"strict"});
    assert!(prepare_codex_request(&r, &profile("normal"))
        .err()
        .unwrap()
        .contains("max_tokens"));
    r.as_object_mut().unwrap().remove("max_tokens");
    r["service_tier"] = json!("default");
    r["stop_sequences"] = json!(["STOP"]);
    let p = prepare_codex_request(&r, &profile("normal")).unwrap();
    assert_eq!(p.service_tier.as_deref(), Some("default"));
    assert_eq!(p.stop_sequences, ["STOP"]);
    r["temperature"] = json!(0);
    assert!(prepare_codex_request(&r, &profile("normal")).is_err());
}

#[tokio::test]
async fn codex_native_tool_images_are_queued_in_order_without_reexecuting_tools() {
    let mut p = profile("continuation");
    p.codex.as_mut().unwrap().native_continuation = true;
    let mut r = tool_request();
    let a = body(forward_codex_profile(p.clone(), r.clone()).await).await;
    append_codex_tool_result(
        &mut r,
        &a,
        json!([
            {"type":"text","text":"observed"},
            {"type":"image","source":{"type":"url","url":"https://example.invalid/one.png"}},
            {"type":"image","source":{"type":"url","url":"https://example.invalid/two.png"}}
        ]),
        false,
    );
    r["messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"role":"user","content":"Continue"}));
    let prepared = prepare_codex_request(&r, &p).unwrap();
    let (resume, reason) = codex_take_paused(&p, &prepared).unwrap();
    assert_eq!(reason, "native_tool_appended");
    let resume = resume.unwrap();
    assert_eq!(resume.appended[0]["role"], "user");
    assert_eq!(
        resume.appended[0]["content"][2]["image_url"],
        "https://example.invalid/one.png"
    );
    assert_eq!(
        resume.appended[0]["content"][3]["image_url"],
        "https://example.invalid/two.png"
    );
    assert_eq!(resume.appended[1]["content"][0]["text"], "Continue");
    assert!(resume.result.unwrap()["contentItems"]
        .as_array()
        .unwrap()
        .iter()
        .all(|v| v["type"] == "inputText"));
}

fn profile(mode: &str) -> ProviderProfile {
    ProviderProfile {
        file_name: "codex.json".to_string(),
        display_name: "Local Codex".to_string(),
        source: ProviderProfileSource::Native,
        model: "test-model".to_string(),
        context_window: None,
        upstream_identity: None,
        identity_override: true,
        base_url: "stdio://codex-app-server".to_string(),
        auth_token: None,
        api_key: None,
        proxy_url: None,
        local_gemini: false,
        transport: ProviderTransport::Codex,
        openai_capabilities: OpenAiCapabilities::default(),
        vision: VisionConfig::default(),
        upstream_url: "stdio://codex-app-server".to_string(),
        client: Client::new(),
        ls: None,
        codex: Some(CodexProviderConfig {
            command: "pwsh.exe".to_string(),
            args: vec![
                "-NoProfile".into(),
                "-File".into(),
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/codex-app-server.ps1")
                    .to_string_lossy()
                    .into_owned(),
                mode.into(),
            ],
            home: None,
            app_server_url: None,
            timeout: Duration::from_secs(15),
            native_continuation: false,
        }),
    }
}

fn request() -> Value {
    json!({"messages": [{"role": "user", "content": "Hello"}], "max_tokens": 100})
}

fn tool_request() -> Value {
    let mut request = request();
    request["tools"] = json!([{"name": "Read", "description": "Read a file", "input_schema": {"type": "object", "properties": {"file_path": {"type": "string"}}, "required": ["file_path"]}}]);
    request
}

async fn body(response: Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), MAX_UPSTREAM_RESPONSE_BYTES)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn codex_profile_loads_without_http_url_or_api_credentials() {
    let dir = env::temp_dir().join(format!("codex-profile-{}", Uuid::new_v4()));
    fs::create_dir(&dir).unwrap();
    let path = dir.join("codex.json");
    fs::write(&path, json!({"name": "Local Codex", "protocol": "codex", "model": "test-model", "codex": {"command": "codex.exe"}}).to_string()).unwrap();
    let profiles =
        load_native_provider_profiles(vec![path.clone()], "http://127.0.0.1:18787").unwrap();
    fs::remove_file(path).unwrap();
    fs::remove_dir(dir).unwrap();
    assert_eq!(profiles[0].transport, ProviderTransport::Codex);
    assert!(profiles[0].api_key.is_none());
    assert!(profiles[0].auth_token.is_none());
    assert_eq!(profiles[0].codex.as_ref().unwrap().command, "codex.exe");
}

#[test]
fn codex_rejects_malformed_process_configuration() {
    for value in [
        json!({"codex": false}),
        json!({"codex": {"command": "codex.cmd"}}),
        json!({"codex": {"home": "relative"}}),
        json!({"codex": {"args": [42]}}),
        json!({"codex": {"timeout_seconds": 0}}),
        json!({"api_key": "unneeded"}),
        json!({"codex": {"app_server_url": "ws://example.com:18788"}}),
        json!({"codex": {"app_server_url": "http://127.0.0.1:18788"}}),
        json!({"codex": {"app_server_url": "ws://127.0.0.1:18788", "home": "C:\\Users\\user\\.codex"}}),
    ] {
        assert!(parse_codex_provider_config(value.as_object().unwrap(), "test.json").is_err());
    }
}

#[test]
fn codex_replays_client_tools_and_images_without_simulating_tool_output() {
    let request = json!({"messages": [
        {"role": "user", "content": "Read"},
        {"role": "assistant", "content": [{"type": "text", "text": "Looking"}, {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"file_path": "x.png"}}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "is_error": true, "content": [{"type": "text", "text": "图像"}, {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "YWJj"}}]}]}
    ]});
    let prepared = prepare_codex_request(&request, &profile("normal")).unwrap();
    assert_eq!(prepared.history[2]["name"], codex_tool_name("Read"));
    assert_eq!(prepared.history[3]["type"], "function_call_output");
    assert_eq!(
        prepared.history[3]["output"][0]["text"],
        "Tool execution failed:"
    );
    assert_eq!(
        prepared.history[3]["output"][2]["image_url"],
        "data:image/png;base64,YWJj"
    );
    assert_ne!(codex_tool_name("Read"), codex_tool_name("read"));
}

#[test]
fn codex_rejects_orphan_tool_results_and_unsupported_content() {
    for content in [
        json!([{"type": "tool_result", "tool_use_id": "unknown", "content": "x"}]),
        json!([{"type": "document", "source": {}}]),
    ] {
        let request = json!({"messages": [{"role": "user", "content": content}]});
        assert!(prepare_codex_request(&request, &profile("normal")).is_err());
    }
}

#[test]
fn codex_preserves_mid_conversation_instruction_roles_and_order() {
    let request = json!({
        "system": "Base instructions",
        "messages": [
            {"role": "user", "content": "你好呀，GPT。"},
            {"role": "system", "content": [{"type": "text", "text": "Runtime context", "cache_control": {"type": "ephemeral"}}]},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "read-1", "name": "Read", "input": {"file_path": "sample.txt"}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "read-1", "content": "File contents"}]},
            {"role": "system", "content": "Context after the tool result"},
            {"role": "developer", "content": "Answer in Chinese"}
        ]
    });
    let prepared = prepare_codex_request(&request, &profile("normal")).unwrap();
    assert!(prepared.instructions.starts_with("Base instructions"));
    assert!(!prepared.instructions.contains("Runtime context"));
    assert_eq!(prepared.history.len(), 6);
    assert_eq!(
        prepared.history[1],
        json!({"type": "message", "role": "system", "content": [{"type": "input_text", "text": "Runtime context"}]})
    );
    assert_eq!(prepared.history[2]["type"], "function_call");
    assert_eq!(prepared.history[3]["call_id"], "read-1");
    assert_eq!(prepared.history[4]["role"], "system");
    assert_eq!(prepared.history[5]["role"], "developer");
}

#[test]
fn codex_turn_scoped_system_text_expires_on_next_user_including_tool_result() {
    let mut request = json!({"messages": [
        {"role": "user", "content": "Read the file"},
        {"role": "system", "clear_at": "never", "content": "Persistent"},
        {"role": "system", "clear_at": "next_user_message", "content": "Temporary"},
        {"role": "assistant", "content": [{"type": "tool_use", "id": "read-1", "name": "Read", "input": {}}]}
    ]});
    // An assistant continuation alone does not clear the reminder.
    request["messages"][3]["content"] = json!("Working");
    let prepared = prepare_codex_request(&request, &profile("normal")).unwrap();
    assert_eq!(prepared.history[2]["content"][0]["text"], "Temporary");
    request["messages"][3]["content"] =
        json!([{"type": "tool_use", "id": "read-1", "name": "Read", "input": {}}]);
    request["messages"].as_array_mut().unwrap().extend([
        json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "read-1", "content": "Done"}]}),
        json!({"role": "system", "clear_at": "next_user_message", "content": "Current reminder"})
    ]);
    let prepared = prepare_codex_request(&request, &profile("normal")).unwrap();
    assert!(!serde_json::to_string(&prepared.history)
        .unwrap()
        .contains("Temporary"));
    assert_eq!(prepared.history[1]["content"][0]["text"], "Persistent");
    assert_eq!(
        prepared.history.last().unwrap()["content"][0]["text"],
        "Current reminder"
    );
}

#[test]
fn codex_rejects_invalid_instruction_content_and_clear_at() {
    for message in [
        json!({"role": "system", "content": [{"type": "image", "source": {"type": "url", "url": "https://example.invalid/x.png"}}]}),
        json!({"role": "system", "content": [{"type": "thinking", "thinking": "not an instruction"}]}),
        json!({"role": "system", "content": "x", "clear_at": "unknown"}),
        json!({"role": "developer", "content": "x", "clear_at": "next_user_message"}),
        json!({"role": "user", "content": "x", "clear_at": "never"}),
    ] {
        let request = json!({"messages": [{"role": "user", "content": "Hello"}, message]});
        assert!(prepare_codex_request(&request, &profile("normal")).is_err());
    }
}

fn append_codex_tool_result(request: &mut Value, answer: &Value, result: Value, error: bool) {
    let call = answer["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|block| block["type"] == "tool_use")
        .unwrap();
    request["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":answer["content"]}),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":call["id"],"content":result,"is_error":error}]})
    ]);
}

#[tokio::test]
async fn codex_native_continuation_keeps_callbacks_errors_usage_and_retries() {
    let mut profile = profile("continuation");
    profile.codex.as_mut().unwrap().native_continuation = true;
    let mut request = tool_request();
    let first = body(forward_codex_profile(profile.clone(), request.clone()).await).await;
    assert_eq!(first["stop_reason"], "tool_use", "{first}");
    assert_eq!(first["usage"]["input_tokens"], 80);
    append_codex_tool_result(&mut request, &first, json!("permission denied"), true);
    let second = body(forward_codex_profile(profile.clone(), request.clone()).await).await;
    assert_eq!(
        second["content"][0]["text"], "Native result: permission denied (error preserved)",
        "{second}"
    );
    assert_eq!(second["usage"]["input_tokens"], 100);
    assert_eq!(second["usage"]["output_tokens"], 15);
    assert_eq!(second["usage"]["reasoning_tokens"], 9);
    // Retrying the result returns the same answer and message ID, not a new
    // model invocation or a second resolution of the native callback.
    let retried = body(forward_codex_profile(profile, request).await).await;
    assert_eq!(retried, second);
}

#[tokio::test]
async fn codex_preserves_every_queued_native_tool_call() {
    let mut profile = profile("continuation-parallel");
    profile.codex.as_mut().unwrap().native_continuation = true;
    let mut request = tool_request();
    let first = body(forward_codex_profile(profile.clone(), request.clone()).await).await;
    append_codex_tool_result(&mut request, &first, json!("first result"), false);
    let second = body(forward_codex_profile(profile.clone(), request.clone()).await).await;
    assert_eq!(
        second["content"][0]["input"]["file_path"], "second.txt",
        "{second}"
    );
    assert_ne!(first["content"][0]["id"], second["content"][0]["id"]);
    append_codex_tool_result(&mut request, &second, json!("second result"), false);
    let final_answer = body(forward_codex_profile(profile, request).await).await;
    assert_eq!(
        final_answer["content"][0]["text"], "Native result: second result",
        "{final_answer}"
    );
}

#[tokio::test]
async fn codex_changed_history_replays_without_reusing_native_state() {
    let mut profile = profile("continuation");
    profile.codex.as_mut().unwrap().native_continuation = true;
    let mut request = tool_request();
    let first = body(forward_codex_profile(profile.clone(), request.clone()).await).await;
    append_codex_tool_result(&mut request, &first, json!("result"), false);
    request["messages"][0]["content"] = json!("Edited user request");
    let second = body(forward_codex_profile(profile, request).await).await;
    assert_eq!(
        second["content"][0]["text"], "Read result: result",
        "{second}"
    );
    let id = first["content"][0]["id"].as_str().unwrap();
    // The original branch remains independently continuable until expiry.
    codex_paused_cache().lock().unwrap().shift_remove(id);
}

#[test]
fn codex_output_preserves_blocks_and_deduplicates_completed_items() {
    let mut output = CodexOutput::new("model".into());
    output.delta("reasoning-1", true, "Public summary").unwrap();
    output
        .complete("reasoning-1", true, "Public summary")
        .unwrap();
    output.delta("commentary", false, "Checking ").unwrap();
    output
        .complete("commentary", false, "Checking files")
        .unwrap();
    assert!(output
        .complete("commentary", false, "Checking files")
        .unwrap()
        .is_empty());
    output.complete("final", false, "Result").unwrap();
    assert_eq!(output.content.len(), 3);
    assert_eq!(output.content[1]["text"], "Checking files");
    assert_eq!(output.content[2]["text"], "Result");
    assert_eq!(output.content[0]["signature"], "");
    assert!(output
        .complete("final", false, "Conflicting result")
        .is_err());
}

#[test]
fn codex_session_identity_accepts_only_conversation_identifiers() {
    assert!(codex_session_key(&json!({"metadata":{"user_id":"same-account"}})).is_none());
    let id = Uuid::new_v4().to_string();
    let a = codex_session_key(
        &json!({"metadata":{"user_id":json!({"session_id":id,"device_id":"device"}).to_string()}}),
    )
    .unwrap();
    let b = codex_session_key(&json!({"metadata":{"user_id":json!({"session_id":id,"device_id":"another-device"}).to_string()}})).unwrap();
    assert_ne!(a, b);
    assert!(codex_session_key(
        &json!({"metadata":{"user_id":format!("user_device_account_x_session_{id}")}})
    )
    .is_some());
    assert!(codex_session_key(&json!({"metadata":{"user_id":"user_x_session_invalid"}})).is_none());
    assert!(codex_session_key(&json!({"metadata":{"session_id":"bad\nidentity"}})).is_none());
}

#[tokio::test]
async fn codex_appended_messages_and_queued_callbacks_are_not_lost() {
    let mut profile = profile("continuation-parallel");
    profile.codex.as_mut().unwrap().native_continuation = true;
    let mut request = tool_request();
    let first = body(forward_codex_profile(profile.clone(), request.clone()).await).await;
    append_codex_tool_result(&mut request, &first, json!("first"), false);
    request["messages"].as_array_mut().unwrap().extend([
        json!({"role":"system","content":"Keep the supplied order"}),
        json!({"role":"user","content":"Then finish"}),
    ]);
    let second = body(forward_codex_profile(profile.clone(), request.clone()).await).await;
    assert_eq!(
        second["content"][0]["input"]["file_path"], "second.txt",
        "{second}"
    );
    append_codex_tool_result(&mut request, &second, json!("second"), false);
    let answer = body(forward_codex_profile(profile, request).await).await;
    assert_eq!(
        answer["content"][0]["text"], "Native result: second | appended:developer,user",
        "{answer}"
    );
}

fn session_request() -> Value {
    let mut request = request();
    request["metadata"] = json!({"session_id":Uuid::new_v4().to_string()});
    request
}

fn append_user_turn(request: &mut Value, answer: &Value, text: &str) {
    request["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":answer["content"]}),
        json!({"role":"user","content":text}),
    ]);
}

fn retained_thread(request: &Value) -> Option<String> {
    let key = codex_session_key(request)?;
    codex_paused_cache()
        .lock()
        .unwrap()
        .values()
        .rev()
        .find(|s| s.session.as_ref() == Some(&key))
        .and_then(|s| s.connection.thread_id.clone())
}

fn clear_session(request: &Value) {
    let key = codex_session_key(request);
    codex_paused_cache()
        .lock()
        .unwrap()
        .retain(|_, s| s.session != key);
}

#[tokio::test]
async fn codex_reuses_idle_thread_across_user_turns_with_usage_and_retry() {
    let mut profile = profile("session");
    profile.codex.as_mut().unwrap().native_continuation = true;
    let mut request = session_request();
    let first = body(forward_codex_profile(profile.clone(), request.clone()).await).await;
    let thread = retained_thread(&request).unwrap();
    append_user_turn(&mut request, &first, "Follow up");
    let second = body(forward_codex_profile(profile.clone(), request.clone()).await).await;
    assert_eq!(retained_thread(&request).unwrap(), thread);
    assert!(second["content"][0]["text"]
        .as_str()
        .unwrap()
        .ends_with("turn 2"));
    assert_eq!(second["usage"]["input_tokens"], 20);
    assert_eq!(
        body(forward_codex_profile(profile, request.clone()).await).await,
        second
    );
    clear_session(&request);
}

#[tokio::test]
async fn codex_sessions_isolate_identical_prompts_and_rebuild_edited_history() {
    let mut profile = profile("session");
    profile.codex.as_mut().unwrap().native_continuation = true;
    let mut a = session_request();
    let b = session_request();
    let first = body(forward_codex_profile(profile.clone(), a.clone()).await).await;
    let first_thread = retained_thread(&a).unwrap();
    let _ = body(forward_codex_profile(profile.clone(), b.clone()).await).await;
    assert_ne!(retained_thread(&b).unwrap(), first_thread);
    append_user_turn(&mut a, &first, "Follow up");
    a["messages"][0]["content"] = json!("Edited original");
    let answer = body(forward_codex_profile(profile, a.clone()).await).await;
    assert_eq!(answer["stop_reason"], "end_turn");
    assert_ne!(retained_thread(&a).unwrap(), first_thread);
    clear_session(&a);
    clear_session(&b);
}

#[tokio::test]
async fn codex_expired_session_and_settings_change_rebuild() {
    let mut profile = profile("session");
    profile.codex.as_mut().unwrap().native_continuation = true;
    for expired in [false, true] {
        let mut request = session_request();
        let first = body(forward_codex_profile(profile.clone(), request.clone()).await).await;
        let first_thread = retained_thread(&request).unwrap();
        append_user_turn(&mut request, &first, "Next");
        if expired {
            let key = codex_session_key(&request);
            for state in codex_paused_cache()
                .lock()
                .unwrap()
                .values_mut()
                .filter(|s| s.session == key)
            {
                state.created = Instant::now() - CODEX_PAUSED_TTL;
            }
        } else {
            request["system"] = json!("New base instructions");
        }
        let _ = body(forward_codex_profile(profile.clone(), request.clone()).await).await;
        assert_ne!(retained_thread(&request).unwrap(), first_thread);
        clear_session(&request);
    }
}

#[test]
fn codex_session_claim_serializes_distinct_concurrent_requests() {
    let profile = profile("session");
    let mut request = session_request();
    let prepared = prepare_codex_request(&request, &profile).unwrap();
    let (lease, _) = CodexRequestLease::claim(&profile, &prepared).unwrap();
    request["messages"][0]["content"] = json!("A different concurrent prompt");
    let other = prepare_codex_request(&request, &profile).unwrap();
    assert!(CodexRequestLease::claim(&profile, &other)
        .err()
        .unwrap()
        .contains("session is already"));
    drop(lease);
    assert!(CodexRequestLease::claim(&profile, &other).is_ok());
}

#[test]
fn codex_auxiliary_requests_have_separate_lanes_in_one_claude_session() {
    let profile = profile("session");
    let mut request = session_request();
    request["system"] = json!("Main coding assistant");
    let main = prepare_codex_request(&request, &profile).unwrap();
    let (lease, _) = CodexRequestLease::claim(&profile, &main).unwrap();
    request["system"] = json!("Generate a conversation title");
    let title = prepare_codex_request(&request, &profile).unwrap();
    assert_ne!(codex_session_lane(&main), codex_session_lane(&title));
    assert!(CodexRequestLease::claim(&profile, &title).is_ok());
    drop(lease);
}

#[tokio::test]
async fn codex_concurrent_retry_waits_then_replays_completed_reply() {
    let profile = profile("session");
    let request = session_request();
    let prepared = prepare_codex_request(&request, &profile).unwrap();
    let (lease, _) = CodexRequestLease::claim(&profile, &prepared).unwrap();
    let expected = json!({"id":"same-reply"});
    let waiting =
        tokio::spawn(async move { CodexRequestLease::claim_wait(&profile, &prepared).await });
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());
    lease.remember(&expected).unwrap();
    drop(lease);
    let (_, cached) = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(cached, Some(expected));
}

#[test]
fn codex_native_system_mapping_preserves_instruction_order_and_text() {
    let original = vec![
        json!({"type":"message","role":"system","content":[{"type":"input_text","text":"instruction"}]}),
        json!({"type":"message","role":"user","content":[{"type":"input_text","text":"request"}]}),
    ];
    let native = codex_native_items(&original);
    assert_eq!(native[0]["role"], "developer");
    assert_eq!(native[0]["content"], original[0]["content"]);
    assert_eq!(native[1], original[1]);
    assert_eq!(original[0]["role"], "system");
}

#[test]
fn codex_tool_defaults_match_client_replay_but_do_not_hide_changed_arguments() {
    let tools = vec![
        json!({"name":"edit","inputSchema":{"properties":{"replace_all":{"type":"boolean","default":false},"other":{"type":"string"}}}}),
    ];
    let missing = json!({"file_path":"a","new_string":"new"});
    let explicit = json!({"file_path":"a","new_string":"new","replace_all":false});
    assert_eq!(codex_tool_arguments(&tools, "edit", &missing), explicit);
    assert_eq!(codex_tool_arguments(&tools, "edit", &explicit), explicit);
    let changed = json!({"file_path":"a","new_string":"new","replace_all":true});
    assert_ne!(codex_tool_arguments(&tools, "edit", &changed), explicit);
    assert_eq!(codex_tool_arguments(&tools, "unknown", &missing), missing);
}

#[tokio::test]
async fn codex_defaulted_tool_arguments_continue_but_explicit_changes_rebuild() {
    let mut profile = profile("continuation");
    profile.codex.as_mut().unwrap().native_continuation = true;
    for changed in [false, true] {
        let mut request = tool_request();
        request["tools"][0]["input_schema"]["properties"]["optional_flag"] =
            json!({"type":"boolean","default":false});
        let first = body(forward_codex_profile(profile.clone(), request.clone()).await).await;
        assert_eq!(first["content"][0]["input"]["optional_flag"], false);
        append_codex_tool_result(&mut request, &first, json!("result"), false);
        let input = request["messages"][1]["content"][0]["input"]
            .as_object_mut()
            .unwrap();
        if changed {
            input.insert("optional_flag".into(), json!(true));
        } else {
            input.remove("optional_flag");
        }
        let answer = body(forward_codex_profile(profile.clone(), request).await).await;
        assert_eq!(
            answer["content"][0]["text"],
            if changed {
                "Read result: result"
            } else {
                "Native result: result"
            }
        );
        codex_paused_cache()
            .lock()
            .unwrap()
            .shift_remove(first["content"][0]["id"].as_str().unwrap());
    }
}

#[test]
fn codex_applies_deferred_inline_tool_additions_removals_and_effort() {
    let request = json!({"tools":[
        {"name":"Read","defer_loading":true,"input_schema":{"type":"object","properties":{"path":{"type":"string"}}}},
        {"name":"bash","type":"bash_20250124"}
    ],"messages":[
        {"role":"user","content":"Read a file"},
        {"role":"system","output_config":{"effort":"high"},"content":[
            {"type":"tool_removal","tool":{"type":"tool_reference","name":"bash"}},
            {"type":"tool_addition","tool":{"type":"tool_reference","name":"Read"}},
            {"type":"tool_addition","tool":{"type":"tool_definition","definition":{"name":"Inspect","input_schema":{"type":"object","properties":{},"additionalProperties":false}}}},
            {"type":"text","text":"Only inspect files"}
        ]}
    ]});
    let prepared = prepare_codex_request(&request, &profile("normal")).unwrap();
    assert_eq!(prepared.tools.len(), 2);
    assert!(prepared.names.values().any(|name| name == "Read"));
    assert!(!prepared.names.values().any(|name| name == "bash"));
    assert_eq!(
        prepared.tools[1]["inputSchema"]["additionalProperties"],
        false
    );
    assert_eq!(
        prepared.history[1]["content"][0]["text"],
        "Only inspect files"
    );
    assert_eq!(prepared.effort.as_deref(), Some("high"));
    let prepared = prepare_codex_request(&json!({"tools":[{"name":"bash","type":"bash_20250124"}],"messages":[{"role":"user","content":"Hello"}]}), &profile("normal")).unwrap();
    assert_eq!(
        prepared.tools[0]["inputSchema"]["properties"]["command"]["type"],
        "string"
    );
}

#[test]
fn codex_accepts_keep_all_context_without_silently_applying_deletion() {
    let mut request = request();
    for context in [
        json!({}),
        json!({"edits":[]}),
        json!({"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}),
    ] {
        request["context_management"] = context;
        assert!(prepare_codex_request(&request, &profile("normal")).is_ok());
    }
    for context in [
        json!({"edits":[{"type":"clear_thinking_20251015","keep":{"type":"thinking_turns","value":1}}]}),
        json!({"edits":[{"type":"clear_tool_uses_20250919"}]}),
        json!({"unknown":true}),
    ] {
        request["context_management"] = context;
        assert!(prepare_codex_request(&request, &profile("normal")).is_err());
    }
}

#[test]
fn codex_rejects_unrepresentable_controls_and_reports_budget_differences() {
    let mut malformed_system = request();
    malformed_system["system"] = json!([{"type":"image","source":{}}]);
    assert!(prepare_codex_request(&malformed_system, &profile("normal")).is_err());
    for field in ["context_management", "mcp_servers"] {
        let mut request = request();
        request[field] = json!(["unsupported"]);
        assert!(prepare_codex_request(&request, &profile("normal")).is_err());
    }
    let request = json!({"max_tokens":500,"temperature":0.2,"thinking":{"type":"enabled","budget_tokens":100},"messages":[{"role":"user","content":"Hi"}]});
    let diagnostics = codex_request_diagnostics(&request).join("\n");
    assert!(diagnostics.contains("max_tokens"));
    assert!(diagnostics.contains("temperature"));
    assert!(diagnostics.contains("budget_tokens"));
}

#[test]
fn codex_continuation_identity_includes_model_tools_instructions_and_transport() {
    let profile = profile("normal");
    let original = prepare_codex_request(&tool_request(), &profile).unwrap();
    let identity = codex_request_identity(&profile, &original);
    for change in ["model", "transport", "instructions", "tools"] {
        let mut profile = profile.clone();
        let mut request = tool_request();
        match change {
            "model" => profile.model = "another-model".into(),
            "transport" => {
                profile.codex.as_mut().unwrap().app_server_url = Some("ws://127.0.0.1:19999".into())
            }
            "instructions" => request["system"] = json!("new instructions"),
            "tools" => request["tools"][0]["description"] = json!("new meaning"),
            _ => unreachable!(),
        }
        let prepared = prepare_codex_request(&request, &profile).unwrap();
        assert_ne!(identity, codex_request_identity(&profile, &prepared));
    }
}

#[test]
fn codex_concurrent_duplicate_continuation_is_not_executed_twice() {
    let profile = profile("normal");
    let call = format!("toolu_{}", Uuid::new_v4().simple());
    let request = json!({"messages":[
        {"role":"assistant","content":[{"type":"tool_use","id":call,"name":"Read","input":{}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":call,"content":"result"}]}
    ]});
    let prepared = prepare_codex_request(&request, &profile).unwrap();
    let (lease, cached) = CodexRequestLease::claim(&profile, &prepared).unwrap();
    assert!(cached.is_none());
    assert!(CodexRequestLease::claim(&profile, &prepared).is_err());
    drop(lease);
    assert!(CodexRequestLease::claim(&profile, &prepared).is_ok());
}

#[tokio::test]
async fn codex_offline_roundtrip_returns_text_usage_and_client_tool_results() {
    let plain = body(forward_codex_profile(profile("normal"), request()).await).await;
    assert_eq!(plain["content"][0]["text"], "你好，Codex。");
    assert_eq!(plain["usage"]["input_tokens"], 37);
    assert_eq!(plain["usage"]["cache_read_input_tokens"], 5);
    let mut request = tool_request();
    let first = body(forward_codex_profile(profile("normal"), request.clone()).await).await;
    assert_eq!(first["stop_reason"], "tool_use");
    assert_eq!(first["content"][0]["name"], "Read");
    request["messages"].as_array_mut().unwrap().extend([
        json!({"role": "assistant", "content": first["content"]}),
        json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": first["content"][0]["id"], "content": "actual Claude result"}]})
    ]);
    let second = body(forward_codex_profile(profile("normal"), request).await).await;
    assert_eq!(
        second["content"][0]["text"],
        "Read result: actual Claude result"
    );
    assert_eq!(second["stop_reason"], "end_turn");
}

#[tokio::test]
async fn codex_streams_text_tool_use_and_failures() {
    for (mode, mut request, expected) in [
        ("normal", request(), "text_delta"),
        ("normal", tool_request(), "input_json_delta"),
        ("failed", request(), "mock login expired"),
    ] {
        request["stream"] = json!(true);
        let response = forward_codex_profile(profile(mode), request).await;
        let bytes = axum::body::to_bytes(response.into_body(), MAX_UPSTREAM_RESPONSE_BYTES)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains(expected), "{text}");
        assert!(text.contains("message_start"));
        if mode != "failed" {
            assert!(text.contains("message_stop"));
        }
    }
}

#[tokio::test]
async fn codex_reports_rpc_errors_bad_arguments_and_native_actions() {
    for (mode, request, expected) in [
        ("rpc-error", request(), "mock unsupported model"),
        ("bad-arguments", tool_request(), "must be an object"),
        ("native-action", request(), "native action"),
    ] {
        let response = forward_codex_profile(profile(mode), request).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(body(response).await["error"]["message"]
            .as_str()
            .unwrap()
            .contains(expected));
    }
}

#[tokio::test]
async fn codex_times_out_and_validates_required_tool_choice() {
    let mut timed = profile("timeout");
    timed.codex.as_mut().unwrap().timeout = Duration::from_millis(800);
    let response = forward_codex_profile(timed, request()).await;
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    let mut required = tool_request();
    required["tool_choice"] = json!({"type": "tool", "name": "Missing"});
    let response = forward_codex_profile(profile("normal"), required).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let mut no_tools = tool_request();
    no_tools["tool_choice"] = json!({"type": "none"});
    let message = body(forward_codex_profile(profile("normal"), no_tools).await).await;
    assert_eq!(message["stop_reason"], "end_turn");
}

#[tokio::test]
async fn codex_websocket_filters_foreign_events_and_interrupts_only_its_thread() {
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("ws://{}", listener.local_addr().unwrap());
    let mock = tokio::spawn(async move {
        let mut interruptions = 0;
        let mut unsubscribed = 0;
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut tools = json!([]);
            while let Some(frame) = socket.next().await {
                let frame = frame.unwrap();
                if frame.is_close() {
                    break;
                }
                if !frame.is_text() {
                    continue;
                }
                let request: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
                let mut result = json!({});
                match request["method"].as_str().unwrap() {
                    "initialize" => {}
                    "initialized" => continue,
                    "config/read" => result = json!({"config": {"mcp_servers": {}}}),
                    "thread/start" => {
                        assert_eq!(request["params"]["environments"], json!([]));
                        tools = request["params"]["dynamicTools"].clone();
                        result = json!({"thread": {"id": "ws-thread"}});
                    }
                    "thread/inject_items" => {}
                    "turn/start" => {
                        for event in [
                            json!({"method":"thread/status/changed","params":{"threadId":"foreign-thread","status":{"type":"active"}}}),
                            json!({"method":"item/agentMessage/delta","params":{"threadId":"foreign-thread","itemId":"foreign-item","delta":"FOREIGN TEXT MUST NOT LEAK"}}),
                            json!({"method":"thread/tokenUsage/updated","params":{"threadId":"foreign-thread","tokenUsage":{"total":{"inputTokens":999999,"outputTokens":999999}}}}),
                            json!({"id":999,"method":"item/tool/call","params":{"threadId":"foreign-thread","turnId":"foreign-turn","tool":"foreign","arguments":{}}}),
                            json!({"method":"turn/completed","params":{"threadId":"foreign-thread","turn":{"id":"foreign-turn","status":"failed"}}}),
                        ] {
                            socket
                                .send(Message::Text(event.to_string().into()))
                                .await
                                .unwrap();
                        }
                        let events = if tools.as_array().unwrap().is_empty() {
                            vec![
                                json!({"method": "item/agentMessage/delta", "params": {"delta": "WS answer"}}),
                                json!({"method": "turn/completed", "params": {"turn": {"id": "ws-turn", "status": "completed"}}}),
                            ]
                        } else {
                            vec![
                                json!({"id": 900, "method": "item/tool/call", "params": {"threadId": "ws-thread", "turnId": "ws-turn", "tool": tools[0]["name"], "arguments": {"file_path": "sample.txt"}}}),
                            ]
                        };
                        for event in events {
                            socket
                                .send(Message::Text(event.to_string().into()))
                                .await
                                .unwrap();
                        }
                        result = json!({"turn": {"id": "ws-turn"}});
                    }
                    "turn/interrupt" => {
                        assert_eq!(request["params"]["threadId"], "ws-thread");
                        assert_eq!(request["params"]["turnId"], "ws-turn");
                        interruptions += 1;
                    }
                    "thread/unsubscribe" => {
                        unsubscribed += 1;
                    }
                    other => panic!("Unexpected RPC {other}"),
                }
                socket
                    .send(Message::Text(
                        json!({"id": request["id"], "result": result})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
            }
        }
        (interruptions, unsubscribed)
    });
    let mut profile = profile("normal");
    profile.codex.as_mut().unwrap().app_server_url = Some(address);
    let answer = body(forward_codex_profile(profile.clone(), request()).await).await;
    assert_eq!(answer["content"][0]["text"], "WS answer", "{answer}");
    let call = body(forward_codex_profile(profile, tool_request()).await).await;
    assert_eq!(call["stop_reason"], "tool_use", "{call}");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), mock)
            .await
            .unwrap()
            .unwrap(),
        (1, 2)
    );
}

#[tokio::test]
async fn codex_disconnected_stream_interrupts_and_unsubscribes_its_thread() {
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("ws://{}", listener.local_addr().unwrap());
    let mock = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let mut cleanup = Vec::new();
        while let Some(frame) = socket.next().await {
            let frame = frame.unwrap();
            if frame.is_close() {
                break;
            }
            if !frame.is_text() {
                continue;
            }
            let request: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
            let result = match request["method"].as_str().unwrap() {
                "initialized" => continue,
                "config/read" => json!({"config": {"mcp_servers": {}}}),
                "thread/start" => json!({"thread": {"id": "cancel-thread"}}),
                "turn/start" => json!({"turn": {"id": "cancel-turn"}}),
                "turn/interrupt" => {
                    assert_eq!(
                        request["params"],
                        json!({"threadId": "cancel-thread", "turnId": "cancel-turn"})
                    );
                    cleanup.push("interrupt");
                    continue;
                }
                "thread/unsubscribe" => {
                    assert_eq!(request["params"]["threadId"], "cancel-thread");
                    cleanup.push("unsubscribe");
                    continue;
                }
                _ => json!({}),
            };
            socket
                .send(Message::Text(
                    json!({"id": request["id"], "result": result})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            if request["method"] == "turn/start" {
                socket.send(Message::Text(json!({"method": "item/agentMessage/delta", "params": {"delta": "still working"}}).to_string().into())).await.unwrap();
            }
        }
        cleanup
    });
    let mut profile = profile("normal");
    profile.codex.as_mut().unwrap().app_server_url = Some(address);
    let mut request = request();
    request["stream"] = json!(true);
    let response = forward_codex_profile(profile, request).await;
    let mut stream = response.into_body().into_data_stream();
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(bytes) = stream.next().await {
            if String::from_utf8_lossy(&bytes.unwrap()).contains("still working") {
                return;
            }
        }
        panic!("No live text was streamed");
    })
    .await
    .unwrap();
    drop(stream);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), mock)
            .await
            .unwrap()
            .unwrap(),
        vec!["interrupt", "unsubscribe"]
    );
}

// Explicitly invoked: the ordinary test suite never spends account quota.
#[tokio::test]
#[ignore]
async fn codex_live_bridge_two_native_tool_cycles() {
    let mut profile = profile("normal");
    profile.model = env::var("CLAUDE_BRIDGE_CODEX_TEST_MODEL").unwrap();
    let config = profile.codex.as_mut().unwrap();
    config.app_server_url = Some(env::var("CLAUDE_BRIDGE_CODEX_TEST_URL").unwrap());
    config.native_continuation = true;
    config.timeout = Duration::from_secs(120);
    profile.openai_capabilities.default_reasoning_effort = Some("low".into());
    let mut request = tool_request();
    request["thinking"] = json!({"type":"adaptive"});
    request["messages"][0]["content"] = json!("Use the supplied Read tool to read first.txt. Its content names a second file. Then Read that second file. Finally answer with the verification word from the second file. Never read both files at once and do not invent file contents.");
    let mut thread_turn = None;
    for (expected_path, result) in [
        ("first.txt", "Read second-verification.txt next."),
        (
            "second-verification.txt",
            "The verification word is NATIVE-BRIDGE-6429. No more tools are needed.",
        ),
    ] {
        let message = body(forward_codex_profile(profile.clone(), request.clone()).await).await;
        assert_eq!(message["stop_reason"], "tool_use", "{message}");
        let call = message["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["type"] == "tool_use")
            .unwrap();
        assert_eq!(call["input"]["file_path"], expected_path, "{message}");
        {
            let cache = codex_paused_cache().lock().unwrap();
            let state = cache.get(call["id"].as_str().unwrap()).unwrap();
            let current = (
                state.connection.thread_id.clone(),
                state.connection.turn_id.clone(),
            );
            assert!(current.0.is_some() && current.1.is_some());
            if let Some(previous) = &thread_turn {
                assert_eq!(&current, previous, "Native thread/turn was replaced");
            } else {
                thread_turn = Some(current);
            }
        }
        request["messages"].as_array_mut().unwrap().extend([
            json!({"role":"assistant","content":message["content"]}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":call["id"],"content":result}]})
        ]);
    }
    let answer = body(forward_codex_profile(profile, request).await).await;
    assert_eq!(answer["stop_reason"], "end_turn", "{answer}");
    assert!(
        answer["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["text"]
                .as_str()
                .is_some_and(|text| text.contains("NATIVE-BRIDGE-6429"))),
        "{answer}"
    );
}

#[tokio::test]
#[ignore]
async fn codex_live_appended_context_and_next_user_turn_keep_thread() {
    let mut profile = profile("normal");
    profile.model = env::var("CLAUDE_BRIDGE_CODEX_TEST_MODEL").unwrap();
    profile.openai_capabilities.default_reasoning_effort = Some("low".into());
    let config = profile.codex.as_mut().unwrap();
    config.app_server_url = Some(env::var("CLAUDE_BRIDGE_CODEX_TEST_URL").unwrap());
    config.native_continuation = true;
    config.timeout = Duration::from_secs(120);
    let mut request = tool_request();
    request["metadata"] = json!({"session_id":Uuid::new_v4().to_string()});
    request["messages"][0]["content"] = json!("Read first.txt using the supplied Read tool. Its content names the next file to Read. After both reads answer with the verification word. Do not invent contents.");
    let mut original = None;
    for (expected, result) in [
        ("first.txt", "Now read second.txt"),
        ("second.txt", "Verification word: CONTINUITY-7421"),
    ] {
        let answer = body(forward_codex_profile(profile.clone(), request.clone()).await).await;
        let call = answer["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["type"] == "tool_use")
            .unwrap();
        assert_eq!(call["input"]["file_path"], expected, "{answer}");
        let current = {
            let cache = codex_paused_cache().lock().unwrap();
            let state = cache.get(call["id"].as_str().unwrap()).unwrap();
            (
                state.connection.thread_id.clone().unwrap(),
                state.connection.turn_id.clone().unwrap(),
            )
        };
        if let Some(ref previous) = original {
            assert_eq!(&current, previous);
        } else {
            original = Some(current);
        }
        append_codex_tool_result(&mut request, &answer, json!(result), false);
        request["messages"].as_array_mut().unwrap().push(json!({"role":"user","content":"Continue following the original task; do not repeat a completed Read."}));
    }
    request["messages"].as_array_mut().unwrap().push(
        json!({"role":"system","content":"Prefix the final verification word with APPENDED-OK."}),
    );
    let answer = body(forward_codex_profile(profile.clone(), request.clone()).await).await;
    let text = answer["content"].to_string();
    assert!(
        text.contains("CONTINUITY-7421") && text.contains("APPENDED-OK"),
        "{answer}"
    );
    let thread = original.unwrap().0;
    assert_eq!(retained_thread(&request).unwrap(), thread);
    append_user_turn(&mut request, &answer, "Without tools, repeat the verification word from our previous turn and append NEXT-TURN-OK.");
    let next = body(forward_codex_profile(profile, request.clone()).await).await;
    assert!(
        next["content"].to_string().contains("NEXT-TURN-OK"),
        "{next}"
    );
    assert!(
        next["content"].to_string().contains("CONTINUITY-7421"),
        "{next}"
    );
    assert_eq!(retained_thread(&request).unwrap(), thread);
    println!(
        "continuity evidence: {}",
        json!({"same_thread":true,"same_tool_turn":true,"appended_context":true,"next_user_turn":true,"status":codex_continuity_status()})
    );
    clear_session(&request);
}

#[tokio::test]
#[ignore]
async fn codex_live_native_callback_continuation() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let mut profile = profile("normal");
        profile.model = env::var("CLAUDE_BRIDGE_CODEX_TEST_MODEL").unwrap();
        profile.codex.as_mut().unwrap().app_server_url =
            Some(env::var("CLAUDE_BRIDGE_CODEX_TEST_URL").unwrap());
        let request = tool_request();
        let prepared = prepare_codex_request(&request, &profile).unwrap();
        let mut connection = CodexConnection::start(profile.codex.as_ref().unwrap()).await.unwrap();
        connection.rpc(1, "initialize", json!({"clientInfo": {"name": "claude_code_bridge", "version": env!("CARGO_PKG_VERSION")}, "capabilities": {"experimentalApi": true}})).await.unwrap();
        connection.send(json!({"method":"initialized","params":{}})).await.unwrap();
        let config = connection.rpc(2, "config/read", json!({"includeLayers":false})).await.unwrap();
        let thread = connection.rpc(3, "thread/start", json!({
            "model": profile.model, "ephemeral":true,"approvalPolicy":"never","sandbox":"read-only","environments":[],
            "dynamicTools":prepared.tools,"baseInstructions":prepared.instructions,
            "config":codex_tool_only_config(&config["config"])
        })).await.unwrap();
        let thread_id = thread["thread"]["id"].as_str().unwrap().to_string();
        connection.thread_id = Some(thread_id.clone());
        connection.send(json!({"id":4,"method":"turn/start","params":{"threadId":thread_id,"effort":"low","input":[{"type":"text","text":"Call the supplied Read tool to read sample.txt. After the result, reply with the verification word found in it. Do not use another tool."}]}})).await.unwrap();
        let mut called = false;
        let mut answer = String::new();
        loop {
            let event = connection.read().await.unwrap();
            match event["method"].as_str() {
                Some("turn/started") => connection.turn_id = event["params"]["turn"]["id"].as_str().map(str::to_owned),
                Some("item/tool/call") => {
                    assert!(!called, "Unexpected repeated tool call");
                    called = true;
                    connection.turn_id = event["params"]["turnId"].as_str().map(str::to_owned);
                    // The next Claude request may append a system message after
                    // the tool result. Preserve its role while the native call waits.
                    connection.rpc(5, "thread/inject_items", json!({"threadId":thread_id,"items":[{"type":"message","role":"system","content":[{"type":"input_text","text":"Answer with only the verification word from the tool result."}]}]})).await.unwrap();
                    connection.send(json!({"id":event["id"],"result":{"contentItems":[{"type":"inputText","text":"The verification word is NATIVE-CONTINUITY-928."}],"success":true}})).await.unwrap();
                }
                Some("item/agentMessage/delta") => answer.push_str(event["params"]["delta"].as_str().unwrap()),
                Some("turn/completed") => {
                    assert_eq!(event["params"]["turn"]["status"], "completed", "{event}");
                    connection.turn_id = None;
                    break;
                }
                _ => {}
            }
        }
        connection.shutdown().await.unwrap();
        assert!(called);
        assert!(answer.contains("NATIVE-CONTINUITY-928"), "{answer}");
    }).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn codex_live_client_tool_roundtrip() {
    let model = env::var("CLAUDE_BRIDGE_CODEX_TEST_MODEL")
        .expect("Set CLAUDE_BRIDGE_CODEX_TEST_MODEL to an available local Codex model");
    let command =
        env::var("CLAUDE_BRIDGE_CODEX_TEST_COMMAND").unwrap_or_else(|_| "codex.exe".to_string());
    let mut profile = profile("normal");
    profile.model = model;
    profile.codex = Some(CodexProviderConfig {
        command,
        args: Vec::new(),
        home: None,
        app_server_url: env::var("CLAUDE_BRIDGE_CODEX_TEST_URL").ok(),
        timeout: Duration::from_secs(120),
        native_continuation: true,
    });
    profile.openai_capabilities.default_reasoning_effort = Some("low".to_string());
    let mut request = tool_request();
    request["messages"][0]["content"] = json!("Read sample.txt with the supplied Claude Code Read tool. Do not respond before invoking the tool.");
    request["messages"].as_array_mut().unwrap().extend([
        json!({"role": "system", "content": [{"type": "text", "text": "Use the supplied client tools for file access."}]}),
        json!({"role": "developer", "content": "Report only the requested verification word after reading the file."})
    ]);
    request["tool_choice"] = json!({"type": "tool", "name": "Read"});
    let first = body(forward_codex_profile(profile.clone(), request.clone()).await).await;
    assert_eq!(first["stop_reason"], "tool_use", "{first}");
    assert_eq!(
        first["content"].as_array().unwrap().last().unwrap()["name"],
        "Read",
        "{first}"
    );
    let call = first["content"].as_array().unwrap().last().unwrap();
    request["messages"].as_array_mut().unwrap().extend([
        json!({"role": "assistant", "content": first["content"]}),
        json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": call["id"], "content": "The verification word is BLUEBIRD-739. Reply with exactly this word; no more tool calls."}]}),
        json!({"role": "system", "clear_at": "next_user_message", "content": "Use the tool result already returned."})
    ]);
    request["tool_choice"] = json!({"type": "none"});
    request["stream"] = json!(true);
    let response = forward_codex_profile(profile, request).await;
    let bytes = axum::body::to_bytes(response.into_body(), MAX_UPSTREAM_RESPONSE_BYTES)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let answer: String = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|event| {
            event
                .pointer("/delta/text")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect();
    assert!(answer.contains("BLUEBIRD-739"), "{text}");
    assert!(text.contains("message_stop"), "{text}");
}
#[tokio::test]
async fn codex_completed_output_survives_cleanup_errors_and_flushes_tool_prefix() {
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message;
    for tool in [false,true] {
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address=format!("ws://{}",listener.local_addr().unwrap());
        let mock=tokio::spawn(async move {
            let (stream,_)=listener.accept().await.unwrap();
            let mut socket=tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut tools=json!([]);
            while let Some(Ok(frame))=socket.next().await {
                if !frame.is_text() {if frame.is_close(){break;}continue;}
                let r:Value=serde_json::from_str(frame.to_text().unwrap()).unwrap();
                let result=match r["method"].as_str().unwrap() {
                    "initialized"=>continue,
                    "initialize"=>json!({}),
                    "config/read"=>json!({"config":{"mcp_servers":{}}}),
                    "thread/start"=>{tools=r["params"]["dynamicTools"].clone();json!({"thread":{"id":"review-thread"}})},
                    "thread/inject_items"=>json!({}),
                    "turn/start"=>{
                        socket.send(Message::Text(json!({"method":"item/agentMessage/delta","params":{"itemId":"answer","delta":"hello ST"}}).to_string().into())).await.unwrap();
                        let event=if tool {json!({"id":900,"method":"item/tool/call","params":{"threadId":"review-thread","turnId":"review-turn","tool":tools[0]["name"],"arguments":{"file_path":"sample.txt"}}})}
                            else {json!({"method":"turn/completed","params":{"turn":{"id":"review-turn","status":"completed"}}})};
                        socket.send(Message::Text(event.to_string().into())).await.unwrap();
                        json!({"turn":{"id":"review-turn"}})
                    },
                    "turn/interrupt"|"thread/unsubscribe"=>{
                        if socket.send(Message::Text(json!({"id":r["id"],"error":{"code":-1,"message":"already ended"}}).to_string().into())).await.is_err(){break;}
                        continue;
                    },
                    other=>panic!("Unexpected RPC {other}"),
                };
                if socket.send(Message::Text(json!({"id":r["id"],"result":result}).to_string().into())).await.is_err(){break;}
            }
        });
        let mut p=profile("normal");p.codex.as_mut().unwrap().app_server_url=Some(address);
        p.codex.as_mut().unwrap().native_continuation=false;
        let mut r=if tool {tool_request()} else {request()};
        r["stop_sequences"]=json!(["STOP"]);
        let answer=body(forward_codex_profile(p,r).await).await;
        assert_eq!(answer["content"][0]["text"],"hello ST","{answer}");
        assert_eq!(answer["stop_reason"],if tool {"tool_use"}else{"end_turn"});
        tokio::time::timeout(Duration::from_secs(5),mock).await.unwrap().unwrap();
    }
}
