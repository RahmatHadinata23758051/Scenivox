-- First live 9Router storyboard smoke hardening:
-- 1. Support explicit UNPRICED upstream-quota pricing snapshots (without inventing USD rates).
-- 2. Enforce optional max_requests ceiling directly on cost_consents.
-- 3. Record reservation latency_ms telemetry alongside requested combo and upstream model.
-- 4. Track whether model_availability_evidence was derived from an authorized response.

CREATE TABLE model_pricing_new (
    provider_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    pricing_version TEXT NOT NULL,
    currency TEXT NOT NULL CHECK(currency IN ('USD', 'UNPRICED')),
    input_cost_per_token_micros INTEGER NOT NULL CHECK(input_cost_per_token_micros >= 0),
    output_cost_per_token_micros INTEGER NOT NULL CHECK(output_cost_per_token_micros >= 0),
    request_fixed_cost_micros INTEGER NOT NULL CHECK(request_fixed_cost_micros >= 0),
    verified INTEGER NOT NULL CHECK(verified IN (0, 1)),
    pricing_sha256 TEXT NOT NULL CHECK(length(pricing_sha256)=64 AND pricing_sha256 NOT GLOB '*[^0-9a-f]*'),
    updated_at TEXT NOT NULL,
    PRIMARY KEY(provider_id, model_id),
    FOREIGN KEY(provider_id, model_id) REFERENCES model_registry(provider_id, model_id),
    CHECK(
        currency != 'UNPRICED'
        OR (
            verified = 0
            AND input_cost_per_token_micros = 0
            AND output_cost_per_token_micros = 0
            AND request_fixed_cost_micros = 0
        )
    )
);

INSERT INTO model_pricing_new (
    provider_id,
    model_id,
    pricing_version,
    currency,
    input_cost_per_token_micros,
    output_cost_per_token_micros,
    request_fixed_cost_micros,
    verified,
    pricing_sha256,
    updated_at
)
SELECT
    provider_id,
    model_id,
    pricing_version,
    currency,
    input_cost_per_token_micros,
    output_cost_per_token_micros,
    request_fixed_cost_micros,
    verified,
    pricing_sha256,
    updated_at
FROM model_pricing;

DROP TABLE model_pricing;
ALTER TABLE model_pricing_new RENAME TO model_pricing;

CREATE TABLE budget_reservations_stage (
    id TEXT PRIMARY KEY NOT NULL,
    cost_consent_id TEXT NOT NULL,
    project_id TEXT,
    job_id TEXT,
    provider_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    requested_model_id TEXT,
    actual_upstream_model TEXT,
    scope TEXT NOT NULL,
    owner_id TEXT NOT NULL,
    lease_epoch INTEGER NOT NULL,
    estimated_input_tokens INTEGER NOT NULL,
    max_output_tokens INTEGER NOT NULL,
    reserved_micros INTEGER NOT NULL,
    reserved_tokens INTEGER NOT NULL,
    actual_input_tokens INTEGER,
    actual_output_tokens INTEGER,
    actual_micros INTEGER,
    overage_micros INTEGER NOT NULL,
    pricing_snapshot_json TEXT NOT NULL,
    pricing_sha256 TEXT NOT NULL,
    status TEXT NOT NULL,
    error_code TEXT,
    created_at TEXT NOT NULL,
    settled_at TEXT,
    idempotency_key TEXT,
    dispatch_claimed INTEGER NOT NULL,
    dispatch_claimed_at TEXT,
    replay_blocked INTEGER NOT NULL,
    canonical_request_sha256 TEXT,
    model_snapshot_sha256 TEXT,
    endpoint_identity TEXT,
    inference_provenance TEXT
);

INSERT INTO budget_reservations_stage
SELECT
    id,
    cost_consent_id,
    project_id,
    job_id,
    provider_id,
    model_id,
    requested_model_id,
    actual_upstream_model,
    scope,
    owner_id,
    lease_epoch,
    estimated_input_tokens,
    max_output_tokens,
    reserved_micros,
    reserved_tokens,
    actual_input_tokens,
    actual_output_tokens,
    actual_micros,
    overage_micros,
    pricing_snapshot_json,
    pricing_sha256,
    status,
    error_code,
    created_at,
    settled_at,
    idempotency_key,
    dispatch_claimed,
    dispatch_claimed_at,
    replay_blocked,
    canonical_request_sha256,
    model_snapshot_sha256,
    endpoint_identity,
    inference_provenance
