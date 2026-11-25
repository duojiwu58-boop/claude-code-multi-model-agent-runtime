// Local OpenAI resources, persisted beside bridge state. All operations are
// authenticated by the router. No prompt text is written to service logs.
const API_STORE_LIMIT: usize = 32 * 1024 * 1024;

fn api_store_path(state: &AppState) -> PathBuf {
    state.bridge_state_path.with_extension("openai.json")
}

fn api_store_transaction<T>(
    state: &AppState,
    write: bool,
    f: impl FnOnce(&mut Value) -> Result<T, String>,
) -> Result<T, String> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK
        .lock()
        .map_err(|_| "OpenAI resource store lock failed")?;
    let path = api_store_path(state);
    let mut store = match fs::metadata(&path) {
        Ok(m) if m.len() > API_STORE_LIMIT as u64 => {
            return Err("OpenAI resource store exceeds its size limit".into())
        }
        Ok(_) => serde_json::from_slice::<Value>(
            &fs::read(&path).map_err(|_| "Cannot read OpenAI resource store")?,
        )
        .map_err(|_| "Invalid OpenAI resource store; restore its backup")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            json!({"version":1,"responses":{},"conversations":{}})
        }
        Err(_) => return Err("Cannot access OpenAI resource store".into()),
    };
    if store["version"] != 1
        || !store["responses"].is_object()
        || !store["conversations"].is_object()
    {
        return Err("Unknown OpenAI resource store format".into());
    }
    for collection in ["files", "chats"] {
        if store.get(collection).is_none() {
            store[collection] = json!({});
        }
        if !store[collection].is_object() {
            return Err(format!("Invalid OpenAI store collection '{collection}'"));
        }
    }
    let out = f(&mut store)?;
    if write {
        if store["responses"].as_object().unwrap().len() > 128
            || store["conversations"].as_object().unwrap().len() > 64
            || store["files"].as_object().unwrap().len() > 64
            || store["chats"].as_object().unwrap().len() > 128
        {
            return Err(
                "Local OpenAI store is full; delete old responses/conversations or use store=false"
                    .into(),
            );
        }
        let encoded = store.to_string();
        if encoded.len() > API_STORE_LIMIT {
            return Err(
                "Local OpenAI store is full (32 MiB); delete old resources or use store=false"
                    .into(),
            );
        }
        write_state_atomically(&path, &encoded)?;
    }
    Ok(out)
}

fn api_profile_key(profile: &ProviderProfile) -> String {
    codex_hash(
        &json!({"file":profile.file_name,"model":profile.model,"url":profile.upstream_url,"protocol":profile.transport.as_str()}),
    )
}

fn api_resource_error(message: String) -> Response {
    let status = if message.starts_with("Not found:") {
        StatusCode::NOT_FOUND
    } else if message.contains("busy") || message.contains("changed during") {
        StatusCode::CONFLICT
    } else if message.contains("store") || message.contains("persist") {
        StatusCode::INSUFFICIENT_STORAGE
    } else {
        StatusCode::BAD_REQUEST
    };
    api_error(status, message)
}

fn api_assign_ids(items: &mut [Value]) -> Result<(), String> {
    let mut seen = HashSet::new();
    for i in items {
        if !i.is_object() {
            return Err("Every input item must be a JSON object".into());
        }
        if i.get("id").is_none() {
            i["id"] = json!(format!("item_{}", Uuid::new_v4().simple()));
        }
        if !seen.insert(api_string(i, "id")?.to_string()) {
            return Err("Duplicate input item ID".into());
        }
    }
    Ok(())
}

fn api_busy_keys() -> &'static std::sync::Mutex<HashSet<String>> {
    static BUSY: std::sync::OnceLock<std::sync::Mutex<HashSet<String>>> =
        std::sync::OnceLock::new();
    BUSY.get_or_init(Default::default)
}
struct ApiConversationLease(String);
impl ApiConversationLease {
    fn claim(state: &AppState, id: &str) -> Result<Self, String> {
        let key = format!("{}:{id}", api_store_path(state).display());
        if !api_busy_keys()
            .lock()
            .map_err(|_| "Conversation lock failed")?
            .insert(key.clone())
        {
            return Err("Conversation is busy; retry after the active response completes".into());
        }
        Ok(Self(key))
    }
}
impl Drop for ApiConversationLease {
    fn drop(&mut self) {
        if let Ok(mut busy) = api_busy_keys().lock() {
            busy.remove(&self.0);
        }
    }
}

