use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use scenivox::{
    agent::{
        AgentConfig, AgentError, AgentExecutionMode, AgentLimits, AgentRuntime, AgentTaskInput,
        RepairOutcome, ScriptedSimulatedBackend, SimulatedArtifactOutput,
        SimulatedMediaProbeResult, SimulatedStepOutcome, WorkspaceChangeAction,
    },
    db,
    domain::{
        CreateProject, CreateRevision, ModelSelection, PlanStoryboard, ReviewCommand, Revision,
        RevisionInput, SceneInput, StoryboardInput, Target,
    },
    provider::{
        DeterministicMockLlm, ProviderCapabilities, ProviderKind, ScriptedTurn, TaskRole, ToolCall,
    },
    skill::AuditStatus,
    store::Store,
    voice::{AudioInfo, AudioProvenance, LocalNarration, SampleFormat},
};
use serde_json::json;
use uuid::Uuid;

static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

fn coder_caps() -> ProviderCapabilities {
    let mut caps =
        ProviderCapabilities::deterministic_mock("coder-v1", vec![TaskRole::SceneCodeGeneration]);
    caps.adapter_version = "1".into();
    caps
}

fn fresh_input(base: &AgentTaskInput, temp_root: &Path) -> AgentTaskInput {
    let job_id = Uuid::new_v4().to_string();
    let staging_root = temp_root.join("staging");
    let workspace_dir = staging_root.join(&job_id);
    fs::create_dir_all(&workspace_dir).unwrap();
    let mut cloned = base.clone();
    cloned.job_id = job_id;
    cloned.workspace_dir = workspace_dir;
    cloned.staging_root = Some(staging_root);
    cloned
}

fn temp_db_and_dirs(temp: &Path) -> (String, PathBuf, PathBuf, PathBuf, String) {
    let idx = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
    let url = format!(
        "sqlite://{}?mode=rwc",
        temp.join(format!("agent-test-{idx}.db"))
            .to_string_lossy()
            .replace('\\', "/")
    );
    let skill_dir = temp.join(format!("skills-{idx}"));
    let staging_root = temp.join("staging");
    let job_id = Uuid::new_v4().to_string();
    let workspace_dir = staging_root.join(&job_id);
    fs::create_dir_all(&workspace_dir).unwrap();
    (url, skill_dir, staging_root, workspace_dir, job_id)
}

fn create_benign_stick_figure_skill(root: &Path) -> PathBuf {
    let dir = root.join("src-stick-figure");
    fs::create_dir_all(dir.join("references")).unwrap();
    fs::create_dir_all(dir.join("scripts")).unwrap();
    let manifest = json!({
        "schema_version": "1",
        "id": "stick-figure",
        "version": "audited-v1",
        "kind": "unified_composition",
        "composition_mode": "unified_timeline",
        "rendering_strategy": "canvas_playwright_ffmpeg",
        "supported_aspect_ratios": ["9:16"],
        "max_duration_ms": null,
        "supports_independent_scene_render": false,
        "code_generation_policy": {
            "languages": ["javascript", "python"],
            "writable_paths": ["web/shots.js", "audio.py", "script.json"],
            "requires_sandbox": true
        },
        "entrypoints": {
            "render": {
                "runtime": "node",
                "script": "scripts/render.js",
                "args_template": ["video", "{out}"],
                "timeout_sec": 300
            }
        },
        "requires": ["node", "playwright", "ffmpeg", "python"],
        "accepted_inputs": ["revision.json", "narration.json", "timeline.json"],
        "produced_outputs": ["final.mp4", "stills/*.jpg"]
    });
    fs::write(
        dir.join("skill.manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    fs::write(
        dir.join("SKILL.md"),
        "# Stick Figure Unified Skill\n\n\
         Read references/scene-guide.md, write unified composition to web/shots.js and audio.py,\n\
         never render 0-frame scenes, and run entrypoint `render`.\n",
    )
    .unwrap();
    fs::write(
        dir.join("references/scene-guide.md"),
        "Canvas must define POSITIVE_FPS=30 and non-zero frame durations matching revision.json.\n",
    )
    .unwrap();
    // Sentinel file proving P2-03A NEVER executes skill scripts on the host.
    fs::write(
        dir.join("scripts/render.js"),
        "throw new Error('P2-03A must never execute skill scripts!');\n",
    )
    .unwrap();
    dir
}

async fn setup_approved_project_with_skill(temp: &Path) -> (AgentTaskInput, Store) {
    let (db_url, skill_dir, staging_root, workspace_dir, job_id) = temp_db_and_dirs(temp);
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool);

    let skill_src = create_benign_stick_figure_skill(temp);
    let (skill_rec, _) = store
        .import_skill(&skill_dir, None, &skill_src)
        .await
        .expect("import benign skill");
    // Host-owned audit_status stays Unaudited (we never forge smoke_verified in P2-03A).
    assert_eq!(skill_rec.audit_status, AuditStatus::Unaudited);

    let target = Target {
        duration_ms: 15_000,
        aspect_ratio: "9:16".into(),
        fps: 30,
        language: "id-ID".into(),
        style_id: "stick-figure".into(),
    };
    let project = store
        .create_project(CreateProject {
            schema_version: "1".into(),
            title: "P2-03A Agent Runtime Demo".into(),
            idea: "Misteri eksperimen rahasia di bawah tanah".into(),
            target: target.clone(),
            model_selection: ModelSelection::default(),
        })
        .await
        .unwrap();

    let revision: Revision = store
        .plan_storyboard(
            &project.id,
            None,
            PlanStoryboard {
                schema_version: "1".into(),
                idea: project.idea.clone(),
                target,
                expected_revision_id: None,
                model_selection: ModelSelection::default(),
            },
            &scenivox::planner::DeterministicMockPlanner,
        )
        .await
        .unwrap();

    store
        .review_revision(
            &project.id,
            &revision.id,
            None,
            ReviewCommand {
                schema_version: "1".into(),
                content_sha256: revision.content_sha256.clone(),
            },
        )
        .await
        .unwrap();
    let (approval, _, _) = store
        .approve_revision(
            &project.id,
            &revision.id,
            None,
            ReviewCommand {
                schema_version: "1".into(),
                content_sha256: revision.content_sha256.clone(),
            },
        )
        .await
        .unwrap();

    let narration = LocalNarration {
        schema_version: "1".into(),
        audio: AudioInfo {
            format: SampleFormat::PcmSigned16,
            sample_rate_hz: 16_000,
            channels: 1,
            frame_count: 240_000,
            duration_ms: 15_000,
            sha256: "a".repeat(64),
        },
        provenance: AudioProvenance::UserSupplied,
    };

    let task_input = AgentTaskInput {
        job_id,
        revision,
        approval,
        skill: skill_rec,
        skill_store_dir: skill_dir,
        workspace_dir,
        staging_root: Some(staging_root),
        narration: Some(narration),
        timeline: None,
    };
    (task_input, store)
}

