//! Scoped CostConsent, verified pricing snapshots, integer-micros accounting, and atomic budget reservation ledger (ADR-02 / P2-02B).
//!
//! # Trust Boundary
//! - Model pricing registration (`set_model_pricing`) and `CostConsent` issuance/revocation
//!   (`create_cost_consent`, `revoke_cost_consent`) are strictly **trusted host/operator actions**.
//!   They must never be exposed as agent/skill tools or triggered autonomously during execution.
//! - Pre-call reservation and reconciliation require an active, unexpired exclusive `WorkerLease`
//!   matching `(owner_id, lease_epoch)` and exact pricing snapshot equality without silent repricing.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const COST_SCHEMA_VERSION: &str = "1";
pub const MAX_COST_SCOPES: usize = 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostScope {
    ProviderProbe,
    StoryboardPlanning,
    SceneCodeGeneration,
    VoiceTts,
    RenderJob,
}

impl CostScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProviderProbe => "provider_probe",
            Self::StoryboardPlanning => "storyboard_planning",
            Self::SceneCodeGeneration => "scene_code_generation",
            Self::VoiceTts => "voice_tts",
            Self::RenderJob => "render_job",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "provider_probe" => Some(Self::ProviderProbe),
            "storyboard_planning" => Some(Self::StoryboardPlanning),
            "scene_code_generation" => Some(Self::SceneCodeGeneration),
            "voice_tts" => Some(Self::VoiceTts),
            "render_job" => Some(Self::RenderJob),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostConsentStatus {
    Active,
    Revoked,
    Halted,
}

impl CostConsentStatus {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "active" => Some(Self::Active),
            "revoked" => Some(Self::Revoked),
            "halted" => Some(Self::Halted),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReservationStatus {
    Reserved,
    Settled,
    SettledConservative,
    SettledOverage,
}

impl ReservationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Settled => "settled",
            Self::SettledConservative => "settled_conservative",
            Self::SettledOverage => "settled_overage",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "reserved" => Some(Self::Reserved),
            "settled" => Some(Self::Settled),
            "settled_conservative" => Some(Self::SettledConservative),
            "settled_overage" => Some(Self::SettledOverage),
            _ => None,
        }
    }
}

/// Distinguishes mock/fixture transport outputs from genuine remote inference (direct TLS or local gateway forwarding to remote upstreams).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceProvenance {
    DeterministicMockFixture,
    ControlledLocalHttpFixture,
    LocalGatewayRemoteInference,
    RealRemoteTlsInference,
}

