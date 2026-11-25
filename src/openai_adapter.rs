//! OpenAI-compatible LLM adapter foundation with injectable mock HTTP transport,
//! strict capability/model binding, redacted credentials, bounded payloads,
//! cooperative/async cancellation, and single-use CostConsent/BudgetReservation accounting (P2-03C).
//!
//! # Security Boundaries
//! - **Zero actual network calls in P2-03C:** `NoNetworkTransport` fails closed and
//!   `MockHttpTransport` provides deterministic scripted responses in-memory without touching sockets.
//! - **No hardcoded credentials or vendor model names:** Endpoints, credentials, headers,
//!   and model identifiers come solely from caller configuration and verified `ModelRecord` /
//!   `ModelSnapshot` metadata. Credentials are wrapped in `RedactedSecret` so neither `Debug`,
//!   `Display`, nor recorded transport telemetry ever prints secret tokens.
//! - **Single-use dispatch & conservative settlement:** Paid/remote calls consume an atomic
//!   single-use `Store::claim_reservation_dispatch` permit immediately before HTTP dispatch.
//!   Replayed reservations never authorize dispatch. Pre-dispatch cancellation releases unclaimed
//!   budget cleanly; mid-flight failures/cancellations/missing usage settle conservatively at
//!   `reserved_micros` and halt consent.

use std::{
    collections::{HashSet, VecDeque},
    fmt,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::Notify;

use crate::{
    cost::{
        BudgetReservation, CostScope, ProviderUsageReport, ReconcileBudgetRequest,
        ReserveBudgetRequest, WorkerLease,
    },
    domain::{ModelRecord, ModelSnapshot},
    provider::{
        FinishReason, LlmRequest, LlmResponse, LlmUsage, MAX_RESPONSE_CONTENT_BYTES,
        MAX_RESPONSE_TOOL_CALLS_BYTES, MAX_TOOL_CALLS_PER_RESPONSE, MAX_TOTAL_REQUEST_BYTES,
        MAX_TOTAL_RESPONSE_BYTES, MessageRole, PROVIDER_SCHEMA_VERSION, ProviderAvailability,
        ProviderCapabilities, ProviderError, ProviderKind, ResponseFormat, TaskRole, ToolCall,
        validate_parameters_against_caps,
    },
    store::{Store, StoreError},
};

pub const MIN_REQUEST_TIMEOUT_MS: u64 = 1;
pub const MAX_REQUEST_TIMEOUT_MS: u64 = 300_000;
pub const MAX_ERROR_EXCERPT_BYTES: usize = 512;

/// Opaque credential wrapper that prevents accidental token leakage in logs, errors, or `Debug`.
#[derive(Clone, Eq, PartialEq)]
pub struct RedactedSecret {
    value: String,
}

impl RedactedSecret {
    pub fn new(secret: impl Into<String>) -> Result<Self, OpenAiAdapterError> {
        let value = secret.into();
        if value.trim().is_empty()
            || value.bytes().any(|b| matches!(b, b'\r' | b'\n' | b'\0'))
            || value.len() > 4096
        {
            return Err(OpenAiAdapterError::InvalidConfig(
                "credential secret must be non-empty, <= 4096 bytes, and contain no CR/LF/NUL"
                    .into(),
            ));
        }
        Ok(Self { value })
    }

    pub fn len(&self) -> usize {
        self.value.len()
    }

    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    fn as_header_secret(&self) -> &str {
        &self.value
    }
}

impl fmt::Debug for RedactedSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl fmt::Display for RedactedSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// Configurable credential source; stores no hardcoded keys and never serializes secrets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CredentialSource {
    None,
    BearerToken(RedactedSecret),
    EnvVar { var_name: String },
}

impl CredentialSource {
    pub fn env_var(name: impl Into<String>) -> Result<Self, OpenAiAdapterError> {
        let var_name = name.into();
        if !is_valid_env_var_name(&var_name) {
            return Err(OpenAiAdapterError::InvalidConfig(
                "credential env var name must be ASCII uppercase letters, digits, or '_'".into(),
            ));
        }
        Ok(Self::EnvVar { var_name })
    }

