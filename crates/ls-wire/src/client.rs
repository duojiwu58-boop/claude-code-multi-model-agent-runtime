//! Pinned-TLS Connect JSON unary and streaming client for official
//! `language_server` instances.

use crate::{
    AuthSummary, ConnectFrameDecoder, DiscoveredEndpoint, FrameError, InstanceSummary,
    InteractionReply, LS_SERVICE_NAME, MAX_UNARY_RESPONSE_BYTES, ModelCatalog, PageUpdateRequest,
    ReadonlyCheckReport, SecurityBoundaryReport, SendMessageOptions, StartCascadeRequest,
    StreamSubscribeRequest, TransportErrorCategory, WireError, encode_connect_frame, redact_value,
};
use futures_util::{Stream, StreamExt};
use reqwest::{Certificate, Client, Url};
use serde_json::{Value, json};
use std::{collections::VecDeque, path::Path, pin::Pin, time::Duration};

pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
pub const DEFAULT_HEALTH_TIMEOUT: Duration = Duration::from_secs(8);
pub const DEFAULT_UNARY_TIMEOUT: Duration = Duration::from_secs(20);
pub const DEFAULT_STREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Pinned-TLS Connect JSON client bound to a verified attached endpoint.
pub struct LsClient {
    client: Client,
    base_url: String,
    endpoint: InstanceSummary,
    csrf_token: String,
    is_https: bool,
}

impl std::fmt::Debug for LsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LsClient")
            .field("base_url", &self.base_url)
            .field("endpoint", &self.endpoint)
            .field("csrf_token", &"[REDACTED]")
            .finish()
    }
}

impl LsClient {
    /// Builds a strict pinned-TLS client trusting only the extracted official
    /// public certificate from `app.asar`. Disables system roots, proxies,
    /// redirects, and automatic retries.
    pub fn from_endpoint_and_cert_pem(
        endpoint: DiscoveredEndpoint,
        pinned_cert_pem: &[u8],
    ) -> Result<Self, WireError> {
        let cert = Certificate::from_pem(pinned_cert_pem).map_err(|err| {
            WireError::not_sent(
                "INVALID_PINNED_CERT",
                format!("failed to parse pinned PEM certificate: {err}"),
            )
        })?;
        let client = Client::builder()
            .no_proxy()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .tls_certs_only(std::iter::empty::<Certificate>())
            .tls_certs_only([cert])
            .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
            .build()
            .map_err(|err| {
                WireError::not_sent(
                    "TLS_CLIENT_BUILD_FAILED",
                    format!("failed to construct pinned-TLS HTTP client: {err}"),
                )
            })?;
        let base_url = format!("https://127.0.0.1:{}", endpoint.https_port);
        let summary = endpoint.summary();
        let csrf_token = endpoint.csrf_token().to_owned();
        Ok(Self {
            client,
            base_url,
            endpoint: summary,
            csrf_token,
            is_https: true,
        })
    }

    /// Constructs a loopback test client targeting a local mock HTTP server.
    /// Still disables proxies, redirects, and retries.
    pub fn new_for_local_mock(
        base_url: String,
        endpoint: InstanceSummary,
        csrf_token: String,
    ) -> Result<Self, WireError> {
        let is_https = base_url.starts_with("https://");
        let client = Client::builder()
            .no_proxy()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
            .build()
            .map_err(|err| {
                WireError::not_sent(
                    "MOCK_CLIENT_BUILD_FAILED",
                    format!("failed to build local mock client: {err}"),
                )
            })?;
        Ok(Self {
            client,
            base_url,
            endpoint,
            csrf_token,
            is_https,
        })
    }

    pub fn instance(&self) -> &InstanceSummary {
        &self.endpoint
    }

    fn method_url(&self, method: &str) -> String {
        format!("{}/{LS_SERVICE_NAME}/{method}", self.base_url)
    }

    fn authorized_post(&self, method: &str) -> reqwest::RequestBuilder {
        self.client
            .post(self.method_url(method))
            .header("Connect-Protocol-Version", "1")
            .header("x-codeium-csrf-token", &self.csrf_token)
    }

