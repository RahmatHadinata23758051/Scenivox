use serde_json::{Value, json};

fn schema(definition: &str) -> Value {
    let mut schema: Value =
        serde_json::from_str(include_str!("../schemas/v1/contracts.schema.json"))
            .expect("valid schema JSON");
    schema.as_object_mut().unwrap().remove("oneOf");
    schema["$ref"] = json!(format!("#/$defs/{definition}"));
    schema
}

fn id() -> &'static str {
    "00000000-0000-4000-8000-000000000001"
}

fn content() -> Value {
    let model = json!({"provider_id":"mock","model_id":"fixture-v1","adapter_version":"1.0.0",
        "parameters":{"temperature":0,"seed":42},"prompt_sha256":"a".repeat(64)});
    json!({"schema_version":"1","idea":"Rahasia bawah tanah",
        "target":{"duration_ms":60000,"aspect_ratio":"9:16","fps":30,"language":"id-ID","style_id":"stick-figure"},
        "storyboard":{"synopsis":"Misteri bawah tanah","hook":"Apa yang tersembunyi?",
            "scenes":[{"id":id(),"order":0,"duration_ms":60000,"visual_direction":"Terowongan",
                "camera":"Pan perlahan","action":"Tokoh berjalan","narration":"Ada rahasia di sini.",
                "caption_hint":"","sfx_cues":[]}]},
        "models":{"storyboard_planning":model,"scene_code_generation":model},
        "asset_hashes":[],"skill_id":"stick-figure","skill_version":"audited-v1"})
}

fn records() -> Vec<(&'static str, Value)> {
    let content = content();
    vec![
        (
            "Project",
            json!({"schema_version":"1","id":id(),"title":"Misteri","idea":"Rahasia bawah tanah",
            "target":content["target"],"model_selection":{},"current_revision_id":null,"approved_revision_id":null}),
        ),
        (
            "Revision",
            json!({"schema_version":"1","id":id(),"project_id":id(),"parent_revision_id":null,
            "version":1,"created_at":"2026-11-16T00:00:00Z","content_sha256":"b".repeat(64),"content":content}),
        ),
        (
            "Approval",
            json!({"schema_version":"1","id":id(),"project_id":id(),"revision_id":id(),
            "content_sha256":"b".repeat(64),"approved_at":"2026-11-16T00:00:00Z"}),
        ),
        (
            "RenderJob",
            json!({"schema_version":"1","id":id(),"project_id":id(),"revision_id":id(),
            "approval_id":id(),"content_sha256":"b".repeat(64),"snapshot":content,"status":"queued",
            "retry_of_job_id":null,"created_at":"2026-11-16T00:00:00Z","error_code":null,"artifact_ids":[],"cost_consent_id":null}),
        ),
        (
            "Model",
            json!({"schema_version":"1","provider_id":"mock","model_id":"fixture-v1","adapter_version":"1.0.0",
            "task_roles":["storyboard_planning","scene_code_generation"],"parameters":{"temperature":0,"seed":0},"enabled":true,"configured":true,
            "availability":"available","reason":"Local deterministic fixture","remote":false}),
        ),
        (
            "LocalNarration",
            json!({"schema_version":"1","audio":{"format":"pcm_signed16","sample_rate_hz":16000,"channels":1,"frame_count":16000,"duration_ms":1000,"sha256":"c".repeat(64)},"provenance":"user_supplied"}),
        ),
        (
            "Error",
            json!({"schema_version":"1","error":{"code":"stale_revision","message":"Revision is no longer current","details":[]}}),
        ),
    ]
}

#[test]
fn versioned_records_validate_and_reject_missing_unknown_or_wrong_version_fields() {
    let root: Value =
        serde_json::from_str(include_str!("../schemas/v1/contracts.schema.json")).unwrap();
    let validator = jsonschema::validator_for(&root).expect("valid self-contained 2020-12 schema");
    for (kind, value) in records() {
        assert!(
            validator.is_valid(&value),
            "valid {kind}: {:?}",
            validator.validate(&value)
        );
        let typed = jsonschema::validator_for(&schema(kind)).unwrap();
        assert!(typed.is_valid(&value));
        let mut invalid = value.clone();
        invalid["schema_version"] = json!("2");
        assert!(!typed.is_valid(&invalid), "reject version for {kind}");
        invalid = value.clone();
        invalid["unexpected"] = json!(true);
        assert!(!typed.is_valid(&invalid), "reject unknown field for {kind}");
        for field in value.as_object().unwrap().keys() {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(!typed.is_valid(&missing), "required {kind}.{field}");
        }
    }
}