    pub fn resolve_bearer(&self) -> Result<Option<RedactedSecret>, OpenAiAdapterError> {
        match self {
            Self::None => Ok(None),
            Self::BearerToken(secret) => Ok(Some(secret.clone())),
            Self::EnvVar { var_name } => {
                let raw = std::env::var(var_name).map_err(|_| {
                    OpenAiAdapterError::Provider(ProviderError::ProviderUnavailable(format!(
                        "required credential environment variable '{var_name}' is missing"
                    )))
                })?;
                Ok(Some(RedactedSecret::new(raw)?))
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HttpHeaderValue {
    Public(String),
    Secret(RedactedSecret),
}

impl HttpHeaderValue {
    pub fn display_value(&self) -> &str {
        match self {
            Self::Public(v) => v,
            Self::Secret(_) => "[REDACTED]",
        }
    }

    pub fn raw_value(&self) -> &str {
        match self {
            Self::Public(v) => v,
            Self::Secret(s) => s.as_header_secret(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub timeout: Duration,
    pub headers: Vec<(String, HttpHeaderValue)>,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum HttpTransportError {
    #[error("request cancelled")]
    Cancelled,
    #[error("request timed out after {0:?}")]
    TimedOut(Duration),
    #[error("real network transport is disabled in P2-03C")]
    NetworkDisabled,
    #[error("http transport error: {0}")]
    Transport(String),
}

/// Race-free cooperative and async cancellation handle.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    inner: Arc<CancelInner>,
}

#[derive(Debug, Default)]
struct CancelInner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::Release);
        self.inner.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
        let notified = self.inner.notify.notified();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

pub type TransportFuture<'a> =
    Pin<Box<dyn Future<Output = Result<HttpResponse, HttpTransportError>> + Send + 'a>>;

/// Injectable HTTP transport abstraction.
pub trait LlmHttpTransport: Send + Sync {
    fn execute<'a>(
        &'a self,
        request: HttpRequest,
        cancel: &'a CancellationToken,
    ) -> TransportFuture<'a>;
}

/// Default fail-closed transport proving P2-03C emits zero live network traffic.
#[derive(Debug, Default)]
pub struct NoNetworkTransport;

impl LlmHttpTransport for NoNetworkTransport {
    fn execute<'a>(
        &'a self,
        _request: HttpRequest,
        _cancel: &'a CancellationToken,
    ) -> TransportFuture<'a> {
        Box::pin(async move { Err(HttpTransportError::NetworkDisabled) })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordedHttpRequest {
    pub method: String,
    pub url: String,
    pub timeout_ms: u64,
    pub headers: Vec<(String, String)>,
    pub body_bytes: usize,
    pub body_sha256: String,
    pub body_json: serde_json::Value,
}

#[derive(Clone, Debug)]
pub enum MockHttpOutcome {
    Response {
        delay: Duration,
        status: u16,
        body: Vec<u8>,
    },
    Error {
        delay: Duration,
        message: String,
    },
    TimedOut {
        delay: Duration,
    },
}

/// Deterministic in-memory mock transport for P2-03C offline adapter & ledger tests.
#[derive(Debug, Default)]
pub struct MockHttpTransport {
    state: Mutex<MockHttpState>,
}

#[derive(Debug, Default)]
struct MockHttpState {
    outcomes: VecDeque<MockHttpOutcome>,
    recorded: Vec<RecordedHttpRequest>,
}

impl MockHttpTransport {
    pub fn new(outcomes: impl IntoIterator<Item = MockHttpOutcome>) -> Self {
        Self {
            state: Mutex::new(MockHttpState {
                outcomes: outcomes.into_iter().collect(),
                recorded: Vec::new(),
            }),
        }
    }

    pub fn push_json_response(
        &self,
        status: u16,
        payload: &serde_json::Value,
    ) -> Result<(), OpenAiAdapterError> {
        let body = serde_json::to_vec(payload).map_err(|err| {
            OpenAiAdapterError::InvalidConfig(format!("failed serializing mock json: {err}"))
        })?;
        let mut guard = self
            .state
            .lock()
            .map_err(|_| OpenAiAdapterError::Internal("mock transport lock poisoned".into()))?;
        guard.outcomes.push_back(MockHttpOutcome::Response {
            delay: Duration::ZERO,
            status,
            body,
        });
        Ok(())
    }

    pub fn push_outcome(&self, outcome: MockHttpOutcome) -> Result<(), OpenAiAdapterError> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| OpenAiAdapterError::Internal("mock transport lock poisoned".into()))?;
        guard.outcomes.push_back(outcome);
        Ok(())
    }

    pub fn recorded_requests(&self) -> Vec<RecordedHttpRequest> {
        self.state
            .lock()
            .expect("mock transport mutex")
            .recorded
            .clone()
    }
}

impl LlmHttpTransport for MockHttpTransport {
    fn execute<'a>(
        &'a self,
        request: HttpRequest,
        cancel: &'a CancellationToken,
    ) -> TransportFuture<'a> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(HttpTransportError::Cancelled);
            }
            let body_json: serde_json::Value =
                serde_json::from_slice(&request.body).unwrap_or(serde_json::Value::Null);
            let record = RecordedHttpRequest {
                method: request.method.clone(),
                url: request.url.clone(),
                timeout_ms: u64::try_from(request.timeout.as_millis()).unwrap_or(u64::MAX),
                headers: request
                    .headers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.display_value().to_owned()))
                    .collect(),
                body_bytes: request.body.len(),
                body_sha256: sha256_hex(&request.body),
                body_json,
            };
            let outcome = {
                let mut guard = self.state.lock().map_err(|_| {
                    HttpTransportError::Transport("mock transport lock poisoned".into())
                })?;
                guard.recorded.push(record);
                guard.outcomes.pop_front().ok_or_else(|| {
                    HttpTransportError::Transport("no mock HTTP outcome queued".into())
                })?
            };

            let delay = match &outcome {
                MockHttpOutcome::Response { delay, .. }
                | MockHttpOutcome::Error { delay, .. }
                | MockHttpOutcome::TimedOut { delay } => *delay,
            };
            if !delay.is_zero() {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(HttpTransportError::Cancelled),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
            if cancel.is_cancelled() {
                return Err(HttpTransportError::Cancelled);
            }

            match outcome {
                MockHttpOutcome::Response { status, body, .. } => Ok(HttpResponse { status, body }),
                MockHttpOutcome::Error { message, .. } => {
                    Err(HttpTransportError::Transport(message))
                }
                MockHttpOutcome::TimedOut { delay } => Err(HttpTransportError::TimedOut(delay)),
            }
        })
    }
}

#[derive(Debug, Error)]
pub enum OpenAiAdapterError {
    #[error("invalid adapter config: {0}")]
    InvalidConfig(String),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("request cancelled")]
    Cancelled,
    #[error("provider request timed out after {timeout_ms}ms")]
    Timeout { timeout_ms: u64 },
    #[error("provider authentication failed (HTTP {status})")]
    AuthenticationFailed { status: u16 },
    #[error("provider rate limited (HTTP {status}): {message}")]
    RateLimited { status: u16, message: String },
    #[error("provider rejected request (HTTP {status}): {message}")]
    HttpStatus { status: u16, message: String },
    #[error("provider transport failed: {0}")]
    Transport(String),
    #[error("duplicate or unpermitted provider dispatch blocked: {0}")]
    DuplicateDispatchBlocked(String),
    #[error("ledger reconciliation failed after dispatch: {0}")]
    ReconciliationFailed(String),
    #[error("internal adapter error: {0}")]
    Internal(String),
}

impl OpenAiAdapterError {
    pub fn code(&self) -> &str {
        match self {
            Self::InvalidConfig(_) => "invalid_schema",
            Self::Provider(err) => err.code(),
            Self::Store(err) => err.code(),
            Self::Cancelled => "request_cancelled",
            Self::Timeout { .. } => "provider_timeout",
            Self::AuthenticationFailed { .. } => "provider_auth_failed",
            Self::RateLimited { .. } => "provider_rate_limited",
            Self::HttpStatus { .. } => "provider_http_error",
            Self::Transport(_) => "provider_transport_error",
            Self::DuplicateDispatchBlocked(_) | Self::ReconciliationFailed(_) => {
                "cost_reconciliation_failed"
            }
            Self::Internal(_) => "internal_error",
        }
    }
}