#[tokio::test]
async fn acceptance_demonstration_reads_skill_writes_js_python_repairs_simulated_error_and_completes()
 {
    let temp = tempfile::tempdir().unwrap();
    let (task_input, _store) = setup_approved_project_with_skill(temp.path()).await;
    let orig_rev_sha = task_input.revision.content_sha256.clone();
    let orig_prompt_sha = task_input
        .revision
        .content
        .models
        .scene_code_generation
        .prompt_sha256
        .clone();

    let provider = DeterministicMockLlm::new(
        coder_caps(),
        vec![
            // 1. Read benign skill instructions & reference guide + frozen revision input.
            ScriptedTurn::tool_calls(
                "Inspecting workspace, reference guide, and frozen revision input.",
                vec![
                    ToolCall {
                        call_id: "c1".into(),
                        name: "workspace_list".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        call_id: "c2".into(),
                        name: "workspace_read".into(),
                        arguments: json!({ "rel_path": "references/scene-guide.md" }),
                    },
                    ToolCall {
                        call_id: "c3".into(),
                        name: "workspace_read".into(),
                        arguments: json!({ "rel_path": "inputs/revision.json" }),
                    },
                ],
                320,
                90,
            ),
            // 2. Write dynamic JavaScript + Python composition (introducing an intentional FPS=0 bug in JS).
            ScriptedTurn::tool_calls(
                "Writing initial unified JS shots and Python audio mix script.",
                vec![
                    ToolCall {
                        call_id: "c4".into(),
                        name: "workspace_write".into(),
                        arguments: json!({
                            "rel_path": "web/shots.js",
                            "content": "export const FPS = 0; // BUG: triggers simulated render failure\nexport const scenes = [1, 2];\n"
                        }),
                    },
                    ToolCall {
                        call_id: "c5".into(),
                        name: "workspace_write".into(),
                        arguments: json!({
                            "rel_path": "audio.py",
                            "content": "TARGET_DURATION_MS = 15000\nSAMPLE_RATE = 16000\n# Simulated audio cue plan\n"
                        }),
                    },
                ],
                480,
                140,
            )
            .with_expected_substring("POSITIVE_FPS=30"),
            // 3. Run simulated render step -> receives simulated error from FPS = 0.
            ScriptedTurn::tool_call(
                "Running simulated render step for initial composition.",
                "c6",
                "skill_run_step",
                json!({ "entrypoint_id": "render", "args": { "out": "final.mp4" } }),
                520,
                40,
            ),
            // 4. Inspect error & repair web/shots.js via workspace_patch, then re-run render step.
            ScriptedTurn::tool_calls(
                "Repairing invalid FPS = 0 in web/shots.js and retrying simulated render.",
                vec![
                    ToolCall {
                        call_id: "c7".into(),
                        name: "workspace_patch".into(),
                        arguments: json!({
                            "rel_path": "web/shots.js",
                            "edits": [{
                                "old_text": "export const FPS = 0; // BUG: triggers simulated render failure",
                                "new_text": "export const FPS = 30; // Repaired to match 30fps target"
                            }]
                        }),
                    },
                    ToolCall {
                        call_id: "c8".into(),
                        name: "skill_run_step".into(),
                        arguments: json!({ "entrypoint_id": "render", "args": { "out": "final.mp4" } }),
                    },
                ],
                640,
                110,
            )
            .with_expected_last_tool("skill_run_step")
            .with_expected_substring("simulated_render_error"),
            // 5. Probe simulated media output & finalize workspace manifest + composition candidate.
            ScriptedTurn::tool_calls(
                "Probing simulated media output and completing composition candidate.",
                vec![
                    ToolCall {
                        call_id: "c9".into(),
                        name: "probe_media".into(),
                        arguments: json!({ "rel_path": "final.mp4" }),
                    },
                    ToolCall {
                        call_id: "c10".into(),
                        name: "complete_composition".into(),
                        arguments: json!({
                            "output_rel_path": "final.mp4",
                            "summary": "Repaired FPS boundary in web/shots.js and produced simulated candidate after simulated probe."
                        }),
                    },
                ],
                720,
                95,
            ),
        ],
    );

    let simulated_step_calls = Arc::new(AtomicUsize::new(0));
    let simulated_probe_calls = Arc::new(AtomicUsize::new(0));
    let step_counter = Arc::clone(&simulated_step_calls);
    let probe_counter = Arc::clone(&simulated_probe_calls);

    let backend = ScriptedSimulatedBackend::new(
        move |req| {
            let n = step_counter.fetch_add(1, Ordering::Relaxed);
            assert_eq!(req.entrypoint_id, "render");
            assert_eq!(req.resolved_args, vec!["video", "final.mp4"]);
            let shots = req
                .workspace_files
                .get("web/shots.js")
                .and_then(|f| f.utf8_text.as_deref())
                .unwrap_or("");
            if shots.contains("FPS = 0") {
                Ok(SimulatedStepOutcome {
                    simulated: true,
                    succeeded: false,
                    exit_code: 2,
                    error_code: Some("simulated_render_error".into()),
                    stdout: String::new(),
                    stderr: format!("SIMULATED ERROR (call {n}): FPS must be > 0 in web/shots.js"),
                    simulated_artifacts: Vec::new(),
                })
            } else if shots.contains("FPS = 30") {
                Ok(SimulatedStepOutcome {
                    simulated: true,
                    succeeded: true,
                    exit_code: 0,
                    error_code: None,
                    stdout: "SIMULATED RENDER OK: wrote simulated placeholder final.mp4 (not a real MP4)"
                        .into(),
                    stderr: String::new(),
                    simulated_artifacts: vec![SimulatedArtifactOutput {
                        rel_path: "final.mp4".into(),
                        content_bytes: b"SIMULATED_CANDIDATE_PLACEHOLDER_NOT_REAL_MP4_V1".to_vec(),
                        simulated: true,
                        description: "Simulated test candidate placeholder (no real video encoded)".into(),
                    }],
                })
            } else {
                unreachable!("unexpected generated source state")
            }
        },
        move |req| {
            probe_counter.fetch_add(1, Ordering::Relaxed);
            assert_eq!(req.rel_path, "final.mp4");
            Ok(SimulatedMediaProbeResult {
                simulated: true,
                real_mp4_rendered: false,
                rel_path: req.rel_path.clone(),
                sha256: req.sha256.clone(),
                byte_size: req.byte_size,
                width: 1080,
                height: 1920,
                fps: 30,
                duration_ms: 15_000,
                has_audio: true,
                motion_detected: true,
                notice:
                    "SIMULATED_MEDIA_PROBE: metadata produced by test double; no real MP4 rendered."
                        .into(),
            })
        },
    );

    let config = AgentConfig {
        execution_mode: AgentExecutionMode::SimulatedTestOnly,
        // P2-03A explicitly avoids fake production audit promotion on the freshly imported fixture.
        require_smoke_verified_skill: false,
        limits: AgentLimits::default(),
    };

    let output = AgentRuntime::run(&config, &provider, None, &task_input, &backend, &backend)
        .expect("agent runtime completes repair loop deterministically");

    // 1. All simulated flags are explicit; zero real MP4 or real execution claimed.
    assert!(output.candidate.simulated);
    assert!(!output.candidate.real_mp4_rendered);
    assert!(output.candidate.simulation_notice.contains("SIMULATED"));
    assert!(output.provenance.all_execution_and_media_outputs_simulated);
    assert!(!output.provenance.real_mp4_rendered);
    assert_eq!(simulated_step_calls.load(Ordering::Relaxed), 2);
    assert_eq!(simulated_probe_calls.load(Ordering::Relaxed), 1);

    // 2. Immutable approved revision + initial prompt hashes preserved untouched, with separate instruction envelope.
    assert_eq!(
        output.provenance.revision_anchor.content_sha256,
        orig_rev_sha
    );
    assert_eq!(
        output
            .provenance
            .revision_anchor
            .approved_initial_prompt_sha256,
        orig_prompt_sha
    );
    assert!(
        output
            .provenance
            .revision_anchor
            .approved_initial_prompt_preserved
    );
    assert!(!output.provenance.revision_anchor.is_legacy_manual_revision);
    assert_eq!(
        output
            .provenance
            .revision_anchor
            .frozen_initial_prompt_artifact_sha256
            .as_deref(),
        Some(orig_prompt_sha.as_str())
    );
    assert!(
        output
            .provenance
            .revision_anchor
            .frozen_initial_prompt_artifact
            .is_some()
    );
    assert_eq!(
        output.provenance.instruction_envelope.envelope_sha256.len(),
        64
    );
    assert_ne!(
        output.provenance.instruction_envelope.envelope_sha256,
        orig_prompt_sha
    );

    // 3. Separate canonical per-turn request hashes, responses, tool calls, workspace changes, and repair outcome.
    assert_eq!(output.provenance.turns.len(), 5);
    let distinct_req_hashes: std::collections::HashSet<&str> = output
        .provenance
        .turns
        .iter()
        .map(|t| t.request_sha256.as_str())
        .collect();
    assert_eq!(distinct_req_hashes.len(), 5);
    assert_eq!(output.provenance.tool_calls.len(), 10);
    assert_eq!(output.provenance.repair_attempts.len(), 1);
    let repair = &output.provenance.repair_attempts[0];
    assert_eq!(repair.outcome, RepairOutcome::Succeeded);
    assert_eq!(repair.trigger_error_code, "simulated_render_error");
    assert_eq!(repair.repaired_paths, vec!["web/shots.js"]);
    assert_eq!(repair.triggered_by_turn, 2);
    assert_eq!(repair.repaired_in_turn, 3);
    assert_eq!(repair.verification_turn, Some(3));

    // Workspace changes cover JS create, Python create, JS patch, and simulated output.
    assert_eq!(output.provenance.workspace_changes.len(), 4);
    assert_eq!(
        output.provenance.workspace_changes[0].action,
        WorkspaceChangeAction::Create
    );
    assert_eq!(
        output.provenance.workspace_changes[2].action,
        WorkspaceChangeAction::Patch
    );
    assert_eq!(
        output.provenance.workspace_changes[3].action,
        WorkspaceChangeAction::SimulatedArtifact
    );
    assert!(output.provenance.workspace_changes[3].simulated);

    // 4. Workspace manifest accurately binds frozen inputs, repaired JS/Python, and simulated artifact.
    assert_eq!(
        output.candidate.workspace_manifest_sha256,
        output.workspace_manifest.manifest_sha256
    );
    let manifest_paths: Vec<&str> = output
        .workspace_manifest
        .entries
        .iter()
        .map(|e| e.rel_path.as_str())
        .collect();
    assert!(manifest_paths.contains(&"inputs/revision.json"));
    assert!(manifest_paths.contains(&"inputs/initial_scene_prompt.json"));
    assert!(manifest_paths.contains(&"inputs/narration.json"));
    assert!(manifest_paths.contains(&"web/shots.js"));
    assert!(manifest_paths.contains(&"audio.py"));
    assert!(manifest_paths.contains(&"final.mp4"));

    // 5. Verify no host paths leaked into provider prompts/responses or candidate summary.
    let serialized_trace = serde_json::to_string(&output).unwrap();
    let temp_str = temp.path().to_string_lossy().to_string();
    assert!(!serialized_trace.contains(&temp_str));
}

