use scenivox::{
    domain::{ModelRecord, ModelSnapshot, Parameters},
    provider::{
        DeterministicMockLlm, FinishReason, LlmMessage, LlmProvider, LlmRequest,
        MAX_RESPONSE_CONTENT_BYTES, ProviderAvailability, ProviderCapabilities, ProviderKind,
        ResponseFormat, ScriptedTurn, TaskRole, ToolCall, ToolDefinition, dispatch_llm_call,
    },
};
use serde_json::json;

fn sample_tools() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "workspace_read".into(),
        description: "Read a workspace file".into(),
        input_schema: json!({"type": "object", "properties": {"rel_path": {"type": "string"}}}),
    }]
}

fn base_request() -> LlmRequest {
    LlmRequest {
        schema_version: "1".into(),
        task_role: TaskRole::SceneCodeGeneration,
        provider_id: "mock".into(),
        model_id: "coder-v1".into(),
        adapter_version: "1.0.0".into(),
        parameters: Parameters {
            temperature: 0.0,
            seed: Some(42),
        },
        max_output_tokens: 1024,
        response_format: ResponseFormat::ToolCalls,
        system_prompt: "Generate deterministic scene code using declared tools.".into(),
        messages: vec![LlmMessage::user("Read SKILL.md first.")],
        tools: sample_tools(),
    }
}

#[test]
fn deterministic_mock_provider_produces_stable_hashes_and_simulated_usage() {
    let script = vec![
        ScriptedTurn::tool_call(
            "Inspecting skill instructions.",
            "call-1",
            "workspace_read",
            json!({"rel_path": "SKILL.md"}),
            120,
            35,
        ),
        ScriptedTurn::final_stop("Done.", 150, 12).with_expected_last_tool("workspace_read"),
    ];

    let provider_a = DeterministicMockLlm::coder_default(script.clone());
    let provider_b = DeterministicMockLlm::coder_default(script);

    let req1 = base_request();
    let res1a = dispatch_llm_call(&provider_a, &req1).expect("first call succeeds");
    let res1b = dispatch_llm_call(&provider_b, &req1).expect("deterministic replay matches");

    assert_eq!(res1a, res1b);
    assert!(res1a.simulated);
    assert!(res1a.usage.simulated);
    assert!(res1a.usage.authoritative);
    assert_eq!(res1a.usage.total_tokens, 155);
    assert_eq!(res1a.finish_reason, FinishReason::ToolCalls);
    assert_eq!(
        res1a.request_sha256,
        req1.canonical_request_sha256().unwrap()
    );

    let mut req2 = req1;
    req2.messages.push(LlmMessage::assistant(
        res1a.content.clone(),
        res1a.tool_calls.clone(),
    ));
    req2.messages.push(
        LlmMessage::tool_result("call-1", "workspace_read", &json!({"content": "# skill"}))
            .unwrap(),
    );

    let res2 = dispatch_llm_call(&provider_a, &req2).expect("second call succeeds");
    assert_eq!(res2.finish_reason, FinishReason::Stop);
    assert_eq!(provider_a.recorded_requests().len(), 2);
}