#[derive(Clone, Debug)]
pub struct OpenAiCompatibleConfig {
    pub capabilities: ProviderCapabilities,
    pub base_url: String,
    pub credential: CredentialSource,
    pub request_timeout: Duration,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub extra_headers: Vec<(String, String)>,
}

impl OpenAiCompatibleConfig {
    pub fn validate(&self) -> Result<(), OpenAiAdapterError> {
        self.capabilities.validate()?;
        if self.capabilities.provider_kind == ProviderKind::DeterministicScriptedMock {
            return Err(OpenAiAdapterError::InvalidConfig(
                "openai-compatible adapter requires remote_api or local_endpoint provider_kind"
                    .into(),
            ));
        }
        validate_endpoint_url(&self.base_url, self.capabilities.remote)?;

        if self.capabilities.remote && matches!(self.credential, CredentialSource::None) {
            return Err(OpenAiAdapterError::InvalidConfig(
                "remote openai-compatible providers require an explicit credential source".into(),
            ));
        }
        if let CredentialSource::EnvVar { var_name } = &self.credential {
            if !is_valid_env_var_name(var_name) {
                return Err(OpenAiAdapterError::InvalidConfig(
                    "invalid credential environment variable name".into(),
                ));
            }
        }

        let timeout_ms = u64::try_from(self.request_timeout.as_millis()).unwrap_or(u64::MAX);
        if !(MIN_REQUEST_TIMEOUT_MS..=MAX_REQUEST_TIMEOUT_MS).contains(&timeout_ms) {
            return Err(OpenAiAdapterError::InvalidConfig(format!(
                "request_timeout must be between {MIN_REQUEST_TIMEOUT_MS}ms and {MAX_REQUEST_TIMEOUT_MS}ms"
            )));
        }
        if self.max_request_bytes == 0 || self.max_request_bytes > MAX_TOTAL_REQUEST_BYTES {
            return Err(OpenAiAdapterError::InvalidConfig(format!(
                "max_request_bytes must be within 1..={MAX_TOTAL_REQUEST_BYTES}"
            )));
        }
        if self.max_response_bytes == 0 || self.max_response_bytes > MAX_TOTAL_RESPONSE_BYTES {
            return Err(OpenAiAdapterError::InvalidConfig(format!(
                "max_response_bytes must be within 1..={MAX_TOTAL_RESPONSE_BYTES}"
            )));
        }
        for (name, value) in &self.extra_headers {
            if name.trim().is_empty()
                || name.eq_ignore_ascii_case("authorization")
                || name.eq_ignore_ascii_case("content-type")
                || name
                    .bytes()
                    .any(|b| matches!(b, b'\r' | b'\n' | b'\0' | b':'))
                || value.bytes().any(|b| matches!(b, b'\r' | b'\n' | b'\0'))
            {
                return Err(OpenAiAdapterError::InvalidConfig(
                    "invalid or forbidden extra HTTP header".into(),
                ));
            }
        }
        Ok(())
    }

    pub fn chat_completions_url(&self) -> Result<String, OpenAiAdapterError> {
        let normalized = validate_endpoint_url(&self.base_url, self.capabilities.remote)?;
        Ok(format!("{normalized}/chat/completions"))
    }
}

/// Configurable OpenAI-compatible adapter binding model selection, wire translation,
/// bounded payloads, cancellation, and atomic budget reservation dispatch.
pub struct OpenAiCompatibleAdapter<T: LlmHttpTransport> {
    config: OpenAiCompatibleConfig,
    transport: T,
}

/// Shared execution context passed into OpenAI-compatible authorized dispatch methods.
#[derive(Clone, Copy)]
pub struct DispatchContext<'a> {
    pub store: &'a Store,
    pub lease: &'a WorkerLease,
    pub model: &'a ModelRecord,
    pub snapshot: Option<&'a ModelSnapshot>,
    pub cancel: &'a CancellationToken,
}

impl<T: LlmHttpTransport> OpenAiCompatibleAdapter<T> {
    pub fn new(config: OpenAiCompatibleConfig, transport: T) -> Result<Self, OpenAiAdapterError> {
        config.validate()?;
        Ok(Self { config, transport })
    }

