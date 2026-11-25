-- P2-01B additive skill bundle registry. Existing v1 tables, revisions, and fail-closed render gates are untouched.
CREATE TABLE skill_bundles (
    skill_id TEXT NOT NULL,
    skill_version TEXT NOT NULL,
    bundle_sha256 TEXT NOT NULL UNIQUE CHECK(length(bundle_sha256)=64 AND bundle_sha256 NOT GLOB '*[^0-9a-f]*'),
    manifest_json TEXT NOT NULL,
    layout_json TEXT NOT NULL,
    storage_rel_dir TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1 CHECK(enabled IN (0,1)),
    audit_status TEXT NOT NULL DEFAULT 'unaudited' CHECK(audit_status IN ('unaudited','static_audited','smoke_verified','rejected')),
    audit_evidence_json TEXT,
    file_count INTEGER NOT NULL CHECK(file_count > 0),
    total_bytes INTEGER NOT NULL CHECK(total_bytes > 0),
    imported_at TEXT NOT NULL,
    PRIMARY KEY(skill_id, skill_version)
);
CREATE INDEX skill_bundles_order ON skill_bundles(skill_id, skill_version);

CREATE TABLE skill_import_idempotency (
    idempotency_key TEXT PRIMARY KEY NOT NULL,
    request_sha256 TEXT NOT NULL CHECK(length(request_sha256)=64 AND request_sha256 NOT GLOB '*[^0-9a-f]*'),
    skill_id TEXT NOT NULL,
    skill_version TEXT NOT NULL,
    response_json TEXT NOT NULL,
    FOREIGN KEY(skill_id, skill_version) REFERENCES skill_bundles(skill_id, skill_version)
);

CREATE TRIGGER skill_bundles_immutable_identity
BEFORE UPDATE OF skill_id, skill_version, bundle_sha256, manifest_json, layout_json, storage_rel_dir, file_count, total_bytes, imported_at ON skill_bundles
BEGIN
    SELECT RAISE(ABORT, 'skill bundle contents are immutable');
END;

CREATE TRIGGER skill_bundles_no_delete
BEFORE DELETE ON skill_bundles
BEGIN
    SELECT RAISE(ABORT, 'skill bundle records are durable');
END;