FROM budget_reservations;

DROP TABLE budget_reservations;

CREATE TABLE cost_consents_new (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT REFERENCES projects(id),
    provider_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    scopes_json TEXT NOT NULL,
    max_cost_micros INTEGER NOT NULL CHECK(max_cost_micros >= 0),
    max_total_tokens INTEGER CHECK(max_total_tokens IS NULL OR max_total_tokens > 0),
    max_requests INTEGER CHECK(max_requests IS NULL OR max_requests >= 1),
    spent_micros INTEGER NOT NULL DEFAULT 0 CHECK(spent_micros >= 0),
    reserved_micros INTEGER NOT NULL DEFAULT 0 CHECK(reserved_micros >= 0),
    spent_tokens INTEGER NOT NULL DEFAULT 0 CHECK(spent_tokens >= 0),
    reserved_tokens INTEGER NOT NULL DEFAULT 0 CHECK(reserved_tokens >= 0),
    spent_requests INTEGER NOT NULL DEFAULT 0 CHECK(spent_requests >= 0),
    reserved_requests INTEGER NOT NULL DEFAULT 0 CHECK(reserved_requests >= 0),
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

INSERT INTO cost_consents_new (
    id,
    project_id,
    provider_id,
    model_id,
    scopes_json,
    max_cost_micros,
    max_total_tokens,
    max_requests,
    spent_micros,
    reserved_micros,
    spent_tokens,
    reserved_tokens,
    spent_requests,
    reserved_requests,
    pricing_snapshot_json,
    pricing_sha256,
    status,
    halt_reason,
    created_at,
    expires_at,
    revoked_at
)
SELECT
    id,
    project_id,
    provider_id,
    model_id,
    scopes_json,
    max_cost_micros,
    max_total_tokens,
    NULL,
    spent_micros,
    reserved_micros,
    spent_tokens,
    reserved_tokens,
    0,
    0,
    pricing_snapshot_json,
    pricing_sha256,
    status,
    halt_reason,
    created_at,
    expires_at,
    revoked_at
FROM cost_consents;

DROP TABLE cost_consents;
ALTER TABLE cost_consents_new RENAME TO cost_consents;

CREATE INDEX cost_consents_created ON cost_consents(created_at, id);

CREATE TRIGGER cost_consents_immutable_terms
BEFORE UPDATE OF id, project_id, provider_id, model_id, scopes_json, max_cost_micros, max_total_tokens, max_requests, pricing_snapshot_json, pricing_sha256, created_at, expires_at ON cost_consents
BEGIN
    SELECT RAISE(ABORT, 'cost consent terms are immutable');
END;

CREATE TRIGGER cost_consents_no_delete
BEFORE DELETE ON cost_consents
BEGIN
    SELECT RAISE(ABORT, 'cost consent records are durable');
END;

CREATE TRIGGER cost_consents_preserve_revocation
BEFORE UPDATE OF status, revoked_at ON cost_consents
WHEN OLD.status = 'revoked'
  AND (NEW.status != 'revoked' OR NEW.revoked_at IS NOT OLD.revoked_at)
BEGIN
    SELECT RAISE(ABORT, 'revocation evidence is immutable');
END;

CREATE TRIGGER cost_consents_halt_only_active
BEFORE UPDATE OF status ON cost_consents
WHEN OLD.status = 'halted' AND NEW.status != 'halted'
BEGIN
    SELECT RAISE(ABORT, 'halted consent status cannot revert');
END;

CREATE TABLE budget_reservations (
    id TEXT PRIMARY KEY NOT NULL,
    cost_consent_id TEXT NOT NULL REFERENCES cost_consents(id),
    project_id TEXT,
    job_id TEXT,
    provider_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    requested_model_id TEXT,
    actual_upstream_model TEXT,
    latency_ms INTEGER CHECK(latency_ms IS NULL OR latency_ms >= 0),
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
    settled_at TEXT,
    idempotency_key TEXT,
    dispatch_claimed INTEGER NOT NULL DEFAULT 0 CHECK(dispatch_claimed IN (0, 1)),
    dispatch_claimed_at TEXT,
    replay_blocked INTEGER NOT NULL DEFAULT 0 CHECK(replay_blocked IN (0, 1)),
    canonical_request_sha256 TEXT,
    model_snapshot_sha256 TEXT,
    endpoint_identity TEXT,
    inference_provenance TEXT CHECK(inference_provenance IS NULL OR inference_provenance IN (
        'deterministic_mock_fixture',
        'controlled_local_http_fixture',
        'local_gateway_remote_inference',
        'real_remote_tls_inference'
    ))
);

INSERT INTO budget_reservations (
    id,
    cost_consent_id,
    project_id,
    job_id,
    provider_id,
    model_id,
    requested_model_id,
    actual_upstream_model,
    latency_ms,
    scope,
    owner_id,
    lease_epoch,
    estimated_input_tokens,
    max_output_tokens,
    reserved_micros,
    reserved_tokens,
    actual_input_tokens,
    actual_output_tokens,
    actual_micros,
    overage_micros,
    pricing_snapshot_json,
    pricing_sha256,
    status,
    error_code,
    created_at,
    settled_at,
    idempotency_key,
    dispatch_claimed,
    dispatch_claimed_at,
    replay_blocked,
    canonical_request_sha256,
    model_snapshot_sha256,
    endpoint_identity,
    inference_provenance
)
SELECT
    id,
    cost_consent_id,
    project_id,
    job_id,
    provider_id,
    model_id,
    requested_model_id,
    actual_upstream_model,
    NULL,
    scope,
    owner_id,
    lease_epoch,
    estimated_input_tokens,
    max_output_tokens,
    reserved_micros,
    reserved_tokens,
    actual_input_tokens,
    actual_output_tokens,
    actual_micros,
    overage_micros,
    pricing_snapshot_json,
    pricing_sha256,
    status,
    error_code,
    created_at,
    settled_at,
    idempotency_key,
    dispatch_claimed,
    dispatch_claimed_at,
    replay_blocked,
    canonical_request_sha256,
    model_snapshot_sha256,
    endpoint_identity,
    inference_provenance
FROM budget_reservations_stage;

DROP TABLE budget_reservations_stage;

CREATE INDEX budget_reservations_consent_status
ON budget_reservations(cost_consent_id, status, created_at, id);

CREATE UNIQUE INDEX budget_reservations_idem_key
ON budget_reservations(idempotency_key)
WHERE idempotency_key IS NOT NULL;

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

CREATE TRIGGER budget_reservations_dispatch_once
BEFORE UPDATE OF dispatch_claimed, dispatch_claimed_at ON budget_reservations
WHEN OLD.dispatch_claimed != 0
  OR OLD.replay_blocked != 0
  OR OLD.status != 'reserved'
  OR NEW.dispatch_claimed != 1
BEGIN
    SELECT RAISE(ABORT, 'reservation dispatch permit is single-use while reserved');
END;

CREATE TRIGGER budget_reservations_immutable_bindings
BEFORE UPDATE OF canonical_request_sha256, model_snapshot_sha256, endpoint_identity ON budget_reservations
BEGIN
    SELECT RAISE(ABORT, 'budget reservation request and endpoint bindings are immutable');
END;

CREATE TRIGGER budget_reservations_immutable_provenance
BEFORE UPDATE OF inference_provenance ON budget_reservations
WHEN OLD.inference_provenance IS NOT NULL
  AND NEW.inference_provenance IS NOT OLD.inference_provenance
BEGIN
    SELECT RAISE(ABORT, 'budget reservation inference provenance is immutable once recorded');
END;

ALTER TABLE model_availability_evidence
    ADD COLUMN derived_from_authorized_response INTEGER NOT NULL DEFAULT 0 CHECK(derived_from_authorized_response IN (0, 1));
ALTER TABLE model_availability_evidence
    ADD COLUMN source_reservation_id TEXT;