    pub fn config(&self) -> &OpenAiCompatibleConfig {
        &self.config
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// Validates that `model`, optional frozen `snapshot`, and `request` all match
    /// this adapter's configured identity and capabilities without fallback.
    pub fn validate_request_binding(
        &self,
        model: &ModelRecord,
        snapshot: Option<&ModelSnapshot>,
        request: &LlmRequest,
    ) -> Result<(), OpenAiAdapterError> {
        self.config.validate()?;
        request.validate()?;
        let caps = &self.config.capabilities;

        if model.schema_version != PROVIDER_SCHEMA_VERSION {
            return Err(ProviderError::InvalidModelSelection(
                "unsupported model record schema_version".into(),
            )
            .into());
        }
        if model.provider_id != caps.provider_id
            || model.model_id != caps.model_id
            || model.adapter_version != caps.adapter_version
        {
            return Err(ProviderError::InvalidModelSelection(format!(
                "configured adapter ({}/{}/{}) does not match ModelRecord ({}/{}/{}); fallback is forbidden",
                caps.provider_id,
                caps.model_id,
                caps.adapter_version,
                model.provider_id,
                model.model_id,
                model.adapter_version
            ))
            .into());
        }
        if request.provider_id != caps.provider_id
            || request.model_id != caps.model_id
            || request.adapter_version != caps.adapter_version
        {
            return Err(ProviderError::InvalidModelSelection(format!(
                "requested model ({}/{}/{}) does not match configured adapter ({}/{}/{}); fallback is forbidden",
                request.provider_id,
                request.model_id,
                request.adapter_version,
                caps.provider_id,
                caps.model_id,
                caps.adapter_version
            ))
            .into());
        }
        if let Some(snap) = snapshot {
            if snap.provider_id != request.provider_id
                || snap.model_id != request.model_id
                || snap.adapter_version != request.adapter_version
                || snap.parameters != request.parameters
            {
                return Err(ProviderError::InvalidModelSelection(
                    "request does not match immutable revision ModelSnapshot; fallback is forbidden"
                        .into(),
                )
                .into());
            }
        }
        if model.remote != caps.remote {
            return Err(ProviderError::InvalidModelSelection(
                "remote flag mismatch between ModelRecord and adapter capabilities".into(),
            )
            .into());
        }
        if !caps.enabled
            || !model.enabled
            || !caps.configured
            || !model.configured
            || caps.availability == ProviderAvailability::Unavailable
            || model.availability == "unavailable"
        {
            return Err(ProviderError::ProviderUnavailable(format!(
                "configured model {}/{} is disabled, unconfigured, or unavailable",
                model.provider_id, model.model_id
            ))
            .into());
        }
        if !caps.supports_role(request.task_role)
            || !model
                .task_roles
                .iter()
                .any(|role| role == request.task_role.as_str())
        {
            return Err(ProviderError::UnsupportedCapability(format!(
                "model {}/{} does not support role '{}'",
                model.provider_id,
                model.model_id,
                request.task_role.as_str()
            ))
            .into());
        }
        if !request.tools.is_empty() && !caps.supports_tool_calling {
            return Err(ProviderError::UnsupportedCapability(
                "configured provider does not support tool calling".into(),
            )
            .into());
        }
        if request.response_format == ResponseFormat::JsonObject && !caps.supports_structured_json {
            return Err(ProviderError::UnsupportedCapability(
                "configured provider does not support structured JSON output".into(),
            )
            .into());
        }
        validate_parameters_against_caps(caps, &request.parameters)?;
        if request.max_output_tokens > caps.max_output_tokens
            || request.max_output_tokens > caps.max_context_tokens
        {
            return Err(ProviderError::UnsupportedParameter(format!(
                "requested max_output_tokens {} exceeds configured capability bounds",
                request.max_output_tokens
            ))
            .into());
        }
        Ok(())
    }

    /// Translates `LlmRequest` to bounded OpenAI `/v1/chat/completions` JSON without dispatch.
    pub fn build_http_request(
        &self,
        request: &LlmRequest,
    ) -> Result<(HttpRequest, String), OpenAiAdapterError> {
        let request_sha256 = request.canonical_request_sha256()?;
        let canonical_bytes = serde_jcs::to_vec(request).map_err(|err| {
            OpenAiAdapterError::Internal(format!("request JCS canonicalization failed: {err}"))
        })?;
        if canonical_bytes.len() > self.config.max_request_bytes {
            return Err(ProviderError::InvalidRequest(format!(
                "canonical request {} bytes exceeds limit {}",
                canonical_bytes.len(),
                self.config.max_request_bytes
            ))
            .into());
        }

        let wire = build_openai_chat_request(request)?;
        let body = serde_json::to_vec(&wire).map_err(|err| {
            OpenAiAdapterError::Internal(format!("wire serialization failed: {err}"))
        })?;
        if body.len() > self.config.max_request_bytes {
            return Err(ProviderError::InvalidRequest(format!(
                "serialized wire request {} bytes exceeds limit {}",
                body.len(),
                self.config.max_request_bytes
            ))
            .into());
        }

        let mut headers = Vec::with_capacity(self.config.extra_headers.len() + 2);
        headers.push((
            "Content-Type".into(),
            HttpHeaderValue::Public("application/json".into()),
        ));
        if let Some(secret) = self.config.credential.resolve_bearer()? {
            let auth_header = RedactedSecret::new(format!("Bearer {}", secret.as_header_secret()))?;
            headers.push(("Authorization".into(), HttpHeaderValue::Secret(auth_header)));
        }
        for (k, v) in &self.config.extra_headers {
            headers.push((k.clone(), HttpHeaderValue::Public(v.clone())));
        }

        let http_req = HttpRequest {
            method: "POST".into(),
            url: self.config.chat_completions_url()?,
            timeout: self.config.request_timeout,
            headers,
            body,
        };
        Ok((http_req, request_sha256))
    }

    /// Executes an already-created `BudgetReservation` by atomically claiming its single-use
    /// dispatch permit BEFORE invoking the transport, then reconciling actual or conservative usage.
    pub async fn execute_for_reservation(
        &self,
        ctx: DispatchContext<'_>,
        reservation_id: &str,
        request: &LlmRequest,
    ) -> Result<(LlmResponse, BudgetReservation), OpenAiAdapterError> {
        if ctx.cancel.is_cancelled() {
            return Err(OpenAiAdapterError::Cancelled);
        }
        self.validate_request_binding(ctx.model, ctx.snapshot, request)?;
        let (http_req, request_sha256) = self.build_http_request(request)?;

        let existing = ctx.store.get_budget_reservation(reservation_id).await?;
        validate_reservation_alignment(&existing, ctx.lease, request)?;

        if ctx.cancel.is_cancelled() {
            return Err(OpenAiAdapterError::Cancelled);
        }

        self.claim_and_execute_reservation(ctx, reservation_id, request, &request_sha256, http_req)
            .await
    }

    /// Atomically reserves budget against an active `CostConsent`, blocks idempotent replays
    /// before claiming single-use dispatch, executes bounded HTTP transport, and reconciles ledger state.
    pub async fn execute_with_new_reservation(
        &self,
        ctx: DispatchContext<'_>,
        reserve_input: ReserveBudgetRequest,
        idempotency_key: Option<&str>,
        request: &LlmRequest,
    ) -> Result<(LlmResponse, BudgetReservation), OpenAiAdapterError> {
        if ctx.cancel.is_cancelled() {
            return Err(OpenAiAdapterError::Cancelled);
        }
        self.validate_request_binding(ctx.model, ctx.snapshot, request)?;
        if reserve_input.provider_id != request.provider_id
            || reserve_input.model_id != request.model_id
        {
            return Err(ProviderError::InvalidModelSelection(
                "reservation model selection must match request without fallback".into(),
            )
            .into());
        }
        if !scope_matches_role(reserve_input.scope, request.task_role) {
            return Err(ProviderError::UnsupportedCapability(format!(
                "cost scope '{}' does not match task role '{}'",
                reserve_input.scope.as_str(),
                request.task_role.as_str()
            ))
            .into());
        }
        if reserve_input.owner_id != ctx.lease.owner_id
            || reserve_input.lease_epoch != ctx.lease.lease_epoch
        {
            return Err(StoreError::Forbidden {
                code: "reservation_ownership_required",
            }
            .into());
        }
        if reserve_input.max_output_tokens < i64::from(request.max_output_tokens) {
            return Err(ProviderError::UnsupportedParameter(
                "reservation max_output_tokens ceiling is smaller than request max_output_tokens"
                    .into(),
            )
            .into());
        }
        let max_total_ctx = u64::from(self.config.capabilities.max_context_tokens);
        let requested_bound = u64::try_from(reserve_input.estimated_input_tokens)
            .ok()
            .and_then(|in_tok| in_tok.checked_add(u64::from(request.max_output_tokens)));
        if requested_bound.is_none_or(|sum| sum > max_total_ctx) {
            return Err(ProviderError::UnsupportedParameter(
                "estimated_input_tokens + max_output_tokens exceeds configured context limit"
                    .into(),
            )
            .into());
        }

        let (http_req, request_sha256) = self.build_http_request(request)?;

        let (reservation, replayed) = ctx
            .store
            .reserve_budget(idempotency_key, reserve_input)
            .await?;
        if replayed || !reservation.dispatch_authorized {
            return Err(OpenAiAdapterError::DuplicateDispatchBlocked(format!(
                "reservation '{}' is a replay or lacks single-use dispatch authorization",
                reservation.id
            )));
        }

        if ctx.cancel.is_cancelled() {
            ctx.store
                .cancel_unclaimed_reservation(
                    &reservation.id,
                    &ctx.lease.owner_id,
                    ctx.lease.lease_epoch,
                )
                .await?;
            return Err(OpenAiAdapterError::Cancelled);
        }

        self.claim_and_execute_reservation(ctx, &reservation.id, request, &request_sha256, http_req)
            .await
    }

    async fn claim_and_execute_reservation(
        &self,
        ctx: DispatchContext<'_>,
        reservation_id: &str,
        request: &LlmRequest,
        request_sha256: &str,
        http_req: HttpRequest,
    ) -> Result<(LlmResponse, BudgetReservation), OpenAiAdapterError> {
        let claimed = ctx
            .store
            .claim_reservation_dispatch(reservation_id, &ctx.lease.owner_id, ctx.lease.lease_epoch)
            .await?;
        debug_assert!(claimed.dispatch_claimed);

        let transport_result = self.execute_http_bounded(http_req, ctx.cancel).await;
        let parsed_result = match transport_result {
            Ok(http_res) => parse_openai_http_response(
                http_res,
                request,
                request_sha256,
                self.config.max_response_bytes,
                &self.config.credential,
            ),
            Err(err) => Err((err, ProviderUsageReport::MissingOrUnverifiable)),
        };

        match parsed_result {
            Ok((response, usage_report)) => {
                let (settled, _) = ctx
                    .store
                    .reconcile_budget_reservation(
                        Some(reservation_id),
                        ReconcileBudgetRequest {
                            reservation_id: reservation_id.to_owned(),
                            owner_id: ctx.lease.owner_id.clone(),
                            lease_epoch: ctx.lease.lease_epoch,
                            usage: usage_report,
                        },
                    )
                    .await
                    .map_err(|err| {
                        OpenAiAdapterError::ReconciliationFailed(format!(
                            "verified settlement failed ({}): {err}",
                            err.code()
                        ))
                    })?;
                Ok((response, settled))
            }
            Err((call_err, conservative_report)) => {
                ctx.store
                    .reconcile_budget_reservation(
                        Some(reservation_id),
                        ReconcileBudgetRequest {
                            reservation_id: reservation_id.to_owned(),
                            owner_id: ctx.lease.owner_id.clone(),
                            lease_epoch: ctx.lease.lease_epoch,
                            usage: conservative_report,
                        },
                    )
                    .await
                    .map_err(|rec_err| {
                        OpenAiAdapterError::ReconciliationFailed(format!(
                            "call failed ({call_err}) and conservative reconciliation failed ({})",
                            rec_err.code()
                        ))
                    })?;
                Err(call_err)
            }
        }
    }

    async fn execute_http_bounded(
        &self,
        http_req: HttpRequest,
        cancel: &CancellationToken,
    ) -> Result<HttpResponse, OpenAiAdapterError> {
        let timeout = self.config.request_timeout;
        let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);

        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(OpenAiAdapterError::Cancelled),
            res = tokio::time::timeout(timeout, self.transport.execute(http_req, cancel)) => res,
        };

