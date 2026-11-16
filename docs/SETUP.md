# Backend setup

Requirements: Rust stable (see `rust-toolchain.toml`), Cargo, and a C toolchain for SQLx's bundled SQLite build if your platform requires it. No Node, Python, animation skill, AI provider, or frontend is used by this bootstrap.

1. Copy `.env.example` to `.env` and adjust values if needed.
2. Start the API: `cargo run -- serve` (or simply `cargo run`). The process loads `.env` when present, migrates the configured SQLite database on startup, then binds to `SCENIVOX_ADDR` (default `127.0.0.1:3000`).
3. Probe it: `curl http://127.0.0.1:3000/health`; expected response: `{"status":"ok"}`. The endpoint checks SQLite with `SELECT 1`.
4. Run checks: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test`.

Configuration:

- `SCENIVOX_ADDR`: socket address; defaults to loopback for local-only use.
- `SCENIVOX_DATABASE_URL`: SQLx SQLite URL; defaults to `sqlite://scenivox.db` in the working directory. Use `?mode=rwc` when creating a new database file; parent directories must already exist. For example, `sqlite://./data/scenivox.db?mode=rwc` (create `data/` first).
- `RUST_LOG`: tracing filter; defaults to `info`.

The current migration creates only a metadata table; product entities and API operations remain out of scope for P0-02.
