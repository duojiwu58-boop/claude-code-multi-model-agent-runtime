fn api_save_chat(state: &AppState, request: &Value, mut response: Value) -> Result<Value, String> {
    response["metadata"] = api_metadata(request)?;
    if request["store"] == true {
        let id = api_string(&response, "id")?.to_owned();
        let mut messages = request["messages"].as_array().cloned().unwrap_or_default();
        if let Some(choices) = response["choices"].as_array() {
            messages.extend(choices.iter().map(|choice| choice["message"].clone()));
        }
        api_store_transaction(state, true, |s| {
            s["chats"][&id] = json!({"completion":response,"messages":messages});
            Ok(())
        })?;
    }
    Ok(response)
}
impl ApiContext {
    fn finish_chat(&self, message: &Value) -> Result<Value, String> {
        api_save_chat(
            &self.state,
            self.chat_request.as_ref().unwrap_or(&json!({})),
            api_chat_object(&self.response_id, self.created, message),
        )
    }
}
async fn api_chat_get(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    match api_store_transaction(&state, false, |s| {
        s["chats"]
            .get(&id)
            .map(|r| r["completion"].clone())
            .ok_or_else(|| format!("Not found: chat completion '{id}'"))
    }) {
        Ok(v) => Json(v).into_response(),
        Err(e) => api_resource_error(e),
    }
}
async fn api_chat_list(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(mut q): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    q.entry("order".into()).or_insert("asc".into());
    match api_store_transaction(&state, false, |s| {
        let mut items: Vec<Value> = s["chats"]
            .as_object()
            .unwrap()
            .values()
            .map(|r| r["completion"].clone())
            .filter(|c| {
                q.get("model").is_none_or(|m| c["model"] == m.as_str())
                    && q.iter()
                        .filter_map(|(k, v)| {
                            k.strip_prefix("metadata[")
                                .and_then(|k| k.strip_suffix(']'))
                                .map(|k| (k, v))
                        })
                        .all(|(k, v)| c["metadata"][k] == v.as_str())
            })
            .collect();
        items.sort_by_key(|v| {
            (
                v["created"].as_u64().unwrap_or(0),
                v["id"].as_str().unwrap_or("").to_string(),
            )
        });
        api_page(items, &q)
    }) {
        Ok(v) => Json(v).into_response(),
        Err(e) => api_resource_error(e),
    }
}
async fn api_chat_update(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(request): Json<Value>,
) -> Response {
    let result = (|| {
        let metadata = api_metadata(&request)?;
        if request.as_object().unwrap().keys().any(|k| k != "metadata") {
            return Err("Only stored chat metadata can be updated".into());
        }
        api_store_transaction(&state, true, |s| {
            let record = s["chats"]
                .get_mut(&id)
                .ok_or_else(|| format!("Not found: chat completion '{id}'"))?;
            record["completion"]["metadata"] = metadata;
            Ok(record["completion"].clone())
        })
    })();
    match result {
        Ok(v) => Json(v).into_response(),
        Err(e) => api_resource_error(e),
    }
}
async fn api_chat_delete(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    match api_store_transaction(&state, true, |s| {
        s["chats"]
            .as_object_mut()
            .unwrap()
            .remove(&id)
            .ok_or_else(|| format!("Not found: chat completion '{id}'"))
    }) {
        Ok(_) => {
            Json(json!({"id":id,"object":"chat.completion.deleted","deleted":true})).into_response()
        }
        Err(e) => api_resource_error(e),
    }
}
async fn api_chat_messages(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(mut q): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    q.entry("order".into()).or_insert("asc".into());
    match api_store_transaction(&state, false, |s| {
        let record = s["chats"]
            .get(&id)
            .ok_or_else(|| format!("Not found: chat completion '{id}'"))?;
        let mut messages = record["messages"]
            .as_array()
            .ok_or("Invalid stored chat messages")?
            .clone();
        for (index, message) in messages.iter_mut().enumerate() {
            message["id"] = json!(format!("{id}_msg_{index}"));
            if message["content"].is_array() {
                message["content_parts"] = message["content"].clone();
                message["content"] = Value::Null;
            } else {
                message["content_parts"] = Value::Null;
            }
        }
        api_page(messages, &q)
    }) {
        Ok(v) => Json(v).into_response(),
        Err(e) => api_resource_error(e),
    }
}
fn api_add_usage(total: &mut Value, usage: &Value) {
    if total.is_null() || usage.is_null() {
        *total = Value::Null;
        return;
    }
    for key in ["prompt_tokens", "completion_tokens", "total_tokens"] {
        total[key] = json!(total[key].as_u64().unwrap_or(0) + usage[key].as_u64().unwrap_or(0));
    }
    for (outer, inner) in [
        ("prompt_tokens_details", "cached_tokens"),
        ("prompt_tokens_details", "cache_write_tokens"),
        ("completion_tokens_details", "reasoning_tokens"),
    ] {
        total[outer][inner] = json!(
            total[outer][inner].as_u64().unwrap_or(0) + usage[outer][inner].as_u64().unwrap_or(0)
        );
    }
}
async fn api_chat_multiple(
    state: Arc<AppState>,
    profile: ProviderProfile,
    mut native: Value,
    request: Value,
) -> Response {
    if let Err(e) = api_metadata(&request) {
        return api_error(StatusCode::BAD_REQUEST, e);
    }
    let count = request["n"].as_u64().unwrap_or(1);
    let streaming = request["stream"] == true;
    let id = format!("chatcmpl-{}", Uuid::new_v4().simple());
    let created = api_now();
    let mut full = json!({"id":id,"object":"chat.completion","created":created,"model":request["model"],"choices":[],"usage":{}});
    let mut single = request.clone();
    single["n"] = json!(1);
    single["store"] = json!(false);
    // Branch requests must not claim the same stateless pending tool callback.
    if count > 1
        && request["messages"]
            .as_array()
            .is_some_and(|m| m.iter().any(|m| m["role"] == "tool"))
    {
        return api_error(StatusCode::BAD_REQUEST,"n>1 with tool-result histories cannot share one native callback; complete that tool roundtrip with n=1 before branching");
    }
    if !streaming {
        for i in 0..count {
            let response = api_codex(
                state.clone(),
                profile.clone(),
                native.clone(),
                Some(single.clone()),
            )
            .await;
            if !response.status().is_success() {
                return response;
            }
            let bytes =
                match axum::body::to_bytes(response.into_body(), MAX_UPSTREAM_RESPONSE_BYTES).await
                {
                    Ok(b) => b,
                    Err(_) => {
                        return api_error(StatusCode::BAD_GATEWAY, "Invalid multi-choice response")
                    }
                };
            let result: Value = match serde_json::from_slice(&bytes) {
                Ok(v) => v,
                Err(_) => return api_error(StatusCode::BAD_GATEWAY, "Invalid multi-choice JSON"),
            };
            let mut choice = result["choices"][0].clone();
            choice["index"] = json!(i);
            full["choices"].as_array_mut().unwrap().push(choice);
            api_add_usage(&mut full["usage"], &result["usage"]);
        }
        return match api_save_chat(&state, &request, full) {
            Ok(v) => Json(v).into_response(),
            Err(e) => api_resource_error(e),
        };
    }
    native["stream"] = json!(true);
    single["stream_options"] = json!({"include_usage":true});
    let (sender, receiver) = tokio::sync::mpsc::channel(32);
    let task = tokio::spawn(async move {
        let result:Result<(),String>=async {
            for i in 0..count {
                let response=api_codex(state.clone(),profile.clone(),native.clone(),Some(single.clone())).await;
                if !response.status().is_success() {return Err(format!("Choice {i} failed with HTTP {}",response.status()));}
                let mut upstream=response.into_body().into_data_stream();
                let mut decoder=SseDataDecoder::default();
                let mut choice=json!({"index":i,"message":{"role":"assistant","content":"","refusal":null},"logprobs":null,"finish_reason":null});
                let mut calls=std::collections::BTreeMap::<u64,Value>::new();
                let mut ended=false;
                while let Some(bytes)=upstream.next().await {
                    for payload in decoder.push_bytes(&bytes.map_err(|_|"Multi-choice stream failed")?)? {
                        if payload=="[DONE]" {ended=true;continue;}
                        let mut chunk:Value=serde_json::from_str(&payload).map_err(|_|"Invalid chat stream JSON")?;
                        if !chunk["error"].is_null() {return Err(chunk["error"]["message"].as_str().unwrap_or("Choice failed").into());}
                        if chunk.get("usage").is_some() {api_add_usage(&mut full["usage"],&chunk["usage"]);continue;}
                        for c in chunk["choices"].as_array_mut().ok_or("Missing choices")? {
                            c["index"]=json!(i);
                            if let Some(t)=c["delta"]["content"].as_str() { let old=choice["message"]["content"].as_str().unwrap_or(""); if old.len()+t.len()>MAX_UPSTREAM_RESPONSE_BYTES {return Err("Choice exceeds local output limit".into());} choice["message"]["content"]=json!(format!("{old}{t}")); }
                            for call in c["delta"]["tool_calls"].as_array().into_iter().flatten() {
                                let index=call["index"].as_u64().ok_or("Missing tool index")?;
                                let item=calls.entry(index).or_insert_with(||json!({"type":"function","function":{"name":"","arguments":""}}));
                                if call.get("id").is_some() {item["id"]=call["id"].clone();}
                                for key in ["name","arguments"] {if let Some(t)=call["function"][key].as_str() {let old=item["function"][key].as_str().unwrap_or("");if old.len()+t.len()>MAX_STREAMED_TOOL_ARGUMENT_BYTES {return Err("Tool call exceeds local limit".into());}item["function"][key]=json!(format!("{old}{t}"));}}
                            }
                            if !c["finish_reason"].is_null() {choice["finish_reason"]=c["finish_reason"].clone();}
                        }
                        chunk["id"]=json!(id);chunk["created"]=json!(created);
                        sender.send(Event::default().data(chunk.to_string())).await.map_err(|_|"Client disconnected")?;
                    }
                }
                if !ended || choice["finish_reason"].is_null() {return Err("Choice ended without a final chunk".into());}
                if !calls.is_empty() {choice["message"]["tool_calls"]=json!(calls.into_values().collect::<Vec<_>>());if choice["message"]["content"]=="" {choice["message"]["content"]=Value::Null;}}
                full["choices"].as_array_mut().unwrap().push(choice);
            }
            let full=api_save_chat(&state,&request,full)?;
            if request.pointer("/stream_options/include_usage")==Some(&json!(true)) {
                sender.send(Event::default().data(json!({"id":id,"object":"chat.completion.chunk","created":created,"model":request["model"],"choices":[],"usage":full["usage"]}).to_string())).await.map_err(|_|"Client disconnected")?;
            }
            Ok(())
        }.await;
        if let Err(e) = result {
            let _ = sender
                .send(
                    Event::default()
                        .data(json!({"error":{"type":"api_error","message":e}}).to_string()),
                )
                .await;
        }
        let _ = sender.send(Event::default().data("[DONE]")).await;
    });
    let output = stream::unfold(
        (receiver, CodexStreamTask(task)),
        |(mut receiver, task)| async move {
            receiver
                .recv()
                .await
                .map(|e| (Ok::<Event, Infallible>(e), (receiver, task)))
        },
    );
    Sse::new(output)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}