        match outcome {
            Ok(Ok(res)) => Ok(res),
            Ok(Err(HttpTransportError::Cancelled)) => Err(OpenAiAdapterError::Cancelled),
            Ok(Err(HttpTransportError::TimedOut(_))) | Err(_) => {
                Err(OpenAiAdapterError::Timeout { timeout_ms })
            }
            Ok(Err(HttpTransportError::NetworkDisabled)) => Err(OpenAiAdapterError::Provider(
                ProviderError::ProviderUnavailable(
                    "real HTTP network execution is disabled in P2-03C".into(),
                ),
            )),
            Ok(Err(HttpTransportError::Transport(msg))) => Err(OpenAiAdapterError::Transport(
                redact_text(&msg, &self.config.credential),
            )),
        }
    }
}

fn validate_reservation_alignment(
    res: &BudgetReservation,
    lease: &WorkerLease,
    request: &LlmRequest,
) -> Result<(), OpenAiAdapterError> {
    if res.owner_id != lease.owner_id || res.lease_epoch != lease.lease_epoch {
        return Err(StoreError::Forbidden {
            code: "reservation_ownership_required",
        }
        .into());
    }
    if res.provider_id != request.provider_id || res.model_id != request.model_id {
        return Err(ProviderError::InvalidModelSelection(
            "existing reservation model does not match request; fallback is forbidden".into(),
        )
        .into());
    }
    if !scope_matches_role(res.scope, request.task_role) {
        return Err(ProviderError::UnsupportedCapability(format!(
            "reservation scope '{}' does not match task role '{}'",
            res.scope.as_str(),
            request.task_role.as_str()
        ))
        .into());
    }
    if res.max_output_tokens < i64::from(request.max_output_tokens) {
        return Err(ProviderError::UnsupportedParameter(
            "reservation max_output_tokens ceiling is smaller than request max_output_tokens"
                .into(),
        )
        .into());
    }
    Ok(())
}

