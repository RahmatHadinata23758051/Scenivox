# Scenivox ADR & Open Decisions — v0.3

"Approved baseline" means established by CEO during planning; it is **not** a claim of technical feasibility proven by implementation.

| ID | Decision | Status | Notes |
|---|---|---|---|
| ADR-001 | Local-first, personal single-user; public GitHub as project home | Approved baseline | Repo exists and is public; backend local-first |
| ADR-002 | Backend and real production pipeline before website UI | Approved baseline | CEO UI references forthcoming; frontend HOLD |
| ADR-003 | Idea → AI storyboard → scene-level revision → immutable approval → generate | Approved baseline | No video render on planning alone |
| ADR-004 | CEO = PO, ChatGPT = PM, OMP = developer, Trello = tracking | Approved baseline | GitHub commits source of code truth |
| ADR-005 | First animation candidate = local stick-figure ZIP | Proposed pending audit | Do not assume unreviewed scripts safe |
| ADR-006 | **Rust + Axum + Tokio** backend; **SQLx + SQLite** metadata | **Approved direction** | Supersedes v0.2 FastAPI suggestion; OMP validates exact design |
| ADR-007 | Polyglot skill runtime (JS/Node, Python, Chromium, FFmpeg) | Approved direction | Avoid rewriting the media engine in Rust |
| ADR-008 | Provider-neutral AI planner, scene-code and TTS adapters | Approved direction | Mock mode required; no bundled keys |
| ADR-009 | Indonesian voice provider and licensing | Open | Test quality, timing and rights |
| ADR-010 | Official repo: `RahmatHadinata23758051/Scenivox` | Confirmed | https://github.com/RahmatHadinata23758051/Scenivox |
| ADR-011 | Backend model catalog and selection per AI task, with safe API/CLI | Approved baseline | Phase 1; no model settings UI yet |
| ADR-012 | 9router/Luna/GPT/Gemini connectivity | Deferred | Documented callable provider endpoints only; credentials/cost gating |
| ADR-013 | OMP is dev harness, not runtime dependency | Approved direction | App needs independent provider/runtime interface |
| ADR-014 | No Redis/Celery/FastAPI second service initially | Proposed default | Revisit only with measured need and approval |
| ADR-015 | Exact website stack and visual direction | **Deferred / HOLD** | CEO wants to supply references |
| ADR-016 | Public repo license / distribution of vendor ZIP & assets | **Open** | CEO approval and third-party licensing audit required |
| ADR-017 | Immutable revision values plus separate review workflow and append-only approval | P0-03 proposed for PM approval | No mutable status/content on revision; editing clears project approval but preserves existing jobs. See DOMAIN_CONTRACT.md. |
| ADR-018 | Versioned strict JSON Schema 2020-12 plus domain validation | P0-03 proposed for PM approval | Unknown fields/versions rejected; JCS SHA-256 content hashes; no DB changes in P0-03. Schema tests do not prove relational invariants. |
| ADR-019 | Keep legacy `/health`, specify separate `/ready` | P0-03 proposed for PM approval | Current health 200 includes DB status in body; not readiness. Future ready 200/503 checks bounded local DB readiness only. No handler changes here. |
| ADR-020 | Frozen model/skill/prompt inputs at revision approval | P0-03 proposed for PM approval | Whole-pair per-role selection, no render overrides or fallback; new revision for execution-input changes. Reproducibility is traceability, not guaranteed identical remote output. |
| ADR-021 | Transactional local commands; retry creates a new job | P0-03 proposed for PM approval | SQLite compare-and-swap current pointer, operation idempotency keys; terminal jobs immutable in lifecycle. Recovery marks active jobs interrupted. No external queue. |

## P0-03 rationale and consequences

- ADR-017 removes the contradiction between immutable revisions and mutable `status`: content records are immutable; review state is separate, superseded is navigation-derived. Current approval is required for new enqueue/retry; prior jobs retain approved inputs after an edit.
- ADR-018 uses one self-contained versioned schema bundle, not a second Rust domain implementation before Phase 1. Tests compile schema and reject malformed records. Relational/timing/hash validation remains mandatory in future domain services. No existing product data requires backfill; P0-02 contains metadata only.
- ADR-019 preserves PM-approved P0-02 behavior rather than silently changing its HTTP contract in a specification task. Health callers inspect its body; future readiness needs bounded checks, 503 on failure and outage tests. Readiness is not implemented in this phase.
- ADR-020 prevents changing a default or run override from altering approved output inputs. It costs storage for frozen prompts/outputs and may block execution when pinned tools disappear; explicit failure is safer than substitution. Cost consent is independent and rechecked before dispatch.
- ADR-021 favors one Rust process and SQLite transactions over distributed state machines. POST replay records prevent accidental double enqueue; remote exactly-once effects are not guaranteed and must never trigger hidden retry/spend.

These are task-local design decisions awaiting PM review, not authorization for Phase 1 implementation, new runtime services, paid APIs or third-party execution. ADR-005 remains conditionally audited only: exact ZIP/Indonesian media feasibility blockers are recorded in audits/P0-01-skill-audit.md.

## Change control
If OMP proposes a decision affecting primary language, scope, execution safety, budget or runtime architecture: record reason + tradeoff + affected task; PM reviews; CEO approves before implementation. Minor internal library changes may be made within approved task provided they're documented and tested.