impl InferenceProvenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeterministicMockFixture => "deterministic_mock_fixture",
            Self::ControlledLocalHttpFixture => "controlled_local_http_fixture",
            Self::LocalGatewayRemoteInference => "local_gateway_remote_inference",
            Self::RealRemoteTlsInference => "real_remote_tls_inference",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "deterministic_mock_fixture" => Some(Self::DeterministicMockFixture),
            "controlled_local_http_fixture" => Some(Self::ControlledLocalHttpFixture),
            "local_gateway_remote_inference" => Some(Self::LocalGatewayRemoteInference),
            "real_remote_tls_inference" => Some(Self::RealRemoteTlsInference),
            _ => None,
        }
    }

    pub fn is_real_remote_inference(self) -> bool {
        matches!(
            self,
            Self::RealRemoteTlsInference | Self::LocalGatewayRemoteInference
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PricingSnapshot {
    pub pricing_version: String,
    pub currency: String,
    pub input_cost_per_token_micros: i64,
    pub output_cost_per_token_micros: i64,
    pub request_fixed_cost_micros: i64,
    pub verified: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetModelPricing {
    pub schema_version: String,
    pub provider_id: String,
    pub model_id: String,
    pub pricing_version: String,
    pub currency: String,
    pub input_cost_per_token_micros: i64,
    pub output_cost_per_token_micros: i64,
    pub request_fixed_cost_micros: i64,
    pub verified: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPricingRecord {
    pub schema_version: String,
    pub provider_id: String,
    pub model_id: String,
    pub pricing: PricingSnapshot,
    pub pricing_sha256: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateCostConsent {
    pub schema_version: String,
    pub project_id: Option<String>,
    pub provider_id: String,
    pub model_id: String,
    pub scopes: Vec<CostScope>,
    pub max_cost_micros: i64,
    #[serde(default)]
    pub max_total_tokens: Option<i64>,
    pub expires_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeCostConsent {
    pub schema_version: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostConsent {
    pub schema_version: String,
    pub id: String,
    pub project_id: Option<String>,
    pub provider_id: String,
    pub model_id: String,
    pub scopes: Vec<CostScope>,
    pub max_cost_micros: i64,
    pub max_total_tokens: Option<i64>,
    pub spent_micros: i64,
    pub reserved_micros: i64,
    pub spent_tokens: i64,
    pub reserved_tokens: i64,
    pub pricing: PricingSnapshot,
    pub pricing_sha256: String,
    pub status: CostConsentStatus,
    pub halt_reason: Option<String>,
    pub created_at: String,
    pub expires_at: String,
    pub revoked_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReserveBudgetRequest {
    pub cost_consent_id: String,
    pub project_id: Option<String>,
    pub job_id: Option<String>,
    pub provider_id: String,
    pub model_id: String,
    pub scope: CostScope,
    pub owner_id: String,
    pub lease_epoch: i64,
    pub canonical_request_sha256: String,
    pub model_snapshot_sha256: String,
    pub endpoint_identity: String,
    pub estimated_input_tokens: i64,
    pub max_output_tokens: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimDispatchBinding {
    pub project_id: Option<String>,
    pub job_id: Option<String>,
    pub provider_id: String,
    pub model_id: String,
    pub scope: CostScope,
    pub canonical_request_sha256: String,
    pub model_snapshot_sha256: String,
    pub endpoint_identity: String,
    pub max_output_tokens: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetReservation {
    pub schema_version: String,
    pub id: String,
    pub cost_consent_id: String,
    pub project_id: Option<String>,
    pub job_id: Option<String>,
    pub provider_id: String,
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_upstream_model: Option<String>,
    pub scope: CostScope,
    pub owner_id: String,
    pub lease_epoch: i64,
    pub canonical_request_sha256: String,
    pub model_snapshot_sha256: String,
    pub endpoint_identity: String,
    pub estimated_input_tokens: i64,
    pub max_output_tokens: i64,
    pub reserved_micros: i64,
    pub reserved_tokens: i64,
    pub actual_input_tokens: Option<i64>,
    pub actual_output_tokens: Option<i64>,
    pub actual_micros: Option<i64>,
    pub overage_micros: i64,
    pub pricing: PricingSnapshot,
    pub pricing_sha256: String,
    pub status: ReservationStatus,
    pub dispatch_claimed: bool,
    pub dispatch_claimed_at: Option<String>,
    pub dispatch_authorized: bool,
    pub inference_provenance: Option<InferenceProvenance>,
    pub error_code: Option<String>,
    pub created_at: String,
    pub settled_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderUsageReport {
    /// Authoritative usage returned by the provider adapter.
    Verified {
        input_tokens: u64,
        output_tokens: u64,
    },
    /// Provider omitted usage metrics or failed mid-flight; triggers conservative settlement at `reserved_micros`.
    MissingOrUnverifiable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconcileBudgetRequest {
    pub reservation_id: String,
    pub owner_id: String,
    pub lease_epoch: i64,
    pub inference_provenance: Option<InferenceProvenance>,
    pub usage: ProviderUsageReport,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerLease {
    pub owner_id: String,
    pub lease_epoch: i64,
    pub acquired_at: String,
    pub heartbeat_at: String,
    pub expires_at: String,
}

/// Persisted endpoint-bound model probe proving live or controlled fixture availability.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelAvailabilityEvidence {
    pub provider_id: String,
    pub model_id: String,
    pub adapter_version: String,
    pub endpoint_identity: String,
    pub availability: String,
    pub probe_provenance: InferenceProvenance,
    pub probe_sha256: String,
    pub echoed_model_id: String,
    pub verified_at: String,
    pub expires_at: String,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordModelAvailabilityProbe {
    pub provider_id: String,
    pub model_id: String,
    pub adapter_version: String,
    pub endpoint_identity: String,
    pub availability: String,
    pub probe_provenance: InferenceProvenance,
    pub probe_sha256: String,
    pub echoed_model_id: String,
    pub expires_at: String,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostGateDecision {
    /// Selected models are zero-cost offline (`remote: false`); no paid `CostConsent` required.
    ZeroCostOffline,
    /// Selected model(s) are remote (`remote: true`) and an active, matching `CostConsent` was verified.
    RemoteConsentVerified { cost_consent_id: String },
}

#[derive(Debug, Error)]
pub enum CostError {
    #[error("verified provider pricing is unavailable")]
    PricingUnavailable,
    #[error("requested cost or token usage exceeds limit or integer bounds")]
    CostLimitExceeded,
}

/// Validates integer pricing fields (`currency == "USD"`, non-negative rates).
pub fn validate_pricing_snapshot(pricing: &PricingSnapshot) -> bool {
    !pricing.pricing_version.trim().is_empty()
        && pricing.pricing_version.len() <= 128
        && pricing.currency == "USD"
        && pricing.input_cost_per_token_micros >= 0
        && pricing.output_cost_per_token_micros >= 0
        && pricing.request_fixed_cost_micros >= 0
}

/// Computes worst-case or actual cost in integer USD micros (`10^-6 USD`) with checked arithmetic.
/// Fails closed if pricing is unverified or if any multiplication/addition overflows signed `i64`.
pub fn compute_usage_cost_micros(
    pricing: &PricingSnapshot,
    input_tokens: u64,
    output_tokens: u64,
) -> Result<i64, CostError> {
    if !pricing.verified || !validate_pricing_snapshot(pricing) {
        return Err(CostError::PricingUnavailable);
    }
    let in_rate = u64::try_from(pricing.input_cost_per_token_micros)
        .map_err(|_| CostError::CostLimitExceeded)?;
    let out_rate = u64::try_from(pricing.output_cost_per_token_micros)
        .map_err(|_| CostError::CostLimitExceeded)?;
    let fixed = u64::try_from(pricing.request_fixed_cost_micros)
        .map_err(|_| CostError::CostLimitExceeded)?;

    let in_cost = input_tokens
        .checked_mul(in_rate)
        .ok_or(CostError::CostLimitExceeded)?;
    let out_cost = output_tokens
        .checked_mul(out_rate)
        .ok_or(CostError::CostLimitExceeded)?;
    let total_u64 = fixed
        .checked_add(in_cost)
        .and_then(|v| v.checked_add(out_cost))
        .ok_or(CostError::CostLimitExceeded)?;
    i64::try_from(total_u64).map_err(|_| CostError::CostLimitExceeded)
}

/// Validates strict UTC RFC-3339 seconds timestamp format `YYYY-MM-DDTHH:MM:SSZ`.
pub fn is_valid_utc_timestamp(ts: &str) -> bool {
    let b = ts.as_bytes();
    if b.len() != 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'Z'
    {
        return false;
    }
    let Some(year) = parse_digits(&b[0..4]) else {
        return false;
    };
    let Some(month) = parse_digits(&b[5..7]) else {
        return false;
    };
    let Some(day) = parse_digits(&b[8..10]) else {
        return false;
    };
    let Some(hour) = parse_digits(&b[11..13]) else {
        return false;
    };
    let Some(minute) = parse_digits(&b[14..16]) else {
        return false;
    };
    let Some(second) = parse_digits(&b[17..19]) else {
        return false;
    };
    if !(1..=12).contains(&month) || hour > 23 || minute > 59 || second > 59 {
        return false;
    }
    let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=max_day).contains(&day)
}

fn parse_digits(bytes: &[u8]) -> Option<u32> {
    if !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse::<u32>().ok()
}
