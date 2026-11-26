//! Provider-independent LLM interface, strict request/capability/parameter validation,
//! and deterministic scripted offline mock provider (ADR-02 / P2-03A).
//!
//! # Guarantees
//! - Explicit validation of provider ID, model ID, adapter version, task role capabilities,
//!   finite temperature (`0.0..=2.0`), optional seed (`0..=2_147_483_647`), and token bounds.
//! - Zero silent fallback or model substitution: mismatches and unavailable/unconfigured
//!   entries fail closed.
//! - Zero real network or paid LLM execution in P2-03A: `remote: true` providers fail closed
//!   with `RemoteExecutionNotAuthorized` (`remote_cost_not_approved`).

use std::collections::HashSet;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::domain::{ModelRecord, ModelSnapshot, Parameters};

pub const PROVIDER_SCHEMA_VERSION: &str = "1";
pub const MAX_MESSAGES_PER_REQUEST: usize = 128;
pub const MAX_TOOLS_PER_REQUEST: usize = 32;
pub const MAX_TOOL_CALLS_PER_RESPONSE: usize = 16;
pub const MAX_MESSAGE_CONTENT_BYTES: usize = 256 * 1024;
pub const MAX_SYSTEM_PROMPT_BYTES: usize = 128 * 1024;
pub const MAX_TOTAL_REQUEST_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_RESPONSE_CONTENT_BYTES: usize = 256 * 1024;
pub const MAX_RESPONSE_TOOL_CALLS_BYTES: usize = 512 * 1024;
pub const MAX_TOTAL_RESPONSE_BYTES: usize = 1024 * 1024;
pub const MAX_CONTEXT_TOKENS: u32 = 2_000_000;
pub const MAX_OUTPUT_TOKENS: u32 = 131_072;
pub const MAX_SEED_VALUE: u32 = 2_147_483_647;

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("invalid llm request: {0}")]
    InvalidRequest(String),
    #[error("invalid model selection: {0}")]
    InvalidModelSelection(String),
    #[error("unsupported capability: {0}")]
    UnsupportedCapability(String),
    #[error("unsupported parameter: {0}")]
    UnsupportedParameter(String),
    #[error("provider unavailable: {0}")]
    ProviderUnavailable(String),
    #[error("remote provider execution not authorized: {0}")]
    RemoteExecutionNotAuthorized(String),
    #[error("provider cost or token limit exceeded: {0}")]
    CostLimitExceeded(String),
    #[error("provider cost accounting or reconciliation failed: {0}")]
    AccountingFailed(String),
    #[error("invalid provider response: {0}")]
    InvalidResponse(String),
    #[error("mock provider script exhausted: {0}")]
    ScriptExhausted(String),
    #[error("internal provider error: {0}")]
    Internal(String),
}

impl ProviderError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "invalid_schema",
            Self::InvalidModelSelection(_) => "invalid_model_selection",
            Self::UnsupportedCapability(_) => "unsupported_capability",
            Self::UnsupportedParameter(_) => "unsupported_parameter",
            Self::ProviderUnavailable(_) | Self::ScriptExhausted(_) => "provider_unavailable",
            Self::RemoteExecutionNotAuthorized(_) => "remote_cost_not_approved",
            Self::CostLimitExceeded(_) => "cost_limit_exceeded",
            Self::AccountingFailed(_) => "cost_reconciliation_failed",
            Self::InvalidResponse(_) => "provider_invalid_response",
            Self::Internal(_) => "internal_error",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskRole {
    StoryboardPlanning,
    SceneCodeGeneration,
}

impl TaskRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StoryboardPlanning => "storyboard_planning",
            Self::SceneCodeGeneration => "scene_code_generation",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "storyboard_planning" => Some(Self::StoryboardPlanning),
            "scene_code_generation" => Some(Self::SceneCodeGeneration),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAvailability {
    Unknown,
    Available,
    Unavailable,
}

impl ProviderAvailability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Available => "available",
            Self::Unavailable => "unavailable",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "unknown" => Some(Self::Unknown),
            "available" => Some(Self::Available),
            "unavailable" => Some(Self::Unavailable),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    DeterministicScriptedMock,
    LocalEndpoint,
    /// Local gateway (e.g. 9Router on loopback HTTP) forwarding to remote upstream providers;
    /// billed and gated identically to remote APIs (`remote = true`).
    LocalGatewayRemoteUpstream,
    RemoteApi,
}