    /// Executes a single Connect JSON unary RPC with strict response-size bounds,
    /// zero retries, and mandatory three-way error classification.
    pub async fn unary_call(
        &self,
        method: &str,
        body: &Value,
        timeout: Duration,
    ) -> Result<Value, WireError> {
        let response = self
            .authorized_post(method)
            .json(body)
            .timeout(timeout)
            .send()
            .await
            .map_err(|err| classify_reqwest_send_error(method, &err))?;

        let status = response.status();
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk_res) = stream.next().await {
            let chunk = chunk_res.map_err(|err| {
                WireError::outcome_unknown(
                    "RESPONSE_STREAM_FAILED",
                    Some(status.as_u16()),
                    format!("RPC {method} response body interrupted: {err}"),
                )
            })?;
            if bytes.len().saturating_add(chunk.len()) > MAX_UNARY_RESPONSE_BYTES {
                return Err(WireError::outcome_unknown(
                    "RESPONSE_EXCEEDS_BOUND",
                    Some(status.as_u16()),
                    format!("RPC {method} response exceeded {MAX_UNARY_RESPONSE_BYTES} byte limit"),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }

        if status.is_success() {
            serde_json::from_slice(&bytes).map_err(|err| {
                WireError::outcome_unknown(
                    "INVALID_RESPONSE_JSON",
                    Some(status.as_u16()),
                    format!("RPC {method} returned invalid JSON: {err}"),
                )
            })
        } else {
            let parsed = serde_json::from_slice::<Value>(&bytes)
                .map(redact_value)
                .ok();
            let server_code = parsed
                .as_ref()
                .and_then(|v| v.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("http_error")
                .to_owned();
            let server_msg = parsed
                .as_ref()
                .and_then(|v| v.get("message"))
                .and_then(Value::as_str)
                .map(str::to_owned);

            let category =
                if status.is_client_error() && status != reqwest::StatusCode::REQUEST_TIMEOUT {
                    TransportErrorCategory::Rejected
                } else {
                    TransportErrorCategory::OutcomeUnknown
                };
            let message = match server_msg {
                Some(m) if !m.is_empty() => {
                    format!(
                        "RPC {method} failed (HTTP {}, code={server_code}): {m}",
                        status.as_u16()
                    )
                }
                _ => format!(
                    "RPC {method} failed (HTTP {}, code={server_code})",
                    status.as_u16()
                ),
            };
            Err(WireError {
                category,
                code: server_code,
                http_status: Some(status.as_u16()),
                message,
                detail: parsed,
            })
        }
    }

    /// Verifies that an untrusted TLS client (no trust anchor) is rejected and
    /// that missing or invalid CSRF headers return HTTP 401.
    pub async fn verify_security_boundaries(&self) -> Result<SecurityBoundaryReport, WireError> {
        let url = self.method_url("GetCapabilities");
        let untrusted_tls_rejected = if self.is_https {
            let untrusted = Client::builder()
                .no_proxy()
                .retry(reqwest::retry::never())
                .redirect(reqwest::redirect::Policy::none())
                .tls_certs_only(std::iter::empty::<Certificate>())
                .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
                .build()
                .map_err(|err| {
                    WireError::not_sent(
                        "UNTRUSTED_CLIENT_BUILD_FAILED",
                        format!("failed to build untrusted TLS probe client: {err}"),
                    )
                })?;
            untrusted
                .post(&url)
                .json(&json!({}))
                .timeout(DEFAULT_HEALTH_TIMEOUT)
                .send()
                .await
                .is_err()
        } else {
            false
        };
        if self.is_https && !untrusted_tls_rejected {
            return Err(WireError::rejected(
                "TLS_TRUST_ANCHOR_NOT_ENFORCED",
                None,
                "TLS handshake succeeded without trust anchor",
            ));
        }

        let missing_resp = self
            .client
            .post(&url)
            .header("Connect-Protocol-Version", "1")
            .json(&json!({}))
            .timeout(DEFAULT_HEALTH_TIMEOUT)
            .send()
            .await
            .map_err(|err| classify_reqwest_send_error("GetCapabilities(missing-csrf)", &err))?;
        let missing_csrf_status = missing_resp.status().as_u16();
        if missing_csrf_status != 401 {
            return Err(WireError::rejected(
                "MISSING_CSRF_NOT_401",
                Some(missing_csrf_status),
                format!("missing CSRF header returned HTTP {missing_csrf_status}, expected 401"),
            ));
        }

        let wrong_resp = self
            .client
            .post(&url)
            .header("Connect-Protocol-Version", "1")
            .header("x-codeium-csrf-token", "invalid-agcn-csrf-token")
            .json(&json!({}))
            .timeout(DEFAULT_HEALTH_TIMEOUT)
            .send()
            .await
            .map_err(|err| classify_reqwest_send_error("GetCapabilities(wrong-csrf)", &err))?;
        let wrong_csrf_status = wrong_resp.status().as_u16();
        if wrong_csrf_status != 401 {
            return Err(WireError::rejected(
                "WRONG_CSRF_NOT_401",
                Some(wrong_csrf_status),
                format!("wrong CSRF header returned HTTP {wrong_csrf_status}, expected 401"),
            ));
        }

        Ok(SecurityBoundaryReport {
            untrusted_tls_rejected,
            missing_csrf_status,
            wrong_csrf_status,
        })
    }

    pub async fn get_capabilities(&self) -> Result<Value, WireError> {
        self.unary_call("GetCapabilities", &json!({}), DEFAULT_HEALTH_TIMEOUT)
            .await
    }

    pub async fn get_auth_status(&self) -> Result<Value, WireError> {
        self.unary_call("GetAuthStatus", &json!({}), DEFAULT_HEALTH_TIMEOUT)
            .await
    }

    pub async fn has_auth_token(&self) -> Result<Value, WireError> {
        self.unary_call("HasAuthToken", &json!({}), DEFAULT_HEALTH_TIMEOUT)
            .await
    }

    pub async fn get_available_models(&self) -> Result<ModelCatalog, WireError> {
        let value = self
            .unary_call("GetAvailableModels", &json!({}), DEFAULT_UNARY_TIMEOUT)
            .await?;
        ModelCatalog::from_rpc_response(&value).ok_or_else(|| {
            WireError::outcome_unknown(
                "INVALID_MODELS_CATALOG",
                Some(200),
                "GetAvailableModels response missing defaultAgentModelId or models map",
            )
        })
    }

    /// Performs the read-only health, auth, capability, and model catalog check.
    pub async fn check_readonly(
        &self,
        verify_security_boundaries: bool,
    ) -> Result<(ReadonlyCheckReport, ModelCatalog), WireError> {
        let security_checks = if verify_security_boundaries {
            Some(self.verify_security_boundaries().await?)
        } else {
            None
        };
        let capabilities = self.get_capabilities().await?;
        let auth_status = self.get_auth_status().await?;
        let has_auth_token = self.has_auth_token().await?;
        let models = self.get_available_models().await.map_err(|mut err| {
            // Boolean only: distinguish missing user auth from model bootstrap
            // failures without exposing account information or credentials.
            err.message.push_str(&format!("; hasToken={:?}", has_auth_token.get("hasToken").and_then(Value::as_bool)));
            err
        })?;

        let mut capabilities_keys: Vec<String> = capabilities
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        capabilities_keys.sort();

        let report = ReadonlyCheckReport {
            instance: self.endpoint.clone(),
            capabilities_keys,
            auth: AuthSummary::from_responses(&auth_status, &has_auth_token),
            default_agent_model_id: models.default_agent_model_id.clone(),
            model_count: models.models.len(),
            security_checks,
        };
        Ok((report, models))
    }

    pub async fn start_cascade(
        &self,
        cascade_id: &str,
        workspace_path: &Path,
    ) -> Result<Value, WireError> {
        let uri = Url::from_directory_path(workspace_path).map_err(|()| {
            WireError::not_sent(
                "INVALID_WORKSPACE_PATH",
                format!(
                    "workspace path cannot be converted to a file:// URI: {}",
                    workspace_path.display()
                ),
            )
        })?;
        let req = StartCascadeRequest {
            cascade_id: cascade_id.to_owned(),
            workspace_uri: uri.as_str().to_owned(),
        };
        self.unary_call("StartCascade", &req.to_wire_json(), DEFAULT_UNARY_TIMEOUT)
            .await
    }

    /// Official HUB applies the preset as a conversation annotation. Request tool
    /// config alone is overridden by server-resolved conversation preferences.
    pub async fn require_conversation_review(&self, cascade_id: &str) -> Result<Value, WireError> {
        self.set_conversation_permission_preset(
            cascade_id,
            "AGENT_PERMISSION_PRESET_REQUEST_REVIEW",
        )
        .await
    }

    pub async fn set_conversation_permission_preset(
        &self,
        cascade_id: &str,
        preset: &str,
    ) -> Result<Value, WireError> {
        if !matches!(
            preset,
            "AGENT_PERMISSION_PRESET_REQUEST_REVIEW"
                | "AGENT_PERMISSION_PRESET_DEFAULT"
                | "AGENT_PERMISSION_PRESET_AUTO"
                | "AGENT_PERMISSION_PRESET_TURBO"
        ) {
            return Err(WireError::not_sent(
                "INVALID_PERMISSION_PRESET",
                "Unknown permission preset",
            ));
        }
        self.unary_call(
            "UpdateConversationAnnotations",
            &json!({
                "cascadeIds": [cascade_id],
                "annotations": {"permissionPreset": preset},
                "mergeAnnotations": true
            }),
            DEFAULT_UNARY_TIMEOUT,
        )
        .await
    }

    pub async fn send_user_message(&self, req: &SendMessageOptions) -> Result<Value, WireError> {
        let mut body = req.to_wire_json();
        let mut saved_uris = Vec::new();
        let result = async {
            if let Some(media) = body.get_mut("media").and_then(Value::as_array_mut) {
                for item in media {
                    if item["mimeType"] != "application/pdf" {
                        continue;
                    }
                    // The HTTP boundary supplies validated original bytes. Never
                    // accept a caller path as a request to copy an arbitrary file.
                    if item.get("uri").is_some() || item["inlineData"].as_str().is_none_or(str::is_empty) {
                        return Err(WireError::not_sent("INVALID_PDF_MEDIA", "PDF requires inline bytes without a caller URI"));
                    }
                    let saved = self.unary_call(
                        "SaveMediaAsArtifact", &json!({"media":item}), DEFAULT_UNARY_TIMEOUT,
                    ).await.map_err(|err| WireError::not_sent(
                        "PDF_UPLOAD_FAILED",
                        format!("PDF artifact save was not confirmed; message not sent; no automatic retry (an artifact may remain): {err}"),
                    ))?;
                    let uri = saved["uri"].as_str().filter(|s| !s.trim().is_empty())
                        .ok_or_else(|| WireError::not_sent("PDF_UPLOAD_MISSING_URI", "PDF artifact response has no URI; message not sent"))?;
                    saved_uris.push(uri.to_owned());
                    item["uri"] = json!(uri);
                    // Match the official frontend: local artifacts carry a URI;
                    // remote artifacts retain their inline payload.
                    if !uri.starts_with("remote-artifact://") {
                        item["inlineData"] = json!("");
                    }
                }
            }
        // Opt-in local acceptance evidence. Never writes transport credentials.
        if let Some(path) = std::env::var_os("LS_HOST_AUDIT_FILE") {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)
                .map_err(|_| WireError::not_sent("AUDIT_FAILED", "Cannot open configured audit file"))?;
            writeln!(file,"{}",body)
                .map_err(|_| WireError::not_sent("AUDIT_FAILED", "Cannot write configured audit evidence"))?;
        }
        self.unary_call(
            "SendUserCascadeMessage",
            &body,
            DEFAULT_UNARY_TIMEOUT,
        )
        .await
        }.await;
        // Successful/uncertain sends may still need the artifact for execution
        // and conversation history. Only clean up when no message was accepted.
        if result
            .as_ref()
            .is_err_and(|e| e.category != TransportErrorCategory::OutcomeUnknown)
        {
            for uri in saved_uris {
                let _ = self
                    .unary_call(
                        "DeleteMediaArtifact",
                        &json!({"uri":uri}),
                        DEFAULT_UNARY_TIMEOUT,
                    )
                    .await;
            }
        }
        result
    }

    pub async fn request_page_update(&self, req: &PageUpdateRequest) -> Result<Value, WireError> {
        self.unary_call(
            "RequestAgentStatePageUpdate",
            &req.to_wire_json(),
            DEFAULT_UNARY_TIMEOUT,
        )
        .await
    }

    pub async fn get_cascade_trajectory(&self, cascade_id: &str) -> Result<Value, WireError> {
        self.unary_call(
            "GetCascadeTrajectory",
            &json!({ "cascadeId": cascade_id }),
            DEFAULT_UNARY_TIMEOUT,
        )
        .await
    }

    pub async fn get_trajectory_file_diffs(
        &self,
        conversation_id: &str,
    ) -> Result<Value, WireError> {
        self.unary_call(
            "GetTrajectoryFileDiffs",
            &json!({ "conversationId": conversation_id }),
            DEFAULT_UNARY_TIMEOUT,
        )
        .await
    }

    pub async fn handle_user_interaction(
        &self,
        cascade_id: &str,
        trajectory_id: &str,
        step_index: u64,
        reply: &InteractionReply,
    ) -> Result<Value, WireError> {
        let body = reply.to_wire_json(cascade_id, trajectory_id, step_index);
        self.unary_call("HandleCascadeUserInteraction", &body, DEFAULT_UNARY_TIMEOUT)
            .await
    }

    pub async fn cancel_cascade_invocation(
        &self,
        cascade_id: &str,
        kill_background_tasks: bool,
    ) -> Result<Value, WireError> {
        self.cancel_cascade_invocation_with_parent(cascade_id, kill_background_tasks, false)
            .await
    }

    pub async fn cancel_cascade_invocation_with_parent(
        &self,
        cascade_id: &str,
        kill_background_tasks: bool,
        notify_parent: bool,
    ) -> Result<Value, WireError> {
        self.unary_call(
            "CancelCascadeInvocation",
            &json!({
                "cascadeId": cascade_id,
                "killBackgroundTasks": kill_background_tasks,
                "notifyParent": notify_parent,
            }),
            DEFAULT_UNARY_TIMEOUT,
        )
        .await
    }

    pub async fn force_stop_cascade_tree(&self, conversation_id: &str) -> Result<Value, WireError> {
        self.unary_call(
            "ForceStopCascadeTree",
            &json!({ "conversationId": conversation_id }),
            DEFAULT_UNARY_TIMEOUT,
        )
        .await
    }

    /// Opens a `StreamAgentStateUpdates` Connect JSON streaming subscription.
    pub async fn open_state_stream(
        &self,
        req: &StreamSubscribeRequest,
    ) -> Result<AgentStateStream, WireError> {
        let framed = encode_connect_frame(&req.to_wire_json()).map_err(|err| {
            WireError::not_sent(
                "STREAM_FRAME_ENCODE_FAILED",
                format!("failed to encode StreamAgentStateUpdates request: {err}"),
            )
        })?;

        let response = tokio::time::timeout(
            DEFAULT_STREAM_CONNECT_TIMEOUT,
            self.authorized_post("StreamAgentStateUpdates")
                .header("Content-Type", "application/connect+json")
                .body(framed)
                .send(),
        )
        .await
        .map_err(|_| {
            WireError::outcome_unknown(
                "STREAM_CONNECT_TIMEOUT",
                None,
                "StreamAgentStateUpdates timed out while connecting",
            )
        })?
        .map_err(|err| classify_reqwest_send_error("StreamAgentStateUpdates", &err))?;

        let status = response.status();
        if !status.is_success() {
            let category =
                if status.is_client_error() && status != reqwest::StatusCode::REQUEST_TIMEOUT {
                    TransportErrorCategory::Rejected
                } else {
                    TransportErrorCategory::OutcomeUnknown
                };
            return Err(WireError {
                category,
                code: format!("HTTP_{}", status.as_u16()),
                http_status: Some(status.as_u16()),
                message: format!(
                    "StreamAgentStateUpdates failed with HTTP {}",
                    status.as_u16()
                ),
                detail: None,
            });
        }

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !content_type.starts_with("application/connect+json") {
            return Err(WireError::outcome_unknown(
                "UNEXPECTED_STREAM_CONTENT_TYPE",
                Some(status.as_u16()),
                format!("unexpected StreamAgentStateUpdates content-type: {content_type}"),
            ));
        }

        Ok(AgentStateStream {
            conversation_id: req.conversation_id.clone(),
            subscriber_id: req.subscriber_id.clone(),
            decoder: ConnectFrameDecoder::default(),
            stream: response.bytes_stream().into_vec_stream(),
            buffered: VecDeque::new(),
            pending_error: None,
            eof: false,
        })
    }
}

fn classify_reqwest_send_error(method: &str, err: &reqwest::Error) -> WireError {
    if err.is_builder() || err.is_connect() {
        WireError::not_sent(
            "TRANSPORT_CONNECT_FAILED",
            format!("RPC {method} could not connect before sending: {err}"),
        )
    } else if err.is_timeout() {
        WireError::outcome_unknown(
            "TRANSPORT_TIMEOUT",
            None,
            format!("RPC {method} timed out; server outcome is unknown: {err}"),
        )
    } else {
        WireError::outcome_unknown(
            "TRANSPORT_ERROR",
            err.status().map(|s| s.as_u16()),
            format!("RPC {method} transport failed; server outcome is unknown: {err}"),
        )
    }
}

/// Active `StreamAgentStateUpdates` Connect JSON stream bound to a specific
/// `(conversation_id, subscriber_id)`.
///
/// Delivers any valid frames decoded before a terminal error frame before
/// returning that error. Dropping the stream cancels the client subscription
/// without canceling engine work.
pub struct AgentStateStream {
    conversation_id: String,
    subscriber_id: String,
    decoder: ConnectFrameDecoder,
    stream: Pin<Box<dyn Stream<Item = Result<Vec<u8>, reqwest::Error>> + Send>>,
    buffered: VecDeque<Value>,
    pending_error: Option<WireError>,
    eof: bool,
}

impl AgentStateStream {
    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    pub fn subscriber_id(&self) -> &str {
        &self.subscriber_id
    }

    /// Reads the next decoded JSON message from the stream.
    ///
    /// Returns `Ok(None)` only after a clean Connect end frame (`flags = 2` with no error).
    pub async fn next_message(&mut self) -> Result<Option<Value>, WireError> {
        loop {
            if let Some(msg) = self.buffered.pop_front() {
                return Ok(Some(msg));
            }
            if let Some(err) = self.pending_error.clone() {
                return Err(err);
            }
            if self.eof {
                return Ok(None);
            }

            match self.stream.next().await {
                Some(Ok(chunk)) => match self.decoder.push(&chunk) {
                    Ok(messages) => {
                        self.buffered.extend(messages);
                    }
                    Err(failure) => {
                        self.buffered.extend(failure.messages);
                        self.pending_error = Some(frame_error_to_wire(failure.error));
                    }
                },
                Some(Err(err)) => {
                    let wire = WireError::outcome_unknown(
                        "STREAM_TRANSPORT_ERROR",
                        err.status().map(|s| s.as_u16()),
                        format!("StreamAgentStateUpdates transport failed: {err}"),
                    );
                    self.pending_error = Some(wire.clone());
                    return Err(wire);
                }
                None => {
                    self.eof = true;
                    if let Err(err) = self.decoder.finish() {
                        let wire = frame_error_to_wire(err);
                        self.pending_error = Some(wire.clone());
                        return Err(wire);
                    }
                    return Ok(None);
                }
            }
        }
    }
}

fn frame_error_to_wire(err: FrameError) -> WireError {
    let message = crate::redact_value(Value::String(err.to_string()))
        .as_str()
        .unwrap_or("[REDACTED]")
        .to_owned();
    match &err {
        FrameError::EndStreamError { code, .. }
            if matches!(
                code.as_str(),
                "invalid_argument" | "permission_denied" | "unauthenticated" | "not_found"
            ) =>
        {
            WireError::rejected(code.clone(), None, message.clone())
        }
        _ => WireError::outcome_unknown("STREAM_FRAME_ERROR", None, message),
    }
}

trait BytesStreamExt {
    fn into_vec_stream(self)
    -> Pin<Box<dyn Stream<Item = Result<Vec<u8>, reqwest::Error>> + Send>>;
}

impl<S, B> BytesStreamExt for S
where
    S: Stream<Item = Result<B, reqwest::Error>> + Send + 'static,
    B: AsRef<[u8]>,
{
    fn into_vec_stream(
        self,
    ) -> Pin<Box<dyn Stream<Item = Result<Vec<u8>, reqwest::Error>> + Send>> {
        Box::pin(self.map(|item| item.map(|b| b.as_ref().to_vec())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PageBounds, TARGET_LS_VERSION};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        time::Instant,
    };

    fn test_summary(port: u16) -> InstanceSummary {
        InstanceSummary {
            pid: 4242,
            https_port: port,
            ls_version: TARGET_LS_VERSION.to_owned(),
        }
    }

    fn read_http_request(socket: &mut std::net::TcpStream) -> Vec<u8> {
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut received = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let n = socket.read(&mut buf).unwrap();
            assert!(n > 0, "request ended before body");
            received.extend_from_slice(&buf[..n]);
            if let Some(at) = received.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&received[..at]).to_ascii_lowercase();
                let length: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .unwrap_or("0")
                    .trim()
                    .parse()
                    .unwrap();
                if received.len() >= at + 4 + length {
                    break;
                }
            }
        }
        received
    }

