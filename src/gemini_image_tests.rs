#[tokio::test]
async fn argon_image_stored_transition_and_next_turn() {
    for tool_result in [false, true] {
        for stream in [false, true] {
            let (base, captured, server) = spawn_probe_mock("/interactions", StatusCode::OK,
                json!({"id":"vision-second","status":"completed","steps":[{"type":"model_output","content":[{"type":"text","text":"red"}]}]})).await;
            let mut profile = argon_test_profile();
            profile.upstream_url = format!("{base}/interactions");
            let cache = Arc::new(RwLock::new(InteractionContinuationCache::default()));
            let first = json!({"max_tokens":262144,"messages":[{"role":"user","content":"Remember ORCHID-7429"}],
                "tools":[{"name":"read_file","input_schema":{"type":"object","properties":{"path":{"type":"string"}}}}]});
            assert_eq!(
                translate_gemini_interactions_request(&first, &profile, &cache).unwrap()
                    ["background"],
                true
            );
            let assistant = if tool_result {
                vec![
                    json!({"type":"tool_use","id":"read-image","name":"read_file","input":{"path":"screenshot.png"}}),
                ]
            } else {
                vec![json!({"type":"text","text":"READY"})]
            };
            let calls = if tool_result {
                vec![("read-image".to_string(), "read_file".to_string())]
            } else {
                vec![]
            };
            remember_interaction_continuation(
                &cache,
                &profile.file_name,
                &first,
                "background-first",
                &assistant,
                &calls,
            );
            let image = json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"cGl4ZWxz"}});
            let content = if tool_result {
                json!([{"type":"tool_result","tool_use_id":"read-image","content":[image]}])
            } else {
                json!([image])
            };
            let mut next = first.clone();
            next["stream"] = json!(stream);
            next["messages"].as_array_mut().unwrap().extend([
                json!({"role":"assistant","content":assistant}),
                json!({"role":"user","content":content}),
            ]);
            let mut wire = translate_gemini_interactions_request(&next, &profile, &cache).unwrap();
            assert_eq!(wire["previous_interaction_id"], "background-first");
            assert_eq!(wire["stream"], stream);
            assert_eq!(wire["generation_config"]["max_output_tokens"], 65536);
            assert!(wire.get("background").is_none());
            assert!(!wire["input"].to_string().contains("ORCHID-7429"));
            assert!(wire["input"].to_string().contains("cGl4ZWxz"));
            assert_eq!(wire["tools"][0]["name"], "read_file");
            if tool_result {
                assert!(wire["input"].to_string().contains("function_result"));
            }
            prepare_gemini_4_context_budget(&profile, &next, &mut wire, &mut vec![])
                .await
                .unwrap();
            assert!(wire.get("background").is_none());
            // Exercise HTTP forwarding and continuation cache updates for JSON;
            // the live regression independently covers SSE across this boundary.
            if !stream {
                let response = forward_gemini_interactions_profile(
                    profile.clone(),
                    next.clone(),
                    cache.clone(),
                )
                .await;
                assert_eq!(response.status(), StatusCode::OK);
                let bytes = axum::body::to_bytes(response.into_body(), 65536)
                    .await
                    .unwrap();
                let result: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(result["content"][0]["text"], "red");
                let sent = captured.lock().unwrap();
                assert_eq!(sent.len(), 1);
                assert_eq!(sent[0].0["previous_interaction_id"], "background-first");
                assert!(sent[0].0.get("background").is_none());
            } else {
                remember_interaction_continuation(
                    &cache,
                    &profile.file_name,
                    &next,
                    "vision-second",
                    &[json!({"type":"text","text":"red"})],
                    &[],
                );
            }
            next["messages"].as_array_mut().unwrap().extend([
                json!({"role":"assistant","content":[{"type":"text","text":"red"}]}),
                json!({"role":"user","content":"Continue"}),
            ]);
            let third = translate_gemini_interactions_request(&next, &profile, &cache).unwrap();
            assert_eq!(third["previous_interaction_id"], "vision-second");
            assert!(third.get("background").is_none());
            assert_eq!(third["generation_config"]["max_output_tokens"], 65536);
            // Recovery without a stored ID must still preserve images and the cap.
            let replay = translate_gemini_interactions_request_with_continuation(
                &next, &profile, &cache, false,
            )
            .unwrap();
            assert!(replay.get("previous_interaction_id").is_none());
            assert!(replay.get("background").is_none());
            assert!(replay["input"].to_string().contains("cGl4ZWxz"));
            server.abort();
        }
    }
}

