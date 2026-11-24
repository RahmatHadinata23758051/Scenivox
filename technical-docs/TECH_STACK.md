# Scenivox Tech Stack & Rationale — ADR-02

**Architectural style:** Rust-first core, in-process AI agent orchestrator, sandboxed polyglot skill runtime.

| Domain | Selected Baseline | Status / Boundary |
|---|---|---|
| API & business core | Rust 2024 + Axum + Clap | **Implemented.** Shared `Store` services; no web UI |
| Async & process supervision | Tokio | **Implemented** for API/DB; **planned** for worker & sandbox supervisor |
| Persistence | SQLite + SQLx (embedded migrations) | **Implemented.** Local metadata, approvals, `render_jobs`; additive `0006+` planned |
| Schemas / canonical hashes | `serde` / `serde_json` / `serde_jcs` / `sha2` | **Implemented.** JSON Schema 2020-12 bundle |
| AI agent & LLM transport | In-process Rust `AgentRuntime` + `LlmProvider` | **Planned Phase 2.** `mock` in CI; opt-in local/remote HTTP client on host |
| Execution isolation | Verified OCI container / Linux namespace+seccomp+cgroups sandbox | **Planned Phase 2.** Mandatory for AI-generated JS/Python; fail closed if unavailable |
| Audio / TTS / alignment | P2-02 PCM WAV inspection + `Timeline` + provider traits | **Foundation implemented;** Indonesian TTS/aligner pending P2-02B |
| Skill rendering engines | Sandboxed Node/Playwright/Canvas, Python, FFmpeg (skill-specific) | **Planned Phase 2.** Invoked inside sandbox by pinned manifest entrypoints |
| Media QA | Host `ffprobe` + multi-frame pixel-diff motion check | **Planned Phase 2.** Mandatory before `RenderJob` succeeds |
| Frontend | Deferred / HOLD (Phase 3) | **On HOLD** until explicit CEO authorization |

## Rationale & Guardrails
- **Why Rust owns the agent loop:** Keeps provider keys, budget enforcement, tool bounds, and SQLite traces inside one audited binary without requiring Claude Code, OMP, or external orchestration services (no FastAPI/Redis/Celery).
- **Why real sandbox isolation is non-negotiable:** AI-generated JS/Python and Chromium rendering cannot be contained by CWD/env filtering alone; Linux/WSL2 container/namespace isolation (`--network=none`, RO mounts, resource caps) is the primary execution target, and unsupported hosts fail closed.
- **No hardcoded unverified crate versions:** Pin exact crate and skill package versions in `Cargo.lock` and skill lockfiles only during each phase's bootstrap/audit.