#[tokio::test]
async fn enforces_writable_paths_ro_skill_bounds_limits_and_prohibits_real_execution() {
    let temp = tempfile::tempdir().unwrap();
    let (task_input, _store) = setup_approved_project_with_skill(temp.path()).await;

    // Case A: Attempting to write outside manifest writable_paths (e.g. SKILL.md, .env, ../escape.js, scripts/render.js)
    // is rejected cleanly by WorkspaceBroker without modifying disk or exposing host paths.
    let forbidden_writes_provider = DeterministicMockLlm::new(
        coder_caps(),
        vec![
            ScriptedTurn::tool_calls(
                "Testing forbidden writes",
                vec![
                    ToolCall {
                        call_id: "b1".into(),
                        name: "workspace_write".into(),
                        arguments: json!({ "rel_path": "SKILL.md", "content": "hacked" }),
                    },
                    ToolCall {
                        call_id: "b2".into(),
                        name: "workspace_write".into(),
                        arguments: json!({ "rel_path": "../escape.js", "content": "hacked" }),
                    },
                    ToolCall {
                        call_id: "b3".into(),
                        name: "workspace_write".into(),
                        arguments: json!({ "rel_path": "inputs/revision.json", "content": "{}" }),
                    },
                    ToolCall {
                        call_id: "b4".into(),
                        name: "workspace_read".into(),
                        arguments: json!({ "rel_path": ".env" }),
                    },
                    ToolCall {
                        call_id: "b5".into(),
                        name: "skill_run_step".into(),
                        arguments: json!({ "entrypoint_id": "sh -c id" }),
                    },
                ],
                100,
                50,
            ),
            ScriptedTurn::final_stop("Giving up without completing.", 120, 20),
        ],
    );

    let noop_backend = ScriptedSimulatedBackend::new(
        |_| unreachable!("forbidden entrypoint must not invoke executor"),
        |_| unreachable!("no probe expected"),
    );

    let cfg = AgentConfig {
        execution_mode: AgentExecutionMode::SimulatedTestOnly,
        require_smoke_verified_skill: false,
        limits: AgentLimits::default(),
    };
    let err = AgentRuntime::run(
        &cfg,
        &forbidden_writes_provider,
        None,
        &task_input,
        &noop_backend,
        &noop_backend,
    )
    .unwrap_err();
    assert_eq!(err.code(), "incomplete_composition");
    assert!(!temp.path().join("escape.js").exists());

    // Case B: Non-simulated backend (`simulated: false`) fails closed immediately.
    let non_sim_provider = DeterministicMockLlm::new(
        coder_caps(),
        vec![ScriptedTurn::tool_call(
            "Invoke step",
            "ns1",
            "skill_run_step",
            json!({ "entrypoint_id": "render" }),
            50,
            20,
        )],
    );
    let non_sim_backend = ScriptedSimulatedBackend::new(
        |_| {
            Ok(SimulatedStepOutcome {
                simulated: false,
                succeeded: true,
                exit_code: 0,
                error_code: None,
                stdout: String::new(),
                stderr: String::new(),
                simulated_artifacts: Vec::new(),
            })
        },
        |_| unreachable!(),
    );
    let err = AgentRuntime::run(
        &cfg,
        &non_sim_provider,
        None,
        &fresh_input(&task_input, temp.path()),
        &non_sim_backend,
        &non_sim_backend,
    )
    .unwrap_err();
    assert!(matches!(err, AgentError::RealExecutionProhibited(_)));
    assert_eq!(err.code(), "sandbox_unavailable");

    // Case C: Requiring smoke_verified rejects freshly imported unaudited skill without fake promotion.
    let strict_cfg = AgentConfig {
        execution_mode: AgentExecutionMode::SimulatedTestOnly,
        require_smoke_verified_skill: true,
        limits: AgentLimits::default(),
    };
    let err = AgentRuntime::run(
        &strict_cfg,
        &forbidden_writes_provider,
        None,
        &fresh_input(&task_input, temp.path()),
        &noop_backend,
        &noop_backend,
    )
    .unwrap_err();
    assert_eq!(err.code(), "skill_unavailable");

    // Case D: Exceeding repair limit fails closed with AgentError::RepairLimitExceeded.
    let repair_loop_provider = DeterministicMockLlm::new(
        coder_caps(),
        vec![
            ScriptedTurn::tool_call(
                "Initial write",
                "r1",
                "workspace_write",
                json!({ "rel_path": "web/shots.js", "content": "bad1" }),
                40,
                20,
            ),
            ScriptedTurn::tool_call(
                "Run 1",
                "r2",
                "skill_run_step",
                json!({ "entrypoint_id": "render" }),
                40,
                20,
            ),
            ScriptedTurn::tool_call(
                "Repair 1",
                "r3",
                "workspace_write",
                json!({ "rel_path": "web/shots.js", "content": "bad2" }),
                40,
                20,
            ),
            ScriptedTurn::tool_call(
                "Run 2",
                "r4",
                "skill_run_step",
                json!({ "entrypoint_id": "render" }),
                40,
                20,
            ),
            ScriptedTurn::tool_call(
                "Repair 2 (exceeds max_repairs=1)",
                "r5",
                "workspace_write",
                json!({ "rel_path": "web/shots.js", "content": "bad3" }),
                40,
                20,
            ),
        ],
    );
    let always_fail_backend = ScriptedSimulatedBackend::new(
        |_| {
            Ok(SimulatedStepOutcome {
                simulated: true,
                succeeded: false,
                exit_code: 1,
                error_code: Some("simulated_render_error".into()),
                stdout: String::new(),
                stderr: "bad composition".into(),
                simulated_artifacts: Vec::new(),
            })
        },
        |_| unreachable!(),
    );
    let limited_cfg = AgentConfig {
        execution_mode: AgentExecutionMode::SimulatedTestOnly,
        require_smoke_verified_skill: false,
        limits: AgentLimits {
            max_repairs: 1,
            ..AgentLimits::default()
        },
    };
    let err = AgentRuntime::run(
        &limited_cfg,
        &repair_loop_provider,
        None,
        &fresh_input(&task_input, temp.path()),
        &always_fail_backend,
        &always_fail_backend,
    )
    .unwrap_err();
    assert_eq!(err.code(), "agent_repair_limit_exceeded");

    // Case E: Exact manifest-declared tool surface (no credentials, pricing, or auth tools).
    let tool_names: Vec<String> = scenivox::agent::WorkspaceBroker::tool_definitions()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(
        tool_names,
        vec![
            "workspace_list",
            "workspace_read",
            "workspace_write",
            "workspace_patch",
            "skill_run_step",
            "probe_media",
            "complete_composition",
        ]
    );

    // Case F: Workspace byte cap, turn limit, and tool call limit fail closed.
    let overshoot_provider = DeterministicMockLlm::new(
        coder_caps(),
        vec![ScriptedTurn::tool_call(
            "Oversized JS write",
            "big1",
            "workspace_write",
            json!({ "rel_path": "web/shots.js", "content": "a".repeat(4096) }),
            50,
            20,
        )],
    );
    let tight_bytes_cfg = AgentConfig {
        execution_mode: AgentExecutionMode::SimulatedTestOnly,
        require_smoke_verified_skill: false,
        limits: AgentLimits {
            max_workspace_bytes: 2048,
            ..AgentLimits::default()
        },
    };
    let err = AgentRuntime::run(
        &tight_bytes_cfg,
        &overshoot_provider,
        None,
        &fresh_input(&task_input, temp.path()),
        &noop_backend,
        &noop_backend,
    )
    .unwrap_err();
    assert_eq!(err.code(), "workspace_limit_exceeded");

    let one_turn_provider = DeterministicMockLlm::new(
        coder_caps(),
        vec![ScriptedTurn::tool_call(
            "List once",
            "lt1",
            "workspace_list",
            json!({}),
            40,
            15,
        )],
    );
    let one_turn_cfg = AgentConfig {
        execution_mode: AgentExecutionMode::SimulatedTestOnly,
        require_smoke_verified_skill: false,
        limits: AgentLimits {
            max_turns: 1,
            ..AgentLimits::default()
        },
    };
    let err = AgentRuntime::run(
        &one_turn_cfg,
        &one_turn_provider,
        None,
        &fresh_input(&task_input, temp.path()),
        &noop_backend,
        &noop_backend,
    )
    .unwrap_err();
    assert_eq!(err.code(), "agent_turn_limit_exceeded");

    let two_calls_provider = DeterministicMockLlm::new(
        coder_caps(),
        vec![ScriptedTurn::tool_calls(
            "Two calls",
            vec![
                ToolCall {
                    call_id: "tc1".into(),
                    name: "workspace_list".into(),
                    arguments: json!({}),
                },
                ToolCall {
                    call_id: "tc2".into(),
                    name: "workspace_list".into(),
                    arguments: json!({}),
                },
            ],
            40,
            15,
        )],
    );
    let one_call_cfg = AgentConfig {
        execution_mode: AgentExecutionMode::SimulatedTestOnly,
        require_smoke_verified_skill: false,
        limits: AgentLimits {
            max_tool_calls: 1,
            ..AgentLimits::default()
        },
    };
    let err = AgentRuntime::run(
        &one_call_cfg,
        &two_calls_provider,
        None,
        &fresh_input(&task_input, temp.path()),
        &noop_backend,
        &noop_backend,
    )
    .unwrap_err();
    assert_eq!(err.code(), "agent_tool_call_limit_exceeded");

    // Case G: probe_media claiming real_mp4_rendered=true fails closed (`sandbox_unavailable`).
    let probe_real_provider = DeterministicMockLlm::new(
        coder_caps(),
        vec![
            ScriptedTurn::tool_calls(
                "Generate and run",
                vec![
                    ToolCall {
                        call_id: "pr1".into(),
                        name: "workspace_write".into(),
                        arguments: json!({ "rel_path": "web/shots.js", "content": "export const FPS = 30;" }),
                    },
                    ToolCall {
                        call_id: "pr2".into(),
                        name: "skill_run_step".into(),
                        arguments: json!({ "entrypoint_id": "render" }),
                    },
                ],
                40,
                20,
            ),
            ScriptedTurn::tool_call(
                "Probe",
                "pr3",
                "probe_media",
                json!({ "rel_path": "final.mp4" }),
                40,
                20,
            ),
        ],
    );
    let illegal_probe_backend = ScriptedSimulatedBackend::new(
        |_| {
            Ok(SimulatedStepOutcome {
                simulated: true,
                succeeded: true,
                exit_code: 0,
                error_code: None,
                stdout: "ok".into(),
                stderr: String::new(),
                simulated_artifacts: vec![SimulatedArtifactOutput {
                    rel_path: "final.mp4".into(),
                    content_bytes: b"SIMULATED_PLACEHOLDER".to_vec(),
                    simulated: true,
                    description: "sim".into(),
                }],
            })
        },
        |req| {
            Ok(SimulatedMediaProbeResult {
                simulated: true,
                real_mp4_rendered: true,
                rel_path: req.rel_path.clone(),
                sha256: req.sha256.clone(),
                byte_size: req.byte_size,
                width: 1080,
                height: 1920,
                fps: 30,
                duration_ms: 15_000,
                has_audio: true,
                motion_detected: true,
                notice: "illegal real_mp4_rendered claim".into(),
            })
        },
    );
    let err = AgentRuntime::run(
        &cfg,
        &probe_real_provider,
        None,
        &fresh_input(&task_input, temp.path()),
        &illegal_probe_backend,
        &illegal_probe_backend,
    )
    .unwrap_err();
    assert!(matches!(err, AgentError::RealExecutionProhibited(_)));
    assert_eq!(err.code(), "sandbox_unavailable");

    // Case H: Tampered approval hash rejects execution immediately.
    let mut bad_approval_input = fresh_input(&task_input, temp.path());
    bad_approval_input.approval.content_sha256 = "f".repeat(64);
    let err = AgentRuntime::run(
        &cfg,
        &one_turn_provider,
        None,
        &bad_approval_input,
        &noop_backend,
        &noop_backend,
    )
    .unwrap_err();
    assert_eq!(err.code(), "hash_mismatch");
}

