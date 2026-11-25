//! Reusable `EngineConnection` and `ConversationSession` orchestrating
//! attached-mode discovery, pinned-TLS Connect RPCs, serialized per-session
//! operations, `PROD_UI` streaming, paging, live interaction validation, and
//! owned-session cancellation.

use crate::{
    SessionExecutionStatus, TaskDisposition,
    interaction::{
        InteractionRegistry, InteractionSubmitRequest, InteractionValidationError,
        PendingInteraction,
    },
    state_store::{AgentStateStore, ResolvedPageBounds},
};
use ls_wire::{
    AgentStateStream, DiscoverySelectError, InstanceSummary, LsClient, ModelCatalog, PageBounds,
    PageUpdateRequest, ReadonlyCheckReport, SendMessageOptions, StopScope, StreamSubscribeRequest,
    TransportErrorCategory, WireError, discover_instances, load_pinned_cert_pem, select_endpoint,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fmt,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

/// Structured engine-layer error preserving the mandatory three-way transport classification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineError {
    pub category: TransportErrorCategory,
    pub reason_code: String,
    pub message_zh: String,
    pub detail: Option<String>,
    pub candidates: Vec<InstanceSummary>,
}

impl EngineError {
    pub fn not_sent(reason_code: impl Into<String>, message_zh: impl Into<String>) -> Self {
        Self {
            category: TransportErrorCategory::NotSent,
            reason_code: reason_code.into(),
            message_zh: message_zh.into(),
            detail: None,
            candidates: Vec::new(),
        }
    }

    pub fn from_wire(err: WireError, message_zh: impl Into<String>) -> Self {
        Self {
            category: err.category,
            reason_code: err.code,
            message_zh: message_zh.into(),
            detail: Some(err.message),
            candidates: Vec::new(),
        }
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.detail {
            Some(detail) => write!(
                f,
                "[{:?}:{}] {} ({detail})",
                self.category, self.reason_code, self.message_zh
            ),
            None => write!(
                f,
                "[{:?}:{}] {}",
                self.category, self.reason_code, self.message_zh
            ),
        }
    }
}

impl std::error::Error for EngineError {}

impl From<InteractionValidationError> for EngineError {
    fn from(err: InteractionValidationError) -> Self {
        Self {
            category: err.category,
            reason_code: err.reason_code,
            message_zh: err.message_zh,
            detail: None,
            candidates: Vec::new(),
        }
    }
}

/// Verified attached connection to a single official `language_server` instance.
#[derive(Clone)]
pub struct EngineConnection {
    client: Arc<LsClient>,
    readonly_report: ReadonlyCheckReport,
    models: ModelCatalog,
    epoch_counter: Arc<AtomicU64>,
    host: Option<Arc<Mutex<crate::ls_host::LsHost>>>,
}

impl EngineConnection {
    /// Discovers, verifies, pins TLS from `app.asar`, and executes read-only
    /// health/auth/model checks before entering `Ready`.
    pub async fn connect(
        requested_pid: Option<u32>,
        verify_security_boundaries: bool,
    ) -> Result<Self, EngineError> {
        let report = discover_instances()
            .map_err(|err| EngineError::from_wire(err, "无法扫描官方 language_server 实例"))?;
        let endpoint = match select_endpoint(&report, requested_pid) {
            Ok(ep) => ep,
            Err(DiscoverySelectError::NoLiveInstance) => {
                return Err(EngineError::not_sent(
                    "NO_LIVE_INSTANCE",
                    "附着诊断未检测到有效 LS 实例；正常使用请选择本客户端的独立启动模式。",
                ));
            }
            Err(DiscoverySelectError::UnsupportedVersion {
                found_versions,
                target,
            }) => {
                return Err(EngineError::not_sent(
                    "UNSUPPORTED_LS_VERSION",
                    format!(
                        "检测到未验证的官方服务版本 {:?}，当前首版仅支持 {}。",
                        found_versions, target
                    ),
                ));
            }
            Err(DiscoverySelectError::RequestedPidNotFound { pid, available }) => {
                let mut err = EngineError::not_sent(
                    "REQUESTED_PID_NOT_FOUND",
                    format!("指定的官方服务 PID {pid} 未运行或未通过核验。"),
                );
                err.candidates = available;
                return Err(err);
            }
            Err(DiscoverySelectError::MultipleInstances { candidates }) => {
                let mut err = EngineError::not_sent(
                    "MULTIPLE_INSTANCES",
                    format!(
                        "发现 {} 个有效的官方服务实例，请明确指定目标 PID（客户端不猜测连接目标）。",
                        candidates.len()
                    ),
                );
                err.candidates = candidates;
                return Err(err);
            }
        };

        let cert_pem = load_pinned_cert_pem(&report.install_dir)
            .map_err(|err| EngineError::from_wire(err, "无法从官方 app.asar 读取固定 TLS 证书"))?;
        Self::from_verified_endpoint(endpoint, &cert_pem, verify_security_boundaries).await
    }

    async fn from_verified_endpoint(
        endpoint: ls_wire::DiscoveredEndpoint,
        cert_pem: &[u8],
        verify_security_boundaries: bool,
    ) -> Result<Self, EngineError> {
        let client = LsClient::from_endpoint_and_cert_pem(endpoint, cert_pem)
            .map_err(|err| EngineError::from_wire(err, "构建固定证书 TLS 客户端失败"))?;

        let (readonly_report, models) = client
            .check_readonly(verify_security_boundaries)
            .await
            .map_err(|err| {
                EngineError::from_wire(err, "只读健康与鉴权检查失败，禁止进入已连接状态")
            })?;

        Ok(Self {
            client: Arc::new(client),
            readonly_report,
            models,
            epoch_counter: Arc::new(AtomicU64::new(1)),
            host: None,
        })
    }

    /// Starts and owns a private engine; never searches official GUI discovery files.
    pub async fn start_owned(
        config: crate::ls_host::LsHostConfig,
        verify_security: bool,
    ) -> Result<Self, EngineError> {
        let host = crate::ls_host::LsHost::start(&config)?;
        let mut conn =
            Self::from_verified_endpoint(host.endpoint(), host.cert(), verify_security).await?;
        if conn
            .readonly_report
            .auth
            .has_auth_token_bools
            .get("hasToken")
            != Some(&true)
        {
            return Err(EngineError::not_sent(
                "LOGIN_REQUIRED",
                "引擎登录无效，请由既有登录维护流程处理。",
            ));
        }
        conn.host = Some(Arc::new(Mutex::new(host)));
        Ok(conn)
    }

    pub async fn recheck(&self, verify_security: bool) -> Result<Self, EngineError> {
        self.verify_instance_unchanged()?;
        let (report, models) = self.client.check_readonly(verify_security).await
            .map_err(|err| EngineError::from_wire(err, "重新核验连接失败"))?;
        if self.is_owned() && report.auth.has_auth_token_bools.get("hasToken") != Some(&true) {
            return Err(EngineError::not_sent(
                "LOGIN_REQUIRED",
                "引擎登录无效，请由既有登录维护流程处理。",
            ));
        }
        let mut conn = self.clone();
        conn.readonly_report = report;
        conn.models = models;
        Ok(conn)
    }

    pub fn replace_models(&mut self, models: ModelCatalog) { self.models = models; }

    pub fn is_owned(&self) -> bool {
        self.host.is_some()
    }

    pub fn check_owned_alive(&self) -> Result<(), EngineError> {
        if let Some(host) = &self.host {
            host.lock()
                .map_err(|_| EngineError::not_sent("HOST_LOCK_FAILED", "引擎锁异常"))?
                .check_alive()?;
        }
        Ok(())
    }

    pub fn stop_owned(&self) -> Result<(), EngineError> {
        if let Some(host) = &self.host {
            host.lock()
                .map_err(|_| EngineError::not_sent("HOST_LOCK_FAILED", "引擎锁异常"))?
                .stop()?;
        }
        Ok(())
    }

    /// Constructs a connection from an existing `LsClient` (used in local mock tests).
    pub fn from_parts(
        client: Arc<LsClient>,
        readonly_report: ReadonlyCheckReport,
        models: ModelCatalog,
    ) -> Self {
        Self {
            client,
            readonly_report,
            models,
            epoch_counter: Arc::new(AtomicU64::new(1)),
            host: None,
        }
    }

    pub fn client(&self) -> Arc<LsClient> {
        Arc::clone(&self.client)
    }

    pub fn instance(&self) -> &InstanceSummary {
        self.client.instance()
    }

    pub fn models(&self) -> &ModelCatalog {
        &self.models
    }

    pub fn readonly_report(&self) -> &ReadonlyCheckReport {
        &self.readonly_report
    }

    pub fn current_epoch(&self) -> u64 {
        self.epoch_counter.load(Ordering::SeqCst)
    }

