# Backend setup

Requirements: Rust stable (see `rust-toolchain.toml`), Cargo, and a C toolchain for SQLx's bundled SQLite build if your platform requires it. WAV inspection requires no provider, database, Python, animation skill, or frontend.

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

P2-02 adds `cargo run -- voice inspect --file <relative-path>` for user-supplied PCM WAV validation. Run it from the directory containing the audio; empty paths, absolute paths, `.` / `./` components, parent traversal (`..`), non-regular files, and symlinks resolving outside that directory are rejected. Opened-handle metadata validation is local single-user defense-in-depth and does not eliminate kernel-level TOCTOU or symlink races on shared/untrusted filesystems. The command reports metadata only; it does not store audio, use SQLite, synthesize/align speech, or run a render. No TTS binary is installed by the application.