#[tokio::test]
async fn qa_approved_input_and_workspace_binding_regressions() {
    let temp = tempfile::tempdir().unwrap();
    let (task_input, _store) = setup_approved_project_with_skill(temp.path()).await;
    let cfg = AgentConfig {
        execution_mode: AgentExecutionMode::SimulatedTestOnly,
        require_smoke_verified_skill: false,
        limits: AgentLimits::default(),
    };
    let noop_backend = ScriptedSimulatedBackend::new(|_| unreachable!(), |_| unreachable!());
    let dummy_provider =
        DeterministicMockLlm::new(coder_caps(), vec![ScriptedTurn::final_stop("ok", 10, 5)]);

    // 1. Mismatched skill_version against approved revision is rejected cleanly.
    let mut mismatched_skill = fresh_input(&task_input, temp.path());
    mismatched_skill.skill.skill_version = "other-v2".into();
    let err = AgentRuntime::run(
        &cfg,
        &dummy_provider,
        None,
        &mismatched_skill,
        &noop_backend,
        &noop_backend,
    )
    .unwrap_err();
    assert_eq!(err.code(), "skill_unavailable");

    // 2. Non-empty workspace (pre-existing file) is rejected fail-closed.
    let non_empty_ws = fresh_input(&task_input, temp.path());
    fs::write(non_empty_ws.workspace_dir.join("stale.txt"), b"leftover").unwrap();
    let err = AgentRuntime::run(
        &cfg,
        &dummy_provider,
        None,
        &non_empty_ws,
        &noop_backend,
        &noop_backend,
    )
    .unwrap_err();
    assert_eq!(err.code(), "unsafe_workspace_path");

    // 3. Simulated artifact colliding with `inputs/revision.json` or writable `web/shots.js` fails closed.
    let collision_input = fresh_input(&task_input, temp.path());
    let collision_provider = DeterministicMockLlm::new(
        coder_caps(),
        vec![ScriptedTurn::tool_calls(
            "Write and run",
            vec![
                ToolCall {
                    call_id: "col1".into(),
                    name: "workspace_write".into(),
                    arguments: json!({ "rel_path": "web/shots.js", "content": "export const FPS = 30;" }),
                },
                ToolCall {
                    call_id: "col2".into(),
                    name: "skill_run_step".into(),
                    arguments: json!({ "entrypoint_id": "render" }),
                },
            ],
            40,
            20,
        )],
    );
    let colliding_backend = ScriptedSimulatedBackend::new(
        |_| {
            Ok(SimulatedStepOutcome {
                simulated: true,
                succeeded: true,
                exit_code: 0,
                error_code: None,
                stdout: "ok".into(),
                stderr: String::new(),
                simulated_artifacts: vec![SimulatedArtifactOutput {
                    rel_path: "inputs/revision.json".into(),
                    content_bytes: b"overwritten".to_vec(),
                    simulated: true,
                    description: "bad collision".into(),
                }],
            })
        },
        |_| unreachable!(),
    );
    let err = AgentRuntime::run(
        &cfg,
        &collision_provider,
        None,
        &collision_input,
        &colliding_backend,
        &colliding_backend,
    )
    .unwrap_err();
    assert_eq!(err.code(), "unsafe_workspace_path");

    // 4. External file injection / disk tamper before `complete_composition` is caught by disk re-verification.
    let tamper_input = fresh_input(&task_input, temp.path());
    let tamper_ws = tamper_input.workspace_dir.clone();
    let tamper_provider = DeterministicMockLlm::new(
        coder_caps(),
        vec![ScriptedTurn::tool_calls(
            "Run, probe, complete",
            vec![
                ToolCall {
                    call_id: "t1".into(),
                    name: "workspace_write".into(),
                    arguments: json!({ "rel_path": "web/shots.js", "content": "export const FPS = 30;" }),
                },
                ToolCall {
                    call_id: "t2".into(),
                    name: "skill_run_step".into(),
                    arguments: json!({ "entrypoint_id": "render" }),
                },
                ToolCall {
                    call_id: "t3".into(),
                    name: "probe_media".into(),
                    arguments: json!({ "rel_path": "final.mp4" }),
                },
                ToolCall {
                    call_id: "t4".into(),
                    name: "complete_composition".into(),
                    arguments: json!({ "output_rel_path": "final.mp4", "summary": "done" }),
                },
            ],
            40,
            20,
        )],
    );
    let injecting_backend = ScriptedSimulatedBackend::new(
        |_| {
            Ok(SimulatedStepOutcome {
                simulated: true,
                succeeded: true,
                exit_code: 0,
                error_code: None,
                stdout: "ok".into(),
                stderr: String::new(),
                simulated_artifacts: vec![SimulatedArtifactOutput {
                    rel_path: "final.mp4".into(),
                    content_bytes: b"SIMULATED_OK".to_vec(),
                    simulated: true,
                    description: "sim".into(),
                }],
            })
        },
        move |req| {
            // Side-channel untracked write right before `complete_composition`.
            fs::write(tamper_ws.join("untracked_rogue.bin"), b"rogue").unwrap();
            Ok(SimulatedMediaProbeResult {
                simulated: true,
                real_mp4_rendered: false,
                rel_path: req.rel_path.clone(),
                sha256: req.sha256.clone(),
                byte_size: req.byte_size,
                width: 1080,
                height: 1920,
                fps: 30,
                duration_ms: 15_000,
                has_audio: true,
                motion_detected: true,
                notice: "SIMULATED_PROBE".into(),
            })
        },
    );
    let err = AgentRuntime::run(
        &cfg,
        &tamper_provider,
        None,
        &tamper_input,
        &injecting_backend,
        &injecting_backend,
    )
    .unwrap_err();
    assert_eq!(err.code(), "unsafe_workspace_path");
}