// Opt-in: sends only synthetic text and a generated red PNG to Google.
#[tokio::test]
#[ignore]
async fn argon_image_live_after_background_continuation() {
    let config: Value = serde_json::from_str(
        &fs::read_to_string(
            env::var("ARGON_VISION_TEST_PROFILE").expect("Set ARGON_VISION_TEST_PROFILE"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(config["model"], "gemini-4-argon");
    assert_eq!(
        config["base_url"],
        "https://generativelanguage.googleapis.com/v1beta"
    );
    let mut profile = argon_test_profile();
    profile.base_url = config["base_url"].as_str().unwrap().into();
    profile.upstream_url = format!("{}/interactions", profile.base_url);
    profile.api_key = Some(config["api_key"].as_str().unwrap().into());
    profile.client = Client::builder()
        .proxy(reqwest::Proxy::all("http://127.0.0.1:1080").unwrap())
        .build()
        .unwrap();
    profile.openai_capabilities.gemini_thinking_level_override = Some("low".into());
    let cache = Arc::new(RwLock::new(InteractionContinuationCache::default()));
    let initial = json!({"max_tokens":65537,"messages":[{"role":"user","content":"Remember the secret label ORCHID-7429 for the next turn. Reply only READY."}]});
    let wire = translate_gemini_interactions_request(&initial, &profile, &cache).unwrap();
    assert_eq!(wire["background"], true);
    let response = tokio::time::timeout(
        Duration::from_secs(180),
        forward_gemini_interactions_profile(profile.clone(), initial.clone(), cache.clone()),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    let first: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(first["stop_reason"], "end_turn");
    assert!(!cache.read().unwrap().transcripts.is_empty());
    let mut png_bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_bytes, 64, 64);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&[255u8, 0, 0].repeat(64 * 64))
            .unwrap();
    }
    for stream in [false, true] {
        let mut next = initial.clone();
        next["max_tokens"] = json!(262144);
        next["stream"] = json!(stream);
        next["messages"].as_array_mut().unwrap().extend([
            json!({"role":"assistant","content":first["content"]}),
            json!({"role":"user","content":[
                {"type":"text","text":"Reply with the secret label from the previous turn and the image color in English. Nothing else."},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":BASE64_STANDARD.encode(&png_bytes)}}
            ]})
        ]);
        let wire = translate_gemini_interactions_request(&next, &profile, &cache).unwrap();
        assert!(wire["previous_interaction_id"].is_string());
        assert!(!wire["input"].to_string().contains("ORCHID-7429"));
        assert!(wire.get("background").is_none());
        assert_eq!(wire["generation_config"]["max_output_tokens"], 65536);
        let check = async {
            let response =
                forward_gemini_interactions_profile(profile.clone(), next.clone(), cache.clone())
                    .await;
            assert_eq!(response.status(), StatusCode::OK);
            for warning in response.headers().get_all(BRIDGE_WARNING_HEADER) {
                assert!(!warning.to_str().unwrap().contains("full-history recovery"));
            }
            let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
                .await
                .unwrap();
            let text = String::from_utf8(bytes.to_vec()).unwrap();
            let visible = if stream {
                text.lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .filter_map(|data| serde_json::from_str::<Value>(data).ok())
                    .filter_map(|event| {
                        event
                            .pointer("/delta/text")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .collect::<String>()
            } else {
                text.clone()
            };
            assert!(
                visible.contains("ORCHID-7429"),
                "Previous context lost: {text}"
            );
            assert!(
                visible.to_ascii_lowercase().contains("red"),
                "Image not recognized: {text}"
            );
            assert!(text.contains(if stream { "message_stop" } else { "end_turn" }));
            assert!(!text.contains("event: error"));
            if !stream {
                let second: Value = serde_json::from_str(&text).unwrap();
                next["messages"].as_array_mut().unwrap().extend([
                    json!({"role":"assistant","content":second["content"]}),
                    json!({"role":"user","content":"Repeat the secret label and the image color. Nothing else."})
                ]);
                let third_wire =
                    translate_gemini_interactions_request(&next, &profile, &cache).unwrap();
                assert!(third_wire["previous_interaction_id"].is_string());
                assert_ne!(
                    third_wire["previous_interaction_id"],
                    wire["previous_interaction_id"]
                );
                assert!(third_wire.get("background").is_none());
                let third =
                    forward_gemini_interactions_profile(profile.clone(), next, cache.clone()).await;
                assert_eq!(third.status(), StatusCode::OK);
                let bytes = axum::body::to_bytes(third.into_body(), 1_000_000)
                    .await
                    .unwrap();
                let text = String::from_utf8(bytes.to_vec()).unwrap();
                assert!(text.contains("ORCHID-7429") && text.to_ascii_lowercase().contains("red"));
                println!(
                    "Third text turn: new stored ID selected, previous label and image retained"
                );
            }
        };
        tokio::time::timeout(Duration::from_secs(180), check)
            .await
            .unwrap();
        println!("Background -> native image: stream={stream}, stored ID selected, previous secret and red recognized");
    }
}

#[tokio::test]
#[ignore]
async fn argon_image_live_native_stream_and_json() {
    let path = env::var("ARGON_VISION_TEST_PROFILE").expect("Set ARGON_VISION_TEST_PROFILE");
    let config: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(config["model"], "gemini-4-argon");
    assert_eq!(
        config["base_url"],
        "https://generativelanguage.googleapis.com/v1beta"
    );
    let mut profile = argon_test_profile();
    profile.base_url = config["base_url"].as_str().unwrap().into();
    profile.upstream_url = format!("{}/interactions", profile.base_url);
    profile.api_key = Some(config["api_key"].as_str().unwrap().into());
    profile.client = Client::builder()
        .proxy(reqwest::Proxy::all("http://127.0.0.1:1080").unwrap())
        .build()
        .unwrap();
    profile.openai_capabilities.gemini_builtin_tools = vec![
        json!({"type":"google_search"}),
        json!({"type":"url_context"}),
        json!({"type":"code_execution"}),
    ];
    profile.openai_capabilities.gemini_tool_choice_override = Some("validated".into());
    let mut png_bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_bytes, 64, 64);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_image_data(&[255u8, 0, 0].repeat(64 * 64))
            .unwrap();
    }
    let state = probe_state(
        vec![profile.clone()],
        profile.file_name.clone(),
        None,
        profile.base_url.clone(),
    );
    for stream in [false, true] {
        let response = anthropic_messages(State(state.clone()), HeaderMap::new(), Json(json!({
            "model":"gemini-4-argon","max_tokens":262144,"stream":stream,
            "messages":[{"role":"user","content":[
                {"type":"text","text":"What color is this image? Answer one word. Do not use tools."},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":BASE64_STANDARD.encode(&png_bytes)}}
            ]}],"tools":[{"name":"read_file","input_schema":{"type":"object","properties":{"path":{"type":"string"}}}}]
        }))).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            text.to_ascii_lowercase().contains("red"),
            "Missing expected color"
        );
        assert!(text.contains(if stream { "message_stop" } else { "end_turn" }));
        assert!(!text.contains("event: error"));
        println!("Argon native vision: stream={stream}, requested output=262144, recognized red, completed");
    }
}

