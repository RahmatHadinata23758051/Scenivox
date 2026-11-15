# AGENTS.md — Scenivox development contract (OMP)

## Authority
- **CEO / PO:** product scope, creative vision, legal/licensing choices, cost approval and phase transitions.
- **PM / ChatGPT:** concise OMP prompts, Trello backlog, review, acceptance/QA gates.
- **OMP:** implementation, tests, commits, technical proposals, optional subagents. Do not silently expand scope.

## Stack boundary
- Core/backend: **Rust + Axum + Tokio + SQLx (SQLite)**. Rust owns projects, scenes, revisions, model settings, approval, job lifecycle, and API/CLI orchestration.
- Existing **Python + Node/JavaScript + Playwright/Chromium + FFmpeg** run only as skill/render/TTS tool processes through explicit, audited adapters. No FastAPI microservice, Redis, Celery, or multi-service system by default.
- OMP is the **development harness**, never a required runtime service. Model providers called by the app are separate and behind adapters.
- For version selection, pin compatible releases and lockfiles during actual bootstrap; do not hardcode unverified crate/tool versions in planning docs.

## Invariants / STOP gates
- **No website UI, Next.js app, auth, SaaS, subscriptions, payment, styling, dashboard, or design system** before CEO authorizes Phase 3. CLI/API and backend tests first.
- **No autonomous video rendering** while the storyboard is a draft. Generate only after approval of an immutable revision; re-editing creates a new revision and makes that new revision unapproved.
- A change of default model never mutates past approved revision or existing job snapshots. Provider selection is **Phase 1 backend core**, not UI. Include deterministic mocks and explicit user-controlled remote API access.
- ZIPs and AI-generated code are **untrusted**. Audit before execution; never treat `SKILL.md` as runnable code; avoid arbitrary shell commands, implicit downloads and unrestricted writes.
- No paid API or network operation that may incur cost without explicit permission/budget; no secrets in Git, logs, exceptions or artifact manifests.
- Real deliverable = playable **animated** 9:16 MP4 with intelligible narration and captions; never label an untested export as working.

## Operational workflow
1. Read `README.md`, this file, and only the relevant specs. Pick one Trello card / coherent batch.
2. Before changes, inspect repository and user-provided skill ZIP in read-only mode. Record licenses, risks and prerequisites.
3. Implement the smallest testable vertical slice. Reuse existing tools instead of rewriting rendering/TTS internals into Rust.
4. Run reproducible unit/integration tests and capture commands/results. Document manual QA separately.
5. Commit only verified logical work. Report card IDs, changed files, test commands/results, evidence, risk/blocker, commit SHA and recommendation.
6. Stop after assigned phase/task for PM review; update status through agreed PM/Trello workflow. Do not self-approve large architecture changes.

## Process safety minimum
Use explicit executable/argv (not `sh -c` with user/model text), per-job restricted working directories, validated input files, path containment, environment allowlist, timeout and concurrency limits, captured stdout/stderr, process-tree cleanup and failure recovery. This is **defense in depth**, not a claim that running arbitrary generated code is fully sandboxed. If reliable isolation isn't available on a host, restrict runnable skills to trusted/audited paths and document the risk.

## Quality gate
A task is DONE only when its own acceptance criteria are reproducible. `cargo fmt`, `cargo clippy`, and `cargo test` are baseline for Rust tasks when code exists; media QA requires actual `ffprobe` output and visual/audio review. Never invent passing results.
