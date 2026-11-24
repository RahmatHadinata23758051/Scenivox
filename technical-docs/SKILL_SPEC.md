# Scenivox Skill & Agent Execution Contract — ADR-02

**Status:** Specification for Phase 2 skill bundles, AI agent tool runtime, unified composition, and Linux OCI container isolation (including Windows via WSL2). No third-party skill execution is enabled at `53db65bc72b0cc37162a5bbcac3689f126cdcf97`.

---

## 1. Untrusted Boundaries

Scenivox treats **all four** of the following as untrusted:
1. **Skill bundles & instructions (`SKILL.md`, `references/`, `scripts/`):** Never executed on the host; `SKILL.md` is untrusted text context only.
2. **Model outputs:** Structured JSON, tool calls, and text are schema-, path-, and size-validated before any workspace action.
3. **AI-generated composition code (JS/Python):** Written only to manifest-declared `writable_paths` and executed strictly inside an isolated Linux OCI container.
4. **Produced media artifacts (`final.mp4`, stills, audio mixes):** Inspected via `ffmpeg`/`ffprobe` **only** inside an isolated, bounded media-validation OCI container—never parsed directly on an unrestricted host.

---

## 2. Skill Bundle & Manifest Contract

A skill bundle is an explicitly imported local directory or ZIP archive pinned by SHA-256 (`bundle_sha256`). Silent internet downloads during import or render are prohibited.

### Expected Layout
- `skill.manifest.json` — Strict machine-readable manifest (`additionalProperties: false`, required).
- `SKILL.md` — Creative/workflow instructions for the agent (untrusted text data only).
- `references/` *(optional)* — Read-only documentation/examples for bounded agent reading.
- `scripts/` / `engine/` / `template/` / `assets/` *(optional)* — Read-only staged runtime assets/helpers.
- `LICENSE` / `THIRD_PARTY_NOTICES` *(recommended)* — Preserved in bundle provenance.

### Pinned Manifest Schema (`skill.manifest.json`, v1 planned)
```json
{
  "schema_version": "1",
  "id": "stick-figure",
  "version": "0.1.0",
  "kind": "unified_composition",
  "composition_mode": "unified_timeline",
  "rendering_strategy": "canvas_playwright_ffmpeg",
  "supported_aspect_ratios": ["9:16"],
  "max_duration_ms": null,
  "supports_independent_scene_render": false,
  "code_generation_policy": {
    "languages": ["javascript", "python"],
    "writable_paths": ["web/shots.js", "audio.py", "script.json", "captions.json"],
    "requires_sandbox": true
  },
  "entrypoints": {
    "stills": { "runtime": "node", "script": "engine/tools/render.js", "args_template": ["stills", "{times}"], "timeout_sec": 60 },
    "render": { "runtime": "node", "script": "engine/tools/render.js", "args_template": ["video", "{out}"], "timeout_sec": 600 }
  },
  "requires": ["node", "chromium", "ffmpeg", "python"],
  "accepted_inputs": ["revision.json", "narration.wav", "timeline.json", "words.json"],
  "produced_outputs": ["final.mp4", "stills/*.jpg"],
  "audit_status": "static_audited"
}
```
`null` denotes **unverified** limits.

---

## 3. Import, Pinning & Discovery

1. **Import checks:** Reject zip bombs (max uncompressed size, file count, compression ratio), path traversal (`..`, `/`, `:`), symlinks/hardlinks, and duplicate members before copying into Scenivox's skill store.
2. **Immutable pinning:** Store `(skill_id, skill_version, bundle_sha256, manifest_json, audit_status, enabled)` in SQLite `skill_bundles`. Edits require a new version/hash; revisions and jobs stay bound to their pinned hash.
3. **Read-only discovery:** `GET /v1/skills` and `scenivox skill list` never execute skill scripts or download packages.

---

## 4. Unified Composition & Manifest-Declared Agent Tools

Under **ADR-02**, Scenivox's in-process Rust agent (`src/agent.rs`) drives unified composition generation without requiring Claude Code, OMP, or Antigravity:

- **Unified composition:** The agent receives `revision.json`, `LocalNarration`, and P2-02 `timeline.json` / `words.json`, generating a single cohesive composition (`final.mp4`) using the skill's own internal rendering strategy.
- **Manifest-declared tools and entrypoints only (NO unrestricted shell access):**
  - `workspace_list(rel_dir)` — bounded listing inside job workspace / RO skill `references/`.
  - `workspace_read(rel_path, offset, limit)` — bounded UTF-8 read (`<= 128 KiB`).
  - `workspace_write(rel_path, content)` / `workspace_patch(rel_path, edits)` — restricted strictly to manifest `writable_paths` (never RO mounts or host files).
  - `skill_run_step(entrypoint_id, args)` — runs **only** an allowlisted manifest entrypoint inside the Linux OCI sandbox with validated argument templates (never raw `sh -c` or arbitrary commands).
  - `probe_media(rel_path)` — runs `ffprobe` / frame-diff inspection inside the **isolated MediaQA OCI container** and returns bounded structured metrics.
  - `complete_composition(output_rel_path, summary)` — hashes all workspace files (`workspace_manifest_sha256`) and hands candidate media to final host-controlled MediaQA.

---

## 5. Linux OCI Container Baseline & Prohibitions

Linux OCI container isolation is the **MVP execution baseline**, including on **Windows hosts through WSL2**:

- **Mandatory container controls (composition & MediaQA):**
  - `--network=none` unconditionally (no DNS/HTTP egress or package downloads).
  - `--read-only` root filesystem + RO mounts for skill bundle/runtime and frozen inputs; isolated size-bounded RW job workspace only.
  - `--cap-drop=ALL`, `--security-opt=no-new-privileges`, non-root UID/GID.
  - Explicit `--memory`, `--cpus`, `--pids-limit`, wall-clock timeout, bounded stdout/stderr, and container/process-tree kill on timeout/cancel/crash.
- **Explicitly prohibited:**
  - Host secrets / API keys / `.env` / `scenivox.db` inside containers.
  - Docker or Podman socket mounts (`/var/run/docker.sock`, etc.).
  - `--privileged` containers or added Linux capabilities.
  - Unrestricted host filesystem mounts (`/` or `$HOME` or repo workspace).
  - Outbound or inbound network access during generated-code execution or MediaQA.
- **Fail closed:** If no verified Linux OCI / WSL2 isolation backend is available, execution returns `503 sandbox_unavailable` and starts nothing.

---

## 6. `stick-figure` Vertical Slice Baseline (P0-01B Audited)

Static audit of `C:\Users\user\Downloads\stick-figure-windows-v2.zip` (`f3b7dc6e75b476bae156a4eb5c4eba3dd395765f056715e5cb0d3d0c65ac17a5`, 29/29 files matching `stick-figure-windows/`) established:
- **Reusable core:** Canvas 2D renderer (`lib.js`, `chars.js`, `env.js`, `main.js`, `index.html`, OFL 1.1 fonts `Poppins-Bold.ttf` & `Lora-Variable.ttf`), Playwright `1.49.1` (`render.js`), and FFmpeg (`1080×1920`, 30 fps `libx264` + `aac`).
- **Excluded:** `INSTAL.bat`, `install-windows.sh`, `make.sh`, English `tts_kokoro.py` / `align.py`, and `tts_elevenlabs.py`.
- **Offline mock vertical slice (`P2-03B`):** Runs `DeterministicMockLlm` + `stick-figure` bundle + user/fixture WAV inside the Linux OCI `--network=none` sandbox and validates output inside the isolated MediaQA OCI container **before** remote LLM or real Indonesian TTS integration.
