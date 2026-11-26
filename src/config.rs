use std::{
    env,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    cost::{PricingSnapshot, compute_usage_cost_micros},
    openai_adapter::{CredentialSource, ModelTokenizationProfile, OpenAiCompatibleConfig},
    provider::{ProviderCapabilities, ProviderKind},
};

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid SCENIVOX_ADDR: {0}")]
    Address(#[from] std::net::AddrParseError),
    #[error("SCENIVOX_DATABASE_URL must use sqlite:")]
    InvalidDatabaseUrl,
    #[error("SCENIVOX_SKILL_STORE_DIR must not be empty")]
    InvalidSkillStoreDir,
    #[error("SCENIVOX_SKILL_IMPORT_ROOTS contains an empty entry")]
    InvalidSkillImportRoots,
    #[error("SCENIVOX_ASSET_STORE_DIR must not be empty")]
    InvalidAssetStoreDir,
    #[error("SCENIVOX_ASSET_IMPORT_ROOTS contains an empty entry")]
    InvalidAssetImportRoots,
    #[error("invalid SCENIVOX_LLM_* configuration: {0}")]
    InvalidLlmConfig(String),
}

/// Explicit opt-in configuration for live OpenAI-compatible inference preparation.
/// Defaults to disabled; never hardcodes provider credentials or vendor model names.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveLlmConfig {
    pub enabled: bool,
    pub provider_id: String,
    pub model_id: String,
    pub adapter_version: String,
    pub provider_kind: ProviderKind,
    pub base_url: String,
    pub endpoint_allowlist: Vec<String>,
    #[serde(default)]
    pub gateway_combo_routing: bool,
    #[serde(default)]
    pub tokenization_encoding: Option<String>,
    pub credential_env_var: String,
    pub request_timeout_ms: u64,
    pub connect_timeout_ms: u64,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub max_input_tokens: u64,
    pub max_output_tokens: u32,
    pub max_cost_micros: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposedLiveCallSummary {
    pub provider_id: String,
    pub model_id: String,
    pub adapter_version: String,
    pub endpoint_identity: String,
    pub endpoint_allowlist: Vec<String>,
    pub credential_env_var: String,
    pub canonical_request_sha256: String,
    pub conservative_input_tokens: u64,
    pub max_output_tokens: u32,
    pub estimated_max_cost_micros: i64,
    pub consent_budget_micros: i64,
}