    #[tokio::test]
    async fn pdf_artifact_transport_preserves_bytes_and_handles_failures_without_resending() {
        for case in [
            "local",
            "remote",
            "second_save_fails",
            "missing_uri",
            "send_unknown",
            "send_rejected",
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let uri = if case == "remote" {
                "remote-artifact://test/document.pdf"
            } else {
                "file:///test/document.pdf"
            };
            let server = std::thread::spawn(move || {
                let sequence = match case {
                    "second_save_fails" => vec![
                        ("SaveMediaAsArtifact", 200),
                        ("SaveMediaAsArtifact", 400),
                        ("DeleteMediaArtifact", 200),
                    ],
                    "missing_uri" => vec![("SaveMediaAsArtifact", 200)],
                    "send_rejected" => vec![
                        ("SaveMediaAsArtifact", 200),
                        ("SendUserCascadeMessage", 400),
                        ("DeleteMediaArtifact", 200),
                    ],
                    _ => vec![
                        ("SaveMediaAsArtifact", 200),
                        ("SendUserCascadeMessage", 200),
                    ],
                };
                for (method, status) in sequence {
                    let (mut socket, _) = listener.accept().unwrap();
                    let raw = read_http_request(&mut socket);
                    let at = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
                    assert!(String::from_utf8_lossy(&raw[..at]).contains(method));
                    let body: Value = serde_json::from_slice(&raw[at + 4..]).unwrap();
                    match method {
                        "SaveMediaAsArtifact" => {
                            assert_eq!(body["media"]["inlineData"], "JVBERi0xLjcK");
                            assert_eq!(body["media"]["displayName"], "sample.pdf");
                            assert!(body["media"].get("uri").is_none());
                        }
                        "SendUserCascadeMessage" => {
                            assert_eq!(body["items"], json!([{"text":"exact user text"}]));
                            assert_eq!(body["media"][0]["uri"], uri);
                            assert_eq!(
                                body["media"][0]["inlineData"],
                                if case == "remote" { "JVBERi0xLjcK" } else { "" }
                            );
                            assert_eq!(body["media"][1]["mimeType"], "image/png");
                            if case == "send_unknown" {
                                drop(socket);
                                continue;
                            }
                        }
                        "DeleteMediaArtifact" => assert_eq!(body["uri"], uri),
                        _ => unreachable!(),
                    }
                    let response = if status == 400 {
                        json!({"code":"invalid_argument","message":"fixture rejection"})
                    } else if method == "SaveMediaAsArtifact" && case != "missing_uri" {
                        json!({"uri":uri})
                    } else {
                        json!({"success":true})
                    }
                    .to_string();
                    write!(socket, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
                }
                listener.set_nonblocking(true).unwrap();
                let until = Instant::now() + Duration::from_millis(150);
                while Instant::now() < until {
                    assert!(
                        matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
                        "unexpected retry/cleanup in {case}"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
            });
            let client = LsClient::new_for_local_mock(
                format!("http://{addr}"),
                test_summary(addr.port()),
                "test-csrf".into(),
            )
            .unwrap();
            let pdf = json!({"mimeType":"application/pdf","inlineData":"JVBERi0xLjcK","description":"sample.pdf","displayName":"sample.pdf"});
            let mut media = vec![
                pdf.clone(),
                json!({"mimeType":"image/png","inlineData":"image fixture"}),
            ];
            if case == "second_save_fails" {
                media.push(pdf);
            }
            let req = SendMessageOptions {
                cascade_id: "test".into(),
                text: "exact user text".into(),
                text_items: None,
                images: media,
                plan_model: "model".into(),
                client_request_id: "test".into(),
                client_message_id: None,
                tool_config: None,
            };
            let result = client.send_user_message(&req).await;
            match case {
                "local" | "remote" => assert!(result.is_ok(), "{result:?}"),
                "send_unknown" => assert_eq!(
                    result.unwrap_err().category,
                    TransportErrorCategory::OutcomeUnknown
                ),
                "send_rejected" => assert_eq!(
                    result.unwrap_err().category,
                    TransportErrorCategory::Rejected
                ),
                _ => assert_eq!(
                    result.unwrap_err().category,
                    TransportErrorCategory::NotSent
                ),
            }
            assert!(
                req.images[0].get("uri").is_none(),
                "do not mutate original history"
            );
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn lost_response_after_complete_request_returns_outcome_unknown_without_retry() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let _ = read_http_request(&mut socket);
            drop(socket); // Drop response completely after accepting request body.
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_millis(350);
            let mut extra = 0usize;
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok(_) => extra += 1,
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) => panic!("{err}"),
                }
            }
            extra
        });

