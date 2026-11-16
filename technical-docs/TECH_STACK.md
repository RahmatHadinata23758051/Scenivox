# Scenivox Tech Stack & Rationale — v0.3

**Architectural style:** Rust-first core, polyglot media skill runtime. Choose tools per component, not one language for everything.

| Domain | Selected baseline | Reason / boundary |
|---|---|---|
| API & business core | Rust + Axum | Typed domain, efficient API, structured errors; **not** media renderer |
| Async tasks/process IO | Tokio | Lightweight coordination, subprocess supervision; don't block request handlers |
| Persistence | SQLite + SQLx | Local-first metadata, migrations, transactions; no Redis/Postgres initially |
| Schemas / interchange | serde/serde_json + versioned JSON Schema | Durable API, skill/job I/O; validate generated structured output |
| HTTP client for supported model endpoints | reqwest or equivalent, adapter-specific | User-configured providers only; network/cost explicitly controlled |
| CLI | clap or minimal Rust CLI | Full backend workflow without website |
| Animation skill | Existing Node/JS Canvas + Playwright/Chromium and/or Python | Reuse audited engine's native runtime; prove 60s feasibility |
| Audio/TTS | Existing Python or external-audio WAV through adapter | No fixed provider until voice quality/license check |
| Caption/alignment | skill-native tools or adapted alignment module | Indonesian phoneme/word accuracy must be measured |
| Video mux/inspect | FFmpeg/ffprobe subprocess | Native media codec pipeline; high-performance encoding without reinventing codecs |
| Assets | Local filesystem, content hashes | Keep media out of SQLite; portable project workspaces |
| Frontend | **Unselected / deferred** (Next.js candidate only) | CEO wants independent design/UX phase |

## Rust core responsibilities
1. Project, storyboard, revisions/approval, model settings and effective selection snapshots.
2. REST + CLI shared domain services, validation, migrations and safe artifact access.
3. Persistent job scheduler and concurrency budget; stage progress, log metadata and recovery.
4. Controlled executable runners for trusted/audited skills/TTS/FFmpeg with argv and timeout.

## Non-Rust responsibilities
- AI provider performs inference through an adapter if accessible; code-generation and scene construction can call Python/Node/Chromium as the chosen skill requires.
- Chromium frame rasterization and FFmpeg encode dominate many CPU/RAM/video tasks. **Rust does not automatically improve render throughput.** Benchmark, then optimize bottlenecks.
- No second FastAPI service unless measurements prove an unavoidable need and PM/CEO approve the change.

## Bootstrap guidance
Pin compatible dependency versions during Phase 0 in `Cargo.lock` and language-specific lockfiles. Suggested command family: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `ffprobe`, and optional local smoke scripts. Model provider integration should be separable from mock CI tests; CI must not require external accounts.

## References
- Axum: https://docs.rs/axum/latest/axum/
- Tokio process lifecycle: https://docs.rs/tokio/latest/tokio/process/struct.Command.html
- SQLx: https://docs.rs/sqlx/latest/sqlx/
