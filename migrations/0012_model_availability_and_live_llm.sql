-- P2-03C Track B: Trusted model availability probe evidence table.
-- Prevents bare model_registry status toggles from authorizing live/remote inference
-- without endpoint-bound, unexpired, provenance-tagged probe verification.
CREATE TABLE model_availability_evidence (
    provider_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    adapter_version TEXT NOT NULL,
    endpoint_identity TEXT NOT NULL,
    availability TEXT NOT NULL CHECK(availability IN ('unknown', 'available', 'unavailable')),
    probe_provenance TEXT NOT NULL CHECK(probe_provenance IN (
        'deterministic_mock_fixture',
        'controlled_local_http_fixture',
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