        let client = LsClient::new_for_local_mock(
            format!("http://{addr}"),
            test_summary(addr.port()),
            "test-csrf".into(),
        )
        .unwrap();
        let err = client
            .unary_call(
                "SendUserCascadeMessage",
                &json!({"text":"FAULT"}),
                Duration::from_secs(5),
            )
            .await
            .unwrap_err();
        assert_eq!(err.category, TransportErrorCategory::OutcomeUnknown);
        assert_eq!(server.join().unwrap(), 0, "must never retry after send");
    }

    #[tokio::test]
    async fn classifies_4xx_as_rejected_5xx_as_unknown_and_connect_refused_as_not_sent() {
        // 1. Connect refused -> NotSent
        let closed_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_addr = closed_listener.local_addr().unwrap();
        drop(closed_listener);
        let dead_client = LsClient::new_for_local_mock(
            format!("http://{dead_addr}"),
            test_summary(dead_addr.port()),
            "test-csrf".into(),
        )
        .unwrap();
        let err = dead_client
            .unary_call("GetCapabilities", &json!({}), Duration::from_secs(6))
            .await
            .unwrap_err();
        assert_eq!(err.category, TransportErrorCategory::NotSent);

        // 2. HTTP 401 -> Rejected (and redacts token in error detail)
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for (status, body) in [
                (
                    "401 Unauthorized",
                    r#"{"code":"unauthenticated","message":"bad auth","csrfToken":"leak-me"}"#,
                ),
                (
                    "503 Service Unavailable",
                    r#"{"code":"unavailable","message":"busy"}"#,
                ),
            ] {
                let (mut socket, _) = listener.accept().unwrap();
                let _ = read_http_request(&mut socket);
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(resp.as_bytes()).unwrap();
            }
        });

        let client = LsClient::new_for_local_mock(
            format!("http://{addr}"),
            test_summary(addr.port()),
            "test-csrf".into(),
        )
        .unwrap();

        let err_401 = client
            .unary_call("GetCapabilities", &json!({}), Duration::from_secs(3))
            .await
            .unwrap_err();
        assert_eq!(err_401.category, TransportErrorCategory::Rejected);
        assert_eq!(err_401.http_status, Some(401));
        assert_eq!(err_401.detail.unwrap()["csrfToken"], "[REDACTED]");

        let err_503 = client
            .unary_call("SendUserCascadeMessage", &json!({}), Duration::from_secs(3))
            .await
            .unwrap_err();
        assert_eq!(err_503.category, TransportErrorCategory::OutcomeUnknown);
        assert_eq!(err_503.http_status, Some(503));

        server.join().unwrap();
    }

    #[tokio::test]
    async fn state_stream_delivers_preceding_messages_before_terminal_end_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let _ = read_http_request(&mut socket);
            let mut body =
                encode_connect_frame(&json!({"update":{"conversationId":"c1","fullyIdle":false}}))
                    .unwrap();
            let mut end_err = encode_connect_frame(
                &json!({"error":{"code":"canceled","message":"stream ended"}}),
            )
            .unwrap();
            end_err[0] = 2;
            body.extend_from_slice(&end_err);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/connect+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(header.as_bytes()).unwrap();
            socket.write_all(&body).unwrap();
        });

        let client = LsClient::new_for_local_mock(
            format!("http://{addr}"),
            test_summary(addr.port()),
            "test-csrf".into(),
        )
        .unwrap();
        let mut stream = client
            .open_state_stream(&StreamSubscribeRequest {
                conversation_id: "c1".into(),
                subscriber_id: "sub-1".into(),
                initial_steps_page_bounds: PageBounds::default_initial(),
            })
            .await
            .unwrap();

        let first = stream.next_message().await.unwrap().unwrap();
        assert_eq!(first["update"]["conversationId"], "c1");
        let terminal = stream.next_message().await.unwrap_err();
        assert_eq!(terminal.category, TransportErrorCategory::OutcomeUnknown);
        assert!(terminal.message.contains("canceled"));
        server.join().unwrap();
    }
}