impl ProviderKind {
    /// Returns true whenever inference executes against paid/quota-limited remote upstreams
    /// (either direct TLS or via a local gateway forwarding to remote providers).
    pub fn is_remote_backed(self) -> bool {
        matches!(self, Self::RemoteApi | Self::LocalGatewayRemoteUpstream)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCapabilities {
    pub schema_version: String,
    pub provider_kind: ProviderKind,
    pub provider_id: String,
    pub model_id: String,
    pub adapter_version: String,
    pub task_roles: Vec<TaskRole>,
    pub supports_tool_calling: bool,
    pub supports_structured_json: bool,
    pub supports_seed: bool,
    pub min_temperature: f64,
    pub max_temperature: f64,
    pub max_context_tokens: u32,
    pub max_output_tokens: u32,
    pub enabled: bool,
    pub configured: bool,
    pub availability: ProviderAvailability,
    pub reason: String,
    pub remote: bool,
}

impl ProviderCapabilities {
    pub fn deterministic_mock(model_id: &str, task_roles: Vec<TaskRole>) -> Self {
        Self {
            schema_version: PROVIDER_SCHEMA_VERSION.into(),
            provider_kind: ProviderKind::DeterministicScriptedMock,
            provider_id: "mock".into(),
            model_id: model_id.into(),
            adapter_version: "1.0.0".into(),
            task_roles,
            supports_tool_calling: true,
            supports_structured_json: true,
            supports_seed: true,
            min_temperature: 0.0,
            max_temperature: 2.0,
            max_context_tokens: 128_000,
            max_output_tokens: 16_384,
            enabled: true,
            configured: true,
            availability: ProviderAvailability::Available,
            reason: "Deterministic offline scripted mock provider".into(),
            remote: false,
        }
    }

    pub fn supports_role(&self, role: TaskRole) -> bool {
        self.task_roles.contains(&role)
    }

    pub fn validate(&self) -> Result<(), ProviderError> {
        self.validate_core(true)
    }

    /// Validates adapter capability bounds for an already-authorized dispatch configuration,
    /// permitting `ProviderAvailability::Available` on verified remote endpoints.
    pub fn validate_for_adapter_config(&self) -> Result<(), ProviderError> {
        self.validate_core(false)
    }

    fn validate_core(&self, reject_remote_available: bool) -> Result<(), ProviderError> {
        if self.schema_version != PROVIDER_SCHEMA_VERSION {
            return Err(ProviderError::InvalidModelSelection(
                "unsupported provider capability schema_version".into(),
            ));
        }
        if !is_safe_identifier(&self.provider_id)
            || !is_safe_identifier(&self.model_id)
            || !is_safe_identifier(&self.adapter_version)
        {
            return Err(ProviderError::InvalidModelSelection(
                "invalid provider_id, model_id, or adapter_version".into(),
            ));
        }
        if self.task_roles.is_empty() {
            return Err(ProviderError::UnsupportedCapability(
                "provider must declare at least one task role".into(),
            ));
        }
        let mut seen = HashSet::new();
        for role in &self.task_roles {
            if !seen.insert(*role) {
                return Err(ProviderError::UnsupportedCapability(
                    "duplicate task_role in capabilities".into(),
                ));
            }
        }
        if !self.min_temperature.is_finite()
            || !self.max_temperature.is_finite()
            || !(0.0..=2.0).contains(&self.min_temperature)
            || !(0.0..=2.0).contains(&self.max_temperature)
            || self.min_temperature > self.max_temperature
        {
            return Err(ProviderError::UnsupportedParameter(
                "invalid supported temperature range".into(),
            ));
        }
        if self.max_context_tokens == 0
            || self.max_context_tokens > MAX_CONTEXT_TOKENS
            || self.max_output_tokens == 0
            || self.max_output_tokens > MAX_OUTPUT_TOKENS
            || self.max_output_tokens > self.max_context_tokens
        {
            return Err(ProviderError::UnsupportedParameter(
                "invalid token capacity bounds".into(),
            ));
        }
        if reject_remote_available
            && self.remote
            && self.availability == ProviderAvailability::Available
        {
            return Err(ProviderError::InvalidModelSelection(
                "remote providers cannot be claimed available without verified remote consent & probe"
                    .into(),
            ));
        }
        if self.provider_kind.is_remote_backed() != self.remote {
            return Err(ProviderError::InvalidModelSelection(
                "provider_kind remote_api or local_gateway_remote_upstream must match remote=true"
                    .into(),
            ));
        }
        if self.provider_kind == ProviderKind::DeterministicScriptedMock
            && self.provider_id != "mock"
        {
            return Err(ProviderError::InvalidModelSelection(
                "deterministic_scripted_mock must use provider_id='mock'".into(),
            ));
        }
        Ok(())
    }

    pub fn validate_against_model_record(
        &self,
        model: &ModelRecord,
        expected_role: TaskRole,
        parameters: &Parameters,
    ) -> Result<(), ProviderError> {
        self.validate()?;
        if model.schema_version != PROVIDER_SCHEMA_VERSION {
            return Err(ProviderError::InvalidModelSelection(
                "unsupported model record schema_version".into(),
            ));
        }
        if self.provider_id != model.provider_id
            || self.model_id != model.model_id
            || self.adapter_version != model.adapter_version
        {
            return Err(ProviderError::InvalidModelSelection(format!(
                "provider capability ({}/{}/{}) does not match selected model ({}/{}/{}); fallback is forbidden",
                self.provider_id,
                self.model_id,
                self.adapter_version,
                model.provider_id,
                model.model_id,
                model.adapter_version
            )));
        }
        if self.remote != model.remote {
            return Err(ProviderError::InvalidModelSelection(
                "remote flag mismatch between provider and model registry".into(),
            ));
        }
        if self.remote || model.remote {
            return Err(ProviderError::RemoteExecutionNotAuthorized(
                "remote LLM execution is disabled in P2-03A offline foundation".into(),
            ));
        }
        if !self.enabled
            || !model.enabled
            || !self.configured
            || !model.configured
            || self.availability != ProviderAvailability::Available
            || model.availability != "available"
        {
            return Err(ProviderError::ProviderUnavailable(format!(
                "selected model {}/{} is disabled, unconfigured, or not available",
                model.provider_id, model.model_id
            )));
        }
        if !self.supports_role(expected_role)
            || !model
                .task_roles
                .iter()
                .any(|role| role == expected_role.as_str())
        {
            return Err(ProviderError::UnsupportedCapability(format!(
                "model {}/{} does not support role '{}'",
                model.provider_id,
                model.model_id,
                expected_role.as_str()
            )));
        }
        validate_parameters_against_caps(self, parameters)?;
        Ok(())
    }

    pub fn validate_against_snapshot(
        &self,
        snapshot: &ModelSnapshot,
        expected_role: TaskRole,
    ) -> Result<(), ProviderError> {
        self.validate_against_snapshot_with_policy(snapshot, expected_role, false)
    }

    pub fn validate_against_snapshot_for_adapter(
        &self,
        snapshot: &ModelSnapshot,
        expected_role: TaskRole,
    ) -> Result<(), ProviderError> {
        self.validate_against_snapshot_with_policy(snapshot, expected_role, true)
    }

    fn validate_against_snapshot_with_policy(
        &self,
        snapshot: &ModelSnapshot,
        expected_role: TaskRole,
        allow_remote: bool,
    ) -> Result<(), ProviderError> {
        if allow_remote {
            self.validate_for_adapter_config()?;
        } else {
            self.validate()?;
        }
        if self.provider_id != snapshot.provider_id
            || self.model_id != snapshot.model_id
            || self.adapter_version != snapshot.adapter_version
        {
            return Err(ProviderError::InvalidModelSelection(format!(
                "provider capability ({}/{}/{}) does not match frozen revision snapshot ({}/{}/{}); fallback is forbidden",
                self.provider_id,
                self.model_id,
                self.adapter_version,
                snapshot.provider_id,
                snapshot.model_id,
                snapshot.adapter_version
            )));
        }
        if !allow_remote && self.remote {
            return Err(ProviderError::RemoteExecutionNotAuthorized(
                "remote LLM execution is disabled in P2-03A offline foundation".into(),
            ));
        }
        if !self.enabled || !self.configured || self.availability != ProviderAvailability::Available
        {
            return Err(ProviderError::ProviderUnavailable(format!(
                "provider {}/{} is unavailable",
                self.provider_id, self.model_id
            )));
        }
        if !self.supports_role(expected_role) {
            return Err(ProviderError::UnsupportedCapability(format!(
                "provider {}/{} does not support role '{}'",
                self.provider_id,
                self.model_id,
                expected_role.as_str()
            )));
        }
        validate_parameters_against_caps(self, &snapshot.parameters)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmMessage {
    pub role: MessageRole,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

impl LlmMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
        }
    }

    pub fn assistant(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
            tool_name: None,
        }
    }

    pub fn tool_result(
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content_json: &serde_json::Value,
    ) -> Result<Self, ProviderError> {
        let serialized = serde_json::to_string(content_json).map_err(|err| {
            ProviderError::Internal(format!("failed serializing tool result: {err}"))
        })?;
        Ok(Self {
            role: MessageRole::Tool,
            content: serialized,
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
            tool_name: Some(tool_name.into()),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseFormat {
    Text,
    ToolCalls,
    JsonObject,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmRequest {
    pub schema_version: String,
    pub task_role: TaskRole,
    pub provider_id: String,
    pub model_id: String,
    pub adapter_version: String,
    pub parameters: Parameters,
    pub max_output_tokens: u32,
    pub response_format: ResponseFormat,
    pub system_prompt: String,
    pub messages: Vec<LlmMessage>,
    pub tools: Vec<ToolDefinition>,
}

impl LlmRequest {
    pub fn canonical_request_sha256(&self) -> Result<String, ProviderError> {
        canonical_jcs_sha256(self)
    }

    pub fn validate(&self) -> Result<(), ProviderError> {
        if self.schema_version != PROVIDER_SCHEMA_VERSION {
            return Err(ProviderError::InvalidRequest(
                "unsupported llm request schema_version".into(),
            ));
        }
        if !is_safe_identifier(&self.provider_id)
            || !is_safe_identifier(&self.model_id)
            || !is_safe_identifier(&self.adapter_version)
        {
            return Err(ProviderError::InvalidModelSelection(
                "invalid provider_id, model_id, or adapter_version".into(),
            ));
        }
        validate_base_parameters(&self.parameters)?;
        if self.max_output_tokens == 0 || self.max_output_tokens > MAX_OUTPUT_TOKENS {
            return Err(ProviderError::UnsupportedParameter(format!(
                "max_output_tokens must be between 1 and {MAX_OUTPUT_TOKENS}"
            )));
        }
        if self.system_prompt.len() > MAX_SYSTEM_PROMPT_BYTES {
            return Err(ProviderError::InvalidRequest(
                "system_prompt exceeds byte limit".into(),
            ));
        }
        if self.messages.is_empty() || self.messages.len() > MAX_MESSAGES_PER_REQUEST {
            return Err(ProviderError::InvalidRequest(
                "messages must contain between 1 and 128 entries".into(),
            ));
        }
        if self.tools.len() > MAX_TOOLS_PER_REQUEST {
            return Err(ProviderError::InvalidRequest(
                "too many tools declared in request".into(),
            ));
        }
        let mut tool_names = HashSet::new();
        for tool in &self.tools {
            if !is_safe_identifier(&tool.name) {
                return Err(ProviderError::InvalidRequest(format!(
                    "invalid tool name '{}'",
                    tool.name
                )));
            }
            if tool.description.trim().is_empty() || !tool.input_schema.is_object() {
                return Err(ProviderError::InvalidRequest(format!(
                    "tool '{}' requires non-empty description and object input_schema",
                    tool.name
                )));
            }
            if !tool_names.insert(tool.name.as_str()) {
                return Err(ProviderError::InvalidRequest(format!(
                    "duplicate tool definition '{}'",
                    tool.name
                )));
            }
        }
        if self.response_format == ResponseFormat::ToolCalls && self.tools.is_empty() {
            return Err(ProviderError::InvalidRequest(
                "tool_calls response format requires at least one tool definition".into(),
            ));
        }
        for (idx, msg) in self.messages.iter().enumerate() {
            if msg.content.len() > MAX_MESSAGE_CONTENT_BYTES {
                return Err(ProviderError::InvalidRequest(format!(
                    "message[{idx}] exceeds byte limit"
                )));
            }
            match msg.role {
                MessageRole::User => {
                    if msg.content.trim().is_empty()
                        || !msg.tool_calls.is_empty()
                        || msg.tool_call_id.is_some()
                        || msg.tool_name.is_some()
                    {
                        return Err(ProviderError::InvalidRequest(format!(
                            "invalid user message fields at index {idx}"
                        )));
                    }
                }
                MessageRole::Assistant => {
                    if (msg.content.trim().is_empty() && msg.tool_calls.is_empty())
                        || msg.tool_call_id.is_some()
                        || msg.tool_name.is_some()
                    {
                        return Err(ProviderError::InvalidRequest(format!(
                            "invalid assistant message fields at index {idx}"
                        )));
                    }
                }
                MessageRole::Tool => {
                    let has_ids = msg
                        .tool_call_id
                        .as_deref()
                        .is_some_and(|s| !s.trim().is_empty())
                        && msg.tool_name.as_deref().is_some_and(is_safe_identifier);
                    if !has_ids || msg.content.trim().is_empty() || !msg.tool_calls.is_empty() {
                        return Err(ProviderError::InvalidRequest(format!(
                            "invalid tool result message at index {idx}"
                        )));
                    }
                }
            }
        }
        canonical_jcs_sha256(self)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub authoritative: bool,
    pub simulated: bool,
}

impl LlmUsage {
    pub fn new(
        input_tokens: u64,
        output_tokens: u64,
        authoritative: bool,
        simulated: bool,
    ) -> Self {
        Self {
            input_tokens,
            output_tokens,
            total_tokens: input_tokens.saturating_add(output_tokens),
            authoritative,
            simulated,
        }
    }

    pub fn validate(&self) -> Result<(), ProviderError> {
        let Some(expected) = self.input_tokens.checked_add(self.output_tokens) else {
            return Err(ProviderError::InvalidResponse(
                "usage token sum overflows u64".into(),
            ));
        };
        if self.total_tokens != expected {
            return Err(ProviderError::InvalidResponse(
                "usage total_tokens does not match input_tokens + output_tokens".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmResponse {
    pub schema_version: String,
    pub provider_id: String,
    pub model_id: String,
    pub adapter_version: String,
    pub response_id: String,
    pub request_sha256: String,
    pub response_sha256: String,
    pub finish_reason: FinishReason,
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: LlmUsage,
    pub simulated: bool,
}

#[derive(Serialize)]
struct ResponseHashPayload<'a> {
    schema_version: &'a str,
    provider_id: &'a str,
    model_id: &'a str,
    adapter_version: &'a str,
    response_id: &'a str,
    request_sha256: &'a str,
    finish_reason: FinishReason,
    content: &'a str,
    tool_calls: &'a [ToolCall],
    usage: &'a LlmUsage,
    simulated: bool,
}

impl LlmResponse {
    pub fn compute_response_sha256(&self) -> Result<String, ProviderError> {
        canonical_jcs_sha256(&ResponseHashPayload {
            schema_version: &self.schema_version,
            provider_id: &self.provider_id,
            model_id: &self.model_id,
            adapter_version: &self.adapter_version,
            response_id: &self.response_id,
            request_sha256: &self.request_sha256,
            finish_reason: self.finish_reason,
            content: &self.content,
            tool_calls: &self.tool_calls,
            usage: &self.usage,
            simulated: self.simulated,
        })
    }
}

/// Provider-independent LLM interface implemented in-process inside the Rust host.
///
/// # Timeout Model
/// `generate` is synchronous in P2-03A; `AgentRuntime` checks deadlines cooperatively before and
/// after each call and cannot preempt a blocked synchronous call mid-flight until cancellable async
/// transports are added in a later phase.
pub trait LlmProvider: Send + Sync {
    fn capabilities(&self) -> &ProviderCapabilities;
    fn generate(&self, request: &LlmRequest) -> Result<LlmResponse, ProviderError>;
}

/// Enforces all capability, model identity, parameter, and non-fallback checks before invoking
/// `provider.generate`, then verifies response provenance and tool-call invariants.
pub fn dispatch_llm_call(
    provider: &dyn LlmProvider,
    request: &LlmRequest,
) -> Result<LlmResponse, ProviderError> {
    request.validate()?;
    let caps = provider.capabilities();
    caps.validate()?;

    if caps.provider_id != request.provider_id
        || caps.model_id != request.model_id
        || caps.adapter_version != request.adapter_version
    {
        return Err(ProviderError::InvalidModelSelection(format!(
            "requested model {}/{} ({}) does not match registered provider {}/{} ({}); silent fallback forbidden",
            request.provider_id,
            request.model_id,
            request.adapter_version,
            caps.provider_id,
            caps.model_id,
            caps.adapter_version
        )));
    }
    if caps.remote || caps.provider_kind == ProviderKind::RemoteApi {
        return Err(ProviderError::RemoteExecutionNotAuthorized(
            "remote provider calls require explicit remote cost authorization and are disabled in P2-03A"
                .into(),
        ));
    }
    if caps.provider_kind != ProviderKind::DeterministicScriptedMock {
        return Err(ProviderError::ProviderUnavailable(
            "offline P2-03A runtime only permits trusted deterministic scripted mock providers"
                .into(),
        ));
    }
    if !caps.enabled || !caps.configured || caps.availability != ProviderAvailability::Available {
        return Err(ProviderError::ProviderUnavailable(format!(
            "provider {}/{} is unavailable: {}",
            caps.provider_id, caps.model_id, caps.reason
        )));
    }
    if !caps.supports_role(request.task_role) {
        return Err(ProviderError::UnsupportedCapability(format!(
            "provider {}/{} does not support task_role '{}'",
            caps.provider_id,
            caps.model_id,
            request.task_role.as_str()
        )));
    }
    if !request.tools.is_empty() && !caps.supports_tool_calling {
        return Err(ProviderError::UnsupportedCapability(
            "selected provider does not support tool calling".into(),
        ));
    }
    if request.response_format == ResponseFormat::JsonObject && !caps.supports_structured_json {
        return Err(ProviderError::UnsupportedCapability(
            "selected provider does not support structured JSON output".into(),
        ));
    }
    validate_parameters_against_caps(caps, &request.parameters)?;
    if request.max_output_tokens > caps.max_output_tokens {
        return Err(ProviderError::UnsupportedParameter(format!(
            "requested max_output_tokens {} exceeds model limit {}",
            request.max_output_tokens, caps.max_output_tokens
        )));
    }

    let request_bytes = serde_jcs::to_vec(request)
        .map_err(|err| ProviderError::Internal(format!("JCS canonicalization failed: {err}")))?;
    if request_bytes.len() > MAX_TOTAL_REQUEST_BYTES {
        return Err(ProviderError::InvalidRequest(format!(
            "total request payload {} bytes exceeds {} byte limit",
            request_bytes.len(),
            MAX_TOTAL_REQUEST_BYTES
        )));
    }
    let expected_request_sha256: String = Sha256::digest(&request_bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let response = provider.generate(request)?;
    validate_response(request, caps, &expected_request_sha256, &response)?;
    Ok(response)
}

fn validate_response(
    request: &LlmRequest,
    caps: &ProviderCapabilities,
    expected_request_sha256: &str,
    response: &LlmResponse,
) -> Result<(), ProviderError> {
    if response.schema_version != PROVIDER_SCHEMA_VERSION {
        return Err(ProviderError::InvalidResponse(
            "unsupported response schema_version".into(),
        ));
    }
    if caps.provider_kind == ProviderKind::DeterministicScriptedMock
        && (!response.simulated || !response.usage.simulated)
    {
        return Err(ProviderError::InvalidResponse(
            "non-simulated response rejected in offline P2-03A mock runtime".into(),
        ));
    }
    if response.content.len() > MAX_RESPONSE_CONTENT_BYTES {
        return Err(ProviderError::InvalidResponse(
            "provider response content exceeds byte limit".into(),
        ));
    }
    let tool_calls_bytes = serde_json::to_vec(&response.tool_calls).map_err(|err| {
        ProviderError::InvalidResponse(format!("failed sizing tool_calls: {err}"))
    })?;
    if tool_calls_bytes.len() > MAX_RESPONSE_TOOL_CALLS_BYTES {
        return Err(ProviderError::InvalidResponse(
            "provider response tool_calls exceed byte limit".into(),
        ));
    }
    let total_response_bytes = serde_json::to_vec(response)
        .map_err(|err| ProviderError::InvalidResponse(format!("failed sizing response: {err}")))?;
    if total_response_bytes.len() > MAX_TOTAL_RESPONSE_BYTES {
        return Err(ProviderError::InvalidResponse(
            "provider response envelope exceeds total byte limit".into(),
        ));
    }
    if response.provider_id != request.provider_id
        || response.model_id != request.model_id
        || response.adapter_version != request.adapter_version
    {
        return Err(ProviderError::InvalidResponse(
            "provider response mutated model identity; silent substitution rejected".into(),
        ));
    }
    if response.response_id.trim().is_empty() {
        return Err(ProviderError::InvalidResponse(
            "provider response_id must not be empty".into(),
        ));
    }
    if response.request_sha256 != expected_request_sha256 {
        return Err(ProviderError::InvalidResponse(
            "provider response request_sha256 does not match canonical request".into(),
        ));
    }
    if response.compute_response_sha256()? != response.response_sha256 {
        return Err(ProviderError::InvalidResponse(
            "provider response_sha256 digest mismatch".into(),
        ));
    }
    response.usage.validate()?;
    if response.usage.output_tokens > u64::from(request.max_output_tokens) {
        return Err(ProviderError::InvalidResponse(
            "provider response output_tokens exceeded requested max_output_tokens".into(),
        ));
    }
    if response.tool_calls.len() > MAX_TOOL_CALLS_PER_RESPONSE {
        return Err(ProviderError::InvalidResponse(
            "provider returned too many tool calls in a single turn".into(),
        ));
    }
    let allowed_tools: HashSet<&str> = request.tools.iter().map(|t| t.name.as_str()).collect();
    let mut seen_call_ids = HashSet::new();
    for call in &response.tool_calls {
        if call.call_id.trim().is_empty() || !seen_call_ids.insert(call.call_id.as_str()) {
            return Err(ProviderError::InvalidResponse(
                "duplicate or empty tool call_id in provider response".into(),
            ));
        }
        if !allowed_tools.contains(call.name.as_str()) {
            return Err(ProviderError::InvalidResponse(format!(
                "provider invoked undeclared tool '{}'",
                call.name
            )));
        }
        if !call.arguments.is_object() {
            return Err(ProviderError::InvalidResponse(format!(
                "tool call '{}' arguments must be a JSON object",
                call.name
            )));
        }
    }
    if response.finish_reason == FinishReason::ToolCalls && response.tool_calls.is_empty() {
        return Err(ProviderError::InvalidResponse(
            "finish_reason 'tool_calls' requires at least one tool call".into(),
        ));
    }
    if response.finish_reason != FinishReason::ToolCalls && !response.tool_calls.is_empty() {
        return Err(ProviderError::InvalidResponse(
            "non-tool finish_reason must not carry tool calls".into(),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScriptedTurn {
    pub finish_reason: FinishReason,
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Optional invariant assertions evaluated deterministically against the live request.
    #[serde(default)]
    pub expected_last_tool_name: Option<String>,
    #[serde(default)]
    pub expected_message_substring: Option<String>,
}

impl ScriptedTurn {
    pub fn tool_call(
        content: impl Into<String>,
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        arguments: serde_json::Value,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Self {
        Self {
            finish_reason: FinishReason::ToolCalls,
            content: content.into(),
            tool_calls: vec![ToolCall {
                call_id: call_id.into(),
                name: tool_name.into(),
                arguments,
            }],
            input_tokens,
            output_tokens,
            expected_last_tool_name: None,
            expected_message_substring: None,
        }
    }

    pub fn tool_calls(
        content: impl Into<String>,
        tool_calls: Vec<ToolCall>,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Self {
        Self {
            finish_reason: FinishReason::ToolCalls,
            content: content.into(),
            tool_calls,
            input_tokens,
            output_tokens,
            expected_last_tool_name: None,
            expected_message_substring: None,
        }
    }

    pub fn with_expected_last_tool(mut self, tool_name: impl Into<String>) -> Self {
        self.expected_last_tool_name = Some(tool_name.into());
        self
    }

    pub fn with_expected_substring(mut self, needle: impl Into<String>) -> Self {
        self.expected_message_substring = Some(needle.into());
        self
    }

    pub fn final_stop(content: impl Into<String>, input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            finish_reason: FinishReason::Stop,
            content: content.into(),
            tool_calls: Vec::new(),
            input_tokens,
            output_tokens,
            expected_last_tool_name: None,
            expected_message_substring: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordedMockRequest {
    pub turn_index: usize,
    pub request_sha256: String,
    pub message_count: usize,
}

#[derive(Debug, Default)]
struct MockLlmState {
    next_index: usize,
    recorded_requests: Vec<RecordedMockRequest>,
}

/// Deterministic, zero-cost, offline scripted `LlmProvider` for CI and P2-03A verification.
/// All outputs are explicitly marked `simulated: true`.
#[derive(Debug)]
pub struct DeterministicMockLlm {
    capabilities: ProviderCapabilities,
    script: Vec<ScriptedTurn>,
    state: Mutex<MockLlmState>,
    spoof_model_id: Option<String>,
    force_non_simulated: bool,
}

impl DeterministicMockLlm {
    pub fn new(capabilities: ProviderCapabilities, script: Vec<ScriptedTurn>) -> Self {
        Self {
            capabilities,
            script,
            state: Mutex::new(MockLlmState::default()),
            spoof_model_id: None,
            force_non_simulated: false,
        }
    }

    pub fn coder_default(script: Vec<ScriptedTurn>) -> Self {
        Self::new(
            ProviderCapabilities::deterministic_mock(
                "coder-v1",
                vec![TaskRole::SceneCodeGeneration],
            ),
            script,
        )
    }

    /// Test-only helper simulating a broken provider that attempts silent model substitution.
    pub fn with_spoofed_response_model(mut self, spoofed_model_id: impl Into<String>) -> Self {
        self.spoof_model_id = Some(spoofed_model_id.into());
        self
    }

    /// Test-only helper simulating a response missing `simulated = true` flags.
    pub fn with_non_simulated_response(mut self) -> Self {
        self.force_non_simulated = true;
        self
    }

    pub fn recorded_requests(&self) -> Vec<RecordedMockRequest> {
        self.state
            .lock()
            .expect("mock llm mutex")
            .recorded_requests
            .clone()
    }
}

impl LlmProvider for DeterministicMockLlm {
    fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }

    fn generate(&self, request: &LlmRequest) -> Result<LlmResponse, ProviderError> {
        let request_sha256 = request.canonical_request_sha256()?;
        let mut guard = self.state.lock().map_err(|_| {
            ProviderError::Internal("deterministic mock llm state lock poisoned".into())
        })?;
        let turn_index = guard.next_index;
        let turn = self.script.get(turn_index).ok_or_else(|| {
            ProviderError::ScriptExhausted(format!(
                "no scripted turn configured at index {turn_index}"
            ))
        })?;

        if let Some(expected_tool) = turn.expected_last_tool_name.as_deref() {
            let observed = request
                .messages
                .iter()
                .rev()
                .find(|m| m.role == MessageRole::Tool)
                .and_then(|m| m.tool_name.as_deref());
            if observed != Some(expected_tool) {
                return Err(ProviderError::InvalidRequest(format!(
                    "scripted turn {turn_index} expected last tool '{expected_tool}', observed {observed:?}"
                )));
            }
        }
        if let Some(needle) = turn.expected_message_substring.as_deref() {
            let found = request.messages.iter().any(|m| m.content.contains(needle))
                || request.system_prompt.contains(needle);
            if !found {
                return Err(ProviderError::InvalidRequest(format!(
                    "scripted turn {turn_index} expected request text containing '{needle}'"
                )));
            }
        }

        guard.next_index += 1;
        guard.recorded_requests.push(RecordedMockRequest {
            turn_index,
            request_sha256: request_sha256.clone(),
            message_count: request.messages.len(),
        });

        let model_id = self
            .spoof_model_id
            .clone()
            .unwrap_or_else(|| request.model_id.clone());
        let response_id = format!(
            "mock-{}-{}-t{}-{}",
            request.provider_id,
            request.model_id,
            turn_index,
            &request_sha256[..12]
        );
        let is_simulated = !self.force_non_simulated;
        let usage = LlmUsage::new(turn.input_tokens, turn.output_tokens, true, is_simulated);
        let mut response = LlmResponse {
            schema_version: PROVIDER_SCHEMA_VERSION.into(),
            provider_id: request.provider_id.clone(),
            model_id,
            adapter_version: request.adapter_version.clone(),
            response_id,
            request_sha256,
            response_sha256: String::new(),
            finish_reason: turn.finish_reason,
            content: turn.content.clone(),
            tool_calls: turn.tool_calls.clone(),
            usage,
            simulated: is_simulated,
        };
        response.response_sha256 = response.compute_response_sha256()?;
        Ok(response)
    }
}

pub fn validate_base_parameters(p: &Parameters) -> Result<(), ProviderError> {
    if !p.temperature.is_finite() || !(0.0..=2.0).contains(&p.temperature) {
        return Err(ProviderError::UnsupportedParameter(
            "temperature must be finite and within 0.0..=2.0".into(),
        ));
    }
    if p.seed.is_some_and(|s| s > MAX_SEED_VALUE) {
        return Err(ProviderError::UnsupportedParameter(
            "seed must be within 0..=2147483647".into(),
        ));
    }
    Ok(())
}

pub fn validate_parameters_against_caps(
    caps: &ProviderCapabilities,
    p: &Parameters,
) -> Result<(), ProviderError> {
    validate_base_parameters(p)?;
    if !(caps.min_temperature..=caps.max_temperature).contains(&p.temperature) {
        return Err(ProviderError::UnsupportedParameter(format!(
            "temperature {} outside supported model range {}..={}",
            p.temperature, caps.min_temperature, caps.max_temperature
        )));
    }
    if p.seed.is_some() && !caps.supports_seed {
        return Err(ProviderError::UnsupportedParameter(format!(
            "model {}/{} does not support explicit seed; omitting or falling back is forbidden",
            caps.provider_id, caps.model_id
        )));
    }
    Ok(())
}

fn is_safe_identifier(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 128
        && v.bytes()
            .enumerate()
            .all(|(i, c)| c.is_ascii_alphanumeric() || (i > 0 && matches!(c, b'.' | b'_' | b'-')))
}

pub(crate) fn canonical_jcs_sha256<T: Serialize>(value: &T) -> Result<String, ProviderError> {
    let bytes = serde_jcs::to_vec(value)
        .map_err(|err| ProviderError::Internal(format!("JCS canonicalization failed: {err}")))?;
    let digest = Sha256::digest(&bytes);
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}
