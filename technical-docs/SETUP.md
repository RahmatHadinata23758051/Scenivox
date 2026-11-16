# Backend setup

Requirements: Rust stable (see `rust-toolchain.toml`), Cargo, and a C toolchain for SQLx's bundled SQLite build if your platform requires it. No Node, Python, animation skill, AI provider, or frontend is used by this bootstrap.

1. Copy `.env.example` to `.env` and adjust values if needed.
2. Start the API: `cargo run -- serve` (or simply `cargo run`). The process loads `.env` when present, migrates the configured SQLite database on startup, then binds to `SCENIVOX_ADDR` (default `127.0.0.1:3000`).
3. Probe it: `curl http://127.0.0.1:3000/health`; expected response: `{"status":"ok"}`. The endpoint checks SQLite with `SELECT 1`.
4. Run checks: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test`.

Configuration:

- `SCENIVOX_ADDR`: socket address; defaults to loopback for local-only use.
- `SCENIVOX_DATABASE_URL`: SQLx SQLite URL; `.env.example` uses `sqlite://scenivox.db?mode=rwc`, which creates the database file on first startup. Parent directories must already exist.
- `RUST_LOG`: tracing filter; defaults to `info`.

P1-01 migrations add projects, immutable revisions, storyboards and scenes; startup applies them before serving. `cargo run -- project --help` lists the local persistence commands. Create inputs are strict JSON files; revision input supplies a complete snapshot and expected parent pointer. The HTTP API exposes corresponding routes under `/v1/projects`.

Project deletion is soft deletion: API reads hide the project and its history, but immutable rows and idempotency records remain in SQLite. There is no restore command. Revision `asset_hashes` are metadata only; SFX asset references are rejected until an asset registry exists. No provider, skill, or network request is triggered by persistence commands.