impl LiveLlmConfig {
    pub fn from_env() -> Result<Option<Self>, ConfigError> {
        let enabled_raw = env::var("SCENIVOX_LLM_ENABLED").ok();
        let enabled = enabled_raw
            .as_deref()
            .is_some_and(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes"));
        let has_config_vars = [
            "SCENIVOX_LLM_BASE_URL",
            "SCENIVOX_LLM_PROVIDER_ID",
            "SCENIVOX_LLM_MODEL_ID",
            "SCENIVOX_LLM_API_KEY_ENV",
        ]
        .iter()
        .any(|k| env::var_os(k).is_some());
        if !enabled && !has_config_vars {
            return Ok(None);
        }
        let req = |key: &str| -> Result<String, ConfigError> {
            let v = env::var(key)
                .map_err(|_| ConfigError::InvalidLlmConfig(format!("missing {key}")))?;
            let t = v.trim().to_owned();
            if t.is_empty() {
                return Err(ConfigError::InvalidLlmConfig(format!(
                    "{key} must not be empty"
                )));
            }
            Ok(t)
        };
        let parse_u64 = |key: &str, default_val: u64| -> Result<u64, ConfigError> {
            match env::var(key) {
                Ok(v) if !v.trim().is_empty() => v.trim().parse::<u64>().map_err(|_| {
                    ConfigError::InvalidLlmConfig(format!("invalid integer for {key}"))
                }),
                _ => Ok(default_val),
            }
        };

        let base_url = req("SCENIVOX_LLM_BASE_URL")?;
        let allowlist = match env::var("SCENIVOX_LLM_ENDPOINT_ALLOWLIST") {
            Ok(raw) if !raw.trim().is_empty() => raw
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect(),
            _ => vec![base_url.clone()],
        };
        let kind = match env::var("SCENIVOX_LLM_PROVIDER_KIND")
            .unwrap_or_else(|_| "remote_api".into())
            .trim()
        {
            "remote_api" => ProviderKind::RemoteApi,
            "local_gateway_remote_upstream" => ProviderKind::LocalGatewayRemoteUpstream,
            "local_endpoint" => ProviderKind::LocalEndpoint,
            other => {
                return Err(ConfigError::InvalidLlmConfig(format!(
                    "unsupported SCENIVOX_LLM_PROVIDER_KIND '{other}'"
                )));
            }
        };

        let gateway_combo_routing = env::var("SCENIVOX_LLM_GATEWAY_COMBO_ROUTING")
            .ok()
            .is_some_and(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes"));
        let tokenization_encoding = env::var("SCENIVOX_LLM_TOKENIZATION_ENCODING")
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty());
        let credential_env_var = match kind {
            ProviderKind::RemoteApi => req("SCENIVOX_LLM_API_KEY_ENV")?,
            ProviderKind::LocalGatewayRemoteUpstream | ProviderKind::LocalEndpoint => {
                env::var("SCENIVOX_LLM_API_KEY_ENV")
                    .ok()
                    .map(|v| v.trim().to_owned())
                    .unwrap_or_default()
            }
            ProviderKind::DeterministicScriptedMock => String::new(),
        };

        let cfg = Self {
            enabled,
            provider_id: req("SCENIVOX_LLM_PROVIDER_ID")?,
            model_id: req("SCENIVOX_LLM_MODEL_ID")?,
            adapter_version: env::var("SCENIVOX_LLM_ADAPTER_VERSION")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "1.0.0".into()),
            provider_kind: kind,
            base_url,
            endpoint_allowlist: allowlist,
            gateway_combo_routing,
            tokenization_encoding,
            credential_env_var,
            request_timeout_ms: parse_u64("SCENIVOX_LLM_REQUEST_TIMEOUT_MS", 30_000)?,
            connect_timeout_ms: parse_u64("SCENIVOX_LLM_CONNECT_TIMEOUT_MS", 5_000)?,
            max_request_bytes: usize::try_from(parse_u64(
                "SCENIVOX_LLM_MAX_REQUEST_BYTES",
                128 * 1024,
            )?)
            .map_err(|_| ConfigError::InvalidLlmConfig("max_request_bytes overflow".into()))?,
            max_response_bytes: usize::try_from(parse_u64(
                "SCENIVOX_LLM_MAX_RESPONSE_BYTES",
                256 * 1024,
            )?)
            .map_err(|_| ConfigError::InvalidLlmConfig("max_response_bytes overflow".into()))?,
            max_input_tokens: parse_u64("SCENIVOX_LLM_MAX_INPUT_TOKENS", 4_096)?,
            max_output_tokens: u32::try_from(parse_u64("SCENIVOX_LLM_MAX_OUTPUT_TOKENS", 1_024)?)
                .map_err(|_| {
                ConfigError::InvalidLlmConfig("max_output_tokens overflow".into())
            })?,
            max_cost_micros: i64::try_from(parse_u64("SCENIVOX_LLM_MAX_COST_MICROS", 50_000)?)
                .map_err(|_| ConfigError::InvalidLlmConfig("max_cost_micros overflow".into()))?,
        };
        cfg.validate()?;
        Ok(Some(cfg))
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.max_input_tokens == 0 || self.max_output_tokens == 0 || self.max_cost_micros <= 0 {
            return Err(ConfigError::InvalidLlmConfig(
                "token ceilings and max_cost_micros must be strictly positive".into(),
            ));
        }
        if self.endpoint_allowlist.is_empty() {
            return Err(ConfigError::InvalidLlmConfig(
                "endpoint_allowlist must not be empty".into(),
            ));
        }
        let require_tls_only = self.provider_kind == ProviderKind::RemoteApi;
        let normalized_base =
            crate::openai_adapter::validate_endpoint_url(&self.base_url, require_tls_only)
                .map_err(|e| ConfigError::InvalidLlmConfig(e.to_string()))?;
        for allowed in &self.endpoint_allowlist {
            crate::openai_adapter::validate_endpoint_url(allowed, require_tls_only)
                .map_err(|e| ConfigError::InvalidLlmConfig(e.to_string()))?;
        }
        if !crate::openai_adapter::endpoint_matches_allowlist(
            &normalized_base,
            &self.endpoint_allowlist,
            require_tls_only,
        )
        .map_err(|e| ConfigError::InvalidLlmConfig(e.to_string()))?
        {
            return Err(ConfigError::InvalidLlmConfig(
                "base_url is not permitted by endpoint_allowlist".into(),
            ));
        }
        if self.gateway_combo_routing
            && self.provider_kind != ProviderKind::LocalGatewayRemoteUpstream
        {
            return Err(ConfigError::InvalidLlmConfig(
                "gateway_combo_routing is permitted only for local_gateway_remote_upstream".into(),
            ));
        }
        if self.provider_kind == ProviderKind::RemoteApi
            || !self.credential_env_var.trim().is_empty()
        {
            let _ = CredentialSource::env_var(&self.credential_env_var)
                .map_err(|e| ConfigError::InvalidLlmConfig(e.to_string()))?;
        }
        if let Some(enc) = &self.tokenization_encoding {
            if ModelTokenizationProfile::for_verified_encoding(
                &self.model_id,
                enc,
                self.gateway_combo_routing,
            )
            .is_none()
            {
                return Err(ConfigError::InvalidLlmConfig(format!(
                    "unsupported SCENIVOX_LLM_TOKENIZATION_ENCODING '{enc}'"
                )));
            }
        }
        Ok(())
    }

