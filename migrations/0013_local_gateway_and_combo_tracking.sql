-- Support local gateway remote-backed inference provenance and preserve requested combo vs
-- actual upstream model telemetry on budget_reservations and model_availability_evidence.

CREATE TABLE model_availability_evidence_new (
    provider_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    adapter_version TEXT NOT NULL,
    endpoint_identity TEXT NOT NULL,
    availability TEXT NOT NULL CHECK(availability IN ('unknown', 'available', 'unavailable')),
    probe_provenance TEXT NOT NULL CHECK(probe_provenance IN (
        'deterministic_mock_fixture',
        'controlled_local_http_fixture',
        'local_gateway_remote_inference',
        'real_remote_tls_inference'
    )),
    probe_sha256 TEXT NOT NULL CHECK(length(probe_sha256)=64 AND probe_sha256 NOT GLOB '*[^0-9a-f]*'),
    echoed_model_id TEXT NOT NULL,
    verified_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    reason TEXT NOT NULL DEFAULT '',
    PRIMARY KEY(provider_id, model_id),
    FOREIGN KEY(provider_id, model_id) REFERENCES model_registry(provider_id, model_id)
);

INSERT INTO model_availability_evidence_new (
    provider_id,
    model_id,
    adapter_version,
    endpoint_identity,
    availability,
    probe_provenance,
    probe_sha256,
    echoed_model_id,
    verified_at,
    expires_at,
    reason
)
SELECT
    provider_id,
    model_id,
    adapter_version,
    endpoint_identity,
    availability,
    probe_provenance,
    probe_sha256,
    echoed_model_id,
    verified_at,
    expires_at,
    reason
FROM model_availability_evidence;

DROP TABLE model_availability_evidence;
ALTER TABLE model_availability_evidence_new RENAME TO model_availability_evidence;

CREATE TABLE budget_reservations_new (
    id TEXT PRIMARY KEY NOT NULL,
    cost_consent_id TEXT NOT NULL REFERENCES cost_consents(id),
    project_id TEXT,
    job_id TEXT,
    provider_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    requested_model_id TEXT,
    actual_upstream_model TEXT,
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

INSERT INTO budget_reservations_new (
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
)
SELECT
    id,
    cost_consent_id,
    project_id,
    job_id,
    provider_id,
    model_id,
    model_id,
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
FROM budget_reservations;

DROP TABLE budget_reservations;
ALTER TABLE budget_reservations_new RENAME TO budget_reservations;

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
