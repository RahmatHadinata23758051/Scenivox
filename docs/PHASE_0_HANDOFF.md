# Phase 0 handoff — OMP (concise scope)

## Preparation by CEO
1. Clone `https://github.com/RahmatHadinata23758051/Scenivox` (initially empty).
2. Copy this bundle's **contents** (`README.md`, `AGENTS.md`, `docs/`) to repo root. Keep this planning ZIP separate from vendor skill files.
3. Ensure `stick-figure-windows-v2.zip` is reachable inside local project workspace. Do not execute it or commit raw ZIP to public GitHub until audited.
4. Give OMP the **P0-01-only** assignment below. No backend bootstrapping yet.

## Suggested OMP prompt
> Read `AGENTS.md` and relevant docs. Execute Trello **P0-01** only: perform a static security/license/entrypoint/dependency audit of the local `stick-figure-windows-v2.zip`. Identify integration risks with our **Rust-first, polyglot-skill** architecture, 60s portrait render and Indonesian audio. Do not run installers or build UI. Create `docs/audits/P0-01-skill-audit.md` with evidence and reproducible safe smoke proposal. Commit verified work; report SHA, blockers, and test/check commands. **Stop for PM review; do not start P0-02.**

## PM gate
Audit must cover provenance, licensing, ZIP integrity, executable files, network installers, generated code trust, isolation feasibility, actual 9:16/60s assumptions, language alignment constraints, OS dependencies and whether a safe smoke run is ready. Outcome may be "blocked / feasible with caveats"; do not fake sample render success.
