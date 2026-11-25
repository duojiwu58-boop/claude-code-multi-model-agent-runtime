use super::*;

#[tokio::test]
async fn ls_lifecycle_rejects_unmanaged_configuration_without_starting() {
    let mut host = LsLifecycle::default();
    assert!(host.select(profile().ls.as_ref()).await.unwrap_err().contains("ls.launch"));
    assert!(host.child.is_none());
    host.select(None).await.unwrap();
}

#[tokio::test]
async fn ls_lifecycle_never_adopts_an_existing_listener() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = profile().ls.unwrap();
    config.host_url = format!("http://{}", listener.local_addr().unwrap());
    config.launch = Some(LsLaunchConfig {
        install_root: std::env::temp_dir(), data_root: std::env::temp_dir(),
    });
    let mut host = LsLifecycle::default();
    assert!(host.select(Some(&config)).await.unwrap_err().contains("already occupied"));
    assert!(host.child.is_none());
    assert!(tokio::net::TcpStream::connect(listener.local_addr().unwrap()).await.is_ok());
}

fn fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/ls-input")
        .join(format!("{name}.json"));
    let mut v: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    fn replace(v: &mut Value) {
        match v {
            Value::String(s) => {
                *s = s.replace("__WORKSPACE__", &std::env::temp_dir().to_string_lossy())
            }
            Value::Array(a) => a.iter_mut().for_each(replace),
            Value::Object(o) => o.values_mut().for_each(replace),
            _ => (),
        }
    }
    replace(&mut v);
    v
}

#[test]
fn ls_captured_plain_requests_are_filtered_without_adapter_credentials() {
    for (case, expected) in [
        ("safe", "USER_ORIGINAL_A: 请说明当前任务。"),
        ("normal", "USER_ORIGINAL_B: 请保留空格  和换行\n第二行。"),
        ("resume", "USER_ORIGINAL_C: 继续上一轮，原样保留历史。"),
        ("dynamic_env", "USER_ORIGINAL_D: 请确认工作目录。"),
        ("prompt_hook", "USER_WITH_HOOK"),
    ] {
        let turn = ls_turn_request(&profile(), &fixture(case)).unwrap();
        assert_eq!(turn.text, expected, "{case}");
        assert_eq!(turn.workspace, std::env::temp_dir().to_string_lossy());
        let wire = serde_json::to_string(&turn).unwrap();
        for forbidden in [
            "PROJECT_INSTRUCTION_MARKER",
            "HOOK_INSTRUCTION_MARKER",
            "CLIENT_TOOL_MARKER",
            "You are a Claude agent",
            "As you answer the user's questions",
        ] {
            assert!(!wire.contains(forbidden), "{case}: {forbidden}");
        }
        if case == "resume" {
            assert_eq!(turn.history, json!([{"role":"user","content":"USER_ORIGINAL_B: 请保留空格  和换行\n第二行。"},{"role":"assistant","content":"LOCAL_ASSISTANT_REPLY"}]).as_array().unwrap().clone());
        }
    }
}

#[test]
fn ls_literal_reminder_and_multiple_user_blocks_are_preserved() {
    let turn = ls_turn_request(&profile(), &fixture("literal_tag")).unwrap();
    assert_eq!(turn.text, "<system-reminder>USER_LITERAL: 这是用户原文，不是客户端提醒。</system-reminder>\nYou are Claude Code. 请分析这句话。");
    let input = fixture("identical_auto_text_as_user");
    let turn = ls_turn_request(&profile(), &input).unwrap();
    assert_eq!(turn.text, input["messages"][0]["content"][1]["text"]);
    let turn = ls_turn_request(&profile(), &fixture("vscode_stream_blocks")).unwrap();
    assert_eq!(
        turn.text_items,
        vec!["USER_STREAM_ORIGINAL", "USER_SECOND_BLOCK"]
    );
    let host = ls_host_protocol::user_payload(&turn.user_content).unwrap();
    assert_eq!(host.text_items, turn.text_items);
}