    pub fn next_epoch(&self) -> u64 {
        self.epoch_counter.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Re-verifies that the attached official instance still matches the original
    /// PID, HTTPS port, and version before resuming an owned session.
    pub fn verify_instance_unchanged(&self) -> Result<(), EngineError> {
        if let Some(host) = &self.host {
            return host
                .lock()
                .map_err(|_| EngineError::not_sent("HOST_LOCK_FAILED", "引擎锁异常"))?
                .verify();
        }
        let report = discover_instances()
            .map_err(|err| EngineError::from_wire(err, "重连前重新扫描官方实例失败"))?;
        let expected = self.instance();
        let matched = report.valid.iter().any(|ep| {
            ep.pid == expected.pid
                && ep.https_port == expected.https_port
                && ep.ls_version == expected.ls_version
        });
        if !matched {
            return Err(EngineError::not_sent(
                "INSTANCE_IDENTITY_CHANGED",
                format!(
                    "官方服务实例身份已变化（原 PID={} 端口={}），已停止写操作，需重新核验连接。",
                    expected.pid, expected.https_port
                ),
            ));
        }
        Ok(())
    }

    /// Creates a new client-owned session in deterministic order:
    /// 1. Validate workspace & resolve model from live `ModelCatalog`;
    /// 2. Call `StartCascade`;
    /// 3. Open initial `PROD_UI` stream with a fresh `connectionEpoch` and UUID `subscriberId`;
    /// 4. Consume the first baseline frame (within bounded timeout) so state is initialized.
    pub async fn create_session(
        &self,
        workspace_path: PathBuf,
        requested_model: Option<&str>,
    ) -> Result<ConversationSession, EngineError> {
        let canonical_ws = workspace_path.canonicalize().map_err(|err| {
            EngineError::not_sent(
                "INVALID_WORKSPACE_PATH",
                format!(
                    "工作目录不存在或无法访问（{}）：{err}",
                    workspace_path.display()
                ),
            )
        })?;
        let (model_id, model_enum) = self.models.resolve(requested_model).ok_or_else(|| {
            EngineError::not_sent(
                "UNKNOWN_MODEL",
                format!(
                    "所选模型 '{}' 不在引擎返回的实时模型目录中，未创建会话。",
                    requested_model.unwrap_or("<default>")
                ),
            )
        })?;

        let cascade_id = Uuid::new_v4().to_string();
        self.client
            .start_cascade(&cascade_id, &canonical_ws)
            .await
            .map_err(|err| EngineError::from_wire(err, "创建会话 StartCascade 失败"))?;

        self.client
            .require_conversation_review(&cascade_id)
            .await
            .map_err(|e| EngineError::from_wire(e, "无法为新会话设置逐次审批；未发送任务"))?;
        let mut session = ConversationSession::new_owned(
            Arc::clone(&self.client),
            cascade_id,
            canonical_ws,
            model_id,
            model_enum,
        );
        let epoch = self.next_epoch();
        session
            .resubscribe(epoch, PageBounds::default_initial())
            .await?;
        // Establish first usable PROD_UI baseline frame before allowing sends.
        if !session.consume_next_frame(Duration::from_secs(5)).await? {
            return Err(EngineError::not_sent(
                "NO_STATE_BASELINE",
                "会话已创建但状态基线未就绪；不能发送消息。",
            ));
        }
        Ok(session)
    }

    /// Re-attaches to an already registered client-owned session (for cross-process
    /// observation, paging, interaction recovery, or stopping).
    pub async fn attach_owned_session(
        &self,
        cascade_id: String,
        workspace_path: PathBuf,
        requested_model: Option<&str>,
        initial_bounds: PageBounds,
    ) -> Result<ConversationSession, EngineError> {
        if Uuid::parse_str(&cascade_id).is_err() {
            return Err(EngineError::not_sent(
                "INVALID_CASCADE_ID",
                format!("无效的会话 UUID：{cascade_id}"),
            ));
        }
        let (model_id, model_enum) = self
            .models
            .resolve(requested_model)
            .or_else(|| self.models.resolve(None))
            .ok_or_else(|| {
                EngineError::not_sent("UNKNOWN_MODEL", "无法从实时模型目录解析默认模型")
            })?;

        let mut session = ConversationSession::new_owned(
            Arc::clone(&self.client),
            cascade_id,
            workspace_path,
            model_id,
            model_enum,
        );
        let epoch = self.next_epoch();
        session.resubscribe(epoch, initial_bounds).await?;
        Ok(session)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalSessionPhase {
    Idle,
    Sending,
    Running,
    Stopping,
    Unknown,
}

/// Correlation details returned when `SendUserCascadeMessage` succeeds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendOutcome {
    pub client_request_id: String,
    pub client_message_id: Option<String>,
    pub response: Value,
}

/// Acceptance details returned when a stop RPC is accepted by the server.
///
/// RPC acceptance does NOT mean execution or background side effects have stopped.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StopAccepted {
    pub scope: StopScope,
    pub stopped_conversation_ids: Vec<String>,
    pub raw_response: Value,
}

/// Verified transition emitted after a stop request is confirmed idle or times out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopTransition {
    pub scope: StopScope,
    pub phase: &'static str,
    pub stopped_conversation_ids: Vec<String>,
}

#[derive(Debug, Clone)]
struct ActiveStop {
    scope: StopScope,
    started_at: Instant,
    stopped_conversation_ids: Vec<String>,
    tree_verified: bool,
}

const DEFAULT_STOP_VERIFY_TIMEOUT: Duration = Duration::from_secs(20);

/// Stop condition for `ConversationSession::observe_until`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObserveStopCondition {
    UntilIdle,
    UntilPendingOrIdle,
    UntilBackgroundOrPendingOrIdle,
    FixedDuration,
}

/// Normalized snapshot of a session's orthogonal states and projected steps.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSnapshot {
    pub conversation_id: String,
    pub trajectory_id: Option<String>,
    pub connection_epoch: u64,
    pub subscriber_id: Option<String>,
    pub selected_model_id: String,
    pub selected_model_enum: String,
    pub execution_status: SessionExecutionStatus,
    pub disposition: TaskDisposition,
    pub has_assistant_output: bool,
    pub fully_idle_raw: Option<bool>,
    pub fully_idle: bool,
    pub frames: u64,
    pub steps_total: u64,
    pub page_bounds: Option<ResolvedPageBounds>,
    pub projected_step_indices: Vec<u64>,
    pub pending_interaction: Option<PendingInteraction>,
    pub active_background_count: usize,
    pub observed_background_ids: Vec<String>,
    pub subagent_ids: Vec<String>,
    pub assistant_texts: Vec<String>,
}

/// Managed client-owned conversation session with per-session operation serialization.
pub struct ConversationSession {
    client: Arc<LsClient>,
    cascade_id: String,
    workspace_path: PathBuf,
    selected_model_id: String,
    selected_model_enum: String,
    owned_by_client: bool,
    state: AgentStateStore,
    interactions: InteractionRegistry,
    auto_approval: crate::auto_approval::AutoApproval,
    local_phase: LocalSessionPhase,
    last_disposition: TaskDisposition,
    awaiting_turn_after: Option<u64>,
    turn_step_floor: u64,
    active_stop: Option<ActiveStop>,
    pending_stop_transition: Option<StopTransition>,
    stop_verify_timeout: Duration,
    stream: Option<AgentStateStream>,
    browsing_history: bool,
    last_live_update: Option<Instant>,
    submitted_interactions: std::collections::HashSet<(String, u64)>,
    serial_op: Arc<AsyncMutex<()>>,
}

impl ConversationSession {
    pub fn new_owned(
        client: Arc<LsClient>,
        cascade_id: String,
        workspace_path: PathBuf,
        selected_model_id: String,
        selected_model_enum: String,
    ) -> Self {
        Self {
            client,
            cascade_id,
            workspace_path,
            selected_model_id,
            selected_model_enum,
            owned_by_client: true,
            state: AgentStateStore::default(),
            interactions: InteractionRegistry::default(),
            auto_approval: crate::auto_approval::AutoApproval::default(),
            local_phase: LocalSessionPhase::Idle,
            last_disposition: TaskDisposition::None,
            awaiting_turn_after: None,
            turn_step_floor: 0,
            active_stop: None,
            pending_stop_transition: None,
            stop_verify_timeout: DEFAULT_STOP_VERIFY_TIMEOUT,
            stream: None,
            browsing_history: false,
            last_live_update: None,
            submitted_interactions: std::collections::HashSet::new(),
            serial_op: Arc::new(AsyncMutex::new(())),
        }
    }

    pub fn cascade_id(&self) -> &str {
        &self.cascade_id
    }

    pub fn workspace_path(&self) -> &PathBuf {
        &self.workspace_path
    }

    pub fn state(&self) -> &AgentStateStore {
        &self.state
    }

    pub fn subscriber_id(&self) -> Option<&str> {
        self.state.subscriber_id.as_deref()
    }

    pub fn connection_epoch(&self) -> u64 {
        self.state.connection_epoch
    }

