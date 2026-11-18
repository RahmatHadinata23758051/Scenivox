-- P1-03 separates mutable workflow state from immutable revision snapshots.
CREATE TABLE revision_reviews (
    project_id TEXT NOT NULL,
    revision_id TEXT NOT NULL,
    review_state TEXT NOT NULL CHECK(review_state IN ('draft','in_review','approved')),
    updated_at TEXT NOT NULL,
    PRIMARY KEY(project_id, revision_id),
    FOREIGN KEY(project_id, revision_id) REFERENCES revisions(project_id, id)
);
INSERT INTO revision_reviews(project_id, revision_id, review_state, updated_at)
SELECT project_id, id, 'draft', created_at FROM revisions;

CREATE TABLE approvals (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL,
    revision_id TEXT NOT NULL,
    content_sha256 TEXT NOT NULL CHECK(length(content_sha256)=64 AND content_sha256 NOT GLOB '*[^0-9a-f]*'),
    approved_at TEXT NOT NULL,
    UNIQUE(project_id, revision_id, content_sha256),
    FOREIGN KEY(project_id, revision_id) REFERENCES revisions(project_id, id)
);
CREATE INDEX approvals_revision ON approvals(project_id, revision_id, approved_at, id);

CREATE TRIGGER approvals_insert_requires_current_review
BEFORE INSERT ON approvals
WHEN NOT EXISTS (
    SELECT 1 FROM revision_reviews rr
    JOIN revisions r ON r.project_id=rr.project_id AND r.id=rr.revision_id
    JOIN projects p ON p.id=rr.project_id
    WHERE rr.project_id=NEW.project_id AND rr.revision_id=NEW.revision_id
      AND rr.review_state='in_review'
      AND p.current_revision_id=NEW.revision_id
      AND r.content_sha256=NEW.content_sha256
      AND p.deleted_at IS NULL
)
BEGIN SELECT RAISE(ABORT,'approval requires current in-review revision and exact hash'); END;

CREATE TRIGGER revision_review_transition_guard
BEFORE UPDATE OF review_state ON revision_reviews
WHEN NOT (
    (OLD.review_state='draft' AND NEW.review_state='in_review') OR
    (OLD.review_state='in_review' AND NEW.review_state='draft') OR
    (OLD.review_state='in_review' AND NEW.review_state='approved' AND EXISTS (
        SELECT 1 FROM approvals a WHERE a.project_id=NEW.project_id
          AND a.revision_id=NEW.revision_id
          AND a.content_sha256=(SELECT content_sha256 FROM revisions
                                WHERE project_id=NEW.project_id AND id=NEW.revision_id)
    ))
)
BEGIN SELECT RAISE(ABORT,'invalid revision review transition'); END;

CREATE TRIGGER approvals_no_update BEFORE UPDATE ON approvals
BEGIN SELECT RAISE(ABORT,'immutable approval'); END;
CREATE TRIGGER approvals_no_delete BEFORE DELETE ON approvals
BEGIN SELECT RAISE(ABORT,'immutable approval'); END;