#[tokio::test]
async fn qa_completion_state_machine_and_media_probe_validation() {
    let temp = tempfile::tempdir().unwrap();
    let (task_input, _store) = setup_approved_project_with_skill(temp.path()).await;
    let cfg = AgentConfig {
        execution_mode: AgentExecutionMode::SimulatedTestOnly,
        require_smoke_verified_skill: false,
        limits: AgentLimits::default(),
    };

    let render_ok_backend = ScriptedSimulatedBackend::new(
        |_| {
            Ok(SimulatedStepOutcome {
                simulated: true,
                succeeded: true,
                exit_code: 0,
                error_code: None,
                stdout: "ok".into(),
                stderr: String::new(),
                simulated_artifacts: vec![SimulatedArtifactOutput {
                    rel_path: "final.mp4".into(),
                    content_bytes: b"SIMULATED_OK".to_vec(),
                    simulated: true,
                    description: "sim".into(),
                }],
            })
        },
        |req| {
            Ok(SimulatedMediaProbeResult {
                simulated: true,
                real_mp4_rendered: false,
                rel_path: req.rel_path.clone(),
                sha256: req.sha256.clone(),
                byte_size: req.byte_size,
                width: 1080,
                height: 1920,
                fps: 30,
                duration_ms: 15_000,
                has_audio: true,
                motion_detected: true,
                notice: "SIMULATED_PROBE".into(),
            })
        },
    );

    // 1. Completing after render WITHOUT `probe_media` fails cleanly and ends as `incomplete_composition`.
    let no_probe_provider = DeterministicMockLlm::new(
        coder_caps(),
        vec![
            ScriptedTurn::tool_calls(
                "Skip probe",
                vec![
                    ToolCall {
                        call_id: "np1".into(),
                        name: "workspace_write".into(),
                        arguments: json!({ "rel_path": "web/shots.js", "content": "export const FPS = 30;" }),
                    },
                    ToolCall {
                        call_id: "np2".into(),
                        name: "skill_run_step".into(),
                        arguments: json!({ "entrypoint_id": "render" }),
                    },
                    ToolCall {
                        call_id: "np3".into(),
                        name: "complete_composition".into(),
                        arguments: json!({ "output_rel_path": "final.mp4", "summary": "skip probe" }),
                    },
                ],
                40,
                20,
            ),
            ScriptedTurn::final_stop("No probe.", 40, 10)
                .with_expected_substring("media_probe_required"),
        ],
    );
    let err = AgentRuntime::run(
        &cfg,
        &no_probe_provider,
        None,
        &fresh_input(&task_input, temp.path()),
        &render_ok_backend,
        &render_ok_backend,
    )
    .unwrap_err();
    assert_eq!(err.code(), "incomplete_composition");

    // 2. Patching workspace AFTER render & probe invalidates candidate (`stale_candidate`).
    let stale_after_patch_provider = DeterministicMockLlm::new(
        coder_caps(),
        vec![
            ScriptedTurn::tool_calls(
                "Render, probe, then silently patch without re-render",
                vec![
                    ToolCall {
                        call_id: "sp1".into(),
                        name: "workspace_write".into(),
                        arguments: json!({ "rel_path": "web/shots.js", "content": "export const FPS = 30;" }),
                    },
                    ToolCall {
                        call_id: "sp2".into(),
                        name: "skill_run_step".into(),
                        arguments: json!({ "entrypoint_id": "render" }),
                    },
                    ToolCall {
                        call_id: "sp3".into(),
                        name: "probe_media".into(),
                        arguments: json!({ "rel_path": "final.mp4" }),
                    },
                    ToolCall {
                        call_id: "sp4".into(),
                        name: "workspace_patch".into(),
                        arguments: json!({
                            "rel_path": "web/shots.js",
                            "edits": [{ "old_text": "FPS = 30", "new_text": "FPS = 30 /* edited */" }]
                        }),
                    },
                    ToolCall {
                        call_id: "sp5".into(),
                        name: "complete_composition".into(),
                        arguments: json!({ "output_rel_path": "final.mp4", "summary": "stale candidate" }),
                    },
                ],
                40,
                20,
            ),
            ScriptedTurn::final_stop("Stale candidate rejected.", 40, 10)
                .with_expected_substring("render_required"),
        ],
    );
    let err = AgentRuntime::run(
        &cfg,
        &stale_after_patch_provider,
        None,
        &fresh_input(&task_input, temp.path()),
        &render_ok_backend,
        &render_ok_backend,
    )
    .unwrap_err();
    assert_eq!(err.code(), "incomplete_composition");

    // 3. `probe_media` returning wrong aspect ratio (1920x1080 instead of 9:16) triggers repair & blocks completion.
    let bad_aspect_backend = ScriptedSimulatedBackend::new(
        |_| {
            Ok(SimulatedStepOutcome {
                simulated: true,
                succeeded: true,
                exit_code: 0,
                error_code: None,
                stdout: "ok".into(),
                stderr: String::new(),
                simulated_artifacts: vec![SimulatedArtifactOutput {
                    rel_path: "final.mp4".into(),
                    content_bytes: b"SIMULATED_OK".to_vec(),
                    simulated: true,
                    description: "sim".into(),
                }],
            })
        },
        |req| {
            Ok(SimulatedMediaProbeResult {
                simulated: true,
                real_mp4_rendered: false,
                rel_path: req.rel_path.clone(),
                sha256: req.sha256.clone(),
                byte_size: req.byte_size,
                width: 1920,
                height: 1080,
                fps: 30,
                duration_ms: 15_000,
                has_audio: true,
                motion_detected: true,
                notice: "wrong aspect".into(),
            })
        },
    );
    let bad_aspect_provider = DeterministicMockLlm::new(
        coder_caps(),
        vec![
            ScriptedTurn::tool_calls(
                "Attempt complete despite wrong aspect ratio",
                vec![
                    ToolCall {
                        call_id: "ba1".into(),
                        name: "workspace_write".into(),
                        arguments: json!({ "rel_path": "web/shots.js", "content": "export const FPS = 30;" }),
                    },
                    ToolCall {
                        call_id: "ba2".into(),
                        name: "skill_run_step".into(),
                        arguments: json!({ "entrypoint_id": "render" }),
                    },
                    ToolCall {
                        call_id: "ba3".into(),
                        name: "probe_media".into(),
                        arguments: json!({ "rel_path": "final.mp4" }),
                    },
                    ToolCall {
                        call_id: "ba4".into(),
                        name: "complete_composition".into(),
                        arguments: json!({ "output_rel_path": "final.mp4", "summary": "should fail" }),
                    },
                ],
                40,
                20,
            ),
            ScriptedTurn::final_stop("Blocked by unresolved repair.", 40, 10)
                .with_expected_substring("unresolved_repair"),
        ],
    );
    let err = AgentRuntime::run(
        &cfg,
        &bad_aspect_provider,
        None,
        &fresh_input(&task_input, temp.path()),
        &bad_aspect_backend,
        &bad_aspect_backend,
    )
    .unwrap_err();
    assert_eq!(err.code(), "incomplete_composition");
}