fn scope_matches_role(scope: CostScope, role: TaskRole) -> bool {
    matches!(
        (scope, role),
        (CostScope::StoryboardPlanning, TaskRole::StoryboardPlanning)
            | (
                CostScope::SceneCodeGeneration,
                TaskRole::SceneCodeGeneration
            )
    )
}

#[derive(Serialize)]
struct OpenAiChatRequest {
    model: String,
    messages: Vec<OpenAiWireMessage>,
    temperature: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<u32>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<OpenAiWireTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<OpenAiResponseFormat>,
    stream: bool,
}

#[derive(Serialize)]
struct OpenAiResponseFormat {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct OpenAiWireTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: OpenAiWireFunctionDef,
}

#[derive(Serialize)]
struct OpenAiWireFunctionDef {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Serialize)]
struct OpenAiWireMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAiWireToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct OpenAiWireToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: OpenAiWireFunctionCall,
}

#[derive(Serialize, Deserialize)]
struct OpenAiWireFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct OpenAiChatResponse {
    id: String,
    model: String,
    choices: Vec<OpenAiChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    #[serde(default)]
    finish_reason: Option<String>,
    message: OpenAiChoiceMessage,
}

#[derive(Deserialize)]
struct OpenAiChoiceMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAiWireToolCall>>,
}

#[derive(Deserialize)]
struct OpenAiUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
    #[serde(default)]
    total_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct OpenAiErrorEnvelope {
    error: OpenAiErrorDetail,
}

#[derive(Deserialize)]
struct OpenAiErrorDetail {
    message: String,
}

fn build_openai_chat_request(
    request: &LlmRequest,
) -> Result<OpenAiChatRequest, OpenAiAdapterError> {
    let mut messages = Vec::with_capacity(request.messages.len() + 1);
    if !request.system_prompt.trim().is_empty() {
        messages.push(OpenAiWireMessage {
            role: "system",
            content: Some(request.system_prompt.clone()),
            tool_calls: None,
            tool_call_id: None,
        });
    }
    for msg in &request.messages {
        match msg.role {
            MessageRole::User => messages.push(OpenAiWireMessage {
                role: "user",
                content: Some(msg.content.clone()),
                tool_calls: None,
                tool_call_id: None,
            }),
            MessageRole::Assistant => {
                let wire_calls = if msg.tool_calls.is_empty() {
                    None
                } else {
                    let mut calls = Vec::with_capacity(msg.tool_calls.len());
                    for tc in &msg.tool_calls {
                        let arguments = serde_json::to_string(&tc.arguments).map_err(|err| {
                            OpenAiAdapterError::Internal(format!(
                                "failed serializing assistant tool_call arguments: {err}"
                            ))
                        })?;
                        calls.push(OpenAiWireToolCall {
                            id: tc.call_id.clone(),
                            kind: "function".into(),
                            function: OpenAiWireFunctionCall {
                                name: tc.name.clone(),
                                arguments,
                            },
                        });
                    }
                    Some(calls)
                };
                messages.push(OpenAiWireMessage {
                    role: "assistant",
                    content: if msg.content.is_empty() {
                        None
                    } else {
                        Some(msg.content.clone())
                    },
                    tool_calls: wire_calls,
                    tool_call_id: None,
                });
            }
            MessageRole::Tool => messages.push(OpenAiWireMessage {
                role: "tool",
                content: Some(msg.content.clone()),
                tool_calls: None,
                tool_call_id: msg.tool_call_id.clone(),
            }),
        }
    }

    let tools: Vec<OpenAiWireTool> = request
        .tools
        .iter()
        .map(|tool| OpenAiWireTool {
            kind: "function",
            function: OpenAiWireFunctionDef {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.input_schema.clone(),
            },
        })
        .collect();

    let tool_choice = match (request.response_format, tools.is_empty()) {
        (_, true) => None,
        (ResponseFormat::ToolCalls, false) => Some("required"),
        (_, false) => Some("auto"),
    };
    let response_format = match request.response_format {
        ResponseFormat::JsonObject => Some(OpenAiResponseFormat {
            kind: "json_object",
        }),
        ResponseFormat::Text | ResponseFormat::ToolCalls => None,
    };

    Ok(OpenAiChatRequest {
        model: request.model_id.clone(),
        messages,
        temperature: request.parameters.temperature,
        seed: request.parameters.seed,
        max_tokens: request.max_output_tokens,
        tools,
        tool_choice,
        response_format,
        stream: false,
    })
}

