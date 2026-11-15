# PRD — Scenivox

**Version:** Draft v0.3 · 2026-11-16 · **Owner:** CEO · **PM:** ChatGPT · **Developer:** OMP  
**Product:** model-agnostic, skill-driven AI video production studio, local-first and personal initially; source intended to be public.

## 1. Problem / outcome
A creator has a video idea but needs an end-to-end way to turn it into structured scenes, narration and *actual animated video* without being locked to Claude Pro or another specific AI platform. The creator wants editorial control **before** paying computational cost to render.

**Success journey:** idea → model-selected AI drafts story and scene direction → creator revises individual scenes/narration/timings → creator approves a specific immutable revision → backend invokes an audited animation skill → narration, captions, effects and video are assembled → user inspects and exports MP4.

## 2. Initial constraints
- **One local user**, Windows + WSL/Linux first; no account management or public hosted SaaS features.
- Target Shorts: 9:16 portrait, 1080×1920 at 30 fps when technically verified, target ~60 seconds, Indonesian voiceover and synchronized captions. Quality threshold is based on observable outputs, not assumptions from example YouTube links.
- Code-driven animated scenes are in-scope. Photorealistic text-to-video is **not guaranteed** by a stickman/Canvas skill.
- **Model choice is backend functionality in Phase 1:** list providers/models, choose defaults by task, per-project settings and per-run overrides, validate capabilities and availability, snapshot effective choices. UI for settings is later.
- Low-cost/local media rendering preferred. External AI inference may be used only through documented APIs with explicit configuration, budget and rights review. No assumption consumer ChatGPT/Gemini/Antigravity/9router usage equals programmatic access.
- **Frontend and visual design STOP gate:** CEO's UI reference/design brief is not yet provided; build no website during Phase 0–2.5.

## 3. Features and priority
| ID | Behavior | Priority |
|---|---|---|
| FR-01 | Create/read/update projects and target format/style/language/duration | P0 |
| FR-02 | Structured storyboard: synopsis, hook, ordered scenes, visual/camera/action, narration, per-scene duration | P0 |
| FR-03 | AI storyboard planner adapter, deterministic mock for development and CI | P0 |
| FR-04 | Review/edit scene content and keep immutable revision history | P0 |
| FR-05 | Approval names a specific immutable storyboard revision/hash; edits require fresh approval | P0 |
| FR-06 | Durable render-job state, logs, retries and artifact manifest | P0 |
| FR-07 | Safe audited skill adapter producing **animated** scene video, not slideshow | P0 |
| FR-08 | Indonesian narration through provider-neutral TTS or user-supplied voice file; timing contract | P0 |
| FR-09 | Captions, audio mixing and playable validated 9:16 MP4 | P0 |
| FR-10 | Select AI provider/model by task with persisted global/project/run choices; capabilities and availability | P0 |
| FR-11 | Fine-grained changed-scene rerender/cache, where supported | P1 |
| FR-12 | Second skill/renderer behind capabilities manifest | P1 |
| FR-13 | Optional external video-gen APIs (explicit cost and rights review) | P1 |
| FR-14 | Web editor/studio with visual design approved by CEO | Deferred |

## 4. Acceptance: golden scenario
Input: `Misteri eksperimen rahasia di bawah tanah yang disembunyikan selama puluhan tahun`, Indonesian language, 9:16, target 60s. AI/model mock can first produce a multi-scene draft. Creator changes narration/scene 3; the new revision has a new hash. Render **must be rejected until explicit approval** of the currently selected revision. Once approved, the engine should actually generate moving frames, narration, caption timing and a playable MP4 with a manifest and real measured properties. If it produces only static images or has no audio, Phase 2 is **not accepted**.

## 5. Nonfunctional requirements
- **Portable:** WSL/Linux first; no undocumented shell script assumptions; Windows-native support assessed per platform rather than claimed.
- **Efficient:** 16 GB RAM consumer machine as design reference; begin with one concurrent browser render. Measure actual elapsed time, CPU/RAM and disk, and do not promise Rust speeds up Chromium/FFmpeg.
- **Traceable:** deterministic mock plans, pinned tool versions, model IDs, prompt settings, asset hashes and source revision/job snapshots.
- **Reliable:** durable SQLite job states; clean timeout/restart behavior and no false-success jobs.
- **Safe:** protect secrets, validate all paths, restrict subprocesses; no uncontrolled execution of AI-generated JS/Python.
- **Licensing:** check original animation skill notices, fonts, voices, sound/music rights before public distribution.

## 6. Out of scope until CEO authorizes
Website/UI design, accounts, multi-tenancy, hosted SaaS payments, marketplace, automatic social publication, unsupported API bypasses, and rewriting third-party rendering frameworks into Rust.

## 7. Open matters
- Target visual quality/aesthetic for first engine; actual sample footage should be reviewed before quality claims.
- Preferred Indonesian voice, licensing and acceptable offline/cloud compromise.
- Official project software license; whether bundled skill source may be redistributed.
- Which model providers have supported callable endpoints and approved usage limits.
- CEO frontend references and approval (Phase 3 only).
