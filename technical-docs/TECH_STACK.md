# Scenivox Tech Stack & Rationale — ADR-02

**Architectural style:** Rust-first core, in-process AI agent orchestrator, Linux OCI sandboxed skill & MediaQA runtime.

| Domain | Selected Baseline | Status / Boundary |
|---|---|---|
| API & business core | Rust 2024 + Axum + Clap | **Implemented.** Shared `Store` services; no web UI |
| Async & process supervision | Tokio | **Implemented** for API/DB; **planned** for exclusive worker & OCI supervisor |
| Persistence | SQLite + SQLx (embedded migrations) | **Implemented.** Local metadata, approvals, `render_jobs`; additive `0006+` planned |
| Schemas / canonical hashes | `serde` / `serde_json` / `serde_jcs` / `sha2` | **Implemented.** JSON Schema 2020-12 bundle |
| AI agent & LLM transport | In-process Rust `AgentRuntime` + `LlmProvider` | **Planned Phase 2.** `mock` in CI; opt-in local/remote HTTP client on host |
| Execution isolation | **Linux OCI container isolation** (native Linux & Windows via **WSL2**) | **Planned Phase 2.** Mandatory for AI-generated JS/Python; fail closed if unavailable |
| Audio / TTS / alignment | User-supplied Indonesian WAV (`P2-02`) first; licensed local TTS evaluation later (`P2-02C`) | **WAV/timing implemented;** local TTS evaluation pending |
| Skill rendering engines | Sandboxed Node/Playwright/Canvas, Python, FFmpeg (manifest-declared only) | **Planned Phase 2.** Invoked inside `--network=none` OCI container |
| Media QA | Isolated, bounded OCI container running `ffmpeg` / `ffprobe` | **Planned Phase 2.** Untrusted media artifacts never probed directly on host |
| Frontend | Deferred / HOLD (Phase 3) | **On HOLD** until explicit CEO authorization |

## Rationale & Guardrails
- **Why Rust owns the agent loop:** Keeps provider keys, atomic budget reservation, tool bounds, and SQLite traces inside one audited binary without requiring Claude Code, OMP, or external orchestration services.
- **Why Linux OCI containers (including Windows via WSL2):** AI-generated JS/Python, Chromium, and FFmpeg/ffprobe cannot be safely contained by host CWD/env filtering alone. Containers run with `--network=none`, `--read-only`, `--cap-drop=ALL`, `--security-opt=no-new-privileges`, resource limits, and zero host secrets or Docker socket access.
- **No hardcoded unverified crate versions:** Pin exact crate and skill package versions in `Cargo.lock` and skill lockfiles only during each phase's bootstrap/audit.