fn parse_openai_http_response(
    http_res: HttpResponse,
    request: &LlmRequest,
    request_sha256: &str,
    max_response_bytes: usize,
    credential: &CredentialSource,
) -> Result<(LlmResponse, ProviderUsageReport), (OpenAiAdapterError, ProviderUsageReport)> {
    if http_res.body.len() > max_response_bytes {
        return Err((
            ProviderError::InvalidResponse(format!(
                "provider HTTP response {} bytes exceeds limit {}",
                http_res.body.len(),
                max_response_bytes
            ))
            .into(),
            ProviderUsageReport::MissingOrUnverifiable,
        ));
    }

    if !(200..300).contains(&http_res.status) {
        let safe_msg = extract_safe_http_error(&http_res.body, credential);
        let mapped = match http_res.status {
            401 | 403 => OpenAiAdapterError::AuthenticationFailed {
                status: http_res.status,
            },
            429 => OpenAiAdapterError::RateLimited {
                status: http_res.status,
                message: safe_msg,
            },
            500..=599 => ProviderError::ProviderUnavailable(format!(
                "provider upstream error (HTTP {}): {safe_msg}",
                http_res.status
            ))
            .into(),
            _ => OpenAiAdapterError::HttpStatus {
                status: http_res.status,
                message: safe_msg,
            },
        };
        return Err((mapped, ProviderUsageReport::MissingOrUnverifiable));
    }

    let wire: OpenAiChatResponse = serde_json::from_slice(&http_res.body).map_err(|err| {
        (
            ProviderError::InvalidResponse(format!("malformed OpenAI JSON response: {err}")).into(),
            ProviderUsageReport::MissingOrUnverifiable,
        )
    })?;

    let usage_report = match &wire.usage {
        Some(u) => validate_wire_usage(u, request.max_output_tokens)
            .map_err(|err| (err.into(), ProviderUsageReport::MissingOrUnverifiable))?,
        None => {
            return Err((
                ProviderError::InvalidResponse(
                    "provider response omitted authoritative usage accounting".into(),
                )
                .into(),
                ProviderUsageReport::MissingOrUnverifiable,
            ));
        }
    };

    let (input_tokens, output_tokens) = match usage_report {
        ProviderUsageReport::Verified {
            input_tokens,
            output_tokens,
        } => (input_tokens, output_tokens),
        ProviderUsageReport::MissingOrUnverifiable => unreachable!(),
    };

    if wire.id.trim().is_empty() {
        return Err((
            ProviderError::InvalidResponse("provider response id must not be empty".into()).into(),
            usage_report,
        ));
    }
    if wire.model.trim().is_empty() || wire.model != request.model_id {
        return Err((
            ProviderError::InvalidResponse(format!(
                "provider response model '{}' does not match requested '{}'; silent substitution rejected",
                wire.model, request.model_id
            ))
            .into(),
            usage_report,
        ));
    }
    if wire.choices.len() != 1 {
        return Err((
            ProviderError::InvalidResponse(format!(
                "provider response must contain exactly 1 choice (got {})",
                wire.choices.len()
            ))
            .into(),
            usage_report,
        ));
    }
    let choice = &wire.choices[0];
    let finish_reason = match choice.finish_reason.as_deref() {
        Some("stop") => FinishReason::Stop,
        Some("tool_calls") => FinishReason::ToolCalls,
        Some("length") => FinishReason::Length,
        Some("content_filter") => FinishReason::ContentFilter,
        other => {
            return Err((
                ProviderError::InvalidResponse(format!(
                    "unsupported or missing finish_reason '{other:?}'"
                ))
                .into(),
                usage_report,
            ));
        }
    };

    let content = choice.message.content.clone().unwrap_or_default();
    if content.len() > MAX_RESPONSE_CONTENT_BYTES {
        return Err((
            ProviderError::InvalidResponse("response content exceeds byte limit".into()).into(),
            usage_report,
        ));
    }

    let raw_calls = choice.message.tool_calls.as_deref().unwrap_or(&[]);
    if raw_calls.len() > MAX_TOOL_CALLS_PER_RESPONSE {
        return Err((
            ProviderError::InvalidResponse("too many tool calls in response".into()).into(),
            usage_report,
        ));
    }

    let allowed_tools: HashSet<&str> = request.tools.iter().map(|t| t.name.as_str()).collect();
    let mut seen_call_ids = HashSet::new();
    let mut tool_calls = Vec::with_capacity(raw_calls.len());
    for raw in raw_calls {
        if raw.kind != "function" {
            return Err((
                ProviderError::InvalidResponse(format!(
                    "unsupported tool_call type '{}'",
                    raw.kind
                ))
                .into(),
                usage_report,
            ));
        }
        if raw.id.trim().is_empty() || !seen_call_ids.insert(raw.id.as_str()) {
            return Err((
                ProviderError::InvalidResponse(
                    "duplicate or empty tool call id in response".into(),
                )
                .into(),
                usage_report,
            ));
        }
        if !allowed_tools.contains(raw.function.name.as_str()) {
            return Err((
                ProviderError::InvalidResponse(format!(
                    "provider invoked undeclared tool '{}'",
                    raw.function.name
                ))
                .into(),
                usage_report,
            ));
        }
        let parsed_args: serde_json::Value = serde_json::from_str(&raw.function.arguments)
            .map_err(|err| {
                (
                    ProviderError::InvalidResponse(format!(
                        "malformed JSON arguments for tool '{}': {err}",
                        raw.function.name
                    ))
                    .into(),
                    usage_report.clone(),
                )
            })?;
        if !parsed_args.is_object() {
            return Err((
                ProviderError::InvalidResponse(format!(
                    "tool '{}' arguments must decode to a JSON object",
                    raw.function.name
                ))
                .into(),
                usage_report,
            ));
        }
        tool_calls.push(ToolCall {
            call_id: raw.id.clone(),
            name: raw.function.name.clone(),
            arguments: parsed_args,
        });
    }

    let tool_calls_bytes = serde_json::to_vec(&tool_calls).map_err(|err| {
        (
            ProviderError::InvalidResponse(format!("failed sizing tool_calls: {err}")).into(),
            usage_report.clone(),
        )
    })?;
    if tool_calls_bytes.len() > MAX_RESPONSE_TOOL_CALLS_BYTES {
        return Err((
            ProviderError::InvalidResponse("response tool_calls exceed byte limit".into()).into(),
            usage_report,
        ));
    }

    if finish_reason == FinishReason::ToolCalls && tool_calls.is_empty() {
        return Err((
            ProviderError::InvalidResponse(
                "finish_reason 'tool_calls' requires at least one tool call".into(),
            )
            .into(),
            usage_report,
        ));
    }
    if finish_reason != FinishReason::ToolCalls && !tool_calls.is_empty() {
        return Err((
            ProviderError::InvalidResponse(
                "non-tool finish_reason must not carry tool calls".into(),
            )
            .into(),
            usage_report,
        ));
    }
    if request.response_format == ResponseFormat::JsonObject && finish_reason == FinishReason::Stop
    {
        let parsed_json: serde_json::Value = serde_json::from_str(&content).map_err(|err| {
            (
                ProviderError::InvalidResponse(format!(
                    "structured json_object response is invalid JSON: {err}"
                ))
                .into(),
                usage_report.clone(),
            )
        })?;
        if !parsed_json.is_object() {
            return Err((
                ProviderError::InvalidResponse(
                    "structured json_object response must be a JSON object".into(),
                )
                .into(),
                usage_report,
            ));
        }
    }

    let usage = LlmUsage::new(input_tokens, output_tokens, true, false);
    usage
        .validate()
        .map_err(|err| (err.into(), usage_report.clone()))?;

    let mut response = LlmResponse {
        schema_version: PROVIDER_SCHEMA_VERSION.into(),
        provider_id: request.provider_id.clone(),
        model_id: request.model_id.clone(),
        adapter_version: request.adapter_version.clone(),
        response_id: wire.id,
        request_sha256: request_sha256.to_owned(),
        response_sha256: String::new(),
        finish_reason,
        content,
        tool_calls,
        usage,
        simulated: false,
    };
    response.response_sha256 = response
        .compute_response_sha256()
        .map_err(|err| (err.into(), usage_report.clone()))?;

    let envelope_bytes = serde_json::to_vec(&response).map_err(|err| {
        (
            ProviderError::InvalidResponse(format!("failed sizing normalized response: {err}"))
                .into(),
            usage_report.clone(),
        )
    })?;
    if envelope_bytes.len() > max_response_bytes {
        return Err((
            ProviderError::InvalidResponse("normalized response exceeds byte limit".into()).into(),
            usage_report,
        ));
    }

    Ok((response, usage_report))
}