#[test]
fn validates_capabilities_parameters_snapshot_and_forbids_silent_fallback() {
    let provider = DeterministicMockLlm::coder_default(vec![ScriptedTurn::final_stop("ok", 10, 5)]);

    // Mismatched model ID cannot fall back to coder-v1.
    let mut wrong_model = base_request();
    wrong_model.model_id = "coder-v2".into();
    let err = dispatch_llm_call(&provider, &wrong_model).unwrap_err();
    assert_eq!(err.code(), "invalid_model_selection");

    // Wrong task role fails with unsupported_capability.
    let mut wrong_role = base_request();
    wrong_role.task_role = TaskRole::StoryboardPlanning;
    let err = dispatch_llm_call(&provider, &wrong_role).unwrap_err();
    assert_eq!(err.code(), "unsupported_capability");

    // Unsupported parameter (seed when supports_seed=false) fails instead of dropping seed.
    let mut no_seed_caps =
        ProviderCapabilities::deterministic_mock("coder-v1", vec![TaskRole::SceneCodeGeneration]);
    no_seed_caps.supports_seed = false;
    let no_seed_provider = DeterministicMockLlm::new(
        no_seed_caps.clone(),
        vec![ScriptedTurn::final_stop("ok", 10, 5)],
    );
    let err = dispatch_llm_call(&no_seed_provider, &base_request()).unwrap_err();
    assert_eq!(err.code(), "unsupported_parameter");

    // Non-finite or out-of-range temperature fails explicitly.
    let mut nan_temp = base_request();
    nan_temp.parameters.temperature = f64::NAN;
    let err = dispatch_llm_call(&provider, &nan_temp).unwrap_err();
    assert_eq!(err.code(), "unsupported_parameter");

    let mut high_temp = base_request();
    high_temp.parameters.temperature = 2.5;
    let err = dispatch_llm_call(&provider, &high_temp).unwrap_err();
    assert_eq!(err.code(), "unsupported_parameter");

    // Unavailable / disabled / remote providers fail closed without fallback.
    let mut disabled_caps =
        ProviderCapabilities::deterministic_mock("coder-v1", vec![TaskRole::SceneCodeGeneration]);
    disabled_caps.enabled = false;
    let disabled_provider = DeterministicMockLlm::new(disabled_caps, vec![]);
    assert_eq!(
        dispatch_llm_call(&disabled_provider, &base_request())
            .unwrap_err()
            .code(),
        "provider_unavailable"
    );

    let mut remote_caps =
        ProviderCapabilities::deterministic_mock("coder-v1", vec![TaskRole::SceneCodeGeneration]);
    remote_caps.provider_id = "remote-api".into();
    remote_caps.provider_kind = ProviderKind::RemoteApi;
    remote_caps.remote = true;
    remote_caps.availability = ProviderAvailability::Unknown;
    let mut remote_req = base_request();
    remote_req.provider_id = "remote-api".into();
    let remote_provider = DeterministicMockLlm::new(remote_caps, vec![]);
    assert_eq!(
        dispatch_llm_call(&remote_provider, &remote_req)
            .unwrap_err()
            .code(),
        "remote_cost_not_approved"
    );

    let mut local_ep_caps =
        ProviderCapabilities::deterministic_mock("coder-v1", vec![TaskRole::SceneCodeGeneration]);
    local_ep_caps.provider_id = "local-ollama".into();
    local_ep_caps.provider_kind = ProviderKind::LocalEndpoint;
    local_ep_caps.remote = false;
    let mut local_ep_req = base_request();
    local_ep_req.provider_id = "local-ollama".into();
    let local_ep_provider = DeterministicMockLlm::new(local_ep_caps, vec![]);
    assert_eq!(
        dispatch_llm_call(&local_ep_provider, &local_ep_req)
            .unwrap_err()
            .code(),
        "provider_unavailable"
    );

    // ModelRecord and ModelSnapshot cross-checks match registry/revision pinning rules.
    let model_record = ModelRecord {
        schema_version: "1".into(),
        provider_id: "mock".into(),
        model_id: "coder-v1".into(),
        adapter_version: "1.0.0".into(),
        task_roles: vec!["scene_code_generation".into()],
        parameters: Parameters {
            temperature: 0.0,
            seed: Some(42),
        },
        enabled: true,
        configured: true,
        availability: "available".into(),
        reason: "Offline mock".into(),
        remote: false,
    };
    provider
        .capabilities()
        .validate_against_model_record(
            &model_record,
            TaskRole::SceneCodeGeneration,
            &Parameters {
                temperature: 0.0,
                seed: Some(42),
            },
        )
        .expect("matching model record passes");

    let snapshot = ModelSnapshot {
        provider_id: "mock".into(),
        model_id: "other-model".into(),
        adapter_version: "1.0.0".into(),
        parameters: Parameters {
            temperature: 0.0,
            seed: Some(42),
        },
        prompt_sha256: "0".repeat(64),
    };
    assert_eq!(
        provider
            .capabilities()
            .validate_against_snapshot(&snapshot, TaskRole::SceneCodeGeneration)
            .unwrap_err()
            .code(),
        "invalid_model_selection"
    );
}

#[test]
fn rejects_spoofed_model_identity_and_undeclared_tool_calls_from_provider() {
    let spoofing_provider =
        DeterministicMockLlm::coder_default(vec![ScriptedTurn::final_stop("ok", 10, 5)])
            .with_spoofed_response_model("substituted-model");
    let err = dispatch_llm_call(&spoofing_provider, &base_request()).unwrap_err();
    assert_eq!(err.code(), "provider_invalid_response");

    let undeclared_tool_provider =
        DeterministicMockLlm::coder_default(vec![ScriptedTurn::tool_calls(
            "Attempting forbidden shell tool",
            vec![ToolCall {
                call_id: "call-bad".into(),
                name: "shell_exec".into(),
                arguments: json!({"cmd": "rm -rf /"}),
            }],
            10,
            5,
        )]);
    let err = dispatch_llm_call(&undeclared_tool_provider, &base_request()).unwrap_err();
    assert_eq!(err.code(), "provider_invalid_response");

    let non_simulated_provider =
        DeterministicMockLlm::coder_default(vec![ScriptedTurn::final_stop("ok", 10, 5)])
            .with_non_simulated_response();
    let err = dispatch_llm_call(&non_simulated_provider, &base_request()).unwrap_err();
    assert_eq!(err.code(), "provider_invalid_response");

    let oversized_provider = DeterministicMockLlm::coder_default(vec![ScriptedTurn::final_stop(
        "x".repeat(MAX_RESPONSE_CONTENT_BYTES + 1),
        10,
        5,
    )]);
    let err = dispatch_llm_call(&oversized_provider, &base_request()).unwrap_err();
    assert_eq!(err.code(), "provider_invalid_response");
}
