-- P2-02B Acceptance QA hardening (additive).
ALTER TABLE budget_reservations ADD COLUMN idempotency_key TEXT;
ALTER TABLE budget_reservations ADD COLUMN dispatch_claimed INTEGER NOT NULL DEFAULT 0 CHECK(dispatch_claimed IN (0, 1));
ALTER TABLE budget_reservations ADD COLUMN dispatch_claimed_at TEXT;
ALTER TABLE budget_reservations ADD COLUMN replay_blocked INTEGER NOT NULL DEFAULT 0 CHECK(replay_blocked IN (0, 1));

CREATE UNIQUE INDEX budget_reservations_idem_key
ON budget_reservations(idempotency_key)
WHERE idempotency_key IS NOT NULL;

CREATE TRIGGER budget_reservations_dispatch_once
BEFORE UPDATE OF dispatch_claimed, dispatch_claimed_at ON budget_reservations
WHEN OLD.dispatch_claimed != 0
  OR OLD.replay_blocked != 0
  OR OLD.status != 'reserved'
  OR NEW.dispatch_claimed != 1
BEGIN
    SELECT RAISE(ABORT, 'reservation dispatch permit is single-use while reserved');
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

ALTER TABLE execution_workspace_evidence ADD COLUMN worker_owner_id TEXT NOT NULL DEFAULT '';
ALTER TABLE execution_workspace_evidence ADD COLUMN worker_lease_epoch INTEGER NOT NULL DEFAULT 0;

CREATE TRIGGER execution_authorizations_claim_requires_job
BEFORE UPDATE OF claimed_job_id ON execution_authorizations
WHEN NEW.claimed_job_id IS NOT NULL AND NOT EXISTS (
    SELECT 1 FROM render_jobs j
    WHERE j.id = NEW.claimed_job_id
      AND j.project_id = NEW.project_id
      AND j.revision_id = NEW.revision_id
      AND j.approval_id = NEW.approval_id
      AND j.content_sha256 = NEW.content_sha256
)
BEGIN
    SELECT RAISE(ABORT, 'execution claim must bind atomically to matching render job');
END;

CREATE TRIGGER execution_workspace_evidence_requires_verified_worker_and_artifact
BEFORE INSERT ON execution_workspace_evidence
WHEN NEW.output_artifact_sha256 IS NULL
  OR length(NEW.output_artifact_sha256) != 64
  OR NEW.worker_owner_id = ''
  OR NEW.worker_lease_epoch < 1
  OR NOT EXISTS (
      SELECT 1 FROM execution_authorizations ea
      WHERE ea.id = NEW.execution_auth_id
        AND ea.status = 'claimed'
        AND ea.claimed_job_id = NEW.job_id
        AND ea.project_id = NEW.project_id
        AND ea.revision_id = NEW.revision_id
  )
  OR NOT EXISTS (
      SELECT 1 FROM worker_leases wl
      WHERE wl.id = 1
        AND wl.owner_id = NEW.worker_owner_id
        AND wl.lease_epoch = NEW.worker_lease_epoch
  )
BEGIN
    SELECT RAISE(ABORT, 'workspace evidence requires verified worker lease and artifact hash');
END;
