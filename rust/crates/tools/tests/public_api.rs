use serde_json::json;

use runtime::{
    permission_enforcer::PermissionEnforcer, PermissionMode, PermissionPolicy,
};
use tools::{
    enforce_permission_check, execute_tool, is_background_task_tool_name, mvp_tool_specs,
    render_tool_result_for_model, GlobalToolRegistry, RuntimeToolDefinition, ToolManifestEntry,
    ToolRegistry, ToolSearchOutput, ToolSource, ToolSpec,
};

fn assert_public_type<T>() {}

#[test]
fn crate_root_exports_tool_api() {
    assert_public_type::<ToolSpec>();
    assert_public_type::<ToolSearchOutput>();

    let specs = mvp_tool_specs();
    assert!(specs.iter().any(|spec| spec.name == "bash"));
    assert!(specs.iter().any(|spec| spec.name == "ToolSearch"));

    assert!(is_background_task_tool_name("TaskCreate"));
    assert!(!is_background_task_tool_name("bash"));

    let registry = ToolRegistry::new(vec![ToolManifestEntry {
        name: "bash".to_string(),
        source: ToolSource::Base,
    }]);
    assert_eq!(registry.entries()[0].name, "bash");

    let runtime_tool = RuntimeToolDefinition {
        name: "ExampleRuntimeTool".to_string(),
        description: Some("Example runtime tool".to_string()),
        input_schema: json!({ "type": "object" }),
        required_permission: PermissionMode::ReadOnly,
    };
    let global = GlobalToolRegistry::builtin()
        .with_runtime_tools(vec![runtime_tool])
        .expect("runtime tool should be accepted");
    assert!(global.has_runtime_tool("ExampleRuntimeTool"));

    assert_eq!(render_tool_result_for_model("bash", "ok"), "ok");

    let enforcer = PermissionEnforcer::new(
        PermissionPolicy::new(PermissionMode::ReadOnly)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess),
    );
    let denied = enforce_permission_check(&enforcer, "bash", &json!({ "command": "pwd" }));
    assert!(denied.is_err());
}

#[test]
fn mvp_tool_specs_keep_public_order_and_key_schemas() {
    let specs = mvp_tool_specs();
    let names = specs.iter().map(|spec| spec.name).collect::<Vec<_>>();

    assert_eq!(
        names,
        vec![
            "bash",
            "read_file",
            "write_file",
            "edit_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "TodoWrite",
            "Skill",
            "Agent",
            "ToolSearch",
            "NotebookEdit",
            "Sleep",
            "SendUserMessage",
            "Config",
            "EnterPlanMode",
            "ExitPlanMode",
            "StructuredOutput",
            "REPL",
            "PowerShell",
            "AskUserQuestion",
            "TaskCreate",
            "RunTaskPacket",
            "TaskGet",
            "TaskList",
            "TaskStop",
            "TaskUpdate",
            "TaskOutput",
            "WorkerCreate",
            "WorkerGet",
            "WorkerObserve",
            "WorkerResolveTrust",
            "WorkerAwaitReady",
            "WorkerSendPrompt",
            "WorkerRestart",
            "WorkerTerminate",
            "WorkerObserveCompletion",
            "TeamCreate",
            "TeamDelete",
            "CronCreate",
            "CronDelete",
            "CronList",
            "LSP",
            "ListMcpResources",
            "ReadMcpResource",
            "McpAuth",
            "RemoteTrigger",
            "MCP",
            "TestingPermission",
        ]
    );

    let bash = specs
        .iter()
        .find(|spec| spec.name == "bash")
        .expect("bash spec should exist");
    assert_eq!(
        bash.input_schema,
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "timeout": { "type": "integer", "minimum": 1 },
                "description": { "type": "string" },
                "run_in_background": { "type": "boolean" },
                "dangerouslyDisableSandbox": { "type": "boolean" },
                "namespaceRestrictions": { "type": "boolean" },
                "isolateNetwork": { "type": "boolean" },
                "filesystemMode": { "type": "string", "enum": ["off", "workspace-only", "allow-list"] },
                "allowedMounts": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    );

    let task_output = specs
        .iter()
        .find(|spec| spec.name == "TaskOutput")
        .expect("TaskOutput spec should exist");
    assert_eq!(
        task_output.input_schema,
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string" },
                "block": { "type": "boolean" },
                "timeout_ms": { "type": "integer", "minimum": 0 }
            },
            "required": ["task_id"],
            "additionalProperties": false
        })
    );
}

#[test]
fn unsupported_tool_error_stays_user_visible() {
    let error = execute_tool("DefinitelyMissingTool", &json!({}))
        .expect_err("unsupported tool should return an error");
    assert_eq!(error, "unsupported tool: DefinitelyMissingTool");
}