#[test]
fn ls_images_files_and_image_only_preserve_content_and_history() {
    for name in [
        "pasted_image",
        "image_only",
        "text_attachment",
        "mixed_attachments",
    ] {
        let request = fixture(name);
        let turn = ls_turn_request(&profile(), &request).unwrap();
        let original = request["messages"][0]["content"].as_array().unwrap();
        let host = ls_host_protocol::user_payload(&turn.user_content).unwrap();
        assert_eq!(host.text_items, turn.text_items);
        assert_eq!(host.media, turn.media);
        if let Some(image) = original.iter().find(|b| b["type"] == "image") {
            assert_eq!(turn.media[0]["inlineData"], image["source"]["data"]);
            assert_eq!(turn.media[0]["mimeType"], "image/png");
        }
        if let Some(doc) = original.iter().find(|b| b["type"] == "document") {
            assert!(turn.text.contains(doc["source"]["data"].as_str().unwrap()));
            assert!(turn.text.contains("fixture.txt"));
            assert!(turn
                .text
                .contains("<system-reminder>literal file content</system-reminder>"));
        }
        if name == "image_only" {
            assert!(turn.text.is_empty());
            assert_eq!(turn.media.len(), 1);
        }
        let history = ls_host_protocol::history_content(&host);
        let mut next = request;
        let messages = next["messages"].as_array_mut().unwrap();
        messages.push(json!({"role":"assistant","content":"answer"}));
        messages.push(json!({"role":"user","content":"next"}));
        let next = ls_turn_request(&profile(), &next).unwrap();
        assert_eq!(next.history[0]["content"], history);
        assert!(next.media.is_empty(), "attachments must not be replayed");
        assert_eq!(
            ls_host_protocol::normalize_history(&next.history).unwrap(),
            next.history
        );
    }
}

#[test]
fn ls_invalid_media_and_old_bypass_are_explicit_errors() {
    let mut request = fixture("pasted_image");
    request["messages"][0]["content"][1]["source"]["data"] = json!("AAAA");
    assert!(ls_turn_request(&profile(), &request).is_err());
    let mut request = fixture("normal");
    request["metadata"]["ls_input"] =
        json!({"version":1,"messages":[{"role":"user","content":"bypass"}]});
    assert!(ls_turn_request(&profile(), &request).is_err());
}

#[test]
fn ls_pdf_preserves_bytes_and_history_without_reupload_on_followup() {
    let mut request = fixture("pdf_attachment");
    let turn = ls_turn_request(&profile(), &request).unwrap();
    let payload = ls_host_protocol::user_payload(&turn.user_content).unwrap();
    assert_eq!(payload.media, turn.media);
    assert_eq!(turn.media[0]["mimeType"], "application/pdf");
    let doc = turn
        .user_content
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["type"] == "document")
        .unwrap();
    assert_eq!(turn.media[0]["inlineData"], doc["source"]["data"]);
    let only = ls_host_protocol::user_payload(&json!([doc])).unwrap();
    assert!(only.text_items.is_empty());
    assert_eq!(only.media.len(), 1);
    request["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":"answer"}),
        json!({"role":"user","content":"next"}),
    ]);
    let next = ls_turn_request(&profile(), &request).unwrap();
    assert!(next.media.is_empty());
    assert_eq!(next.history[0]["content"], payload.content);
    assert_eq!(
        ls_host_protocol::normalize_history(&next.history).unwrap(),
        next.history
    );
}

#[test]
fn ls_image_limits_count_decoded_size_and_aggregate() {
    use base64::Engine;
    let image = |bytes: &[u8]| json!({"type":"image","source":{
        "type":"base64","media_type":"image/png",
        "data":base64::engine::general_purpose::STANDARD.encode(bytes)}});
    let small = image(b"\x89PNG\r\n\x1a\n");
    let mut blocks = vec![small.clone(); 100];
    blocks.push(json!({"type":"text","text":"Describe every image"}));
    let payload = ls_host_protocol::user_payload(&json!(blocks)).unwrap();
    assert_eq!(payload.media.len(), 100);
    assert_eq!(payload.text_items, vec!["Describe every image"]);
    blocks.push(small);
    assert_eq!(ls_host_protocol::user_payload(&json!(blocks)).err().unwrap(),
        "At most 100 images per message are supported");
    let mut bytes = vec![0; 10 * 1024 * 1024];
    bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    let at_limit = image(&bytes);
    assert!(ls_host_protocol::user_payload(&json!([at_limit])).is_ok());
    assert_eq!(ls_host_protocol::user_payload(&json!([
        at_limit, at_limit, at_limit, at_limit
    ])).err().unwrap(), "User input and attachments exceed 32 MiB");
    // These two sizes have equal base64 length: decoded bytes must be checked.
    bytes.push(0);
    assert_eq!(ls_host_protocol::user_payload(&json!([image(&bytes)])).err().unwrap(),
        "Image exceeds 10 MiB");
    assert!(ls_host_protocol::user_payload(&json!(vec![
        json!({"type":"text","text":"x"}); 101
    ])).is_err());
}

