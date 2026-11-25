-- P2-02B Commit 2: CostConsent, verified pricing snapshots, atomic BudgetReservation, and worker lease ownership (additive).
CREATE TABLE model_pricing (
    provider_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    pricing_version TEXT NOT NULL,
    currency TEXT NOT NULL CHECK(currency = 'USD'),
    input_cost_per_token_micros INTEGER NOT NULL CHECK(input_cost_per_token_micros >= 0),
    output_cost_per_token_micros INTEGER NOT NULL CHECK(output_cost_per_token_micros >= 0),
    request_fixed_cost_micros INTEGER NOT NULL CHECK(request_fixed_cost_micros >= 0),
    verified INTEGER NOT NULL CHECK(verified IN (0, 1)),
    pricing_sha256 TEXT NOT NULL CHECK(length(pricing_sha256)=64 AND pricing_sha256 NOT GLOB '*[^0-9a-f]*'),
    updated_at TEXT NOT NULL,
    PRIMARY KEY(provider_id, model_id),
    FOREIGN KEY(provider_id, model_id) REFERENCES model_registry(provider_id, model_id)
);

CREATE TABLE cost_consents (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT REFERENCES projects(id),
    provider_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    scopes_json TEXT NOT NULL,
    max_cost_micros INTEGER NOT NULL CHECK(max_cost_micros > 0),
    max_total_tokens INTEGER CHECK(max_total_tokens IS NULL OR max_total_tokens > 0),
    spent_micros INTEGER NOT NULL DEFAULT 0 CHECK(spent_micros >= 0),
    reserved_micros INTEGER NOT NULL DEFAULT 0 CHECK(reserved_micros >= 0),
    spent_tokens INTEGER NOT NULL DEFAULT 0 CHECK(spent_tokens >= 0),
    reserved_tokens INTEGER NOT NULL DEFAULT 0 CHECK(reserved_tokens >= 0),
    pricing_snapshot_json TEXT NOT NULL,
    pricing_sha256 TEXT NOT NULL CHECK(length(pricing_sha256)=64 AND pricing_sha256 NOT GLOB '*[^0-9a-f]*'),
    status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active', 'revoked', 'halted')),
    halt_reason TEXT,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    revoked_at TEXT,
    FOREIGN KEY(provider_id, model_id) REFERENCES model_registry(provider_id, model_id),
    CHECK(status != 'revoked' OR revoked_at IS NOT NULL)
);
CREATE INDEX cost_consents_created ON cost_consents(created_at, id);

CREATE TRIGGER cost_consents_immutable_terms
BEFORE UPDATE OF id, project_id, provider_id, model_id, scopes_json, max_cost_micros, max_total_tokens, pricing_snapshot_json, pricing_sha256, created_at, expires_at ON cost_consents
BEGIN
    SELECT RAISE(ABORT, 'cost consent terms are immutable');
END;

CREATE TRIGGER cost_consents_no_delete
BEFORE DELETE ON cost_consents
BEGIN
    SELECT RAISE(ABORT, 'cost consent records are durable');
END;

CREATE TABLE budget_reservations (
    id TEXT PRIMARY KEY NOT NULL,
    cost_consent_id TEXT NOT NULL REFERENCES cost_consents(id),
    project_id TEXT,
    job_id TEXT,
    provider_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    scope TEXT NOT NULL CHECK(scope IN ('provider_probe', 'storyboard_planning', 'scene_code_generation', 'voice_tts', 'render_job')),
    owner_id TEXT NOT NULL,
    lease_epoch INTEGER NOT NULL CHECK(lease_epoch >= 1),
    estimated_input_tokens INTEGER NOT NULL CHECK(estimated_input_tokens >= 0),
    max_output_tokens INTEGER NOT NULL CHECK(max_output_tokens >= 0),
    reserved_micros INTEGER NOT NULL CHECK(reserved_micros >= 0),
    reserved_tokens INTEGER NOT NULL CHECK(reserved_tokens >= 0),
    actual_input_tokens INTEGER CHECK(actual_input_tokens IS NULL OR actual_input_tokens >= 0),
    actual_output_tokens INTEGER CHECK(actual_output_tokens IS NULL OR actual_output_tokens >= 0),
    actual_micros INTEGER CHECK(actual_micros IS NULL OR actual_micros >= 0),
    overage_micros INTEGER NOT NULL DEFAULT 0 CHECK(overage_micros >= 0),
    pricing_snapshot_json TEXT NOT NULL,
    pricing_sha256 TEXT NOT NULL CHECK(length(pricing_sha256)=64 AND pricing_sha256 NOT GLOB '*[^0-9a-f]*'),
    status TEXT NOT NULL DEFAULT 'reserved' CHECK(status IN ('reserved', 'settled', 'settled_conservative', 'settled_overage')),
    error_code TEXT,
    created_at TEXT NOT NULL,
    settled_at TEXT
);
CREATE INDEX budget_reservations_consent_status ON budget_reservations(cost_consent_id, status, created_at, id);

CREATE TRIGGER budget_reservations_immutable_terms
BEFORE UPDATE OF id, cost_consent_id, project_id, job_id, provider_id, model_id, scope, owner_id, lease_epoch, estimated_input_tokens, max_output_tokens, reserved_micros, reserved_tokens, pricing_snapshot_json, pricing_sha256, created_at ON budget_reservations
BEGIN
    SELECT RAISE(ABORT, 'budget reservation terms and ownership are immutable');
END;

CREATE TRIGGER budget_reservations_only_once
BEFORE UPDATE ON budget_reservations
WHEN OLD.status != 'reserved'
BEGIN
    SELECT RAISE(ABORT, 'budget reservation already settled');
END;

CREATE TRIGGER budget_reservations_no_delete
BEFORE DELETE ON budget_reservations
BEGIN
    SELECT RAISE(ABORT, 'budget reservations are durable');
END;

CREATE TABLE worker_leases (
    id INTEGER PRIMARY KEY CHECK(id = 1),
    owner_id TEXT NOT NULL,
    lease_epoch INTEGER NOT NULL CHECK(lease_epoch >= 1),
    acquired_at TEXT NOT NULL,
    heartbeat_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
);

CREATE TABLE ledger_idempotency (
    operation TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_sha256 TEXT NOT NULL CHECK(length(request_sha256)=64 AND request_sha256 NOT GLOB '*[^0-9a-f]*'),
    response_json TEXT NOT NULL,
    PRIMARY KEY(operation, idempotency_key)
);
