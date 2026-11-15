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

## Change control
If OMP proposes a decision affecting primary language, scope, execution safety, budget or runtime architecture: record reason + tradeoff + affected task; PM reviews; CEO approves before implementation. Minor internal library changes may be made within approved task provided they're documented and tested.