#[test]
fn ls_pdf_validation_rejects_invalid_sources_sizes_and_aggregate() {
    use base64::Engine;
    let encode = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let doc = |data: String| json!({"type":"document","title":"../folder/test.pdf","source":{"type":"base64","media_type":"application/pdf","data":data}});
    let valid = doc(encode(b"%PDF-1.7\nfixture"));
    let payload = ls_host_protocol::user_payload(&json!([valid])).unwrap();
    assert_eq!(payload.media[0]["displayName"], "test.pdf");
    assert!(payload.media[0].get("uri").is_none());
    for data in ["!!!".to_string(), encode(b"not a pdf")] {
        assert!(ls_host_protocol::user_payload(&json!([doc(data)])).is_err());
    }
    let mut url = valid.clone();
    url["source"] =
        json!({"type":"url","media_type":"application/pdf","url":"http://127.0.0.1/private"});
    assert!(ls_host_protocol::user_payload(&json!([url])).is_err());
    let mut bytes = vec![b' '; 10 * 1024 * 1024];
    bytes[..5].copy_from_slice(b"%PDF-");
    let at_limit = doc(encode(&bytes));
    assert!(ls_host_protocol::user_payload(&json!([at_limit])).is_ok());
    assert!(
        ls_host_protocol::user_payload(&json!([at_limit, at_limit, at_limit, at_limit])).is_err()
    );
    bytes.push(b' ');
    assert!(ls_host_protocol::user_payload(&json!([doc(encode(&bytes))])).is_err());
}

#[test]
fn ls_selection_retains_file_and_code_without_client_instructions() {
    let turn = ls_turn_request(&profile(), &fixture("vscode_selection")).unwrap();
    assert!(turn.text.contains("sample.txt") && turn.text.contains("FIXTURE_DATA"));
    assert!(turn.text.ends_with("USER_EDITOR_ORIGINAL"));
    assert!(!turn.text.contains("ide_selection") && !turn.text.contains("may or may not"));
}

#[test]
fn ls_native_utf8_file_is_decoded_without_touching_prompt_or_document_literals() {
    let request = fixture("native_utf8_file_bytes");
    let turn = ls_turn_request(&profile(), &request).unwrap();
    assert!(turn.text.contains("中文附件"));
    assert!(turn
        .text
        .contains("<system-reminder>literal file content</system-reminder>"));
    assert!(turn.text.ends_with("USER_NATIVE_FILE"));
    let mut next = request;
    next["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":"answer"}),
        json!({"role":"user","content":"ä¸\u{AD} is literal user text"}),
    ]);
    let next = ls_turn_request(&profile(), &next).unwrap();
    assert_eq!(next.text, "ä¸\u{AD} is literal user text");
    assert_eq!(
        next.history[0]["content"],
        ls_host_protocol::history_content(
            &ls_host_protocol::user_payload(&turn.user_content).unwrap()
        )
    );
}

#[test]
fn ls_original_source_rejects_synthetic_and_mismatched_text() {
    let request = json!({"messages":[{"role":"user","content":"[MESSAGE FROM NON-USER SOURCE - NOT USER INPUT]\nliteral"}]});
    assert!(ls_turn_request(&profile(), &request).is_err());
    let rows = vec![json!({"type":"user","message":request["messages"][0]})];
    assert!(ls_turn_request_with_originals(&profile(), &request, Some(&rows)).is_ok());
    let mut rows = rows;
    rows[0]["isMeta"] = json!(true);
    assert!(ls_turn_request_with_originals(&profile(), &request, Some(&rows)).is_err());
    rows[0]["isMeta"] = json!(false);
    rows[0]["message"]["content"] = json!("different");
    assert!(ls_turn_request_with_originals(&profile(), &request, Some(&rows)).is_err());
}