    pub fn has_active_stream(&self) -> bool {
        self.stream.is_some()
    }

    pub fn take_stop_transition(&mut self) -> Option<StopTransition> {
        self.pending_stop_transition.take()
    }

    pub fn tree_stop_evidence(&self) -> Option<(Instant, Vec<String>)> {
        self.active_stop
            .as_ref()
            .filter(|s| s.scope == StopScope::Tree)
            .map(|s| (s.started_at, s.stopped_conversation_ids.clone()))
    }

    pub fn idle_since(&self, since: Instant) -> bool {
        self.has_active_stream()
            && self.last_live_update.is_some_and(|t| t >= since)
            && self.state.fully_idle()
            && !self.state.has_active_background_tasks()
            && InteractionRegistry::extract_live_pending(&self.state).is_none()
    }

    pub fn verify_tree_stop(&mut self, verified: bool) {
        if let Some(stop) = &mut self.active_stop {
            stop.tree_verified = verified;
            self.reconcile_phase_from_live_state();
        }
    }

    pub fn is_browsing_history(&self) -> bool {
        self.browsing_history
    }

    /// Checks whether an accepted stop request exceeded its verification timeout
    /// without observing an idle state with zero active background tasks.
    pub fn check_stop_timeout(&mut self) -> Option<StopTransition> {
        if self.local_phase != LocalSessionPhase::Stopping {
            return None;
        }
        let timed_out = self
            .active_stop
            .as_ref()
            .is_some_and(|stop| stop.started_at.elapsed() >= self.stop_verify_timeout);
        if !timed_out {
            return None;
        }
        // Still Stopping means no fresh frame has verified completion. A cached
        // idle snapshot from before the cancellation is not stop evidence.
        {
            let stop = self.active_stop.take()?;
            self.local_phase = LocalSessionPhase::Unknown;
            self.last_disposition = TaskDisposition::Unconfirmed;
            Some(StopTransition {
                scope: stop.scope,
                phase: "Unknown",
                stopped_conversation_ids: stop.stopped_conversation_ids,
            })
        }
    }

    pub fn disconnect_subscription(&mut self) {
        self.awaiting_turn_after = None;
        self.active_stop = None;
        self.pending_stop_transition = None;
        self.browsing_history = false;
        self.auto_approval.disable();
        self.stream = None;
        self.local_phase = LocalSessionPhase::Unknown;
        self.last_disposition = TaskDisposition::Unconfirmed;
    }

    /// Marks the session when the owned LS process unexpectedly exits.
    /// Returns `true` if session state changed and callers should re-emit status.
    pub fn mark_engine_lost(&mut self) -> bool {
        self.auto_approval.disable();
        self.stream = None;
        self.active_stop = None;
        if self.local_phase != LocalSessionPhase::Unknown || self.browsing_history {
            self.browsing_history = false;
            self.local_phase = LocalSessionPhase::Unknown;
            self.last_disposition = TaskDisposition::Unconfirmed;
            return true;
        }
        false
    }

    /// Re-establishes `StreamAgentStateUpdates` with a new `connection_epoch`,
    /// fresh UUID `subscriberId`, and cleared incremental state baseline.
    /// Dropping any previous stream closes only the client subscription.
    pub async fn resubscribe(
        &mut self,
        connection_epoch: u64,
        initial_bounds: PageBounds,
    ) -> Result<String, EngineError> {
        let _guard = Arc::clone(&self.serial_op).lock_owned().await;
        let subscriber_id = Uuid::new_v4().to_string();
        self.awaiting_turn_after = None;
        self.auto_approval.next(None, Instant::now());
        self.stream = None;
        self.local_phase = LocalSessionPhase::Unknown;
        self.last_live_update = None;
        self.browsing_history = initial_bounds != PageBounds::default_initial();
        self.state
            .reset_for_subscription(connection_epoch, subscriber_id.clone());
        let stream = self
            .client
            .open_state_stream(&StreamSubscribeRequest {
                conversation_id: self.cascade_id.clone(),
                subscriber_id: subscriber_id.clone(),
                initial_steps_page_bounds: initial_bounds,
            })
            .await
            .map_err(|err| {
                self.active_stop = None;
                self.pending_stop_transition = None;
                EngineError::from_wire(err, "建立状态订阅 StreamAgentStateUpdates 失败")
            })?;
        self.stream = Some(stream);
        if self.active_stop.is_some() {
            self.local_phase = LocalSessionPhase::Stopping;
        }
        Ok(subscriber_id)
    }

    /// Requests a history step page bound to the current active `subscriberId`.
    pub async fn request_page(&mut self, bounds: PageBounds) -> Result<Value, EngineError> {
        let _guard = Arc::clone(&self.serial_op).lock_owned().await;
        if !self.has_active_stream() {
            return Err(EngineError::not_sent(
                "NO_ACTIVE_SUBSCRIPTION",
                "订阅已断开，请先核对状态。",
            ));
        }
        if !self.browsing_history && self.execution_status() != SessionExecutionStatus::Idle {
            return Err(EngineError::not_sent(
                "PAGE_REQUIRES_IDLE",
                "执行或等待交互时保持实时尾页；空闲后可查看历史分页。",
            ));
        }
        let Some(subscriber_id) = self.state.subscriber_id.clone() else {
            return Err(EngineError::not_sent(
                "NO_ACTIVE_SUBSCRIPTION",
                "当前会话没有活跃订阅，无法请求步骤分页。",
            ));
        };
        let result = self
            .client
            .request_page_update(&PageUpdateRequest {
                conversation_id: self.cascade_id.clone(),
                subscriber_id,
                step_page_bounds: bounds,
            })
            .await
            .map_err(|err| EngineError::from_wire(err, "请求步骤分页失败"));
        if result.is_ok() {
            self.browsing_history = true;
            self.local_phase = LocalSessionPhase::Unknown;
        }
        result
    }

    /// Serialized per-session send. Never auto-retries on `OutcomeUnknown`.
    pub async fn send_message(
        &mut self,
        text: &str,
        override_model: Option<&str>,
        catalog: &ModelCatalog,
        tool_config: Option<Value>,
        client_message_id: Option<String>,
    ) -> Result<SendOutcome, EngineError> {
        self.send_message_with_images(text, override_model, catalog, tool_config, client_message_id, Vec::new()).await
    }

    pub async fn send_message_with_images(
        &mut self, text: &str, override_model: Option<&str>, catalog: &ModelCatalog,
        tool_config: Option<Value>, client_message_id: Option<String>, images: Vec<Value>,
    ) -> Result<SendOutcome, EngineError> {
        self.send_items_internal(text, None, override_model, catalog, tool_config, client_message_id, images).await
    }

    pub async fn send_user_text_items(&mut self, items: Vec<String>, catalog: &ModelCatalog, tool_config: Option<Value>) -> Result<SendOutcome, EngineError> {
        self.send_user_items(items, Vec::new(), catalog, tool_config).await
    }