    /// Fails closed unless live/gateway inference was explicitly authorized (`SCENIVOX_LLM_ENABLED=true`).
    pub fn ensure_live_dispatch_enabled(&self) -> Result<(), ConfigError> {
        self.validate()?;
        if !self.enabled {
            return Err(ConfigError::InvalidLlmConfig(
                "remote/gateway LLM inference is disabled (set SCENIVOX_LLM_ENABLED=true after CEO approval)"
                    .into(),
            ));
        }
        Ok(())
    }

    pub fn resolve_credential_source(&self) -> Result<CredentialSource, ConfigError> {
        self.validate()?;
        if self.credential_env_var.trim().is_empty() {
            Ok(CredentialSource::None)
        } else {
            CredentialSource::env_var(&self.credential_env_var)
                .map_err(|e| ConfigError::InvalidLlmConfig(e.to_string()))
        }
    }

    pub fn to_adapter_config(
        &self,
        capabilities: ProviderCapabilities,
        tokenization_profile: Option<ModelTokenizationProfile>,
    ) -> Result<OpenAiCompatibleConfig, ConfigError> {
        self.validate()?;
        let resolved_profile = match tokenization_profile {
            Some(p) => Some(p),
            None => self.tokenization_encoding.as_deref().and_then(|enc| {
                ModelTokenizationProfile::for_verified_encoding(
                    &self.model_id,
                    enc,
                    self.gateway_combo_routing,
                )
            }),
        };
        let cfg = OpenAiCompatibleConfig {
            capabilities,
            base_url: self.base_url.clone(),
            endpoint_allowlist: self.endpoint_allowlist.clone(),
            gateway_combo_routing: self.gateway_combo_routing,
            tokenization_profile: resolved_profile,
            credential: self.resolve_credential_source()?,
            request_timeout: Duration::from_millis(self.request_timeout_ms),
            max_request_bytes: self.max_request_bytes,
            max_response_bytes: self.max_response_bytes,
            extra_headers: Vec::new(),
        };
        cfg.validate()
            .map_err(|e| ConfigError::InvalidLlmConfig(e.to_string()))?;
        Ok(cfg)
    }