#[test]
fn ls_wire_preserves_native_image_media_and_verbatim_items() {
    let turn = ls_turn_request(&profile(), &fixture("mixed_attachments")).unwrap();
    let wire = ls_wire::SendMessageOptions {
        cascade_id: "test".into(),
        text: turn.text.clone(),
        text_items: Some(turn.text_items.clone()),
        images: turn.media.clone(),
        plan_model: "model".into(),
        client_request_id: "test".into(),
        client_message_id: None,
        tool_config: None,
    }
    .to_wire_json();
    assert_eq!(wire["media"], json!(turn.media));
    let text: Vec<_> = wire["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["text"].as_str().unwrap())
        .collect();
    assert_eq!(text, turn.text_items);
    assert!(wire.get("system").is_none() && wire.get("history").is_none());
}

#[tokio::test]
async fn ls_original_source_waits_for_async_transcript_write() {
    let root = std::env::temp_dir().join(format!("ls-source-test-{}", Uuid::new_v4()));
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    let session = Uuid::new_v4().to_string();
    let file = project.join(format!("{session}.jsonl"));
    let row = json!({"type":"user","isSidechain":false,"sessionId":session,"message":{"role":"user","content":"original"}});
    let delayed_file = file.clone();
    let delayed_row = row.clone();
    let writer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        fs::write(delayed_file, format!("{}\n", delayed_row)).unwrap();
    });
    let mut p = profile();
    p.ls.as_mut().unwrap().claude_projects_dir = Some(root.clone());
    let request = json!({"metadata":{"user_id":json!({"session_id":session}).to_string()},"messages":[{"role":"user","content":"original"}]});
    let rows = ls_recorded_originals(p.ls.as_ref().unwrap(), &request)
        .await
        .unwrap()
        .unwrap();
    writer.await.unwrap();
    assert_eq!(rows, vec![row]);
    assert!(ls_turn_request_with_originals(&p, &request, Some(&rows)).is_ok());
    fs::remove_file(file).unwrap();
    fs::remove_dir(project).unwrap();
    fs::remove_dir(root).unwrap();
}

fn profile() -> ProviderProfile {
    let mut p = crate::tests::test_provider_profile(Client::new(), "http://127.0.0.1:12345".into());
    p.transport = ProviderTransport::AntigravityLs;
    p.ls = Some(LsProviderConfig {
        launch: None,
        host_url: "http://127.0.0.1:12345".into(),
        token_file: PathBuf::from("C:/unused.token"),
        workspace: "C:/".into(),
        permission_mode: "turbo".into(),
        timeout_seconds: 60,
        claude_projects_dir: None,
    });
    p
}

#[test]
fn ls_only_user_text_crosses_boundary() {
    let text = "  用户原文\r\n<system-reminder>literal user text</system-reminder>  ";
    let request = json!({"system":"FORBIDDEN_SYSTEM","developer":"FORBIDDEN_DEVELOPER","tools":[{"description":"FORBIDDEN_TOOL"}],"max_tokens":20,"messages":[{"role":"user","content":[{"type":"text","text":text}]}]});
    let turn = ls_turn_request(&profile(), &request).unwrap();
    assert_eq!(turn.text, text);
    let out = serde_json::to_string(&turn).unwrap();
    assert!(!out.contains("FORBIDDEN"));
    assert!(turn.history.is_empty());
    assert_eq!(turn.permission_mode, "turbo");
}

#[test]
fn ls_rejects_tool_replay_and_unrepresentable_controls() {
    assert!(ls_turn_request(
        &profile(),
        &json!({"metadata":{"session_id":""},"messages":[{"role":"user","content":"hello"}]})
    )
    .is_err());
    for extra in [
        json!({"tool_choice":{"type":"any"}}),
        json!({"temperature":0.5}),
        json!({"output_config":{"format":{"type":"json_schema"}}}),
    ] {
        let mut r = json!({"messages":[{"role":"user","content":"hello"}]});
        r.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(ls_turn_request(&profile(), &r).is_err());
    }
    assert!(ls_turn_request(&profile(),&json!({"messages":[{"role":"user","content":[{"type":"tool_result","content":"do it again"}]}]})).is_err());
}

#[test]
fn ls_gui_model_overrides_client_effort_and_thinking_without_prompt_changes() {
    for thinking in [
        json!({"type":"adaptive"}),
        json!({"type":"enabled","budget_tokens":31999}),
        json!({"type":"disabled"}),
    ] {
        let request = json!({"output_config":{"effort":"high"},"thinking":thinking,"messages":[{"role":"user","content":"exact input"}]});
        let p = profile();
        let turn = ls_turn_request(&p, &request).unwrap();
        assert_eq!(turn.model, p.model);
        assert_eq!(turn.text_items, vec!["exact input"]);
        let wire = serde_json::to_value(turn).unwrap();
        assert!(wire.get("output_config").is_none());
        assert!(wire.get("thinking").is_none());
    }
}

