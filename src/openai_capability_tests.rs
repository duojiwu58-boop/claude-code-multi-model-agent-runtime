#[tokio::test]
async fn openai_json_mode_validates_before_json_and_stream_delivery() {
    for streaming in [false, true] {
        let state = api_test_state(profile("json-object"));
        let response=responses(State(state.clone()),HeaderMap::new(),Json(json!({"model":"test-model","input":"JSON","store":false,"stream":streaming,"text":{"format":{"type":"json_object"}}}))).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), MAX_UPSTREAM_RESPONSE_BYTES)
            .await
            .unwrap();
        let full = if streaming {
            let events = SseDataDecoder::default().push_bytes(&bytes).unwrap();
            let e: Value = serde_json::from_str(events.last().unwrap()).unwrap();
            assert_eq!(e["type"], "response.completed");
            e["response"].clone()
        } else {
            serde_json::from_slice::<Value>(&bytes).unwrap()
        };
        let text = full["output"][0]["content"][0]["text"].as_str().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(text).unwrap()["arbitrary"]["values"],
            json!([1, true, null])
        );
        assert!(!api_store_path(&state).exists());
    }
    let state = api_test_state(profile("normal"));
    let response = responses(
        State(state.clone()),
        HeaderMap::new(),
        Json(json!({"model":"test-model","input":"JSON","text":{"format":{"type":"json_object"}}})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(!api_store_path(&state).exists());
}

async fn api_wait_background(state: Arc<AppState>, id: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let response = api_json(
                api_response_get(State(state.clone()), axum::extract::Path(id.into())).await,
            )
            .await;
            if !matches!(response["status"].as_str(), Some("queued" | "in_progress")) {
                return response;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap()
}
#[tokio::test]
async fn openai_background_poll_cancel_stream_replay_and_restart() {
    let state = api_test_state(profile("normal"));
    let first = api_json(
        responses(
            State(state.clone()),
            HeaderMap::new(),
            Json(json!({"model":"test-model","input":"hello","background":true})),
        )
        .await,
    )
    .await;
    assert_eq!(first["status"], "queued");
    let id = first["id"].as_str().unwrap();
    let last = api_wait_background(state.clone(), id).await;
    assert_eq!(last["status"], "completed");
    assert_eq!(last["background"], true);
    assert_eq!(
        api_json(api_response_cancel(State(state.clone()), axum::extract::Path(id.into())).await)
            .await,
        last
    );
    let response=responses(State(state.clone()),HeaderMap::new(),Json(json!({"model":"test-model","input":"hello","background":true,"stream":true,"store":false}))).await;
    let bytes = axum::body::to_bytes(response.into_body(), MAX_UPSTREAM_RESPONSE_BYTES)
        .await
        .unwrap();
    let events: Vec<Value> = SseDataDecoder::default()
        .push_bytes(&bytes)
        .unwrap()
        .iter()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(events.last().unwrap()["type"], "response.completed");
    let stream_id = events.last().unwrap()["response"]["id"].as_str().unwrap();
    let replay = api_response_retrieve(
        State(state.clone()),
        axum::extract::Path(stream_id.into()),
        axum::extract::Query(HashMap::from([
            ("stream".into(), "true".into()),
            ("starting_after".into(), "1".into()),
        ])),
    )
    .await;
    let bytes = axum::body::to_bytes(replay.into_body(), MAX_UPSTREAM_RESPONSE_BYTES)
        .await
        .unwrap();
    let replay: Vec<Value> = SseDataDecoder::default()
        .push_bytes(&bytes)
        .unwrap()
        .iter()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(replay, events[2..]);
    api_json(api_response_delete(State(state.clone()), axum::extract::Path(id.into())).await).await;
    api_json(
        api_response_delete(State(state.clone()), axum::extract::Path(stream_id.into())).await,
    )
    .await;
    api_store_transaction(&state, true, |s| {
        s["responses"]["resp_restart"] =
            json!({"response":{"background":true,"status":"in_progress"}});
        Ok(())
    })
    .unwrap();
    api_background_recover(&state).unwrap();
    assert_eq!(
        api_json(
            api_response_get(
                State(state.clone()),
                axum::extract::Path("resp_restart".into())
            )
            .await
        )
        .await["status"],
        "failed"
    );
    api_cleanup(&state);

    let state = api_test_state(profile("timeout"));
    let first = api_json(
        responses(
            State(state.clone()),
            HeaderMap::new(),
            Json(json!({"model":"test-model","input":"hello","background":true})),
        )
        .await,
    )
    .await;
    let id = first["id"].as_str().unwrap();
    assert_eq!(
        api_response_delete(State(state.clone()), axum::extract::Path(id.into()))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let cancelled = tokio::time::timeout(
        Duration::from_secs(5),
        api_response_cancel(State(state.clone()), axum::extract::Path(id.into())),
    )
    .await
    .unwrap();
    assert_eq!(api_json(cancelled).await["status"], "cancelled");
    assert_eq!(
        api_json(api_response_cancel(State(state.clone()), axum::extract::Path(id.into())).await)
            .await["status"],
        "cancelled"
    );
    api_json(api_response_delete(State(state.clone()), axum::extract::Path(id.into())).await).await;
    api_cleanup(&state);
}

#[tokio::test]
async fn openai_stored_chat_and_multiple_choices_json_sse() {
    let state = api_test_state(profile("normal"));
    for streaming in [false, true] {
        let response=api_chat(State(state.clone()),Json(json!({"model":"test-model","messages":[{"role":"user","content":"hello"}],"n":2,"stream":streaming,"stream_options":{"include_usage":true},"store":true,"metadata":{"test":"multi"}}))).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), MAX_UPSTREAM_RESPONSE_BYTES)
            .await
            .unwrap();
        let id = if streaming {
            let chunks = SseDataDecoder::default().push_bytes(&bytes).unwrap();
            assert_eq!(chunks.last().unwrap(), "[DONE]");
            let chunks: Vec<Value> = chunks[..chunks.len() - 1]
                .iter()
                .map(|s| serde_json::from_str(s).unwrap())
                .collect();
            assert!(chunks.iter().all(|c| c["error"].is_null()));
            assert_eq!(chunks.last().unwrap()["usage"]["completion_tokens"], 16);
            assert!(chunks.iter().any(|c| c["choices"][0]["index"] == 1));
            chunks[0]["id"].as_str().unwrap().to_owned()
        } else {
            let full: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(full["choices"].as_array().unwrap().len(), 2);
            assert_eq!(full["usage"]["completion_tokens"], 16);
            full["id"].as_str().unwrap().to_owned()
        };
        let stored =
            api_json(api_chat_get(State(state.clone()), axum::extract::Path(id.clone())).await)
                .await;
        assert_eq!(stored["choices"].as_array().unwrap().len(), 2);
        let updated = api_json(
            api_chat_update(
                State(state.clone()),
                axum::extract::Path(id.clone()),
                Json(json!({"metadata":{"test":"updated"}})),
            )
            .await,
        )
        .await;
        assert_eq!(updated["metadata"]["test"], "updated");
        let list = api_json(
            api_chat_list(
                State(state.clone()),
                axum::extract::Query(HashMap::from([("metadata[test]".into(), "updated".into())])),
            )
            .await,
        )
        .await;
        assert_eq!(list["data"].as_array().unwrap().len(), 1);
        let messages = api_json(
            api_chat_messages(
                State(state.clone()),
                axum::extract::Path(id.clone()),
                axum::extract::Query(HashMap::new()),
            )
            .await,
        )
        .await;
        assert_eq!(messages["data"][0]["content"], "hello");
        assert_eq!(messages["data"][1]["role"], "assistant");
        assert_eq!(messages["data"].as_array().unwrap().len(), 3);
        api_json(api_chat_delete(State(state.clone()), axum::extract::Path(id.clone())).await)
            .await;
        assert_eq!(
            api_chat_get(State(state.clone()), axum::extract::Path(id))
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    api_cleanup(&state);
}

#[tokio::test]
async fn openai_files_resolve_delete_and_reject_binary() {
    let state = api_test_state(profile("normal"));
    api_store_transaction(&state,true,|s|{s["files"]["file-test"]=json!({"file":{"id":"file-test","filename":"a.txt"},"mime":"text/plain","data":BASE64_STANDARD.encode("秘密 TEXT_841")});Ok(())}).unwrap();
    let mut items = vec![
        json!({"type":"message","role":"user","content":[{"type":"input_file","file_id":"file-test"}]}),
    ];
    api_expand_files(&state, &mut items).await.unwrap();
    let native = api_items_to_messages(&items).unwrap();
    assert_eq!(native[0]["content"][0]["source"]["data"], "秘密 TEXT_841");
    let content = api_file_content(
        State(state.clone()),
        axum::extract::Path("file-test".into()),
    )
    .await;
    assert_eq!(
        axum::body::to_bytes(content.into_body(), 1024)
            .await
            .unwrap()
            .as_ref(),
        "秘密 TEXT_841".as_bytes()
    );
    api_json(
        api_file_delete(
            State(state.clone()),
            axum::extract::Path("file-test".into()),
        )
        .await,
    )
    .await;
    assert!(api_file_record(&state, "file-test").is_err());
    let mut content =
        json!([{"type":"input_file","file_data":"data:application/octet-stream;base64,AP8="}]);
    assert!(api_expand_content(&state, &mut content).await.is_err());
    api_cleanup(&state);
}

#[test]
fn openai_custom_text_tools_preserve_raw_arguments_and_history() {
    let req = json!({"model":"test-model","tools":[{"type":"custom","name":"patch","format":{"type":"text"}}],"tool_choice":{"type":"custom","name":"patch"}});
    let raw = "*** Begin Patch\n*** End Patch\n";
    let items = vec![
        json!({"type":"custom_tool_call","name":"patch","call_id":"c1","input":raw}),
        json!({"type":"custom_tool_call_output","call_id":"c1","output":"done"}),
    ];
    let native = api_responses_request(&req, &items, "custom-test").unwrap();
    assert_eq!(
        native["tools"][0]["input_schema"]["properties"]["input"]["type"],
        "string"
    );
    let message =
        json!({"content":[{"type":"tool_use","id":"c1","name":"patch","input":{"input":raw}}]});
    api_validate_custom_output(&message, &req).unwrap();
    let output = api_output_items_for(&message, &req);
    assert_eq!(output[0]["type"], "custom_tool_call");
    assert_eq!(output[0]["input"], raw);
    assert_eq!(
        api_items_to_messages(&output).unwrap()[0]["content"][0]["input"]["input"],
        raw
    );
}
#[tokio::test]
async fn openai_strict_tools_validate_before_delivery() {
    for valid in [true, false] {
        let mut req = tool_request();
        req["tools"][0]["strict"] = json!(true);
        req["tools"][0]["input_schema"] = if valid {
            json!({"type":"object","properties":{"file_path":{"type":"string"}},"required":["file_path"],"additionalProperties":false})
        } else {
            json!({"type":"object","properties":{"count":{"type":"integer"}},"required":["count"],"additionalProperties":false})
        };
        let response = forward_codex_profile(profile("normal"), req).await;
        assert_eq!(
            response.status(),
            if valid {
                StatusCode::OK
            } else {
                StatusCode::BAD_GATEWAY
            }
        );
        let result = body(response).await;
        if valid {
            assert_eq!(result["content"][0]["type"], "tool_use");
        } else {
            assert!(result.to_string().contains("strict JSON Schema"));
        }
    }
    let mut req = tool_request();
    req["tools"][0]["strict"] = json!(true);
    req["tools"][0]["input_schema"] = json!({"$ref":"https://example.invalid/schema.json"});
    assert!(prepare_codex_request(&req, &profile("normal")).is_err());
    req["tools"][0]["input_schema"] = json!({"$ref":"file:///C:/secret.json"});
    assert!(prepare_codex_request(&req, &profile("normal")).is_err());
}
#[test]
fn background_terminal_commit_keeps_conversation_and_response_consistent() {
    for persist in [true, false] {
        for outcome in ["completed", "cancelled", "overflow"] {
            let p = profile("normal");
            let state = api_test_state(p.clone());
            api_store_transaction(&state, true, |s| {
                s["conversations"]["conv_review"] = json!({"items":[]}); Ok(())
            }).unwrap();
            let context = ApiContext::prepare(state.clone(), &p, json!({
                "input":"hello", "background":true, "store":persist, "conversation":"conv_review"
            })).unwrap();
            let completed = context.finish(&json!({"model":"test-model","content":[{"type":"text","text":"answer"}],"stop_reason":"end_turn","usage":{}})).unwrap();
            assert_eq!(completed["background"], true);
            api_store_transaction(&state, false, |s| {
                assert_eq!(s["conversations"]["conv_review"]["items"], json!([]));
                assert!(s["responses"].get(&context.response_id).is_none()); Ok(())
            }).unwrap();
            let (cancel, _) = tokio::sync::watch::channel(false);
            let (changed, _) = tokio::sync::watch::channel(0);
            let job = ApiBackgroundJob {
                state:state.clone(), record:json!({"input":context.input,"profile":context.profile_key,"session":context.session}),
                persist, conversation:context.conversation.clone(), stream_enabled:true, cancel, changed,
                data:std::sync::Mutex::new(ApiBackgroundData {
                    response:json!({"id":context.response_id,"status":"in_progress","background":true,"output":[]}),
                    events:vec![], bytes:0, terminal_at:None,
                }),
            };
            let event = json!({"type":"response.completed","response":completed});
            if outcome == "cancelled" {
                api_background_end(&job,"cancelled",None);
                api_background_push(&job,event).unwrap(); // late completion cannot overwrite cancellation
            } else if outcome == "overflow" {
                job.data.lock().unwrap().bytes=MAX_UPSTREAM_SSE_BUFFER_BYTES;
                assert!(api_background_push(&job,event).is_err());
                api_background_end(&job,"failed",Some("overflow"));
            } else {
                api_background_push(&job,event.clone()).unwrap();
                api_background_push(&job,event).unwrap(); // terminal commit is idempotent
                api_background_end(&job,"cancelled",None);
            }
            let expected=if outcome=="overflow" {"failed"} else {outcome};
            assert_eq!(job.data.lock().unwrap().response["status"],expected);
            api_store_transaction(&state,false,|s| {
                let items=s["conversations"]["conv_review"]["items"].as_array().unwrap();
                assert_eq!(!items.is_empty(),outcome=="completed");
                if persist {
                    assert_eq!(s["responses"][&context.response_id]["response"]["status"],expected);
                    assert_eq!(s["responses"][&context.response_id]["response"]["background"],true);
                } else { assert!(s["responses"].get(&context.response_id).is_none()); }
                Ok(())
            }).unwrap();
            drop(context);
            api_cleanup(&state);
        }
    }
}

#[tokio::test]
async fn native_stream_idle_timeout_ends_stalled_body() {
    let app=axum::Router::new().route("/",axum::routing::get(|| async {
        Body::from_stream(stream::unfold(0,|n|async move {
            if n==2 {return None;}
            if n==1 {tokio::time::sleep(Duration::from_millis(150)).await;}
            Some((Ok::<_,std::io::Error>("data: hello\n\n"),(n+1)))
        }))
    }));
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url=format!("http://{}/",listener.local_addr().unwrap());
    let server=tokio::spawn(async move {axum::serve(listener,app).await.unwrap()});
    let client=Client::builder().no_proxy().build().unwrap();
    let request=apply_upstream_total_timeout(client.get(&url),true).build().unwrap();
    assert!(request.timeout().is_none());
    let upstream=client.execute(request).await.unwrap();
    let mut body=api_native_body(upstream,true,Duration::from_millis(30)).into_data_stream();
    assert!(body.next().await.unwrap().is_ok());
    assert!(body.next().await.unwrap().is_err());
    assert!(body.next().await.is_none());
    let upstream=client.get(&url).send().await.unwrap();
    let bytes=axum::body::to_bytes(api_native_body(upstream,false,Duration::from_millis(1)),1024).await.unwrap();
    assert_eq!(bytes.len(),26);
    server.abort();
}
