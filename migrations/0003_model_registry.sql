-- P1-05 additive model metadata and settings. Existing revisions and snapshots are untouched.
CREATE TABLE model_registry (
    provider_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    adapter_version TEXT NOT NULL,
    task_roles_json TEXT NOT NULL,
    parameters_json TEXT NOT NULL,
    enabled INTEGER NOT NULL CHECK(enabled IN (0,1)),
    configured INTEGER NOT NULL CHECK(configured IN (0,1)),
    availability TEXT NOT NULL CHECK(availability IN ('unknown','available','unavailable')),
    reason TEXT NOT NULL DEFAULT '',
    remote INTEGER NOT NULL CHECK(remote IN (0,1)),
    PRIMARY KEY(provider_id, model_id)
);
CREATE TABLE model_settings (
    id INTEGER PRIMARY KEY CHECK(id = 1),
    defaults_json TEXT NOT NULL
);
INSERT INTO model_registry(provider_id,model_id,adapter_version,task_roles_json,parameters_json,enabled,configured,availability,reason,remote)
VALUES
 ('mock','planner-v1','1', '["storyboard_planning"]','{"temperature":0,"seed":0}',1,1,'available','Deterministic offline mock',0),
 ('mock','coder-v1','1', '["scene_code_generation"]','{"temperature":0,"seed":0}',1,1,'available','Deterministic offline mock',0);
INSERT INTO model_settings(id,defaults_json) VALUES (1, '{"storyboard_planning":{"provider_id":"mock","model_id":"planner-v1"},"scene_code_generation":{"provider_id":"mock","model_id":"coder-v1"}}');
