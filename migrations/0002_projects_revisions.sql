-- Empty Phase 0 databases need no backfill. History is retained by soft deletion.
CREATE TABLE projects (
 id TEXT PRIMARY KEY NOT NULL,
 title TEXT NOT NULL,
 idea TEXT NOT NULL,
 target_json TEXT NOT NULL,
 model_selection_json TEXT NOT NULL,
 current_revision_id TEXT,
 approved_revision_id TEXT,
 created_at TEXT NOT NULL,
 deleted_at TEXT,
 FOREIGN KEY (id,current_revision_id) REFERENCES revisions(project_id,id),
 FOREIGN KEY (id,approved_revision_id) REFERENCES revisions(project_id,id)
);
CREATE TABLE revisions (
 id TEXT PRIMARY KEY NOT NULL,
 project_id TEXT NOT NULL REFERENCES projects(id),
 parent_revision_id TEXT,
 version INTEGER NOT NULL CHECK(version > 0),
 created_at TEXT NOT NULL,
 content_sha256 TEXT NOT NULL CHECK(length(content_sha256)=64),
 content_json TEXT NOT NULL,
 UNIQUE(project_id,id),
 UNIQUE(project_id,version),
 FOREIGN KEY(project_id,parent_revision_id) REFERENCES revisions(project_id,id)
);
CREATE TABLE storyboards (
 revision_id TEXT PRIMARY KEY NOT NULL,
 project_id TEXT NOT NULL,
 synopsis TEXT NOT NULL,
 hook TEXT NOT NULL,
 UNIQUE(project_id,revision_id),
 FOREIGN KEY(project_id,revision_id) REFERENCES revisions(project_id,id)
);
CREATE TABLE scenes (
 project_id TEXT NOT NULL,
 revision_id TEXT NOT NULL,
 id TEXT NOT NULL,
 scene_order INTEGER NOT NULL CHECK(scene_order BETWEEN 0 AND 59),
 duration_ms INTEGER NOT NULL CHECK(duration_ms BETWEEN 1 AND 60000),
 PRIMARY KEY(revision_id,id),
 UNIQUE(revision_id,scene_order),
 FOREIGN KEY(project_id,revision_id) REFERENCES storyboards(project_id,revision_id)
);
CREATE INDEX projects_list ON projects(created_at,id) WHERE deleted_at IS NULL;
CREATE INDEX revisions_list ON revisions(project_id,created_at,id);
CREATE TRIGGER revisions_no_update BEFORE UPDATE ON revisions BEGIN SELECT RAISE(ABORT,'immutable revision'); END;
CREATE TRIGGER revisions_no_delete BEFORE DELETE ON revisions BEGIN SELECT RAISE(ABORT,'immutable revision'); END;
CREATE TRIGGER storyboards_no_update BEFORE UPDATE ON storyboards BEGIN SELECT RAISE(ABORT,'immutable storyboard'); END;
CREATE TRIGGER storyboards_no_delete BEFORE DELETE ON storyboards BEGIN SELECT RAISE(ABORT,'immutable storyboard'); END;
CREATE TRIGGER scenes_no_update BEFORE UPDATE ON scenes BEGIN SELECT RAISE(ABORT,'immutable scene'); END;
CREATE TRIGGER scenes_no_delete BEFORE DELETE ON scenes BEGIN SELECT RAISE(ABORT,'immutable scene'); END;
CREATE TABLE idempotency_records (
 operation TEXT NOT NULL,
 scope TEXT NOT NULL,
 key TEXT NOT NULL,
 project_id TEXT NOT NULL REFERENCES projects(id),
 request_sha256 TEXT NOT NULL,
 response_json TEXT NOT NULL,
 PRIMARY KEY(operation,scope,key)
);
