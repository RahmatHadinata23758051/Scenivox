# Scenivox — Foundation (v0.3)

**Repository:** https://github.com/RahmatHadinata23758051/Scenivox  
**State:** planning / Phase 0 entry gate. Backend and real rendering first; **NO frontend/UI implementation until CEO signoff**.  
**Roles:** CEO/PO = product direction and approval; ChatGPT = PM/backlog/QA; OMP = lead developer, may delegate.

## Product goal
Scenivox is an open-source-intended, local-first, single-user, **model-agnostic AI video studio**. Idea → AI storyboard with narration, timing and direction → human scene edits → approval of a specific revision → animated 9:16 MP4 with Indonesian narration and captions. The output must contain **real moving scenes**, not just a slideshow. Initial target: 60 seconds, 1080×1920, 30 fps where engine permits.

## Approved architectural direction
- **Core:** Rust + Axum + Tokio; persistence via SQLx + SQLite; local filesystem for assets and metadata hashes.
- **Skills:** remain polyglot (existing Python, Node/JavaScript, Canvas/Playwright, Chromium, FFmpeg). The Rust core invokes **audited, controlled subprocesses**; do not rewrite working vendor engines into Rust.
- **AI:** provider-neutral model registry and selection per role (`storyboard_planning`, `scene_code_generation`) in backend API/CLI, with mock adapter and explicit per-project defaults/overrides. Existing app subscriptions **do not imply API access**.
- **Runtime:** OMP is the coding agent during development, **not** a dependency of Scenivox in production. No need for a separate FastAPI application.
- **Phase 3 frontend:** deferred; CEO will supply visual references and approve direction first. Nothing in these docs authorizes building it now.

## Phase gates
0. Audit ZIP and verify skill feasibility; establish Rust development baseline and resolve contracts.
1. Persist projects, scene revisions, approvals, selected AI models, and API/CLI workflows with deterministic mock inference.
2. Actual audited animation generation, Indonesian voice input/provider, subtitles, video jobs and validated MP4.
2.5. Stability, partial rerender/cache, extensibility.
3. Web studio, **hold until CEO approves frontend direction**.

## Read order for OMP
1. `AGENTS.md` — operating rules and stop gates.
2. `docs/PRD.md` — desired behavior and acceptance.
3. `docs/ARCHITECTURE.md` and `docs/TECH_STACK.md` — boundaries and technology choices.
4. `docs/API_CONTRACT.md`, `docs/SKILL_SPEC.md`, `docs/TEST_STRATEGY.md` — interfaces and tests.
5. `docs/BACKLOG.md`, `docs/DELIVERY_PLAN.md`, `docs/DECISIONS.md` — execution and decisions.
6. `docs/PHASE_0_HANDOFF.md` — first approved task scope.
7. P0-03 contracts: `docs/DOMAIN_CONTRACT.md`, `docs/MODEL_CONTRACT.md`, `schemas/v1/contracts.schema.json`; ADR-017–021 in `docs/DECISIONS.md`. These define future Phase 1 behavior, not implemented endpoints.

## Coordination
- **Trello:** https://trello.com/b/fG3KW5bF/ai-video-studio-development (live task statuses win over local backlog Markdown).
- **GitHub:** https://github.com/RahmatHadinata23758051/Scenivox (code/commit source of truth; confirmed empty at planning time).
- The supplied `stick-figure-windows-v2.zip` is a **local input**, not bundled into this planning ZIP. OMP must locate it in the CEO's local project directory and audit it **before executing anything inside it**.
- Public repository currently has no license chosen. Do not assume third-party skill files or media may automatically be republished.

**Scope notice:** These are specifications and starter decisions, not evidence of a functioning backend or rendered video. Technical tradeoffs may be proposed by OMP, but require PM review and CEO signoff for major deviations.