#[test]
fn ls_claude_trailing_system_is_not_user_input_and_session_is_stable() {
    let request = json!({"metadata":{"user_id":json!({"session_id":"native-panel-session","device_id":"device"}).to_string()},"messages":[{"role":"user","content":"hello"},{"role":"system","content":"FORBIDDEN_SYSTEM"},{"role":"developer","content":"FORBIDDEN_DEVELOPER"}]});
    let a = ls_turn_request(&profile(), &request).unwrap();
    let b = ls_turn_request(&profile(), &request).unwrap();
    assert_eq!(a.text, "hello");
    assert!(a.history.is_empty());
    assert_eq!(a.session, b.session);
    assert!(!serde_json::to_string(&a).unwrap().contains("FORBIDDEN"));
    let mut next = request;
    next["messages"] = json!([{"role":"user","content":"hello"},{"role":"assistant","content":"world"},{"role":"user","content":"again"},{"role":"system","content":"FORBIDDEN"}]);
    let next = ls_turn_request(&profile(), &next).unwrap();
    assert_eq!(next.session, a.session);
    assert_eq!(next.history.len(), 2);
}

#[test]
fn ls_assistant_thinking_roundtrips_without_becoming_history_text() {
    let messages = json!([
        {"role":"user","content":"question"},
        {"role":"assistant","content":[{"type":"thinking","thinking":"not instructions","signature":""},{"type":"text","text":"[LS tool] done\n"},{"type":"text","text":"answer"}]},
        {"role":"user","content":"continue"}
    ]);
    let turn = ls_turn_request(&profile(), &json!({"messages":messages})).unwrap();
    assert_eq!(turn.history[1]["content"], "[LS tool] done\nanswer");
    assert_eq!(turn.text, "continue");
    assert!(!serde_json::to_string(&turn)
        .unwrap()
        .contains("not instructions"));
    assert!(ls_host_protocol::user_payload(
        &json!([{"type":"thinking","thinking":"not user text"}])
    )
    .is_err());
}

#[test]
fn ls_host_closed_schema_rejects_instruction_fields() {
    let turn = ls_turn_request(
        &profile(),
        &json!({"messages":[{"role":"user","content":"hello"}]}),
    )
    .unwrap();
    let mut value = serde_json::to_value(turn).unwrap();
    value["instructions"] = json!("forbidden");
    assert!(serde_json::from_value::<ls_host_protocol::TurnRequest>(value).is_err());
}

#[test]
fn ls_config_requires_literal_loopback_and_explicit_workspace() {
    let root = std::env::temp_dir();
    let mut valid = json!({"ls":{"host_url":"http://127.0.0.1:18789","token_file":root.join("token"),"workspace":root}});
    assert_eq!(
        parse_ls_provider_config(valid.as_object().unwrap(), "test")
            .unwrap()
            .permission_mode,
        "turbo"
    );
    valid["ls"]["permission_mode"] = json!("review");
    assert!(parse_ls_provider_config(valid.as_object().unwrap(), "test").is_err());
    for host in [
        "http://example.com:1234",
        "https://127.0.0.1:1234",
        "http://127.0.0.1:1234/rpc",
        "http://user:secret@127.0.0.1:1234",
    ] {
        let config =
            json!({"ls":{"host_url":host,"token_file":root.join("token"),"workspace":root}});
        assert!(parse_ls_provider_config(config.as_object().unwrap(), "test").is_err());
    }
}

#[test]
fn ls_model_controls_only_group_explicit_catalog_variants() {
    let groups = ls_model_groups(&json!({"models":[
        {"id":"neon-high","label":"Gemini 4 Neon (High)"},
        {"id":"neon-low","label":"Gemini 4 Neon (Low)"},
        {"id":"opaque-a","label":"Other (XHigh) (preview)"},
        {"id":"opaque-b","label":"Other (Medium) (preview)"},
        {"id":"plain","label":"No effort"}
    ]}));
    assert_eq!(groups.len(), 3);
    assert_eq!(groups[0]["label"], "Gemini 4 Neon");
    assert_eq!(
        groups[0]["variants"][1],
        json!({"id":"neon-low","level":"low"})
    );
    assert_eq!(groups[1]["variants"][0]["level"], "xhigh");
    assert_eq!(groups[2]["variants"][0]["level"], "默认");
}