    pub async fn send_user_items(&mut self, items: Vec<String>, images: Vec<Value>, catalog: &ModelCatalog, tool_config: Option<Value>) -> Result<SendOutcome, EngineError> {
        if images.is_empty() && items.iter().all(|s|s.trim().is_empty()) { return Err(EngineError::not_sent("EMPTY_MESSAGE","消息内容不能为空。")); }
        let text=items.concat();
        self.send_items_internal(&text, Some(items), None, catalog, tool_config, None, images).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_items_internal(
        &mut self, text: &str, text_items:Option<Vec<String>>, override_model: Option<&str>, catalog: &ModelCatalog,
        tool_config: Option<Value>, client_message_id: Option<String>, images: Vec<Value>,
    ) -> Result<SendOutcome, EngineError> {
        let _guard = Arc::clone(&self.serial_op).lock_owned().await;
        if text.trim().is_empty() && images.is_empty() {
            return Err(EngineError::not_sent("EMPTY_MESSAGE", "消息内容不能为空。"));
        }
        if self.state.frames == 0 && self.has_active_stream() {
            // The send operation already owns serial_op; only merge the baseline here.
            let baseline = self.consume_frame(Duration::from_secs(5)).await;
            if !matches!(baseline, Ok(true)) {
                return Err(baseline.err().unwrap_or_else(|| {
                    EngineError::not_sent("NO_STATE_BASELINE", "尚未取得会话状态基线，未发送消息。")
                }));
            }
        }
        if self.execution_status() != SessionExecutionStatus::Idle {
            return Err(EngineError::not_sent(
                "SESSION_NOT_IDLE",
                "会话仍在执行或状态待核实，未发送消息。",
            ));
        }

        let prev_model_id = self.selected_model_id.clone();
        let prev_model_enum = self.selected_model_enum.clone();
        if let Some(requested) = override_model {
            let (id, model_enum) = catalog.resolve(Some(requested)).ok_or_else(|| {
                EngineError::not_sent(
                    "UNKNOWN_MODEL",
                    format!("所选模型 '{requested}' 不在实时模型目录中，未发送消息。"),
                )
            })?;
            self.selected_model_id = id;
            self.selected_model_enum = model_enum;
        }

        if self.state.frames == 0 {
            self.selected_model_id = prev_model_id;
            self.selected_model_enum = prev_model_enum;
            return Err(EngineError::not_sent(
                "NO_STATE_BASELINE",
                "尚未取得会话状态基线，未发送消息。",
            ));
        }
        let prev_phase = self.local_phase;
        let mut tool_config = tool_config.or_else(|| crate::permissions::tool_config(None));
        let auto_approve = tool_config.as_mut()
            .and_then(Value::as_object_mut)
            .and_then(|obj| obj.remove("localAutoApprove"))
            .and_then(|v| v.as_bool()).unwrap_or(false);
        // Conversation preferences override request tool config. Apply the selected
        // preset before sending, and never send if that update was not confirmed.
        if let Some(preset) = tool_config
            .as_ref()
            .and_then(|v| v.pointer("/permissionConfig/effectivePermissionPreset"))
            .and_then(Value::as_str)
            && let Err(err) = self
                .client
                .set_conversation_permission_preset(&self.cascade_id, preset)
                .await
        {
            self.selected_model_id = prev_model_id;
            self.selected_model_enum = prev_model_enum;
            return Err(EngineError::from_wire(err, "权限设置未确认，消息未发送"));
        }
        let prev_awaiting = self.awaiting_turn_after;
        let prev_floor = self.turn_step_floor;
        self.turn_step_floor = self.state.steps.total_length;
        self.awaiting_turn_after = Some(self.state.steps.total_length);
        self.active_stop = None;
        self.pending_stop_transition = None;
        self.local_phase = LocalSessionPhase::Sending;
        self.last_disposition = TaskDisposition::None;
        let client_request_id = Uuid::new_v4().to_string();

        let opts = SendMessageOptions {
            cascade_id: self.cascade_id.clone(),
            text: text.to_owned(),
            text_items,
            images,
            plan_model: self.selected_model_enum.clone(),
            client_request_id: client_request_id.clone(),
            client_message_id: client_message_id.clone(),
            tool_config,
        };

        match self.client.send_user_message(&opts).await {
            Ok(response) => {
                self.state.task_progress.begin_turn(self.turn_step_floor);
                self.auto_approval.begin_turn(auto_approve);
                self.local_phase = LocalSessionPhase::Running;
                Ok(SendOutcome {
                    client_request_id,
                    client_message_id,
                    response,
                })
            }
            Err(wire) => {
                match wire.category {
                    TransportErrorCategory::NotSent => {
                        self.selected_model_id = prev_model_id;
                        self.selected_model_enum = prev_model_enum;
                        self.turn_step_floor = prev_floor;
                        self.awaiting_turn_after = prev_awaiting;
                        self.local_phase = prev_phase;
                    }
                    TransportErrorCategory::Rejected => {
                        self.selected_model_id = prev_model_id;
                        self.selected_model_enum = prev_model_enum;
                        self.turn_step_floor = prev_floor;
                        self.awaiting_turn_after = prev_awaiting;
                        self.local_phase = LocalSessionPhase::Idle;
                        self.last_disposition = TaskDisposition::Failed;
                    }
                    TransportErrorCategory::OutcomeUnknown => {
                        self.state.task_progress.begin_turn(self.turn_step_floor);
                        // Do not carry a prior turn's auto-approval into an unconfirmed turn.
                        self.auto_approval.disable();
                        // Never auto-retry; caller must inspect session read-only.
                        self.local_phase = LocalSessionPhase::Unknown;
                        self.last_disposition = TaskDisposition::Unconfirmed;
                    }
                }
                Err(EngineError::from_wire(
                    wire,
                    "发送消息 SendUserCascadeMessage 未确认成功（严禁自动重发）",
                ))
            }
        }
    }

    /// Validates `req` against the live state store via `InteractionRegistry`
    /// and submits `HandleCascadeUserInteraction`. Never auto-retries on `OutcomeUnknown`.
    pub async fn respond_interaction(
        &mut self,
        req: &InteractionSubmitRequest,
    ) -> Result<Value, EngineError> {
        let _guard = Arc::clone(&self.serial_op).lock_owned().await;
        if !self.has_active_stream()
            || self.execution_status() != SessionExecutionStatus::WaitingInteraction
        {
            return Err(EngineError::not_sent(
                "INTERACTION_NOT_LIVE",
                "会话没有可核验的实时待处理交互，未提交。",
            ));
        }
        let reply = InteractionRegistry::validate_and_build_reply(&self.state, req)?;
        let interaction_key = (req.trajectory_id.clone(), req.step_index);
        if self.submitted_interactions.contains(&interaction_key) {
            return Err(EngineError::not_sent(
                "INTERACTION_ALREADY_SUBMITTED",
                "此交互已提交或结果未知，不重复发送。",
            ));
        }
        match self
            .client
            .handle_user_interaction(&self.cascade_id, &req.trajectory_id, req.step_index, &reply)
            .await
        {
            Ok(value) => {
                self.submitted_interactions.insert(interaction_key);
                self.local_phase = LocalSessionPhase::Running;
                Ok(value)
            }
            Err(wire) => {
                if wire.category == TransportErrorCategory::OutcomeUnknown {
                    self.submitted_interactions.insert(interaction_key);
                    self.local_phase = LocalSessionPhase::Unknown;
                    self.last_disposition = TaskDisposition::Unconfirmed;
                }
                Err(EngineError::from_wire(
                    wire,
                    "提交交互响应 HandleCascadeUserInteraction 失败（严禁自动重发）",
                ))
            }
        }
    }

    /// Stops this client-owned session according to explicit `StopScope`.
    ///
    /// Transitions into `Stopping` upon RPC acceptance; callers must continue
    /// observing stream state and side effects before treating the task as stopped.
    pub async fn stop(&mut self, scope: StopScope) -> Result<StopAccepted, EngineError> {
        self.stop_with_parent(scope, false).await
    }

    pub async fn stop_with_parent(
        &mut self,
        scope: StopScope,
        notify_parent: bool,
    ) -> Result<StopAccepted, EngineError> {
        let _guard = Arc::clone(&self.serial_op).lock_owned().await;
        if !self.owned_by_client {
            return Err(EngineError::not_sent(
                "SESSION_NOT_OWNED",
                "仅允许停止本客户端登记管理的会话，拒绝停止外部会话。",
            ));
        }

        let prev_phase = self.local_phase;
        self.local_phase = LocalSessionPhase::Stopping;
        let raw_response = match scope {
            StopScope::Invocation => self
                .client
                .cancel_cascade_invocation_with_parent(&self.cascade_id, false, notify_parent)
                .await
                .map_err(|err| self.handle_stop_error(err, prev_phase))?,
            StopScope::Background => self
                .client
                .cancel_cascade_invocation_with_parent(&self.cascade_id, true, notify_parent)
                .await
                .map_err(|err| self.handle_stop_error(err, prev_phase))?,
            StopScope::Tree => self
                .client
                .force_stop_cascade_tree(&self.cascade_id)
                .await
                .map_err(|err| self.handle_stop_error(err, prev_phase))?,
        };

        let stopped_conversation_ids: Vec<String> = raw_response
            .get("stoppedConversationIds")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();

        self.active_stop = Some(ActiveStop {
            scope,
            started_at: Instant::now(),
            stopped_conversation_ids: stopped_conversation_ids.clone(),
            tree_verified: false,
        });
        self.pending_stop_transition = None;

        Ok(StopAccepted {
            scope,
            stopped_conversation_ids,
            raw_response,
        })
    }

    fn handle_stop_error(&mut self, wire: WireError, prev_phase: LocalSessionPhase) -> EngineError {
        if wire.category == TransportErrorCategory::OutcomeUnknown {
            self.local_phase = LocalSessionPhase::Unknown;
            self.last_disposition = TaskDisposition::Unconfirmed;
        } else {
            self.local_phase = prev_phase;
        }
        EngineError::from_wire(wire, "停止请求未能确认执行状态，请执行只读核对")
    }

    pub async fn get_trajectory(&self) -> Result<Value, EngineError> {
        self.client
            .get_cascade_trajectory(&self.cascade_id)
            .await
            .map_err(|err| EngineError::from_wire(err, "只读查询 GetCascadeTrajectory 失败"))
    }

    pub async fn get_file_diffs(&self) -> Result<Value, EngineError> {
        self.client
            .get_trajectory_file_diffs(&self.cascade_id)
            .await
            .map_err(|err| EngineError::from_wire(err, "按需查询 GetTrajectoryFileDiffs 失败"))
    }

    /// Reads and merges a single update frame from the active `PROD_UI` stream
    /// without running local auto-approval side effects.
    pub async fn consume_next_frame_passive(
        &mut self,
        timeout: Duration,
    ) -> Result<bool, EngineError> {
        self.consume_frame(timeout).await
    }

    /// Reads and merges a single update frame from the active `PROD_UI` stream.
    /// Returns `Ok(true)` if an update frame was applied, `Ok(false)` on timeout
    /// when no update arrives. EOF revokes the subscription and returns an error.
    pub async fn consume_next_frame(&mut self, timeout: Duration) -> Result<bool, EngineError> {
        let changed = self.consume_frame(timeout).await?;
        let pending = if self.has_active_stream()
            && !self.browsing_history
            && self.execution_status() == SessionExecutionStatus::WaitingInteraction {
            InteractionRegistry::extract_live_pending(&self.state).filter(|p|
                !self.submitted_interactions.contains(&(p.trajectory_id.clone(), p.step_index)))
        } else { None };
        if let Some(req) = self.auto_approval.next(pending, Instant::now()) {
            self.respond_interaction(&req).await?;
            return Ok(true);
        }
        Ok(changed)
    }

    async fn consume_frame(&mut self, timeout: Duration) -> Result<bool, EngineError> {
        let Some(stream) = self.stream.as_mut() else {
            return Err(EngineError::not_sent(
                "NO_ACTIVE_SUBSCRIPTION",
                "当前会话没有活跃状态流订阅。",
            ));
        };
        let next = match tokio::time::timeout(timeout, stream.next_message()).await {
            Ok(res) => res,
            Err(_) => return Ok(false),
        };
        match next {
            Ok(Some(envelope)) => {
                if let Some(update) = envelope.get("update") {
                    if let Some(conv) = update.get("conversationId").and_then(Value::as_str)
                        && conv != self.cascade_id
                    {
                        self.disconnect_subscription();
                        return Err(EngineError::from_wire(
                            WireError::outcome_unknown(
                                "WRONG_CONVERSATION_ON_STREAM",
                                None,
                                format!(
                                    "received update for conversation {conv}, expected {}",
                                    self.cascade_id
                                ),
                            ),
                            "状态流返回了不匹配的会话 ID",
                        ));
                    }
                    let epoch = self.state.connection_epoch;
                    if let Err(err) = self.state.apply_update(epoch, update) {
                        self.disconnect_subscription();
                        return Err(EngineError::from_wire(
                            WireError::outcome_unknown("STATE_MERGE_FAILED", None, err.to_string()),
                            "状态归并失败，已原子回滚旧基线",
                        ));
                    }
                    self.last_live_update = Some(Instant::now());
                    self.reconcile_phase_from_live_state();
                    return Ok(true);
                }
                Ok(false)
            }
            Ok(None) => {
                self.disconnect_subscription();
                Err(EngineError::from_wire(
                    WireError::outcome_unknown("STATE_STREAM_ENDED", None, "状态流已结束"),
                    "状态订阅已结束，需要重新核对状态",
                ))
            }
            Err(wire) => {
                self.disconnect_subscription();
                Err(EngineError::from_wire(
                    wire,
                    "状态订阅流中断，会话状态已置为待核实",
                ))
            }
        }
    }

    fn reconcile_phase_from_live_state(&mut self) {
        if self.browsing_history && self.local_phase != LocalSessionPhase::Stopping {
            return;
        }
        if self.local_phase != LocalSessionPhase::Stopping
            && let Some(floor) = self.awaiting_turn_after
        {
            let fresh_terminal = self.state.steps.values.iter().any(|(index, step)| {
                *index >= floor
                    && (step
                        .get("plannerResponse")
                        .is_some_and(|p| ["response", "modifiedResponse"].iter().any(|key|
                            p.get(*key).and_then(Value::as_str).is_some_and(|s| !s.is_empty())))
                        || step.get("status").and_then(Value::as_str)
                            == Some("CORTEX_STEP_STATUS_ERROR")
                        || step.get("status").and_then(Value::as_str)
                            == Some("CORTEX_STEP_STATUS_CANCELED")
                        || step
                            .get("errorMessage")
                            .and_then(Value::as_str)
                            .is_some_and(|s| !s.is_empty()))
            });
            if (self.state.steps.total_length > floor && !self.state.fully_idle()) || fresh_terminal
            {
                self.awaiting_turn_after = None;
            } else {
                return;
            }
        }
        let pending = self.interactions.sync_from_state(&self.state);
        if pending.is_some() {
            // Keep Stopping if caller already asked to stop; otherwise live state is waiting.
            if self.local_phase != LocalSessionPhase::Stopping {
                self.local_phase = LocalSessionPhase::Running;
            }
            return;
        }

        if self.state.fully_idle() && !self.state.has_active_background_tasks() {
            let floor = self.turn_step_floor;
            if self.local_phase == LocalSessionPhase::Stopping {
                if self
                    .active_stop
                    .as_ref()
                    .is_some_and(|s| s.scope == StopScope::Tree && !s.tree_verified)
                {
                    return;
                }
                self.awaiting_turn_after = None;
                self.local_phase = LocalSessionPhase::Idle;
                self.last_disposition = TaskDisposition::Canceled;
                if let Some(stop) = self.active_stop.take() {
                    self.pending_stop_transition = Some(StopTransition {
                        scope: stop.scope,
                        phase: "Stopped",
                        stopped_conversation_ids: stop.stopped_conversation_ids,
                    });
                }
            } else if self.state.has_error_step_since(floor) {
                self.local_phase = LocalSessionPhase::Idle;
                self.last_disposition = TaskDisposition::Failed;
            } else if self.state.has_canceled_step_since(floor)
                && !self.state.has_assistant_output_since(floor)
            {
                self.local_phase = LocalSessionPhase::Idle;
                self.last_disposition = TaskDisposition::Canceled;
            } else if self.state.has_assistant_output_since(floor) {
                self.local_phase = LocalSessionPhase::Idle;
                self.last_disposition = TaskDisposition::Completed;
            } else if self.local_phase == LocalSessionPhase::Running
                || self.local_phase == LocalSessionPhase::Unknown
            {
                // Idle without assistant output, error, or cancel evidence is Unconfirmed, never Completed.
                self.local_phase = LocalSessionPhase::Idle;
                self.last_disposition = TaskDisposition::Unconfirmed;
            }
        } else if (self.local_phase == LocalSessionPhase::Idle
            || self.local_phase == LocalSessionPhase::Unknown)
            && (!self.state.fully_idle() || self.state.has_active_background_tasks())
            && self.state.frames > 0
            && self.state.steps.total_length > 0
        {
            self.local_phase = LocalSessionPhase::Running;
        }
    }

    /// Consumes frames until `condition` is met or `max_wait` elapses.
    pub async fn observe_until(
        &mut self,
        max_wait: Duration,
        condition: ObserveStopCondition,
    ) -> Result<SessionSnapshot, EngineError> {
        let deadline = tokio::time::Instant::now() + max_wait;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                if self.local_phase == LocalSessionPhase::Stopping
                    && (!self.state.fully_idle() || self.state.has_active_background_tasks())
                {
                    self.local_phase = LocalSessionPhase::Unknown;
                    self.last_disposition = TaskDisposition::Unconfirmed;
                }
                break;
            }
            let step_wait = remaining.min(Duration::from_millis(500));
            let got_frame = self.consume_next_frame(step_wait).await?;
            if !got_frame {
                continue;
            }
            let pending = InteractionRegistry::extract_live_pending(&self.state).is_some();
            let idle = self.awaiting_turn_after.is_none()
                && self.state.fully_idle()
                && !self.state.has_active_background_tasks();
            let has_bg = self.state.has_active_background_tasks();
            let done = match condition {
                ObserveStopCondition::UntilIdle => idle && self.state.frames > 0,
                ObserveStopCondition::UntilPendingOrIdle => {
                    pending || (idle && self.state.frames > 0)
                }
                ObserveStopCondition::UntilBackgroundOrPendingOrIdle => {
                    has_bg || pending || (idle && self.state.frames > 0)
                }
                ObserveStopCondition::FixedDuration => false,
            };
            if done {
                break;
            }
        }
        Ok(self.snapshot())
    }

    /// Derives orthogonal `SessionExecutionStatus` and `TaskDisposition` without
    /// collapsing `fullyIdle`, assistant output, and task outcome into a single enum.
    pub fn execution_status(&self) -> SessionExecutionStatus {
        match self.local_phase {
            LocalSessionPhase::Sending => SessionExecutionStatus::Sending,
            LocalSessionPhase::Stopping => SessionExecutionStatus::Stopping,
            LocalSessionPhase::Unknown => SessionExecutionStatus::Unknown,
            LocalSessionPhase::Idle | LocalSessionPhase::Running => {
                if InteractionRegistry::extract_live_pending(&self.state).is_some() {
                    SessionExecutionStatus::WaitingInteraction
                } else if self.state.has_active_background_tasks()
                    || (self.local_phase == LocalSessionPhase::Running
                        && (self.awaiting_turn_after.is_some() || !self.state.fully_idle()))
                {
                    SessionExecutionStatus::Running
                } else {
                    SessionExecutionStatus::Idle
                }
            }
        }
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        let mut observed_background_ids: Vec<String> =
            self.state.background_seen.keys().cloned().collect();
        observed_background_ids.sort();
        SessionSnapshot {
            conversation_id: self.cascade_id.clone(),
            trajectory_id: self.state.trajectory_id().map(str::to_owned),
            connection_epoch: self.state.connection_epoch,
            subscriber_id: self.state.subscriber_id.clone(),
            selected_model_id: self.selected_model_id.clone(),
            selected_model_enum: self.selected_model_enum.clone(),
            execution_status: self.execution_status(),
            disposition: self.last_disposition,
            has_assistant_output: self.state.has_assistant_output(),
            fully_idle_raw: self.state.fully_idle_raw,
            fully_idle: self.awaiting_turn_after.is_none() && self.state.fully_idle(),
            frames: self.state.frames,
            steps_total: self.state.steps.total_length,
            page_bounds: self.state.steps.page_bounds.clone(),
            projected_step_indices: self.state.steps.values.keys().copied().collect(),
            pending_interaction: InteractionRegistry::extract_live_pending(&self.state),
            active_background_count: self.state.background_tasks.values.len(),
            observed_background_ids,
            subagent_ids: self.state.structured_subagent_ids(),
            assistant_texts: self.state.assistant_texts(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ls_wire::{AuthSummary, ModelEntry, TARGET_LS_VERSION, encode_connect_frame};
    use serde_json::json;
    use std::{
        collections::BTreeMap,
        io::{Read, Write},
        net::TcpListener,
    };

    fn read_http_req(socket: &mut std::net::TcpStream) -> String {
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut received = Vec::new();
        let mut buf = [0u8; 2048];
        loop {
            let n = socket.read(&mut buf).unwrap();
            assert!(n > 0);
            received.extend_from_slice(&buf[..n]);
            if let Some(at) = received.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&received[..at]).to_ascii_lowercase();
                let len: usize = headers
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .unwrap_or("0")
                    .trim()
                    .parse()
                    .unwrap();
                if received.len() >= at + 4 + len {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&received).into_owned()
    }

    fn mock_catalog() -> ModelCatalog {
        ModelCatalog {
            default_agent_model_id: "default-model".into(),
            models: vec![ModelEntry {
                id: "default-model".into(),
                model: "MODEL_TEST_1".into(),
                label: Some("Test Model".into()),
                extra: json!({}),
            }],
        }
    }

    fn mock_readonly(port: u16) -> ReadonlyCheckReport {
        ReadonlyCheckReport {
            instance: InstanceSummary {
                pid: 9999,
                https_port: port,
                ls_version: TARGET_LS_VERSION.to_owned(),
            },
            capabilities_keys: vec!["capabilities".into()],
            auth: AuthSummary {
                auth_status_bools: BTreeMap::new(),
                auth_status_keys: vec![],
                has_auth_token_bools: BTreeMap::new(),
                has_auth_token_keys: vec![],
            },
            default_agent_model_id: "default-model".into(),
            model_count: 1,
            security_checks: None,
        }
    }

    #[tokio::test]
    async fn send_outcome_unknown_moves_session_to_unknown_without_retry() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut policy_sock, _) = listener.accept().unwrap();
            let policy_req = read_http_req(&mut policy_sock);
            assert!(policy_req.contains("UpdateConversationAnnotations"));
            policy_sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").unwrap();
            drop(policy_sock);
            let (mut sock, _) = listener.accept().unwrap();
            let req = read_http_req(&mut sock);
            assert!(req.contains("SendUserCascadeMessage"));
            drop(sock); // Simulate dropped response after body received.
        });

        let client = Arc::new(
            LsClient::new_for_local_mock(
                format!("http://{addr}"),
                mock_readonly(addr.port()).instance,
                "tok".into(),
            )
            .unwrap(),
        );
        let catalog = mock_catalog();
        let mut session = ConversationSession::new_owned(
            client,
            Uuid::new_v4().to_string(),
            PathBuf::from("."),
            "default-model".into(),
            "MODEL_TEST_1".into(),
        );

        session
            .state
            .apply_update(0, &json!({"fullyIdle":true}))
            .unwrap();
        let err = session
            .send_message("hello", None, &catalog, None, None)
            .await
            .unwrap_err();
        assert_eq!(err.category, TransportErrorCategory::OutcomeUnknown);
        assert_eq!(session.execution_status(), SessionExecutionStatus::Unknown);
        assert_eq!(session.snapshot().disposition, TaskDisposition::Unconfirmed);
        server.join().unwrap();
    }

    #[test]
    fn stale_idle_after_send_does_not_complete_new_turn() {
        let client = Arc::new(
            LsClient::new_for_local_mock(
                "http://127.0.0.1:1".into(),
                mock_readonly(1).instance,
                "tok".into(),
            )
            .unwrap(),
        );
        let mut session = ConversationSession::new_owned(
            client,
            Uuid::new_v4().to_string(),
            PathBuf::from("."),
            "default-model".into(),
            "MODEL_TEST_1".into(),
        );
        session.local_phase = LocalSessionPhase::Running;
        session.awaiting_turn_after = Some(1);
        session.state.apply_update(0, &json!({"fullyIdle":true,"mainTrajectoryUpdate":{"stepsUpdate":{"totalLength":1,"indices":[0],"steps":[{"plannerResponse":{"response":"previous answer"}}]}}})).unwrap();
        session.reconcile_phase_from_live_state();
        assert!(!session.snapshot().fully_idle);
        assert_eq!(session.execution_status(), SessionExecutionStatus::Running);
        assert_eq!(session.snapshot().disposition, TaskDisposition::None);
        // A default-false metadata frame with no new steps is not proof of the new turn.
        session.state.apply_update(0, &json!({})).unwrap();
        session.reconcile_phase_from_live_state();
        assert_eq!(session.awaiting_turn_after, Some(1));
        session.state.apply_update(0, &json!({"fullyIdle":true,"mainTrajectoryUpdate":{"stepsUpdate":{"totalLength":2,"indices":[1],"steps":[{"plannerResponse":{"response":"new answer"}}]}}})).unwrap();
        session.reconcile_phase_from_live_state();
        assert!(session.snapshot().fully_idle);
        assert_eq!(session.snapshot().disposition, TaskDisposition::Completed);
    }

    #[tokio::test]
    async fn disconnected_session_blocks_send_and_can_be_reconciled() {
        let client = Arc::new(
            LsClient::new_for_local_mock(
                "http://127.0.0.1:1".into(),
                mock_readonly(1).instance,
                "tok".into(),
            )
            .unwrap(),
        );
        let mut session = ConversationSession::new_owned(
            client,
            Uuid::new_v4().to_string(),
            PathBuf::from("."),
            "default-model".into(),
            "MODEL_TEST_1".into(),
        );
        session.disconnect_subscription();
        assert_eq!(session.execution_status(), SessionExecutionStatus::Unknown);
        let err = session
            .send_message("hello", None, &mock_catalog(), None, None)
            .await
            .unwrap_err();
        assert_eq!(err.reason_code, "SESSION_NOT_IDLE");
        session
            .state
            .apply_update(0, &json!({"fullyIdle": true}))
            .unwrap();
        session.reconcile_phase_from_live_state();
        assert_eq!(session.execution_status(), SessionExecutionStatus::Idle);
        assert_eq!(session.snapshot().disposition, TaskDisposition::Unconfirmed);
    }

    #[test]
    fn stop_without_fresh_evidence_times_out_even_with_cached_idle() {
        let client = Arc::new(
            LsClient::new_for_local_mock(
                "http://127.0.0.1:1".into(),
                mock_readonly(1).instance,
                "tok".into(),
            )
            .unwrap(),
        );
        let mut session = ConversationSession::new_owned(
            client,
            Uuid::new_v4().to_string(),
            PathBuf::from("."),
            "default-model".into(),
            "MODEL_TEST_1".into(),
        );
        session
            .state
            .apply_update(0, &json!({"fullyIdle": true}))
            .unwrap();
        session.local_phase = LocalSessionPhase::Stopping;
        session.awaiting_turn_after = Some(0);
        session.active_stop = Some(ActiveStop {
            scope: StopScope::Invocation,
            started_at: Instant::now(),
            stopped_conversation_ids: vec![],
            tree_verified: false,
        });
        session.stop_verify_timeout = Duration::ZERO;
        assert_eq!(session.check_stop_timeout().unwrap().phase, "Unknown");
        // A fresh idle frame during a subsequent stop must bypass the send floor.
        session.local_phase = LocalSessionPhase::Stopping;
        session.active_stop = Some(ActiveStop {
            scope: StopScope::Invocation,
            started_at: Instant::now(),
            stopped_conversation_ids: vec![],
            tree_verified: false,
        });
        session
            .state
            .apply_update(0, &json!({"fullyIdle": true}))
            .unwrap();
        session.reconcile_phase_from_live_state();
        assert_eq!(session.take_stop_transition().unwrap().phase, "Stopped");
        assert!(session.awaiting_turn_after.is_none());
    }

    #[tokio::test]
    async fn resubscribe_resets_baseline_and_increments_epoch() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let conv_id = Uuid::new_v4().to_string();
        let conv_clone = conv_id.clone();
        let server = std::thread::spawn(move || {
            for round in 1..=2 {
                let (mut sock, _) = listener.accept().unwrap();
                let req = read_http_req(&mut sock);
                assert!(req.contains("StreamAgentStateUpdates"));
                let mut body = encode_connect_frame(&json!({
                    "update": {
                        "conversationId": conv_clone,
                        "trajectoryId": "traj-1",
                        "fullyIdle": true,
                        "mainTrajectoryUpdate": {
                            "stepsUpdate": {
                                "totalLength": round,
                                "indices": [round - 1],
                                "steps": [{
                                    "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
                                    "status": "CORTEX_STEP_STATUS_DONE",
                                    "plannerResponse": { "response": format!("round-{round}") }
                                }]
                            }
                        }
                    }
                }))
                .unwrap();
                let mut end = encode_connect_frame(&json!({})).unwrap();
                end[0] = 2;
                body.extend_from_slice(&end);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/connect+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                sock.write_all(header.as_bytes()).unwrap();
                sock.write_all(&body).unwrap();
            }
        });

        let client = Arc::new(
            LsClient::new_for_local_mock(
                format!("http://{addr}"),
                mock_readonly(addr.port()).instance,
                "tok".into(),
            )
            .unwrap(),
        );
        let conn = EngineConnection::from_parts(client, mock_readonly(addr.port()), mock_catalog());
        let mut session = conn
            .attach_owned_session(
                conv_id,
                PathBuf::from("."),
                None,
                PageBounds::default_initial(),
            )
            .await
            .unwrap();
        let sub1 = session.subscriber_id().unwrap().to_owned();
        let epoch1 = session.connection_epoch();
        let snap1 = session
            .observe_until(Duration::from_secs(3), ObserveStopCondition::UntilIdle)
            .await
            .unwrap();
        assert_eq!(snap1.assistant_texts, vec!["round-1"]);

        session.awaiting_turn_after = Some(999);
        let epoch2 = conn.next_epoch();
        let sub2 = session
            .resubscribe(epoch2, PageBounds::default_initial())
            .await
            .unwrap();
        assert_ne!(sub1, sub2);
        assert_eq!(session.awaiting_turn_after, None);
        assert_eq!(session.execution_status(), SessionExecutionStatus::Unknown);
        assert!(epoch2 > epoch1);
        assert_eq!(session.state().frames, 0, "resubscribe must clear baseline");

        let snap2 = session
            .observe_until(Duration::from_secs(3), ObserveStopCondition::UntilIdle)
            .await
            .unwrap();
        assert_eq!(snap2.assistant_texts, vec!["round-2"]);
        let eof = session.consume_next_frame(Duration::from_secs(1)).await.unwrap_err();
        assert_eq!(eof.reason_code, "STATE_STREAM_ENDED");
        assert_eq!(
            session.execution_status(),
            SessionExecutionStatus::Unknown,
            "idle EOF must revoke write capability"
        );
        assert!(session.request_page(PageBounds::range(0, 1)).await.is_err());
        server.join().unwrap();
    }

    #[tokio::test]
    async fn model_switch_failure_does_not_overwrite_selected_model() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut policy_sock, _) = listener.accept().unwrap();
            let policy_req = read_http_req(&mut policy_sock);
            assert!(policy_req.contains("UpdateConversationAnnotations"));
            policy_sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").unwrap();
            drop(policy_sock);
            let (mut sock, _) = listener.accept().unwrap();
            let req = read_http_req(&mut sock);
            assert!(req.contains("SendUserCascadeMessage"));
            let body = r#"{"code":"invalid_argument","message":"model rejected"}"#;
            let resp = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).unwrap();
        });

        let client = Arc::new(
            LsClient::new_for_local_mock(
                format!("http://{addr}"),
                mock_readonly(addr.port()).instance,
                "tok".into(),
            )
            .unwrap(),
        );
        let mut catalog = mock_catalog();
        catalog.models.push(ModelEntry {
            id: "alt-model".into(),
            model: "MODEL_ALT_2".into(),
            label: Some("Alt Model".into()),
            extra: json!({}),
        });
        let mut session = ConversationSession::new_owned(
            client,
            Uuid::new_v4().to_string(),
            PathBuf::from("."),
            "default-model".into(),
            "MODEL_TEST_1".into(),
        );
        session
            .state
            .apply_update(0, &json!({ "fullyIdle": true }))
            .unwrap();

        let err = session
            .send_message("hello", Some("alt-model"), &catalog, None, None)
            .await
            .unwrap_err();
        assert_eq!(err.category, TransportErrorCategory::Rejected);
        assert_eq!(session.snapshot().selected_model_id, "default-model");
        assert_eq!(session.snapshot().selected_model_enum, "MODEL_TEST_1");
        assert_eq!(session.snapshot().disposition, TaskDisposition::Failed);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn permission_update_precedes_send_and_rejection_blocks_send() {
        for choice in ["review", "default", "auto", "turbo", "rejected"] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let rejected = choice == "rejected";
            let config =
                crate::permissions::tool_config(Some(if rejected { "auto" } else { choice }))
                    .unwrap();
            let preset = config["permissionConfig"]["effectivePermissionPreset"]
                .as_str()
                .unwrap()
                .to_owned();
            let server = std::thread::spawn(move || {
                for n in 0..if rejected { 1 } else { 2 } {
                    let (mut sock, _) = listener.accept().unwrap();
                    let req = read_http_req(&mut sock);
                    if n == 0 {
                        assert!(req.contains("UpdateConversationAnnotations"));
                        assert!(req.contains(&preset));
                    } else {
                        assert!(req.contains("SendUserCascadeMessage"));
                        assert!(req.contains(&preset));
                    }
                    let status = if rejected {
                        "400 Bad Request"
                    } else {
                        "200 OK"
                    };
                    let body = if rejected {
                        r#"{"code":"invalid_argument","message":"preset rejected"}"#
                    } else {
                        "{}"
                    };
                    write!(sock, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                }
            });
            let client = Arc::new(
                LsClient::new_for_local_mock(
                    format!("http://{addr}"),
                    mock_readonly(addr.port()).instance,
                    "tok".into(),
                )
                .unwrap(),
            );
            let mut session = ConversationSession::new_owned(
                client,
                Uuid::new_v4().to_string(),
                PathBuf::from("."),
                "default-model".into(),
                "MODEL_TEST_1".into(),
            );
            session
                .state
                .apply_update(0, &json!({"fullyIdle":true}))
                .unwrap();
            let result = session
                .send_message("hello", None, &mock_catalog(), Some(config), None)
                .await;
            if rejected {
                assert_eq!(
                    result.unwrap_err().category,
                    TransportErrorCategory::Rejected
                );
                assert_eq!(session.execution_status(), SessionExecutionStatus::Idle);
            } else {
                assert!(result.is_ok());
            }
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn stop_distinguishes_accepted_stopped_and_timeout_unknown() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut sock, _) = listener.accept().unwrap();
                let req = read_http_req(&mut sock);
                assert!(req.contains("CancelCascadeInvocation"));
                let body = r#"{}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                sock.write_all(resp.as_bytes()).unwrap();
            }
        });

        let client = Arc::new(
            LsClient::new_for_local_mock(
                format!("http://{addr}"),
                mock_readonly(addr.port()).instance,
                "tok".into(),
            )
            .unwrap(),
        );
        let mut session = ConversationSession::new_owned(
            client,
            Uuid::new_v4().to_string(),
            PathBuf::from("."),
            "default-model".into(),
            "MODEL_TEST_1".into(),
        );
        session.local_phase = LocalSessionPhase::Running;
        session
            .state
            .apply_update(1, &json!({ "fullyIdle": false }))
            .unwrap();

        // 1. RPC acceptance keeps phase at Stopping (NOT Stopped/Canceled yet).
        session.stop(StopScope::Invocation).await.unwrap();
        assert_eq!(session.execution_status(), SessionExecutionStatus::Stopping);
        assert_eq!(session.snapshot().disposition, TaskDisposition::None);
        assert!(session.take_stop_transition().is_none());

        // 2. Observing fullyIdle with no active background confirms Stopped.
        session
            .state
            .apply_update(1, &json!({ "fullyIdle": true }))
            .unwrap();
        session.reconcile_phase_from_live_state();
        assert_eq!(session.execution_status(), SessionExecutionStatus::Idle);
        assert_eq!(session.snapshot().disposition, TaskDisposition::Canceled);
        let tr = session.take_stop_transition().expect("Stopped transition");
        assert_eq!(tr.phase, "Stopped");

        // 3. If stop times out while still not idle, transition to Unknown/Unconfirmed.
        session.stop_verify_timeout = Duration::ZERO;
        session
            .state
            .apply_update(1, &json!({ "fullyIdle": false }))
            .unwrap();
        session.stop(StopScope::Invocation).await.unwrap();
        let timeout_tr = session
            .check_stop_timeout()
            .expect("Unknown timeout transition");
        assert_eq!(timeout_tr.phase, "Unknown");
        assert_eq!(session.execution_status(), SessionExecutionStatus::Unknown);
        assert_eq!(session.snapshot().disposition, TaskDisposition::Unconfirmed);
        server.join().unwrap();
    }

    #[test]
    fn multi_turn_disposition_does_not_leak_prior_turn_output_or_errors() {
        let client = Arc::new(
            LsClient::new_for_local_mock(
                "http://127.0.0.1:1".into(),
                mock_readonly(1).instance,
                "tok".into(),
            )
            .unwrap(),
        );
        let mut session = ConversationSession::new_owned(
            client,
            Uuid::new_v4().to_string(),
            PathBuf::from("."),
            "default-model".into(),
            "MODEL_TEST_1".into(),
        );
        // Turn 1 produces assistant output at step 0.
        session.local_phase = LocalSessionPhase::Running;
        session
            .state
            .apply_update(
                1,
                &json!({
                    "fullyIdle": true,
                    "mainTrajectoryUpdate": {
                        "stepsUpdate": {
                            "totalLength": 1,
                            "indices": [0],
                            "steps": [{ "plannerResponse": { "response": "turn 1 answer" } }]
                        }
                    }
                }),
            )
            .unwrap();
        session.reconcile_phase_from_live_state();
        assert_eq!(session.snapshot().disposition, TaskDisposition::Completed);

        // Turn 2 starts at floor 1 and becomes idle with NO new assistant output -> Unconfirmed!
        session.turn_step_floor = 1;
        session.local_phase = LocalSessionPhase::Running;
        session.last_disposition = TaskDisposition::None;
        session
            .state
            .apply_update(
                1,
                &json!({
                    "fullyIdle": true,
                    "mainTrajectoryUpdate": {
                        "stepsUpdate": {
                            "totalLength": 2,
                            "indices": [1],
                            "steps": [{ "type": "CORTEX_STEP_TYPE_USER_INPUT", "userInput": { "text": "turn 2" } }]
                        }
                    }
                }),
            )
            .unwrap();
        session.reconcile_phase_from_live_state();
        assert_eq!(session.execution_status(), SessionExecutionStatus::Idle);
        assert_eq!(session.snapshot().disposition, TaskDisposition::Unconfirmed);

        // Turn 3 starts at floor 2 and encounters an error step -> Failed!
        session.turn_step_floor = 2;
        session.local_phase = LocalSessionPhase::Running;
        session.last_disposition = TaskDisposition::None;
        session
            .state
            .apply_update(
                1,
                &json!({
                    "fullyIdle": true,
                    "mainTrajectoryUpdate": {
                        "stepsUpdate": {
                            "totalLength": 3,
                            "indices": [2],
                            "steps": [{ "status": "CORTEX_STEP_STATUS_ERROR", "errorMessage": "model failed" }]
                        }
                    }
                }),
            )
            .unwrap();
        session.reconcile_phase_from_live_state();
        assert_eq!(session.snapshot().disposition, TaskDisposition::Failed);
    }

    #[test]
    fn tree_stop_requires_coordinator_evidence_even_when_parent_is_idle() {
        let client = Arc::new(
            LsClient::new_for_local_mock(
                "http://127.0.0.1:1".into(),
                mock_readonly(1).instance,
                "tok".into(),
            )
            .unwrap(),
        );
        let mut s = ConversationSession::new_owned(
            client,
            Uuid::new_v4().to_string(),
            PathBuf::from("."),
            "default-model".into(),
            "MODEL_TEST_1".into(),
        );
        s.local_phase = LocalSessionPhase::Stopping;
        s.active_stop = Some(ActiveStop {
            scope: StopScope::Tree,
            started_at: Instant::now(),
            stopped_conversation_ids: vec!["child".into()],
            tree_verified: false,
        });
        s.state.apply_update(0, &json!({"fullyIdle":true})).unwrap();
        s.reconcile_phase_from_live_state();
        assert_eq!(s.execution_status(), SessionExecutionStatus::Stopping);
        assert!(s.take_stop_transition().is_none());
        s.verify_tree_stop(false);
        assert_eq!(s.execution_status(), SessionExecutionStatus::Stopping);
        s.verify_tree_stop(true);
        assert_eq!(s.take_stop_transition().unwrap().phase, "Stopped");
    }

    #[tokio::test]
    async fn interaction_success_or_lost_reply_is_not_resent_after_reconnect() {
        for lose_reply in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let id = Uuid::new_v4().to_string();
            let server_id = id.clone();
            let server = std::thread::spawn(move || {
                for round in 0..3 {
                    let (mut sock, _) = listener.accept().unwrap();
                    let req = read_http_req(&mut sock);
                    if round == 1 {
                        assert!(req.contains("HandleCascadeUserInteraction"));
                        if !lose_reply {
                            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").unwrap();
                        }
                    } else {
                        assert!(req.contains("StreamAgentStateUpdates"));
                        let mut body=encode_connect_frame(&json!({"update":{"conversationId":server_id,"trajectoryId":"t","mainTrajectoryUpdate":{"stepsUpdate":{"totalLength":1,"indices":[0],"steps":[{"status":"CORTEX_STEP_STATUS_WAITING","requestedInteraction":{"permission":{"command":"test"}}}]}}}})).unwrap();
                        let mut end = encode_connect_frame(&json!({})).unwrap();
                        end[0] = 2;
                        body.extend(end);
                        sock.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/connect+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).unwrap();
                        sock.write_all(&body).unwrap();
                    }
                }
                listener.set_nonblocking(true).unwrap();
                assert!(
                    listener.accept().is_err(),
                    "duplicate interaction must not open another request"
                );
            });
            let client = Arc::new(
                LsClient::new_for_local_mock(
                    format!("http://{addr}"),
                    mock_readonly(addr.port()).instance,
                    "tok".into(),
                )
                .unwrap(),
            );
            let mut s = ConversationSession::new_owned(
                client,
                id.clone(),
                PathBuf::from("."),
                "default-model".into(),
                "MODEL_TEST_1".into(),
            );
            s.resubscribe(1, PageBounds::default_initial())
                .await
                .unwrap();
            s.consume_next_frame(Duration::from_secs(1)).await.unwrap();
            let req = InteractionSubmitRequest {
                conversation_id: id,
                trajectory_id: "t".into(),
                step_index: 0,
                kind: "permission".into(),
                decision: crate::InteractionDecision::Permission {
                    allow: true,
                    scope: ls_wire::PermissionScope::Once,
                },
            };
            let result = s.respond_interaction(&req).await;
            assert_eq!(result.is_err(), lose_reply);
            s.resubscribe(2, PageBounds::default_initial())
                .await
                .unwrap();
            s.consume_next_frame(Duration::from_secs(1)).await.unwrap();
            assert_eq!(
                s.respond_interaction(&req).await.unwrap_err().reason_code,
                "INTERACTION_ALREADY_SUBMITTED"
            );
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn send_waits_for_baseline_and_rechecks_busy_before_rpc() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            assert!(read_http_req(&mut sock).contains("StreamAgentStateUpdates"));
            let body=encode_connect_frame(&json!({"update":{"mainTrajectoryUpdate":{"stepsUpdate":{"totalLength":1,"indices":[0],"steps":[{"userInput":{"text":"already running"}}]}}}})).unwrap();
            sock.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/connect+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).unwrap();
            sock.write_all(&body).unwrap();
        });
        let client = Arc::new(
            LsClient::new_for_local_mock(
                format!("http://{addr}"),
                mock_readonly(addr.port()).instance,
                "tok".into(),
            )
            .unwrap(),
        );
        let mut s = ConversationSession::new_owned(
            client,
            Uuid::new_v4().to_string(),
            PathBuf::from("."),
            "default-model".into(),
            "MODEL_TEST_1".into(),
        );
        s.resubscribe(1, PageBounds::default_initial())
            .await
            .unwrap();
        assert_eq!(s.execution_status(), SessionExecutionStatus::Unknown);
        assert_eq!(
            s.send_message("do not send", None, &mock_catalog(), None, None)
                .await
                .unwrap_err()
                .reason_code,
            "SESSION_NOT_IDLE"
        );
        assert_eq!(s.state.frames, 1);
        server.join().unwrap();
    }
}