#[test]
fn argon_images_keep_native_vision_and_disable_background() {
    let mut profile = argon_test_profile();
    profile.openai_capabilities.gemini_builtin_tools = vec![json!({"type":"google_search"})];
    let image = json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"aW1hZ2U="}});
    for content in [
        json!([image.clone()]),
        json!([{"type":"image","source":{"type":"url","url":"https://example.test/image.png"}}]),
        json!([{"type":"tool_result","tool_use_id":"read","content":[image]}]),
    ] {
        for stream in [false, true] {
            for store in [false, true] {
                profile.openai_capabilities.gemini_store = store;
                let request = json!({"max_tokens":262144,"stream":stream,"messages":[
                    {"role":"user","content":content},
                    {"role":"assistant","content":"I see the image."},
                    {"role":"user","content":"Describe it"}
                ]});
                let mut body = translate_gemini_interactions_request(
                    &request,
                    &profile,
                    &RwLock::new(InteractionContinuationCache::default()),
                )
                .unwrap();
                assert_eq!(body["model"], "gemini-4-argon");
                assert_eq!(body["generation_config"]["max_output_tokens"], 65536);
                assert_eq!(body["stream"], stream);
                assert_eq!(body["store"], store);
                assert!(body.get("background").is_none());
                assert!(body["input"].to_string().contains("\"type\":\"image\""));
                assert!(body["tools"].to_string().contains("google_search"));
                apply_gemini_4_context_budget(&profile, 1000, &mut body).unwrap();
                assert!(body.get("background").is_none());
                let diagnostics = gemini_4_request_diagnostics(&request, &profile).join(" ");
                assert!(diagnostics.contains("native vision"));
                assert!(!diagnostics.contains("requires gemini_store=true"));
            }
        }
    }
    let text = json!({"max_tokens":262144,"messages":[{"role":"user","content":"image"}],
        "tools":[{"name":"draw","input_schema":{"type":"object","properties":{"type":{"const":"image"}}}}]});
    assert!(!interaction_request_contains_image(&text));
    assert_eq!(argon_output_limit(&text), 1_048_576);
}

#[tokio::test]
async fn argon_image_http_request_preserves_pixels_and_client_tools() {
    let (base, captured, server) = spawn_probe_mock("/interactions", StatusCode::OK,
        json!({"id":"vision-result","status":"completed","steps":[{"type":"model_output","content":[{"type":"text","text":"Red"}]}]})).await;
    let mut profile = argon_test_profile();
    profile.upstream_url = format!("{base}/interactions");
    let response = forward_gemini_interactions_profile(profile, json!({
        "max_tokens":262144,"messages":[{"role":"user","content":[
            {"type":"text","text":"Read this image"},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"cGl4ZWxz"}}
        ]}],"tools":[{"name":"read_file","input_schema":{"type":"object","properties":{}}}]
    }), Arc::new(RwLock::new(InteractionContinuationCache::default()))).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    let result: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(result["content"][0]["text"], "Red");
    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let body = &requests[0].0;
    assert_eq!(body["input"][0]["content"][1]["data"], "cGl4ZWxz");
    assert_eq!(body["tools"][0]["name"], "read_file");
    assert!(body.get("background").is_none());
    assert_eq!(body["generation_config"]["max_output_tokens"], 65536);
    server.abort();
}