    pub fn summarize_proposed_call(
        &self,
        canonical_request_sha256: &str,
        conservative_input_tokens: u64,
        max_output_tokens: u32,
        pricing: &PricingSnapshot,
    ) -> Result<ProposedLiveCallSummary, ConfigError> {
        self.validate()?;
        if conservative_input_tokens > self.max_input_tokens
            || max_output_tokens > self.max_output_tokens
        {
            return Err(ConfigError::InvalidLlmConfig(
                "proposed token counts exceed configured LiveLlmConfig ceilings".into(),
            ));
        }
        let max_cost = compute_usage_cost_micros(
            pricing,
            conservative_input_tokens,
            u64::from(max_output_tokens),
        )
        .map_err(|e| ConfigError::InvalidLlmConfig(e.to_string()))?;
        if max_cost > self.max_cost_micros {
            return Err(ConfigError::InvalidLlmConfig(format!(
                "estimated worst-case cost {max_cost} micros exceeds budget {}",
                self.max_cost_micros
            )));
        }
        Ok(ProposedLiveCallSummary {
            provider_id: self.provider_id.clone(),
            model_id: self.model_id.clone(),
            adapter_version: self.adapter_version.clone(),
            endpoint_identity: self.base_url.trim_end_matches('/').to_owned(),
            endpoint_allowlist: self.endpoint_allowlist.clone(),
            credential_env_var: self.credential_env_var.clone(),
            canonical_request_sha256: canonical_request_sha256.to_owned(),
            conservative_input_tokens,
            max_output_tokens,
            estimated_max_cost_micros: max_cost,
            consent_budget_micros: self.max_cost_micros,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub address: SocketAddr,
    pub database_url: String,
    pub skill_store_dir: PathBuf,
    pub trusted_skill_import_roots: Vec<PathBuf>,
    pub asset_store_dir: PathBuf,
    pub trusted_asset_import_roots: Vec<PathBuf>,
    pub live_llm: Option<LiveLlmConfig>,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let mut config = Self::from_values_with_stores(
            env::var("SCENIVOX_ADDR").ok().as_deref(),
            env::var("SCENIVOX_DATABASE_URL").ok().as_deref(),
            env::var("SCENIVOX_SKILL_STORE_DIR").ok().as_deref(),
            env::var("SCENIVOX_ASSET_STORE_DIR").ok().as_deref(),
        )?;
        if let Some(raw_roots) = env::var_os("SCENIVOX_SKILL_IMPORT_ROOTS") {
            if !raw_roots.is_empty() {
                config.trusted_skill_import_roots =
                    parse_import_roots(&raw_roots).ok_or(ConfigError::InvalidSkillImportRoots)?;
            }
        }
        if let Some(raw_roots) = env::var_os("SCENIVOX_ASSET_IMPORT_ROOTS") {
            if !raw_roots.is_empty() {
                config.trusted_asset_import_roots =
                    parse_import_roots(&raw_roots).ok_or(ConfigError::InvalidAssetImportRoots)?;
            }
        }
        config.live_llm = LiveLlmConfig::from_env()?;
        Ok(config)
    }

    pub fn from_values(
        address: Option<&str>,
        database_url: Option<&str>,
    ) -> Result<Self, ConfigError> {
        Self::from_values_with_skill_store(address, database_url, None)
    }

    pub fn from_values_with_skill_store(
        address: Option<&str>,
        database_url: Option<&str>,
        skill_store_dir: Option<&str>,
    ) -> Result<Self, ConfigError> {
        Self::from_values_with_stores(address, database_url, skill_store_dir, None)
    }

    pub fn from_values_with_stores(
        address: Option<&str>,
        database_url: Option<&str>,
        skill_store_dir: Option<&str>,
        asset_store_dir: Option<&str>,
    ) -> Result<Self, ConfigError> {
        let address = address.unwrap_or("127.0.0.1:3000").parse()?;
        let database_url = database_url.unwrap_or("sqlite://scenivox.db?mode=rwc");
        if !database_url.starts_with("sqlite:") {
            return Err(ConfigError::InvalidDatabaseUrl);
        }

        let skill_store_dir = match skill_store_dir {
            Some(raw) => {
                if raw.trim().is_empty() {
                    return Err(ConfigError::InvalidSkillStoreDir);
                }
                PathBuf::from(raw)
            }
            None => default_store_dir(database_url, "skills"),
        };

        let asset_store_dir = match asset_store_dir {
            Some(raw) => {
                if raw.trim().is_empty() {
                    return Err(ConfigError::InvalidAssetStoreDir);
                }
                PathBuf::from(raw)
            }
            None => default_store_dir(database_url, "assets"),
        };

        Ok(Self {
            address,
            database_url: database_url.to_owned(),
            skill_store_dir,
            trusted_skill_import_roots: Vec::new(),
            asset_store_dir,
            trusted_asset_import_roots: Vec::new(),
            live_llm: None,
        })
    }
}

fn parse_import_roots(raw_roots: &std::ffi::OsStr) -> Option<Vec<PathBuf>> {
    let mut roots = Vec::new();
    for path in env::split_paths(raw_roots) {
        if path.as_os_str().is_empty() {
            return None;
        }
        roots.push(path);
    }
    Some(roots)
}

pub(crate) fn default_store_dir(database_url: &str, folder: &str) -> PathBuf {
    let raw_path = database_url
        .strip_prefix("sqlite://")
        .or_else(|| database_url.strip_prefix("sqlite:"))
        .unwrap_or("")
        .split('?')
        .next()
        .unwrap_or("");
    if !raw_path.is_empty() && raw_path != ":memory:" {
        if let Some(parent) = Path::new(raw_path).parent() {
            if !parent.as_os_str().is_empty() {
                return parent.join(folder);
            }
        }
    }
    PathBuf::from(folder)
}