#[tokio::test]
async fn qa_runtime_limits_timeouts_context_bounds_and_prompt_provenance() {
    let temp = tempfile::tempdir().unwrap();
    let (task_input, store) = setup_approved_project_with_skill(temp.path()).await;
    let noop_backend = ScriptedSimulatedBackend::new(|_| unreachable!(), |_| unreachable!());

    // 1. Non-mock LocalEndpoint provider fails closed even with matching snapshot identity.
    let mut local_ep_caps = coder_caps();
    local_ep_caps.provider_kind = ProviderKind::LocalEndpoint;
    local_ep_caps.provider_id = "local-llm".into();
    let local_ep_provider = DeterministicMockLlm::new(coder_caps(), vec![]);
    let cfg = AgentConfig {
        execution_mode: AgentExecutionMode::SimulatedTestOnly,
        require_smoke_verified_skill: false,
        limits: AgentLimits::default(),
    };
    let err = AgentRuntime::run(
        &cfg,
        &local_ep_provider,
        Some(&local_ep_caps),
        &fresh_input(&task_input, temp.path()),
        &noop_backend,
        &noop_backend,
    )
    .unwrap_err();
    assert_eq!(err.code(), "provider_unavailable");

    // 2. Deadline exceeded immediately after slow tool callback fails with agent_execution_timeout.
    let slow_provider = DeterministicMockLlm::new(
        coder_caps(),
        vec![ScriptedTurn::tool_calls(
            "Slow step",
            vec![
                ToolCall {
                    call_id: "sl1".into(),
                    name: "workspace_write".into(),
                    arguments: json!({ "rel_path": "web/shots.js", "content": "export const FPS = 30;" }),
                },
                ToolCall {
                    call_id: "sl2".into(),
                    name: "skill_run_step".into(),
                    arguments: json!({ "entrypoint_id": "render" }),
                },
            ],
            30,
            10,
        )],
    );
    let slow_backend = ScriptedSimulatedBackend::new(
        |_| {
            std::thread::sleep(std::time::Duration::from_millis(60));
            Ok(SimulatedStepOutcome {
                simulated: true,
                succeeded: true,
                exit_code: 0,
                error_code: None,
                stdout: "ok".into(),
                stderr: String::new(),
                simulated_artifacts: Vec::new(),
            })
        },
        |_| unreachable!(),
    );
    let timeout_cfg = AgentConfig {
        execution_mode: AgentExecutionMode::SimulatedTestOnly,
        require_smoke_verified_skill: false,
        limits: AgentLimits {
            max_execution_ms: 25,
            ..AgentLimits::default()
        },
    };
    let err = AgentRuntime::run(
        &timeout_cfg,
        &slow_provider,
        None,
        &fresh_input(&task_input, temp.path()),
        &slow_backend,
        &slow_backend,
    )
    .unwrap_err();
    assert_eq!(err.code(), "agent_execution_timeout");

    // 3. Aggregate conversation context byte cap fails closed (`context_limit_exceeded`).
    let context_cfg = AgentConfig {
        execution_mode: AgentExecutionMode::SimulatedTestOnly,
        require_smoke_verified_skill: false,
        limits: AgentLimits {
            max_total_context_bytes: 4_096,
            ..AgentLimits::default()
        },
    };
    let ctx_provider = DeterministicMockLlm::new(
        coder_caps(),
        vec![ScriptedTurn::tool_call(
            "x".repeat(3_500),
            "ctx1",
            "workspace_list",
            json!({}),
            30,
            10,
        )],
    );
    let err = AgentRuntime::run(
        &context_cfg,
        &ctx_provider,
        None,
        &fresh_input(&task_input, temp.path()),
        &noop_backend,
        &noop_backend,
    )
    .unwrap_err();
    assert_eq!(err.code(), "context_limit_exceeded");

    // 4. Legacy manual revision (`planning_provenance: None`) preserves prompt digest and records limitation note.
    let target = task_input.revision.content.target.clone();
    let manual_project = store
        .create_project(CreateProject {
            schema_version: "1".into(),
            title: "Legacy Manual Project".into(),
            idea: "Manual revision without planner".into(),
            target: target.clone(),
            model_selection: ModelSelection::default(),
        })
        .await
        .unwrap();
    let mut manual_content_models = task_input.revision.content.models.clone();
    manual_content_models.storyboard_planning.prompt_sha256 = "b".repeat(64);
    manual_content_models.scene_code_generation.prompt_sha256 = "c".repeat(64);
    let manual_rev = store
        .create_revision(
            &manual_project.id,
            CreateRevision {
                schema_version: "1".into(),
                expected_revision_id: None,
                content: RevisionInput {
                    schema_version: "1".into(),
                    idea: "Manual idea".into(),
                    target,
                    storyboard: StoryboardInput {
                        synopsis: "Manual synopsis".into(),
                        hook: "Manual hook".into(),
                        scenes: vec![SceneInput {
                            id: None,
                            order: 0,
                            duration_ms: 15_000,
                            visual_direction: "Manual direction".into(),
                            camera: "Wide".into(),
                            action: "Manual action".into(),
                            narration: "Narasi manual.".into(),
                            caption_hint: "Manual caption".into(),
                            sfx_cues: Vec::new(),
                        }],
                    },
                    models: manual_content_models,
                    asset_hashes: Vec::new(),
                    skill_id: task_input.skill.skill_id.clone(),
                    skill_version: task_input.skill.skill_version.clone(),
                    planning_provenance: None,
                },
            },
        )
        .await
        .unwrap();
    store
        .review_revision(
            &manual_project.id,
            &manual_rev.id,
            None,
            ReviewCommand {
                schema_version: "1".into(),
                content_sha256: manual_rev.content_sha256.clone(),
            },
        )
        .await
        .unwrap();
    let (manual_approval, _, _) = store
        .approve_revision(
            &manual_project.id,
            &manual_rev.id,
            None,
            ReviewCommand {
                schema_version: "1".into(),
                content_sha256: manual_rev.content_sha256.clone(),
            },
        )
        .await
        .unwrap();

    let mut manual_input = fresh_input(&task_input, temp.path());
    manual_input.revision = manual_rev;
    manual_input.approval = manual_approval;

    let manual_provider = DeterministicMockLlm::new(
        coder_caps(),
        vec![ScriptedTurn::tool_calls(
            "Complete manual revision",
            vec![
                ToolCall {
                    call_id: "m1".into(),
                    name: "workspace_write".into(),
                    arguments: json!({ "rel_path": "web/shots.js", "content": "export const FPS = 30;" }),
                },
                ToolCall {
                    call_id: "m2".into(),
                    name: "skill_run_step".into(),
                    arguments: json!({ "entrypoint_id": "render" }),
                },
                ToolCall {
                    call_id: "m3".into(),
                    name: "probe_media".into(),
                    arguments: json!({ "rel_path": "final.mp4" }),
                },
                ToolCall {
                    call_id: "m4".into(),
                    name: "complete_composition".into(),
                    arguments: json!({ "output_rel_path": "final.mp4", "summary": "manual ok" }),
                },
            ],
            60,
            30,
        )],
    );
    let ok_backend = ScriptedSimulatedBackend::new(
        |_| {
            Ok(SimulatedStepOutcome {
                simulated: true,
                succeeded: true,
                exit_code: 0,
                error_code: None,
                stdout: "ok".into(),
                stderr: String::new(),
                simulated_artifacts: vec![SimulatedArtifactOutput {
                    rel_path: "final.mp4".into(),
                    content_bytes: b"SIMULATED_MANUAL".to_vec(),
                    simulated: true,
                    description: "sim".into(),
                }],
            })
        },
        |req| {
            Ok(SimulatedMediaProbeResult {
                simulated: true,
                real_mp4_rendered: false,
                rel_path: req.rel_path.clone(),
                sha256: req.sha256.clone(),
                byte_size: req.byte_size,
                width: 1080,
                height: 1920,
                fps: 30,
                duration_ms: 15_000,
                has_audio: true,
                motion_detected: true,
                notice: "SIMULATED_PROBE".into(),
            })
        },
    );

    let manual_out = AgentRuntime::run(
        &cfg,
        &manual_provider,
        None,
        &manual_input,
        &ok_backend,
        &ok_backend,
    )
    .expect("legacy manual revision completes cleanly with explicit limitation record");
    assert!(
        manual_out
            .provenance
            .revision_anchor
            .is_legacy_manual_revision
    );
    assert!(
        manual_out
            .provenance
            .revision_anchor
            .frozen_initial_prompt_artifact
            .is_none()
    );
    assert!(
        manual_out
            .provenance
            .revision_anchor
            .legacy_manual_revision_note
            .is_some()
    );
    assert_eq!(
        manual_out
            .provenance
            .revision_anchor
            .approved_initial_prompt_sha256,
        "c".repeat(64)
    );
    assert!(
        !manual_out
            .workspace_manifest
            .entries
            .iter()
            .any(|e| e.rel_path == "inputs/initial_scene_prompt.json")
    );
}

