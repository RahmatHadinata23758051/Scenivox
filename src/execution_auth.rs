//! Single-use execution authorization bound to exact approved revision, pinned skill bundle, and sandbox policy hashes,
//! cleanly separated from post-execution workspace evidence (ADR-02 / P2-02B).

use serde::{Deserialize, Serialize};

pub const EXECUTION_AUTH_SCHEMA_VERSION: &str = "1";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionAuthStatus {
    Issued,
    Claimed,
    EvidenceRecorded,
}

impl ExecutionAuthStatus {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "issued" => Some(Self::Issued),
            "claimed" => Some(Self::Claimed),
            "evidence_recorded" => Some(Self::EvidenceRecorded),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueExecutionAuthorization {
    pub schema_version: String,
    pub project_id: String,
    pub revision_id: String,
    pub content_sha256: String,
    pub skill_id: String,
    pub skill_version: String,
    pub bundle_sha256: String,
    pub runtime_policy_sha256: String,
    pub expires_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionAuthorization {
    pub schema_version: String,
    pub id: String,
    pub project_id: String,
    pub revision_id: String,
    pub approval_id: String,
    pub content_sha256: String,
    pub skill_id: String,
    pub skill_version: String,
    pub bundle_sha256: String,
    pub runtime_policy_sha256: String,
    pub status: ExecutionAuthStatus,
    pub claimed_job_id: Option<String>,
    pub issued_at: String,
    pub expires_at: String,
    pub claimed_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimExecutionAuthorization {
    pub schema_version: String,
    pub job_id: String,
    pub project_id: String,
    pub revision_id: String,
    pub content_sha256: String,
    pub skill_id: String,
    pub skill_version: String,
    pub bundle_sha256: String,
    pub runtime_policy_sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordWorkspaceEvidence {
    pub schema_version: String,
    pub job_id: String,
    pub workspace_manifest_sha256: String,
    #[serde(default)]
    pub output_artifact_sha256: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionWorkspaceEvidence {
    pub schema_version: String,
    pub id: String,
    pub execution_auth_id: String,
    pub project_id: String,
    pub revision_id: String,
    pub job_id: String,
    pub workspace_manifest_sha256: String,
    pub output_artifact_sha256: Option<String>,
    pub recorded_at: String,
}
