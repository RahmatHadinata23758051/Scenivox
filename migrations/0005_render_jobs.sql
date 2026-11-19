-- P1-04 durable authorization envelope. No worker or renderer is installed.
CREATE TABLE render_jobs (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL,
    revision_id TEXT NOT NULL,
    approval_id TEXT NOT NULL,
    content_sha256 TEXT NOT NULL CHECK(length(content_sha256)=64 AND content_sha256 NOT GLOB '*[^0-9a-f]*'),
    snapshot_json TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('queued','preparing','voicing','animating','mixing','validating','succeeded','failed','cancelled','interrupted')),
    retry_of_job_id TEXT REFERENCES render_jobs(id),
    created_at TEXT NOT NULL,
    error_code TEXT,
    artifact_ids_json TEXT NOT NULL DEFAULT '[]',
    cost_consent_id TEXT,
    request_sha256 TEXT NOT NULL CHECK(length(request_sha256)=64 AND request_sha256 NOT GLOB '*[^0-9a-f]*'),
    UNIQUE(project_id,id),
    FOREIGN KEY(project_id,revision_id) REFERENCES revisions(project_id,id),
    FOREIGN KEY(approval_id) REFERENCES approvals(id),
    CHECK((status='failed' AND error_code IS NOT NULL) OR (status='interrupted' AND error_code IS NOT NULL) OR (status NOT IN ('failed','interrupted') AND error_code IS NULL)),
    CHECK(status!='succeeded' OR json_array_length(artifact_ids_json)>0)
);
CREATE INDEX render_jobs_project_created ON render_jobs(project_id,created_at,id);
CREATE TABLE render_job_idempotency (
    project_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_sha256 TEXT NOT NULL CHECK(length(request_sha256)=64 AND request_sha256 NOT GLOB '*[^0-9a-f]*'),
    job_id TEXT NOT NULL REFERENCES render_jobs(id),
    PRIMARY KEY(project_id,idempotency_key)
);
CREATE TRIGGER render_jobs_snapshot_matches_revision
BEFORE INSERT ON render_jobs
WHEN NOT EXISTS (
    SELECT 1 FROM revisions r
    JOIN approvals a ON a.project_id=r.project_id AND a.revision_id=r.id AND a.content_sha256=r.content_sha256
    JOIN projects p ON p.id=r.project_id
    JOIN revision_reviews rr ON rr.project_id=r.project_id AND rr.revision_id=r.id
    WHERE r.project_id=NEW.project_id AND r.id=NEW.revision_id
      AND r.content_sha256=NEW.content_sha256 AND a.id=NEW.approval_id
      AND r.content_json=NEW.snapshot_json
      AND p.current_revision_id=r.id AND p.approved_revision_id=r.id
      AND rr.review_state='approved' AND p.deleted_at IS NULL
)
BEGIN SELECT RAISE(ABORT,'render job snapshot must match approved revision'); END;
CREATE TRIGGER render_jobs_immutable_inputs
BEFORE UPDATE OF project_id,revision_id,approval_id,content_sha256,snapshot_json,retry_of_job_id,created_at,cost_consent_id,request_sha256 ON render_jobs
BEGIN SELECT RAISE(ABORT,'render job inputs are immutable'); END;
CREATE TRIGGER render_jobs_transition_guard
BEFORE UPDATE OF status ON render_jobs
WHEN NOT (
    (OLD.status='queued' AND NEW.status IN ('preparing','cancelled','interrupted')) OR
    (OLD.status='preparing' AND NEW.status IN ('voicing','failed','cancelled','interrupted')) OR
    (OLD.status='voicing' AND NEW.status IN ('animating','failed','cancelled','interrupted')) OR
    (OLD.status='animating' AND NEW.status IN ('mixing','failed','cancelled','interrupted')) OR
    (OLD.status='mixing' AND NEW.status IN ('validating','failed','cancelled','interrupted')) OR
    (OLD.status='validating' AND NEW.status IN ('succeeded','failed','cancelled','interrupted'))
)
BEGIN SELECT RAISE(ABORT,'invalid render job transition'); END;
CREATE TRIGGER render_jobs_no_delete BEFORE DELETE ON render_jobs
BEGIN SELECT RAISE(ABORT,'render job is durable'); END;