struct ApiContext {
    state: Arc<AppState>,
    request: Value,
    input: Vec<Value>,
    session: String,
    profile_key: String,
    response_id: String,
    created: u64,
    conversation: Option<String>,
    _lease: Option<ApiConversationLease>,
    chat_request: Option<Value>,
}

impl ApiContext {
    fn prepare(
        state: Arc<AppState>,
        profile: &ProviderProfile,
        request: Value,
    ) -> Result<Self, String> {
        let previous = request["previous_response_id"].as_str();
        let conversation = request["conversation"]
            .as_str()
            .or_else(|| request.pointer("/conversation/id").and_then(Value::as_str));
        if previous.is_some() && conversation.is_some() {
            return Err("previous_response_id and conversation are mutually exclusive".into());
        }
        if request
            .get("previous_response_id")
            .is_some_and(|v| !v.is_null() && v.as_str().is_none())
        {
            return Err("previous_response_id must be a string".into());
        }
        if request.get("conversation").is_some_and(|v| !v.is_null()) && conversation.is_none() {
            return Err("conversation must be an ID or {id}".into());
        }
        let lease = conversation
            .map(|id| ApiConversationLease::claim(&state, id))
            .transpose()?;
        let key = api_profile_key(profile);
        let mut session = format!("openai:{}", Uuid::new_v4());
        let mut input = Vec::new();
        if let Some(id) = previous {
            let parent = api_store_transaction(&state, false, |s| {
                s["responses"]
                    .get(id)
                    .cloned()
                    .ok_or_else(|| format!("Not found: response '{id}'"))
            })?;
            if parent["profile"] != key {
                return Err(
                    "Response belongs to a different provider/model; select the original profile"
                        .into(),
                );
            }
            if matches!(
                parent["response"]["status"].as_str(),
                Some("queued" | "in_progress")
            ) {
                return Err("Previous response is busy; wait for background completion".into());
            }
            input = parent["input"]
                .as_array()
                .ok_or("Invalid saved input")?
                .clone();
            input.extend(
                parent["response"]["output"]
                    .as_array()
                    .ok_or("Invalid saved output")?
                    .clone(),
            );
            session = api_string(&parent, "session")?.to_string();
        }
        if let Some(id) = conversation {
            let conv = api_store_transaction(&state, false, |s| {
                s["conversations"]
                    .get(id)
                    .cloned()
                    .ok_or_else(|| format!("Not found: conversation '{id}'"))
            })?;
            if conv
                .get("profile")
                .is_some_and(|v| !v.is_null() && v != &json!(key))
            {
                return Err("Conversation belongs to a different provider/model".into());
            }
            input = conv["items"]
                .as_array()
                .ok_or("Invalid conversation items")?
                .clone();
            session = format!("openai:{id}");
        }
        let mut incoming = api_input_items(request.get("input").unwrap_or(&json!([])))?;
        api_assign_ids(&mut incoming)?;
        input.extend(incoming);
        api_assign_ids(&mut input)?;
        if serde_json::to_vec(&input)
            .map_err(|_| "Invalid input")?
            .len()
            > MAX_UPSTREAM_RESPONSE_BYTES
        {
            return Err("Expanded input exceeds the local 8 MiB limit".into());
        }
        Ok(Self {
            state,
            request: request.clone(),
            input,
            session,
            profile_key: key,
            response_id: format!("resp_{}", Uuid::new_v4().simple()),
            created: api_now(),
            conversation: conversation.map(str::to_owned),
            _lease: lease,
            chat_request: None,
        })
    }

    fn finish(&self, message: &Value) -> Result<Value, String> {
        api_validate_custom_output(message, &self.request)?;
        let mut response =
            api_response_object(&self.response_id, self.created, &self.request, message);
        if let Some(id) = &self.conversation {
            response["conversation"] = json!({"id":id});
        }
        // Background completion is committed by its worker only after replay
        // capacity/cancellation have been resolved under the job's terminal lock.
        if self.request["background"] == true {
            response["background"] = json!(true);
            return Ok(response);
        }
        let persist = self.request["store"] != false;
        if persist || self.conversation.is_some() {
            api_store_transaction(&self.state, true, |store| {
                if let Some(id) = &self.conversation {
                    let c = store["conversations"]
                        .get_mut(id)
                        .ok_or("Conversation changed during generation")?;
                    let mut items = self.input.clone();
                    items.extend(api_output_items_for(message, &self.request));
                    c["items"] = json!(items);
                    c["profile"] = json!(self.profile_key);
                }
                if persist {
                    store["responses"][&self.response_id] = json!({"response":response,"input":self.input,"profile":self.profile_key,"session":self.session});
                }
                Ok(())
            })?;
        }
        Ok(response)
    }
}

