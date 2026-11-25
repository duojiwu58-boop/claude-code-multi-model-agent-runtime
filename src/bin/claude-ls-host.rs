//! Authenticated user-mode owned LS host. No arbitrary RPC proxy.
#[path = "../ls_agent_state.rs"]
mod persistence;
#[path = "../ls_agent_stream.rs"]
mod projection;
#[path = "../ls_host_protocol.rs"]
mod protocol;
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use futures_util::stream;
use ls_agent_core::{
    ConversationSession, EngineConnection, InteractionSubmitRequest, TaskDisposition,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::sync::{mpsc, Mutex};

struct Session {
    engine: ConversationSession,
    history: Vec<Value>,
    binding: (String, String, String),
    busy: bool,
    uncertain: bool,
}
type Sessions = Arc<Mutex<HashMap<String, Arc<Mutex<Session>>>>>;

struct Output {
    tx: mpsc::Sender<String>,
    failed: std::sync::atomic::AtomicBool,
}
impl Output {
    async fn send(&self, value: String) -> Result<(), ()> {
        let result = tokio::time::timeout(Duration::from_secs(10), self.tx.send(value)).await;
        if matches!(result, Ok(Ok(()))) {
            Ok(())
        } else {
            self.failed
                .store(true, std::sync::atomic::Ordering::Relaxed);
            Err(())
        }
    }
    fn is_closed(&self) -> bool {
        self.tx.is_closed() || self.failed.load(std::sync::atomic::Ordering::Relaxed)
    }
}
#[derive(Clone)]
struct Host {
    launch_id: Option<String>,
    token: Arc<String>,
    connection: EngineConnection,
    sessions: Sessions,
    registry: Arc<PathBuf>,
    dormant: Arc<Mutex<HashMap<String, persistence::SessionRecord>>>,
    shutdown: tokio::sync::watch::Sender<bool>,
}
fn record(key: &str, s: &Session) -> persistence::SessionRecord {
    persistence::SessionRecord {
        session: key.into(),
        cascade: s.engine.cascade_id().into(),
        profile: s.binding.0.clone(),
        model: s.binding.1.clone(),
        workspace: s.binding.2.clone(),
        history: s.history.clone(),
        uncertain: s.uncertain || s.busy,
    }
}
fn error(status: StatusCode, message: impl ToString) -> Response {
    (
        status,
        Json(json!({"type":"error","error":{"type":"api_error","message":message.to_string()}})),
    )
        .into_response()
}
fn authenticated(host: &Host, headers: &HeaderMap) -> bool {
    if headers.contains_key("origin") {
        return false;
    }
    let expected = format!("Bearer {}", host.token);
    let actual = headers
        .get("authorization")
        .map(|h| h.as_bytes())
        .unwrap_or_default();
    actual.len() == expected.len()
        && actual
            .iter()
            .zip(expected.as_bytes())
            .fold(0u8, |a, (x, y)| a | (x ^ y))
            == 0
}
fn event(name: &str, data: Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}
fn message(id: &str, model: &str, text: &str) -> Value {
    json!({"id":id,"type":"message","role":"assistant","model":model,"content":[{"type":"text","text":text}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}})
}

async fn health(State(host): State<Host>, headers: HeaderMap) -> Response {
    if !authenticated(&host, &headers) {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    if let Err(e) = host.connection.check_owned_alive() {
        return error(StatusCode::SERVICE_UNAVAILABLE, e);
    }
    Json(json!({"status":"ok","execution":"ls-owned-tools","launch_id":host.launch_id,"instance":host.connection.instance(),"models":host.connection.models(),"usage_status":"unknown"})).into_response()
}

async fn shutdown(State(host): State<Host>, headers: HeaderMap) -> Response {
    if !authenticated(&host, &headers) {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    if let Err(e) = host.connection.stop_owned() {
        return error(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    let _ = host.shutdown.send(true);
    Json(json!({"status":"stopped"})).into_response()
}

async fn turn(
    State(host): State<Host>,
    headers: HeaderMap,
    Json(req): Json<protocol::TurnRequest>,
) -> Response {
    if !authenticated(&host, &headers) {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    let payload = match protocol::user_payload(&req.user_content) {
        Ok(payload) => payload,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    if protocol::normalize_history(&req.history).as_ref() != Ok(&req.history)
        || req.text_items.concat() != req.text
        || payload.text_items != req.text_items
        || payload.media != req.media
        || payload.content != req.user_content
    {
        return error(StatusCode::BAD_REQUEST, "Invalid normalized text history");
    }
    if req.session.is_empty()
        || req.session.len() > 200
        || (req.text.trim().is_empty() && req.media.is_empty())
        || !(5..=3600).contains(&req.timeout_seconds)
        || req.permission_mode != "turbo"
        || !PathBuf::from(&req.workspace).is_absolute()
    {
        return error(StatusCode::BAD_REQUEST, "Invalid turn controls");
    }
    if let Err(e) = host.connection.check_owned_alive() {
        return error(StatusCode::SERVICE_UNAVAILABLE, e);
    }
    let workspace = match std::fs::canonicalize(&req.workspace) {
        Ok(w) if w.is_dir() => w,
        _ => return error(StatusCode::BAD_REQUEST, "Workspace must exist"),
    };
    let binding = (
        req.profile.clone(),
        req.model.clone(),
        workspace.to_string_lossy().into_owned(),
    );
    let key = req.session.clone();
    if host.dormant.lock().await.contains_key(&key) {
        return error(
            StatusCode::CONFLICT,
            "Registered session requires explicit /resume; never replay a task after Host restart",
        );
    }
    // Serialize creation only. Running sessions have independent locks and streams.
    let mut sessions = host.sessions.lock().await;
    let session = if let Some(s) = sessions.get(&key) {
        s.clone()
    } else {
        if !req.history.is_empty() {
            return error(
                StatusCode::CONFLICT,
                "Unknown session; history import/replay is forbidden",
            );
        }
        if sessions.len() >= 256 {
            return error(
                StatusCode::TOO_MANY_REQUESTS,
                "Host session capacity reached",
            );
        }
        let engine = match host
            .connection
            .create_session(workspace, Some(&req.model))
            .await
        {
            Ok(e) => e,
            Err(e) => return error(StatusCode::BAD_GATEWAY, e),
        };
        let s = Arc::new(Mutex::new(Session {
            engine,
            history: vec![],
            binding: binding.clone(),
            busy: false,
            uncertain: false,
        }));
        sessions.insert(key, s.clone());
        s
    };
    drop(sessions);
    let mut s = session.lock().await;
    if s.busy || s.uncertain || s.binding != binding || s.history != req.history {
        return error(StatusCode::CONFLICT,"Busy, uncertain, changed model/workspace/profile or mismatched history; use an explicit new session");
    }
    let floor = s.engine.state().steps.total_length;
    s.busy = true;
    if let Err(e) = persistence::save(&host.registry, &record(&req.session, &s)) {
        s.busy = false;
        return error(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    drop(s);
    let (tx, mut rx) = mpsc::channel::<String>(64);
    let streaming = req.stream;
    tokio::spawn(run_turn(
        host,
        session,
        req,
        floor,
        Output {
            tx,
            failed: std::sync::atomic::AtomicBool::new(false),
        },
    ));
    if !streaming {
        let text=rx.recv().await.unwrap_or_else(|| json!({"type":"error","error":{"type":"api_error","message":"Host task ended without response"}}).to_string());
        let mut value: Value = serde_json::from_str(&text).unwrap();
        let usage_known = value
            .as_object_mut()
            .and_then(|v| v.remove("ls_usage_status"))
            .is_some_and(|v| v == "native");
        let status = if value["type"] == "error" {
            StatusCode::BAD_GATEWAY
        } else {
            StatusCode::OK
        };
        let mut response = (status, Json(value)).into_response();
        response.headers_mut().insert(
            "x-ls-usage-status",
            if usage_known {
                "native"
            } else {
                "unknown; integer-zero-placeholders"
            }
            .parse()
            .unwrap(),
        );
        return response;
    }
    let body = Body::from_stream(stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|s| (Ok::<_, std::io::Error>(s), rx))
    }));
    let mut response = body.into_response();
    response.headers_mut().insert(
        "content-type",
        if streaming {
            "text/event-stream"
        } else {
            "application/json"
        }
        .parse()
        .unwrap(),
    );
    response.headers_mut().insert(
        "x-ls-usage-status",
        "pending; see message_delta.ls_usage_status"
            .parse()
            .unwrap(),
    );
    response
}

async fn run_turn(
    host: Host,
    session: Arc<Mutex<Session>>,
    req: protocol::TurnRequest,
    floor: u64,
    tx: Output,
) {
    {
        let mut s = session.lock().await;
        let config = ls_agent_core::permissions::mode_tool_config("turbo", true);
        if let Err(e) = s
            .engine
            .send_user_items(req.text_items.clone(), req.media.clone(), host.connection.models(), config)
            .await
        {
            s.busy = false;
            s.uncertain = true;
            let payload =
                json!({"type":"error","error":{"type":"api_error","message":e.to_string()}});
            let _ = tx
                .send(if req.stream {
                    event("error", payload)
                } else {
                    payload.to_string()
                })
                .await;
            return;
        }
    }
    let id = format!("msg_{}", uuid::Uuid::new_v4().simple());
    let mut timeline = projection::Timeline::default();
    let start = std::time::Instant::now();
    let mut heartbeat = start;
    let mut failure = None;
    if req.stream {
        let mut m = message(&id, &req.model, "");
        m["content"] = json!([]);
        m["stop_reason"] = Value::Null;
        let _ = tx
            .send(event(
                "message_start",
                json!({"type":"message_start","message":m}),
            ))
            .await;
    }
    loop {
        if tx.is_closed() {
            failure = Some("HTTP client disconnected".to_string());
            break;
        }
        if start.elapsed() > Duration::from_secs(req.timeout_seconds) {
            failure = Some("Task deadline exceeded".to_string());
            break;
        }
        if let Err(e) = host.connection.check_owned_alive() {
            failure = Some(e.to_string());
            break;
        }
        let mut s = session.lock().await;
        if let Err(e) = s
            .engine
            .consume_next_frame(Duration::from_millis(200))
            .await
        {
            failure = Some(e.to_string());
            break;
        }
        let snapshot = s.engine.snapshot();
        let updates = timeline.observe(&s.engine.state().steps.values, floor);
        drop(s);
        let events = match updates {
            Ok(events) => events,
            Err(e) => {
                failure = Some(e);
                break;
            }
        };
        if req.stream {
            for payload in events {
                let name = payload["type"].as_str().unwrap().to_owned();
                let _ = tx.send(event(&name, payload)).await;
            }
        }
        if failure.is_some() {
            break;
        }
        if matches!(
            snapshot.disposition,
            TaskDisposition::Failed | TaskDisposition::Canceled | TaskDisposition::Unconfirmed
        ) {
            failure = Some(format!("LS task ended: {:?}", snapshot.disposition));
            break;
        }
        if snapshot.fully_idle && snapshot.disposition == TaskDisposition::Completed {
            break;
        }
        if req.stream && heartbeat.elapsed() >= Duration::from_secs(10) {
            let _ = tx.send(event("ping", json!({"type":"ping"}))).await;
            heartbeat = std::time::Instant::now();
        }
        tokio::task::yield_now().await;
    }
    let mut s = session.lock().await;
    if let Some(reason) = failure {
        // RPC acceptance is not stop confirmation. Keep uncertainty until live idle evidence.
        s.uncertain = true;
        if s.engine.snapshot().disposition != TaskDisposition::Canceled {
            let _ = stop_and_verify(&host, &mut s).await;
        }
        s.busy = false;
        let payload = json!({"type":"error","error":{"type":"api_error","message":reason}});
        let _ = tx
            .send(if req.stream {
                event("error", payload)
            } else {
                payload.to_string()
            })
            .await;
        return;
    }
    let usage = host
        .connection
        .client()
        .get_cascade_trajectory(s.engine.cascade_id())
        .await
        .ok()
        .and_then(|v| projection::native_usage(&v, floor));
    let usage_status = if usage.is_some() { "native" } else { "unknown" };
    let usage = usage.unwrap_or_else(|| json!({"input_tokens":0,"output_tokens":0}));
    let text = timeline.history_text();
    let original = protocol::user_payload(&req.user_content).expect("validated user content");
    s.history.push(json!({"role":"user","content":protocol::history_content(&original)}));
    s.history.push(json!({"role":"assistant","content":text}));
    s.busy = false;
    if let Err(e) = persistence::save(&host.registry, &record(&req.session, &s)) {
        s.uncertain = true;
        let payload = json!({"type":"error","error":{"type":"api_error","message":format!("Task completed but session persistence failed: {e}")}});
        let _ = tx
            .send(if req.stream {
                event("error", payload)
            } else {
                payload.to_string()
            })
            .await;
        return;
    }
    drop(s);
    if req.stream {
        for payload in timeline.close() {
            let _ = tx.send(event("content_block_stop", payload)).await;
        }
        let _=tx.send(event("message_delta",json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":usage,"ls_usage_status":usage_status}))).await;
        let _ = tx
            .send(event("message_stop", json!({"type":"message_stop"})))
            .await;
    } else {
        let mut m = message(&id, &req.model, &text);
        m["content"] = json!(timeline.content);
        m["usage"] = usage;
        m["ls_usage_status"] = json!(usage_status);
        let _ = tx.send(m.to_string()).await;
    }
}

async fn interactions(State(host): State<Host>, headers: HeaderMap) -> Response {
    if !authenticated(&host, &headers) {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    let sessions: Vec<_> = host
        .sessions
        .lock()
        .await
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut rows = vec![];
    for (key, s) in sessions {
        let s = s.lock().await;
        let tools:Vec<Value>=s.engine.state().steps.values.iter().rev().take(20).map(|(i,v)|json!({"index":i,"type":v["type"],"status":v["status"],"tool":v.pointer("/generic/toolName").or_else(||v.pointer("/generic/name")),"command":v.pointer("/runCommand/commandLine")})).collect();
        rows.push(json!({"session":key,"busy":s.busy,"uncertain":s.uncertain,"snapshot":s.engine.snapshot(),"tools":tools}));
    }
    let dormant: Vec<String> = host.dormant.lock().await.keys().cloned().collect();
    Json(json!({"sessions":rows,"dormant_sessions":dormant})).into_response()
}
async fn respond(
    State(host): State<Host>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !authenticated(&host, &headers) {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    let session = host
        .sessions
        .lock()
        .await
        .get(body["session"].as_str().unwrap_or(""))
        .cloned();
    let Some(session) = session else {
        return error(StatusCode::NOT_FOUND, "Unknown owned session");
    };
    let request: InteractionSubmitRequest =
        match serde_json::from_value(body["interaction"].clone()) {
            Ok(r) => r,
            Err(_) => return error(StatusCode::BAD_REQUEST, "Invalid interaction response"),
        };
    let mut guard = session.lock().await;
    if guard.uncertain {
        return error(
            StatusCode::CONFLICT,
            "Uncertain session cannot accept approvals; inspect or cancel it",
        );
    }
    match guard.engine.respond_interaction(&request).await {
        Ok(r) => Json(r).into_response(),
        Err(e) => error(StatusCode::CONFLICT, e),
    }
}
async fn cancel(State(host): State<Host>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if !authenticated(&host, &headers) {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    let session = host
        .sessions
        .lock()
        .await
        .get(body["session"].as_str().unwrap_or(""))
        .cloned();
    let Some(session) = session else {
        return error(StatusCode::NOT_FOUND, "Unknown owned session");
    };
    let mut s = session.lock().await;
    match stop_and_verify(&host, &mut s).await {
        Ok(confirmed) => (
            if confirmed {
                StatusCode::OK
            } else {
                StatusCode::ACCEPTED
            },
            Json(json!({"status":if confirmed {"stopped"} else {"unknown"},"confirmed":confirmed})),
        )
            .into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, e),
    }
}

async fn stop_and_verify(host: &Host, s: &mut Session) -> Result<bool, String> {
    let mut ids = s.engine.snapshot().subagent_ids;
    let root = s.engine.cascade_id().to_owned();
    ids.push(root.clone());
    ids.sort();
    ids.dedup();
    let since = std::time::Instant::now();
    let accepted = s
        .engine
        .stop(ls_wire::StopScope::Tree)
        .await
        .map_err(|e| e.to_string())?;
    if !accepted
        .stopped_conversation_ids
        .iter()
        .all(|id| ids.contains(id))
    {
        return Ok(false);
    }
    // Fresh subscriptions discard buffered pre-cancel idle frames.
    s.engine
        .resubscribe(
            host.connection.next_epoch(),
            ls_wire::PageBounds::default_initial(),
        )
        .await
        .map_err(|e| e.to_string())?;
    let mut children = vec![];
    for id in ids.iter().filter(|id| *id != &root) {
        children.push(
            host.connection
                .attach_owned_session(
                    id.clone(),
                    PathBuf::from(&s.binding.2),
                    Some(&s.binding.1),
                    ls_wire::PageBounds::default_initial(),
                )
                .await
                .map_err(|e| e.to_string())?,
        );
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        s.engine
            .consume_next_frame_passive(Duration::from_millis(200))
            .await
            .map_err(|e| e.to_string())?;
        for child in &mut children {
            child
                .consume_next_frame_passive(Duration::from_millis(100))
                .await
                .map_err(|e| e.to_string())?;
        }
        if s.engine.idle_since(since) && children.iter().all(|child| child.idle_since(since)) {
            s.engine.verify_tree_stop(true);
            return Ok(true);
        }
    }
    Ok(false)
}

async fn resume(State(host): State<Host>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if !authenticated(&host, &headers) {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    let key = body["session"].as_str().unwrap_or("");
    let mut dormant = host.dormant.lock().await;
    let Some(record) = dormant.get(key).cloned() else {
        return error(StatusCode::NOT_FOUND, "No dormant registered session");
    };
    if host
        .connection
        .models()
        .resolve(Some(&record.model))
        .is_none()
    {
        return error(
            StatusCode::CONFLICT,
            "Registered model is unavailable; no silent fallback on resume",
        );
    }
    // Reattach/observe only. An uncertain send stays blocked even after idle: no implicit retry.
    let mut engine = match host
        .connection
        .attach_owned_session(
            record.cascade,
            PathBuf::from(&record.workspace),
            Some(&record.model),
            ls_wire::PageBounds::default_initial(),
        )
        .await
    {
        Ok(s) => s,
        Err(e) => return error(StatusCode::BAD_GATEWAY, e),
    };
    if let Err(e) = engine
        .consume_next_frame_passive(Duration::from_secs(5))
        .await
    {
        return error(StatusCode::BAD_GATEWAY, e);
    }
    let snapshot = engine.snapshot();
    host.sessions.lock().await.insert(
        key.into(),
        Arc::new(Mutex::new(Session {
            engine,
            history: record.history,
            binding: (record.profile, record.model, record.workspace),
            busy: false,
            uncertain: record.uncertain,
        })),
    );
    dormant.remove(key);
    Json(json!({"snapshot":snapshot,"uncertain":record.uncertain,"replayed":false})).into_response()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--exit-on-stdin-close") {
        // Start before engine initialization: a failed/killed bridge cannot leave
        // a Host behind even during startup. Process exit closes the LS Job.
        std::thread::spawn(|| {
            use std::io::Read;
            let mut byte = [0u8; 1];
            while matches!(std::io::stdin().read(&mut byte), Ok(n) if n > 0) {}
            std::process::exit(0);
        });
    }
    let value = |key: &str| {
        args.windows(2)
            .find(|a| a[0] == key)
            .map(|a| a[1].clone())
            .ok_or_else(|| format!("Missing {key}"))
    };
    let data = PathBuf::from(value("--data-root")?);
    let token_file = PathBuf::from(value("--token-file")?);
    if !data.is_absolute() || !token_file.is_absolute() {
        return Err("Absolute data-root and token-file required".into());
    }
    let token = std::fs::read_to_string(token_file)?.trim().to_owned();
    if token.len() < 32 {
        return Err("Host token must contain at least 32 characters".into());
    }
    let port: u16 = value("--port")?.parse()?;
    eprintln!("LS_HOST_CONTEXT user_profile={:?} proxy_endpoint={:?}",
        std::env::var_os("USERPROFILE"),
        std::env::var("HTTPS_PROXY").ok().and_then(|v| url::Url::parse(&v).ok()).map(|u| format!("{}:{}", u.host_str().unwrap_or(""), u.port_or_known_default().unwrap_or(0))));
    let mut config = ls_agent_core::ls_host::LsHostConfig::for_data_root(data)?;
    if let Ok(install) = value("--install-root") {
        config.install_root = PathBuf::from(install);
    }
    let registry = config.data_root.join("bridge-sessions");
    let connection = EngineConnection::start_owned(config, false).await?;
    let records = persistence::load(&registry)?;
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let host = Host {
        launch_id: value("--launch-id").ok(),
        token: Arc::new(token),
        connection,
        sessions: Arc::new(Mutex::new(HashMap::new())),
        registry: Arc::new(registry),
        dormant: Arc::new(Mutex::new(
            records
                .into_iter()
                .map(|r| (r.session.clone(), r))
                .collect(),
        )),
        shutdown: shutdown_tx,
    };
    let app = Router::new()
        .route("/health", post(health))
        .route("/models", post(health))
        .route("/turn", post(turn))
        .route("/interactions", post(interactions))
        .route("/respond", post(respond))
        .route("/cancel", post(cancel))
        .route("/resume", post(resume))
        .route("/shutdown", post(shutdown))
        // The internal request carries canonical originals as well as native
        // media. The public bridge caps HTTP input at 64 MiB and validates each
        // turn's decoded content at 32 MiB before this local authenticated hop.
        .layer(DefaultBodyLimit::max(128 * 1024 * 1024))
        .with_state(host.clone());
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    println!(
        "LS_HOST_READY port={} pid={}",
        listener.local_addr()?.port(),
        host.connection.instance().pid
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! { _=tokio::signal::ctrl_c()=>{}, _=shutdown_rx.changed()=>{} }
        })
        .await?;
    host.connection.stop_owned()?;
    Ok(())
}