struct FailingAccountingProvider {
    caps: ProviderCapabilities,
    err_kind: fn() -> scenivox::provider::ProviderError,
}

impl scenivox::provider::LlmProvider for FailingAccountingProvider {
    fn capabilities(&self) -> &ProviderCapabilities {
        &self.caps
    }

    fn generate(
        &self,
        _request: &scenivox::provider::LlmRequest,
    ) -> Result<scenivox::provider::LlmResponse, scenivox::provider::ProviderError> {
        Err((self.err_kind)())
    }
}

#[tokio::test]
async fn agent_stops_immediately_on_accounting_failure_or_cost_overage() {
    let temp = tempfile::tempdir().unwrap();
    let (task_input, _store) = setup_approved_project_with_skill(temp.path()).await;
    let mut cfg = AgentConfig {
        require_smoke_verified_skill: false,
        ..AgentConfig::default()
    };
    cfg.limits.max_turns = 8;

    let backend = ScriptedSimulatedBackend::new(
        |_| panic!("skill backend must not run after accounting failure or overage"),
        |_| panic!("media inspector must not run after accounting failure or overage"),
    );

    // 1. CostLimitExceeded halts agent run cleanly before tool execution or completion.
    let overage_provider = FailingAccountingProvider {
        caps: coder_caps(),
        err_kind: || {
            scenivox::provider::ProviderError::CostLimitExceeded("reserved budget exceeded".into())
        },
    };
    let overage_err = AgentRuntime::run(
        &cfg,
        &overage_provider,
        None,
        &fresh_input(&task_input, temp.path()),
        &backend,
        &backend,
    )
    .unwrap_err();
    assert_eq!(overage_err.code(), "cost_limit_exceeded");

    // 2. AccountingFailed halts agent run cleanly before tool execution or completion.
    let accounting_provider = FailingAccountingProvider {
        caps: coder_caps(),
        err_kind: || {
            scenivox::provider::ProviderError::AccountingFailed("reconciliation conflict".into())
        },
    };
    let acct_err = AgentRuntime::run(
        &cfg,
        &accounting_provider,
        None,
        &fresh_input(&task_input, temp.path()),
        &backend,
        &backend,
    )
    .unwrap_err();
    assert_eq!(acct_err.code(), "cost_reconciliation_failed");
}
