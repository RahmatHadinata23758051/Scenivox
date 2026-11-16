# Scenivox Skill Contract & Audit — v0.3

## Concept
An AI skill is **not** inherently an executable plugin. A `SKILL.md` may contain instructions/prompts, but Scenivox needs a separately audited executor, data schema, compatibility/capability manifest and security policy. We reuse the skill's appropriate Python/JS/FFmpeg implementations instead of porting them to Rust.

## Manifest (desired contract, not verified capabilities)
```json
{
  "id": "stick-figure",
  "version": "0.1.0",
  "kind": "code_animation",
  "supported_aspect_ratios": ["9:16"],
  "max_duration_seconds": null,
  "supports_independent_scene_render": null,
  "requires": ["python", "node", "chromium", "ffmpeg"],
  "input_schema_version": "1",
  "output_schema_version": "1",
  "accepted_inputs": ["storyboard.json", "voice.wav", "timings.json"],
  "possible_outputs": ["shots", "mp4", "manifest.json"]
}
```
A `null` denotes **unverified**, not guaranteed. P0-01 must audit before publishing supported runtime features, clip duration, animation types, or partial rerender semantics.

## Candidate ZIP
The creator provided `stick-figure-windows-v2.zip` locally; initial inventory indicates a Windows-oriented adaptation of an existing animation skill, using JS Canvas + Playwright/Chromium, Python audio/align scripts and FFmpeg. Attribution to Artem Novitckii is noted in the supplied README; confirm upstream licensing and included subdependencies before distribution. Known likely risks: English-first voice/alignment, OS-specific scripts/dependency downloads, sandbox claims, 60s support and process resource demands.

## Integration order (OMP tasks)
1. Record SHA-256, safe inventory and provenance; check for ZIP path traversal, symlinks, executable installers, post-install scripts, network behavior, asset/font/voice licensing. **Static audit first**.
2. Document exact local prerequisites and safe audit-only commands. **Do not run unreviewed installers**. If a smoke test is permitted, run in an isolated temporary workspace with no tokens or personal files and capture version/probe output.
3. Test the supplied sample honestly (duration, dimensions, moving frames, sound, captions, RSS/time), only after audit approval.
4. Implement typed `RenderInput`/`RenderOutput` JSON and validated data-to-code translation; Rust orchestrates a known executable by argv, no arbitrary shell.
5. Demonstrate one edited scene and an approved full 60s target, with measured `ffprobe`, audio tests and inspection.
6. Use verified Indonesian WAV + alignment as initial TTS integration path. A voice provider choice requires quality/licensing check.
7. Add partial-scene rerender and second skill only if measured engine constraints permit.

## Safety concern: AI-generated scene code
Generated JS/Python is executable and must be treated as **untrusted code**. Merely checking path names or running Chromium does not create a full security sandbox. Prefer constrained declarative scene data, vetted renderer functions and validated AST/templates. If arbitrary code execution is necessary, require an isolated execution environment with network/secret/filesystem restrictions, host-specific fail-closed behavior and explicit user awareness.

## Sample acceptance
A `render` job must yield actual animated frames/video, not static stills repackaged as an MP4. `ffprobe` plus selected-frame inspection and narration/caption review provide evidence; a successful script exit code alone is insufficient.

## Possible future skills
- `krishnagoswami52/stickman-skill` with alternate Canvas/3D approach, independently audited.
- Remotion or equivalent if motion-graphics requirements and operational costs justify it.
- External generative video providers behind approved budget and rights contracts, separate from code-rendering skills.
