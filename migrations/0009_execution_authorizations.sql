-- P2-02B Commit 3: Single-use ExecutionAuthorization and separated append-only ExecutionWorkspaceEvidence (additive).
CREATE TABLE execution_authorizations (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL,
    revision_id TEXT NOT NULL,
    approval_id TEXT NOT NULL REFERENCES approvals(id),
    content_sha256 TEXT NOT NULL CHECK(length(content_sha256)=64 AND content_sha256 NOT GLOB '*[^0-9a-f]*'),
    skill_id TEXT NOT NULL,
    skill_version TEXT NOT NULL,
    bundle_sha256 TEXT NOT NULL CHECK(length(bundle_sha256)=64 AND bundle_sha256 NOT GLOB '*[^0-9a-f]*'),
    runtime_policy_sha256 TEXT NOT NULL CHECK(length(runtime_policy_sha256)=64 AND runtime_policy_sha256 NOT GLOB '*[^0-9a-f]*'),
    status TEXT NOT NULL DEFAULT 'issued' CHECK(status IN ('issued', 'claimed', 'evidence_recorded')),
    claimed_job_id TEXT UNIQUE,
    issued_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    claimed_at TEXT,
    FOREIGN KEY(project_id, revision_id) REFERENCES revisions(project_id, id),
    FOREIGN KEY(skill_id, skill_version) REFERENCES skill_bundles(skill_id, skill_version),
    CHECK(
        (status = 'issued' AND claimed_job_id IS NULL AND claimed_at IS NULL)
        OR (status IN ('claimed', 'evidence_recorded') AND claimed_job_id IS NOT NULL AND claimed_at IS NOT NULL)
    )
);
CREATE INDEX execution_authorizations_project ON execution_authorizations(project_id, revision_id, issued_at, id);

CREATE TRIGGER execution_authorizations_immutable_terms
BEFORE UPDATE OF id, project_id, revision_id, approval_id, content_sha256, skill_id, skill_version, bundle_sha256, runtime_policy_sha256, issued_at, expires_at ON execution_authorizations
BEGIN
    SELECT RAISE(ABORT, 'execution authorization terms are immutable');
END;

CREATE TRIGGER execution_authorizations_transition_guard
BEFORE UPDATE ON execution_authorizations
WHEN NOT (
    (OLD.status = 'issued' AND NEW.status = 'claimed' AND OLD.claimed_job_id IS NULL AND NEW.claimed_job_id IS NOT NULL)
    OR (OLD.status = 'claimed' AND NEW.status = 'evidence_recorded' AND NEW.claimed_job_id = OLD.claimed_job_id AND NEW.claimed_at = OLD.claimed_at)
)
BEGIN
    SELECT RAISE(ABORT, 'invalid execution authorization transition');
END;

CREATE TRIGGER execution_authorizations_no_delete
BEFORE DELETE ON execution_authorizations
BEGIN
    SELECT RAISE(ABORT, 'execution authorizations are durable');
END;

CREATE TABLE execution_workspace_evidence (
    id TEXT PRIMARY KEY NOT NULL,
    execution_auth_id TEXT NOT NULL UNIQUE REFERENCES execution_authorizations(id),
    project_id TEXT NOT NULL,
    revision_id TEXT NOT NULL,
    job_id TEXT NOT NULL UNIQUE,
    workspace_manifest_sha256 TEXT NOT NULL CHECK(length(workspace_manifest_sha256)=64 AND workspace_manifest_sha256 NOT GLOB '*[^0-9a-f]*'),
    output_artifact_sha256 TEXT CHECK(output_artifact_sha256 IS NULL OR (length(output_artifact_sha256)=64 AND output_artifact_sha256 NOT GLOB '*[^0-9a-f]*')),
    recorded_at TEXT NOT NULL,
    FOREIGN KEY(project_id, revision_id) REFERENCES revisions(project_id, id)
);

CREATE TRIGGER execution_workspace_evidence_immutable
BEFORE UPDATE ON execution_workspace_evidence
BEGIN
    SELECT RAISE(ABORT, 'workspace evidence is immutable');
END;

CREATE TRIGGER execution_workspace_evidence_no_delete
BEFORE DELETE ON execution_workspace_evidence
BEGIN
    SELECT RAISE(ABORT, 'workspace evidence is durable');
END;