async fn api_response_get(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if let Some(job) = api_background_find(&state, &id) {
        if let Ok(data) = job.data.lock() {
            return Json(data.response.clone()).into_response();
        }
    }
    match api_store_transaction(&state, false, |s| {
        s["responses"]
            .get(&id)
            .map(|v| v["response"].clone())
            .ok_or_else(|| format!("Not found: response '{id}'"))
    }) {
        Ok(v) => Json(v).into_response(),
        Err(e) => api_resource_error(e),
    }
}
async fn api_response_delete(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let job = api_background_find(&state, &id);
    if job
        .as_ref()
        .is_some_and(|j| j.data.lock().is_ok_and(|d| d.terminal_at.is_none()))
    {
        return api_error(
            StatusCode::CONFLICT,
            "Cancel the active background response before deleting it",
        );
    }
    match api_store_transaction(&state, true, |s| {
        s["responses"]
            .as_object_mut()
            .unwrap()
            .remove(&id)
            .or_else(|| job.as_ref().map(|_| Value::Null))
            .ok_or_else(|| format!("Not found: response '{id}'"))
    }) {
        Ok(_) => {
            if let Ok(mut jobs) = api_background_jobs().lock() {
                jobs.remove(&api_background_key(&state, &id));
            }
            Json(json!({"id":id,"object":"response.deleted","deleted":true})).into_response()
        }
        Err(e) => api_resource_error(e),
    }
}

fn api_page(mut items: Vec<Value>, q: &HashMap<String, String>) -> Result<Value, String> {
    let order = q.get("order").map(String::as_str).unwrap_or("desc");
    if !matches!(order, "asc" | "desc") {
        return Err("order must be asc or desc".into());
    }
    if order == "desc" {
        items.reverse();
    }
    let limit = q
        .get("limit")
        .map(|s| s.parse::<usize>())
        .transpose()
        .map_err(|_| "Invalid limit")?
        .unwrap_or(20);
    if !(1..=100).contains(&limit) {
        return Err("limit must be between 1 and 100".into());
    }
    if let Some(after) = q.get("after") {
        let n = items
            .iter()
            .position(|i| i["id"] == after.as_str())
            .ok_or("Unknown after cursor")?;
        items = items[n + 1..].to_vec();
    }
    let more = items.len() > limit;
    items.truncate(limit);
    Ok(
        json!({"object":"list","first_id":items.first().map(|v|v["id"].clone()),"last_id":items.last().map(|v|v["id"].clone()),"has_more":more,"data":items}),
    )
}

async fn api_response_inputs(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    match api_store_transaction(&state, false, |s| {
        let r = s["responses"]
            .get(&id)
            .ok_or_else(|| format!("Not found: response '{id}'"))?;
        api_page(
            r["input"].as_array().ok_or("Invalid stored input")?.clone(),
            &q,
        )
    }) {
        Ok(v) => Json(v).into_response(),
        Err(e) => api_resource_error(e),
    }
}

fn api_metadata(request: &Value) -> Result<Value, String> {
    if !request.is_object() {
        return Err("Request must be a JSON object".into());
    }
    let m = request.get("metadata").cloned().unwrap_or(json!({}));
    let o = m.as_object().ok_or("metadata must be an object")?;
    if o.len() > 16
        || o.iter()
            .any(|(k, v)| k.len() > 64 || v.as_str().is_none_or(|s| s.len() > 512))
    {
        return Err(
            "metadata requires at most 16 string values (64 byte keys, 512 byte values)".into(),
        );
    }
    Ok(m)
}