fn validate_wire_usage(
    usage: &OpenAiUsage,
    max_output_tokens: u32,
) -> Result<ProviderUsageReport, ProviderError> {
    let sum = usage
        .prompt_tokens
        .checked_add(usage.completion_tokens)
        .ok_or_else(|| ProviderError::InvalidResponse("usage token sum overflows u64".into()))?;
    if let Some(reported_total) = usage.total_tokens {
        if reported_total != sum {
            return Err(ProviderError::InvalidResponse(
                "usage total_tokens does not match prompt_tokens + completion_tokens".into(),
            ));
        }
    }
    if usage.completion_tokens > u64::from(max_output_tokens) {
        return Err(ProviderError::InvalidResponse(
            "completion_tokens exceeded requested max_output_tokens".into(),
        ));
    }
    Ok(ProviderUsageReport::Verified {
        input_tokens: usage.prompt_tokens,
        output_tokens: usage.completion_tokens,
    })
}

fn validate_endpoint_url(raw: &str, remote: bool) -> Result<String, OpenAiAdapterError> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty()
        || trimmed.len() > 512
        || trimmed
            .bytes()
            .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
        || trimmed.contains('@')
        || trimmed.contains('?')
        || trimmed.contains('#')
    {
        return Err(OpenAiAdapterError::InvalidConfig(
            "invalid base_url format".into(),
        ));
    }
    let lower = trimmed.to_ascii_lowercase();
    let authority_and_path = if let Some(rest) = lower.strip_prefix("https://") {
        rest
    } else if !remote {
        let Some(rest) = lower.strip_prefix("http://") else {
            return Err(OpenAiAdapterError::InvalidConfig(
                "base_url must use https:// or loopback http://".into(),
            ));
        };
        let host_port = rest.split('/').next().unwrap_or("");
        if !is_loopback_host_port(host_port) {
            return Err(OpenAiAdapterError::InvalidConfig(
                "non-HTTPS local_endpoint base_url is restricted to loopback hosts".into(),
            ));
        }
        rest
    } else {
        return Err(OpenAiAdapterError::InvalidConfig(
            "remote openai-compatible base_url must use https://".into(),
        ));
    };

    let host = authority_and_path.split('/').next().unwrap_or("");
    if host.is_empty() || host.starts_with(':') {
        return Err(OpenAiAdapterError::InvalidConfig(
            "base_url must include a non-empty host".into(),
        ));
    }
    Ok(trimmed.to_owned())
}

fn is_loopback_host_port(host_port: &str) -> bool {
    if let Some(rest) = host_port.strip_prefix("[::1]") {
        return rest.is_empty() || rest.strip_prefix(':').is_some_and(is_port);
    }
    let (host, port) = match host_port.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (host_port, None),
    };
    let host_ok = matches!(host, "127.0.0.1" | "localhost");
    host_ok && port.is_none_or(is_port)
}

fn is_port(raw: &str) -> bool {
    !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()) && raw.parse::<u16>().is_ok()
}

fn is_valid_env_var_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .enumerate()
            .all(|(i, b)| b.is_ascii_uppercase() || b == b'_' || (i > 0 && b.is_ascii_digit()))
}

fn extract_safe_http_error(body: &[u8], credential: &CredentialSource) -> String {
    let raw = match serde_json::from_slice::<OpenAiErrorEnvelope>(body) {
        Ok(env) => env.error.message,
        Err(_) => String::from_utf8_lossy(body).into_owned(),
    };
    redact_text(&truncate_utf8(&raw, MAX_ERROR_EXCERPT_BYTES), credential)
}

fn redact_text(input: &str, credential: &CredentialSource) -> String {
    match credential {
        CredentialSource::BearerToken(secret) => {
            input.replace(secret.as_header_secret(), "[REDACTED]")
        }
        CredentialSource::EnvVar { var_name } => match std::env::var(var_name) {
            Ok(val) if !val.trim().is_empty() => input.replace(&val, "[REDACTED]"),
            _ => input.to_owned(),
        },
        CredentialSource::None => input.to_owned(),
    }
}

fn truncate_utf8(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.trim().to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", input[..end].trim())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