#[test]
fn storyboard_and_snapshot_constraints_reject_unsafe_or_malformed_inputs() {
    let validator = jsonschema::validator_for(&schema("RevisionContent")).unwrap();
    for (pointer, invalid) in [
        ("/target/duration_ms", json!(0)),
        ("/target/duration_ms", json!(60001)),
        ("/target/aspect_ratio", json!("16:9")),
        ("/storyboard/scenes", json!([])),
        ("/storyboard/scenes/0/duration_ms", json!(1.5)),
        ("/storyboard/scenes/0/order", json!(-1)),
        ("/storyboard/scenes/0/id", json!("../../file")),
        (
            "/models/scene_code_generation/parameters/temperature",
            json!(3),
        ),
        ("/models/scene_code_generation/parameters/seed", json!(1.5)),
        (
            "/models/scene_code_generation/prompt_sha256",
            json!("not-a-hash"),
        ),
    ] {
        let mut value = content();
        *value.pointer_mut(pointer).unwrap() = invalid;
        assert!(!validator.is_valid(&value), "must reject {pointer}");
    }
    let mut secret = content();
    secret["models"]["scene_code_generation"]["api_key"] = json!("forbidden");
    assert!(!validator.is_valid(&secret));
    let selections = jsonschema::validator_for(&schema("Selections")).unwrap();
    assert!(selections.is_valid(&json!({})));
    assert!(!selections.is_valid(&json!({"storyboard_planning":{"model_id":"planner"}})));
    assert!(
        !selections.is_valid(&json!({"unknown_role":{"provider_id":"mock","model_id":"planner"}}))
    );
}

#[test]
fn model_schema_requires_valid_parameters_and_unique_capabilities() {
    let validator = jsonschema::validator_for(&schema("Model")).unwrap();
    let mut model = records()
        .into_iter()
        .find(|(kind, _)| *kind == "Model")
        .unwrap()
        .1;
    assert!(validator.is_valid(&model));
    model.as_object_mut().unwrap().remove("parameters");
    assert!(!validator.is_valid(&model));
    model["parameters"] = json!({"temperature":2.1});
    assert!(!validator.is_valid(&model));
    model["parameters"] = json!({"temperature":0,"seed":1.5});
    assert!(!validator.is_valid(&model));
    model["parameters"] = json!({"temperature":0});
    model["task_roles"] = json!(["storyboard_planning", "storyboard_planning"]);
    assert!(!validator.is_valid(&model));
}

#[test]
fn job_and_registry_states_are_closed_enums() {
    let entries = records();
    for (kind, field, invalid) in [
        ("RenderJob", "status", "approved"),
        ("Model", "availability", "assumed_available"),
        ("LocalNarration", "provenance", "verified_speech"),
    ] {
        let mut value = entries
            .iter()
            .find(|(name, _)| *name == kind)
            .unwrap()
            .1
            .clone();
        value[field] = json!(invalid);
        assert!(
            !jsonschema::validator_for(&schema(kind))
                .unwrap()
                .is_valid(&value)
        );
    }
}

#[test]
fn local_narration_schema_enforces_audio_bounds_and_unknown_fields() {
    let validator = jsonschema::validator_for(&schema("LocalNarration")).unwrap();
    let valid = records()
        .into_iter()
        .find(|(kind, _)| *kind == "LocalNarration")
        .unwrap()
        .1;
    assert!(validator.is_valid(&valid));

    for (pointer, invalid) in [
        ("/audio/format", json!("pcm_float32")),
        ("/audio/sample_rate_hz", json!(7999)),
        ("/audio/sample_rate_hz", json!(192001)),
        ("/audio/channels", json!(0)),
        ("/audio/channels", json!(3)),
        ("/audio/frame_count", json!(0)),
        ("/audio/duration_ms", json!(0)),
        ("/audio/duration_ms", json!(60001)),
        ("/audio/sha256", json!("bad-hash")),
    ] {
        let mut candidate = valid.clone();
        *candidate.pointer_mut(pointer).unwrap() = invalid;
        assert!(!validator.is_valid(&candidate), "must reject {pointer}");
    }

    let mut extra_audio_field = valid;
    extra_audio_field["audio"]["unexpected"] = json!(true);
    assert!(!validator.is_valid(&extra_audio_field));
}
