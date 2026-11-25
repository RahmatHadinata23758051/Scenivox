-- P2-03C Final Ledger QA: bind budget reservations to canonical request hash, model snapshot hash,
-- endpoint identity, and record explicit transport/inference provenance immutably.

ALTER TABLE budget_reservations ADD COLUMN canonical_request_sha256 TEXT;
ALTER TABLE budget_reservations ADD COLUMN model_snapshot_sha256 TEXT;
ALTER TABLE budget_reservations ADD COLUMN endpoint_identity TEXT;
ALTER TABLE budget_reservations ADD COLUMN inference_provenance TEXT
    CHECK(inference_provenance IS NULL OR inference_provenance IN (
        'deterministic_mock_fixture',
        'controlled_local_http_fixture',
        'real_remote_tls_inference'
    ));

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