fn api_conversation_view(c: &Value) -> Value {
    json!({"id":c["id"],"object":"conversation","created_at":c["created_at"],"metadata":c["metadata"]})
}
async fn api_conversation_create(
    State(state): State<Arc<AppState>>,
    Json(request): Json<Value>,
) -> Response {
    let result = (|| {
        let metadata = api_metadata(&request)?;
        let mut items = api_input_items(request.get("items").unwrap_or(&json!([])))?;
        api_items_to_messages(&items)?;
        api_assign_ids(&mut items)?;
        let id = format!("conv_{}", Uuid::new_v4().simple());
        let conv = json!({"id":id,"created_at":api_now(),"metadata":metadata,"items":items,"profile":null});
        api_store_transaction(&state, true, |s| {
            s["conversations"][&id] = conv.clone();
            Ok(api_conversation_view(&conv))
        })
    })();
    match result {
        Ok(v) => Json(v).into_response(),
        Err(e) => api_resource_error(e),
    }
}
async fn api_conversation_get(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    match api_store_transaction(&state, false, |s| {
        s["conversations"]
            .get(&id)
            .map(api_conversation_view)
            .ok_or_else(|| format!("Not found: conversation '{id}'"))
    }) {
        Ok(v) => Json(v).into_response(),
        Err(e) => api_resource_error(e),
    }
}
async fn api_conversation_update(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(request): Json<Value>,
) -> Response {
    let result = (|| {
        let _lease = ApiConversationLease::claim(&state, &id)?;
        let metadata = api_metadata(&request)?;
        api_store_transaction(&state, true, |s| {
            let c = s["conversations"]
                .get_mut(&id)
                .ok_or_else(|| format!("Not found: conversation '{id}'"))?;
            c["metadata"] = metadata;
            Ok(api_conversation_view(c))
        })
    })();
    match result {
        Ok(v) => Json(v).into_response(),
        Err(e) => api_resource_error(e),
    }
}
async fn api_conversation_delete(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let result = (|| {
        let _lease = ApiConversationLease::claim(&state, &id)?;
        api_store_transaction(&state, true, |s| {
            s["conversations"]
                .as_object_mut()
                .unwrap()
                .remove(&id)
                .ok_or_else(|| format!("Not found: conversation '{id}'"))
        })
    })();
    match result {
        Ok(_) => {
            Json(json!({"id":id,"object":"conversation.deleted","deleted":true})).into_response()
        }
        Err(e) => api_resource_error(e),
    }
}
async fn api_conversation_items(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    match api_store_transaction(&state, false, |s| {
        let c = s["conversations"]
            .get(&id)
            .ok_or_else(|| format!("Not found: conversation '{id}'"))?;
        api_page(c["items"].as_array().ok_or("Invalid items")?.clone(), &q)
    }) {
        Ok(v) => Json(v).into_response(),
        Err(e) => api_resource_error(e),
    }
}
async fn api_conversation_append(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(request): Json<Value>,
) -> Response {
    let result = (|| {
        let _lease = ApiConversationLease::claim(&state, &id)?;
        let mut items = api_input_items(&request["items"])?;
        api_items_to_messages(&items)?;
        api_assign_ids(&mut items)?;
        api_store_transaction(&state, true, |s| {
            let c = s["conversations"]
                .get_mut(&id)
                .ok_or_else(|| format!("Not found: conversation '{id}'"))?;
            let old = c["items"].as_array_mut().ok_or("Invalid items")?;
            if items.iter().any(|i| old.iter().any(|o| o["id"] == i["id"])) {
                return Err("Duplicate conversation item ID".into());
            }
            old.extend(items.clone());
            Ok(
                json!({"object":"list","data":items,"has_more":false,"first_id":items.first().map(|v|v["id"].clone()),"last_id":items.last().map(|v|v["id"].clone())}),
            )
        })
    })();
    match result {
        Ok(v) => Json(v).into_response(),
        Err(e) => api_resource_error(e),
    }
}
async fn api_conversation_item_get(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((id, item)): axum::extract::Path<(String, String)>,
) -> Response {
    match api_store_transaction(&state, false, |s| {
        s["conversations"][&id]["items"]
            .as_array()
            .and_then(|a| a.iter().find(|i| i["id"] == item))
            .cloned()
            .ok_or_else(|| format!("Not found: item '{item}'"))
    }) {
        Ok(v) => Json(v).into_response(),
        Err(e) => api_resource_error(e),
    }
}
async fn api_conversation_item_delete(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((id, item)): axum::extract::Path<(String, String)>,
) -> Response {
    let result = (|| {
        let _lease = ApiConversationLease::claim(&state, &id)?;
        api_store_transaction(&state, true, |s| {
            let items = s["conversations"][&id]["items"]
                .as_array_mut()
                .ok_or_else(|| format!("Not found: conversation '{id}'"))?;
            let i = items
                .iter()
                .position(|i| i["id"] == item)
                .ok_or_else(|| format!("Not found: item '{item}'"))?;
            items.remove(i);
            Ok(())
        })
    })();
    match result {
        Ok(_) => Json(json!({"id":item,"object":"conversation.item.deleted","deleted":true}))
            .into_response(),
        Err(e) => api_resource_error(e),
    }
}
