use crate::TaskStatus;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use super::{
    agent_permission_policy, allowed_tools_for_subagent, apply_tool_cache_controls,
    build_agent_system_prompt, build_system_blocks_with_cache_controls, classify_lane_failure,
    debug_json_input_summary, derive_agent_state, enqueue_background_agent_notification,
    execute_agent_with_mode, execute_agent_with_spawn, execute_tool, final_assistant_text,
    global_task_registry, maybe_commit_provenance, mvp_tool_specs, new_agent_session,
    normalize_tool_input_string, permission_mode_from_plugin, persist_agent_terminal_state,
    push_output_block, render_tool_result_for_model, run_task_output, run_task_packet,
    should_log_tool_stop_without_start, tool_specs_for_allowed_tools, track_tool_block_index,
    AgentInput, AgentJob, AgentToolOutput, GlobalToolRegistry, LaneFailureClass,
    ProviderRuntimeClient, SubagentToolExecutor, TaskOutputInput,
};
use api::{AuthSource, CacheControl, OutputContentBlock, ProviderClient};
use runtime::ProviderFallbackConfig;
use runtime::{
    drain_session_notifications, permission_enforcer::PermissionEnforcer, with_active_tool_context,
    with_active_tool_session, ApiRequest, AssistantEvent, ConversationMessage, ConversationRuntime,
    LaneEventName, PermissionMode, PermissionPolicy, RuntimeError, Session, SessionStore,
    TaskPacket, ToolExecutor,
};
use serde_json::json;

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }

    fn remove(key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        std::env::remove_var(key);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

fn temp_path(name: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    std::env::temp_dir().join(format!("clawd-tools-{unique}-{name}"))
}

struct CwdGuard {
    previous: PathBuf,
}

impl CwdGuard {
    fn set(path: &Path) -> Self {
        let previous = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(path).expect("set cwd");
        Self { previous }
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.previous).expect("restore cwd");
    }
}

fn run_git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .unwrap_or_else(|error| panic!("git {} failed: {error}", args.join(" ")));
    assert!(
        status.success(),
        "git {} exited with {status}",
        args.join(" ")
    );
}

fn init_git_repo(path: &Path) {
    std::fs::create_dir_all(path).expect("create repo");
    run_git(path, &["init", "--quiet", "-b", "main"]);
    run_git(path, &["config", "user.email", "tests@example.com"]);
    run_git(path, &["config", "user.name", "Tools Tests"]);
    std::fs::write(path.join("README.md"), "initial\n").expect("write readme");
    run_git(path, &["add", "README.md"]);
    run_git(path, &["commit", "-m", "initial commit", "--quiet"]);
}

fn commit_file(path: &Path, file: &str, contents: &str, message: &str) {
    std::fs::write(path.join(file), contents).expect("write file");
    run_git(path, &["add", file]);
    run_git(path, &["commit", "-m", message, "--quiet"]);
}

fn permission_policy_for_mode(mode: PermissionMode) -> PermissionPolicy {
    mvp_tool_specs()
        .into_iter()
        .fold(PermissionPolicy::new(mode), |policy, spec| {
            policy.with_tool_requirement(spec.name, spec.required_permission)
        })
}

#[test]
fn exposes_mvp_tools() {
    let names = mvp_tool_specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();
    assert!(names.contains(&"bash"));
    assert!(names.contains(&"read_file"));
    assert!(names.contains(&"WebFetch"));
    assert!(names.contains(&"WebSearch"));
    assert!(names.contains(&"TodoWrite"));
    assert!(names.contains(&"Skill"));
    assert!(names.contains(&"Agent"));
    assert!(names.contains(&"ToolSearch"));
    assert!(names.contains(&"NotebookEdit"));
    assert!(names.contains(&"Sleep"));
    assert!(names.contains(&"SendUserMessage"));
    assert!(names.contains(&"Config"));
    assert!(names.contains(&"EnterPlanMode"));
    assert!(names.contains(&"ExitPlanMode"));
    assert!(names.contains(&"StructuredOutput"));
    assert!(names.contains(&"REPL"));
    assert!(names.contains(&"PowerShell"));
    assert!(names.contains(&"WorkerCreate"));
    assert!(names.contains(&"WorkerObserve"));
    assert!(names.contains(&"WorkerAwaitReady"));
    assert!(names.contains(&"WorkerSendPrompt"));
}

#[test]
fn tool_specs_can_hide_background_task_tools_for_sync_agent_turns() {
    let names = tool_specs_for_allowed_tools(None, true)
        .into_iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();

    assert!(names.contains(&"Agent"));
    assert!(!names.contains(&"TaskCreate"));
    assert!(!names.contains(&"TaskList"));
    assert!(!names.contains(&"TaskOutput"));
    assert!(!names.contains(&"RunTaskPacket"));
}

#[test]
fn subagent_provider_requests_get_prompt_cache_controls() {
    let source_messages = vec![
        ConversationMessage::user_text("first prompt"),
        ConversationMessage::assistant(vec![runtime::ContentBlock::Text {
            text: "assistant text".to_string(),
        }]),
        ConversationMessage::tool_result("tool-1", "bash", "tool output", false),
    ];
    let messages = super::convert_messages(&source_messages);

    let mut tools = vec![api::ToolDefinition {
        name: "bash".to_string(),
        description: None,
        input_schema: json!({}),
        cache_control: None,
    }];
    apply_tool_cache_controls(&mut tools);

    let system = build_system_blocks_with_cache_controls(&[
        "stable system".to_string(),
        "stable subagent instruction".to_string(),
        runtime::SYSTEM_PROMPT_DYNAMIC_BOUNDARY.to_string(),
        "dynamic system".to_string(),
    ])
    .expect("system blocks should be built");

    let request = api::MessageRequest {
        model: "claude-opus-4-6".to_string(),
        max_tokens: 1024,
        messages,
        cache_control: Some(CacheControl::ephemeral()),
        system: Some(system),
        tools: Some(tools),
        stream: true,
        ..Default::default()
    };

    let summary = api::summarize_prompt_cache_controls(&request);
    assert!(summary.enabled);
    assert_eq!(summary.automatic_cache_control_count, 1);
    assert_eq!(summary.system_cache_control_count, 2);
    assert_eq!(summary.tool_cache_control_count, 1);
    assert_eq!(summary.message_cache_control_count, 0);
    assert_eq!(summary.cache_control_count, 4);

    let system = request.system.as_ref().expect("system blocks should exist");
    assert_eq!(system.len(), 3);
    assert!(system[0].cache_control.is_some());
    assert!(system[1].cache_control.is_some());
    assert!(system[1].text.contains("stable subagent instruction"));
    assert!(system[2].cache_control.is_none());
    assert!(request
        .tools
        .as_ref()
        .expect("tools should exist")
        .last()
        .is_some_and(|tool| tool.cache_control.is_some()));
}

#[test]
fn agent_tool_description_scopes_background_mode() {
    let agent = mvp_tool_specs()
        .into_iter()
        .find(|spec| spec.name == "Agent")
        .expect("Agent tool should be present");

    assert!(agent
        .description
        .contains("non-background Agent calls run concurrently"));
    assert!(agent
        .description
        .contains("Use run_in_background only when the user explicitly asks"));
}

#[test]
fn render_tool_result_for_model_formats_async_agent_launches() {
    let rendered = render_tool_result_for_model(
        "Agent",
        r#"{
  "status": "async_launched",
  "agentId": "agent-123",
  "description": "Review the diff",
  "prompt": "Check reuse issues",
  "outputFile": "/tmp/agent-123.md"
}"#,
    );

    assert!(rendered.contains("Async agent launched successfully."));
    assert!(rendered.contains("agentId: agent-123"));
    assert!(rendered.contains("Briefly tell the user what you launched and end your response."));
    assert!(rendered.contains("output_file: /tmp/agent-123.md"));
    assert!(!rendered.contains("\"status\": \"async_launched\""));
    assert!(!rendered.contains("Check reuse issues"));
}

#[test]
fn render_tool_result_for_model_formats_completed_agent_results() {
    let rendered = render_tool_result_for_model(
        "Agent",
        r#"{
  "status": "completed",
  "agentId": "agent-123",
  "description": "Code Quality Review",
  "outputFile": "/tmp/agent-123.md",
  "manifestFile": "/tmp/agent-123.json",
  "laneEvents": [{"event":"lane.started"}],
  "derivedState": "finished_cleanable",
  "result": "No quality issues found."
}"#,
    );

    assert!(rendered.contains("Agent completed: Code Quality Review"));
    assert!(rendered.contains("No quality issues found."));
    assert!(rendered.contains("no background task lookup is needed"));
    assert!(!rendered.contains("agent-123"));
    assert!(!rendered.contains("manifestFile"));
    assert!(!rendered.contains("laneEvents"));
}

#[test]
fn render_tool_result_for_model_preserves_non_agent_outputs() {
    let raw = r#"{"status":"completed","agentId":"agent-123"}"#;
    assert_eq!(render_tool_result_for_model("bash", raw), raw);
}

#[test]
fn rejects_unknown_tool_names() {
    let error = execute_tool("nope", &json!({})).expect_err("tool should be rejected");
    assert!(error.contains("unsupported tool"));
}

#[test]
fn confirms_issue_02_file_tool_dispatch_rejects_outside_absolute_paths() {
    let outside_dir = temp_path("issue-02-outside-dir");
    fs::create_dir_all(&outside_dir).expect("outside dir should create");
    let outside_file = outside_dir.join("secret.txt");
    fs::write(&outside_file, "alpha\nneedle\n").expect("outside file should write");
    let outside_write = outside_dir.join("created-by-tool.txt");

    let outcomes = [
        (
            "read_file",
            execute_tool(
                "read_file",
                &json!({
                    "path": outside_file.display().to_string()
                }),
            )
            .is_err(),
        ),
        (
            "write_file",
            execute_tool(
                "write_file",
                &json!({
                    "path": outside_write.display().to_string(),
                    "content": "created outside workspace"
                }),
            )
            .is_err(),
        ),
        (
            "edit_file",
            execute_tool(
                "edit_file",
                &json!({
                    "path": outside_file.display().to_string(),
                    "old_string": "alpha",
                    "new_string": "omega"
                }),
            )
            .is_err(),
        ),
        (
            "glob_search",
            execute_tool(
                "glob_search",
                &json!({
                    "pattern": "**/*.txt",
                    "path": outside_dir.display().to_string()
                }),
            )
            .is_err(),
        ),
        (
            "grep_search",
            execute_tool(
                "grep_search",
                &json!({
                    "pattern": "needle",
                    "path": outside_dir.display().to_string(),
                    "glob": "**/*.txt",
                    "output_mode": "content"
                }),
            )
            .is_err(),
        ),
    ];
    let accepted = outcomes
        .iter()
        .filter_map(|(tool, rejected)| (!rejected).then_some(*tool))
        .collect::<Vec<_>>();
    let _ = fs::remove_dir_all(&outside_dir);

    assert!(
        accepted.is_empty(),
        "file tool dispatch accepted outside paths instead of enforcing workspace boundary: {accepted:?}"
    );
}

#[test]
fn file_tool_dispatch_uses_active_session_workspace_root_not_process_cwd() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let session_workspace = temp_path("session-workspace-root");
    let process_cwd = temp_path("process-cwd-root");
    fs::create_dir_all(&session_workspace).expect("create session workspace");
    fs::create_dir_all(&process_cwd).expect("create process cwd");
    let session_file = session_workspace.join("readable.txt");
    let cwd_file = process_cwd.join("cwd-only.txt");
    fs::write(&session_file, "session-root-content\n").expect("write session file");
    fs::write(&cwd_file, "cwd-content\n").expect("write cwd file");

    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&process_cwd).expect("set process cwd");
    let session_result = with_active_tool_context(
        Some("session-workspace-test"),
        Some(&session_workspace),
        || {
            execute_tool(
                "read_file",
                &json!({
                    "path": session_file.display().to_string()
                }),
            )
        },
    )
    .expect("session workspace file should be readable");
    let cwd_result = with_active_tool_context(
        Some("session-workspace-test"),
        Some(&session_workspace),
        || {
            execute_tool(
                "read_file",
                &json!({
                    "path": cwd_file.display().to_string()
                }),
            )
        },
    );
    std::env::set_current_dir(original_dir).expect("restore cwd");

    assert!(session_result.contains("session-root-content"));
    assert!(cwd_result
        .expect_err("process cwd file should be outside active session workspace")
        .contains("escapes workspace boundary"));
    let _ = fs::remove_dir_all(session_workspace);
    let _ = fs::remove_dir_all(process_cwd);
}

#[test]
fn worker_tools_gate_prompt_delivery_until_ready_and_support_auto_trust() {
    let created = execute_tool(
        "WorkerCreate",
        &json!({
            "cwd": "/tmp/worktree/repo",
            "trusted_roots": ["/tmp/worktree"]
        }),
    )
    .expect("WorkerCreate should succeed");
    let created_output: serde_json::Value = serde_json::from_str(&created).expect("json");
    let worker_id = created_output["worker_id"]
        .as_str()
        .expect("worker id")
        .to_string();
    assert_eq!(created_output["status"], "spawning");
    assert_eq!(created_output["trust_auto_resolve"], true);

    let gated = execute_tool(
        "WorkerSendPrompt",
        &json!({
            "worker_id": worker_id,
            "prompt": "ship the change"
        }),
    )
    .expect_err("prompt delivery before ready should fail");
    assert!(gated.contains("not ready for prompt delivery"));

    let observed = execute_tool(
        "WorkerObserve",
        &json!({
            "worker_id": created_output["worker_id"],
            "screen_text": "Do you trust the files in this folder?\n1. Yes, proceed\n2. No"
        }),
    )
    .expect("WorkerObserve should auto-resolve trust");
    let observed_output: serde_json::Value = serde_json::from_str(&observed).expect("json");
    assert_eq!(observed_output["status"], "spawning");
    assert_eq!(observed_output["trust_gate_cleared"], true);
    assert_eq!(
        observed_output["events"][1]["payload"]["type"],
        "trust_prompt"
    );
    assert_eq!(
        observed_output["events"][2]["payload"]["resolution"],
        "auto_allowlisted"
    );

    let ready = execute_tool(
        "WorkerObserve",
        &json!({
            "worker_id": created_output["worker_id"],
            "screen_text": "Ready for your input\n>"
        }),
    )
    .expect("WorkerObserve should mark worker ready");
    let ready_output: serde_json::Value = serde_json::from_str(&ready).expect("json");
    assert_eq!(ready_output["status"], "ready_for_prompt");

    let await_ready = execute_tool(
        "WorkerAwaitReady",
        &json!({
            "worker_id": created_output["worker_id"]
        }),
    )
    .expect("WorkerAwaitReady should succeed");
    let await_ready_output: serde_json::Value = serde_json::from_str(&await_ready).expect("json");
    assert_eq!(await_ready_output["ready"], true);

    let accepted = execute_tool(
        "WorkerSendPrompt",
        &json!({
            "worker_id": created_output["worker_id"],
            "prompt": "ship the change"
        }),
    )
    .expect("WorkerSendPrompt should succeed after ready");
    let accepted_output: serde_json::Value = serde_json::from_str(&accepted).expect("json");
    assert_eq!(accepted_output["status"], "running");
    assert_eq!(accepted_output["prompt_delivery_attempts"], 1);
    assert_eq!(accepted_output["prompt_in_flight"], true);
}

#[test]
fn worker_create_merges_config_trusted_roots_without_per_call_override() {
    use std::fs;
    // Write a .claw/settings.json in a temp dir with trustedRoots
    let worktree = temp_path("config-trust-worktree");
    let claw_dir = worktree.join(".claw");
    fs::create_dir_all(&claw_dir).expect("create .claw dir");
    // Use the actual OS temp dir so the worktree path matches the allowlist
    let tmp_root = std::env::temp_dir().to_str().expect("utf-8").to_string();
    let settings = format!("{{\"trustedRoots\": [\"{tmp_root}\"]}}");
    fs::write(claw_dir.join("settings.json"), settings).expect("write settings");

    // WorkerCreate with no per-call trusted_roots — config should supply them
    let cwd = worktree.to_str().expect("valid utf-8").to_string();
    let created = execute_tool(
        "WorkerCreate",
        &json!({
            "cwd": cwd
            // trusted_roots intentionally omitted
        }),
    )
    .expect("WorkerCreate should succeed");
    let output: serde_json::Value = serde_json::from_str(&created).expect("json");

    // worktree is under /tmp, so config roots auto-resolve trust
    assert_eq!(
        output["trust_auto_resolve"], true,
        "config-level trustedRoots should auto-resolve trust without per-call override"
    );

    fs::remove_dir_all(&worktree).ok();
}

#[test]
fn worker_terminate_sets_finished_status() {
    // Create a worker in running state
    let created = execute_tool(
        "WorkerCreate",
        &json!({"cwd": "/tmp/terminate-test", "trusted_roots": ["/tmp"]}),
    )
    .expect("WorkerCreate should succeed");
    let output: serde_json::Value = serde_json::from_str(&created).expect("json");
    let worker_id = output["worker_id"].as_str().expect("worker_id").to_string();

    // Terminate
    let terminated = execute_tool("WorkerTerminate", &json!({"worker_id": worker_id}))
        .expect("WorkerTerminate should succeed");
    let term_output: serde_json::Value = serde_json::from_str(&terminated).expect("json");
    assert_eq!(
        term_output["status"], "finished",
        "terminated worker should be finished"
    );
    assert_eq!(
        term_output["prompt_in_flight"], false,
        "prompt_in_flight should be cleared on termination"
    );
}

#[test]
fn worker_restart_resets_to_spawning() {
    // Create and advance worker to ready_for_prompt
    let created = execute_tool(
        "WorkerCreate",
        &json!({"cwd": "/tmp/restart-test", "trusted_roots": ["/tmp"]}),
    )
    .expect("WorkerCreate should succeed");
    let output: serde_json::Value = serde_json::from_str(&created).expect("json");
    let worker_id = output["worker_id"].as_str().expect("worker_id").to_string();

    // Advance to ready_for_prompt via observe
    execute_tool(
        "WorkerObserve",
        &json!({"worker_id": worker_id, "screen_text": "Ready for input\n>"}),
    )
    .expect("WorkerObserve should succeed");

    // Restart
    let restarted = execute_tool("WorkerRestart", &json!({"worker_id": worker_id}))
        .expect("WorkerRestart should succeed");
    let restart_output: serde_json::Value = serde_json::from_str(&restarted).expect("json");
    assert_eq!(
        restart_output["status"], "spawning",
        "restarted worker should return to spawning"
    );
    assert_eq!(
        restart_output["prompt_in_flight"], false,
        "prompt_in_flight should be cleared on restart"
    );
    assert_eq!(
        restart_output["trust_gate_cleared"], false,
        "trust_gate_cleared should be reset on restart (re-trust required)"
    );
}

#[test]
fn worker_get_returns_worker_state() {
    let created = execute_tool(
        "WorkerCreate",
        &json!({"cwd": "/tmp/worker-get-test", "trusted_roots": ["/tmp"]}),
    )
    .expect("WorkerCreate should succeed");
    let created_output: serde_json::Value = serde_json::from_str(&created).expect("json");
    let worker_id = created_output["worker_id"].as_str().expect("worker_id");

    let fetched = execute_tool("WorkerGet", &json!({"worker_id": worker_id}))
        .expect("WorkerGet should succeed");
    let fetched_output: serde_json::Value = serde_json::from_str(&fetched).expect("json");
    assert_eq!(fetched_output["worker_id"], worker_id);
    assert_eq!(fetched_output["status"], "spawning");
    assert_eq!(fetched_output["cwd"], "/tmp/worker-get-test");
}

#[test]
fn worker_get_on_unknown_id_returns_error() {
    let result = execute_tool(
        "WorkerGet",
        &json!({"worker_id": "worker_nonexistent_get_00000000"}),
    );
    assert!(
        result.is_err(),
        "WorkerGet on unknown id should return error"
    );
    assert!(
        result.unwrap_err().contains("worker not found"),
        "error should mention worker not found"
    );
}

#[test]
fn worker_await_ready_on_spawning_worker_returns_not_ready() {
    let created = execute_tool(
        "WorkerCreate",
        &json!({"cwd": "/tmp/worker-await-not-ready"}),
    )
    .expect("WorkerCreate should succeed");
    let created_output: serde_json::Value = serde_json::from_str(&created).expect("json");
    let worker_id = created_output["worker_id"].as_str().expect("worker_id");

    // Worker is still in spawning — await_ready should return not-ready snapshot
    let snapshot = execute_tool("WorkerAwaitReady", &json!({"worker_id": worker_id}))
        .expect("WorkerAwaitReady should succeed even when not ready");
    let snap_output: serde_json::Value = serde_json::from_str(&snapshot).expect("json");
    assert_eq!(
        snap_output["ready"], false,
        "WorkerAwaitReady on a spawning worker must return ready=false"
    );
    assert_eq!(snap_output["worker_id"], worker_id);
}

#[test]
fn worker_send_prompt_on_non_ready_worker_returns_error() {
    let created = execute_tool(
        "WorkerCreate",
        &json!({"cwd": "/tmp/worker-send-not-ready"}),
    )
    .expect("WorkerCreate should succeed");
    let created_output: serde_json::Value = serde_json::from_str(&created).expect("json");
    let worker_id = created_output["worker_id"].as_str().expect("worker_id");

    let result = execute_tool(
        "WorkerSendPrompt",
        &json!({"worker_id": worker_id, "prompt": "too early"}),
    );
    assert!(
        result.is_err(),
        "WorkerSendPrompt on a non-ready worker should fail"
    );
}

#[test]
fn recovery_loop_state_file_reflects_transitions() {
    // End-to-end proof: .claw/worker-state.json reflects every transition
    // through the stall-detect -> resolve-trust -> ready loop.
    use std::fs;

    // Use a real temp CWD so state file can be written
    let worktree = temp_path("recovery-loop-state");
    fs::create_dir_all(&worktree).expect("create worktree");
    let cwd = worktree.to_str().expect("utf-8").to_string();
    let state_path = worktree.join(".claw").join("worker-state.json");

    // 1. Create worker WITHOUT trusted_roots
    let created =
        execute_tool("WorkerCreate", &json!({"cwd": cwd})).expect("WorkerCreate should succeed");
    let created_output: serde_json::Value = serde_json::from_str(&created).expect("json");
    let worker_id = created_output["worker_id"]
        .as_str()
        .expect("worker_id")
        .to_string();
    // State file should exist after create
    assert!(
        state_path.exists(),
        "state file should be written after WorkerCreate"
    );
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&state_path).expect("read state"))
            .expect("parse state");
    assert_eq!(state["status"], "spawning");
    assert_eq!(state["is_ready"], false);
    assert!(
        state["seconds_since_update"].is_number(),
        "seconds_since_update must be present"
    );

    // 2. Force trust_required via observe
    execute_tool(
        "WorkerObserve",
        &json!({"worker_id": worker_id, "screen_text": "Do you trust the files in this folder?"}),
    )
    .expect("WorkerObserve should succeed");
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&state_path).expect("read state"))
            .expect("parse state");
    assert_eq!(
        state["status"], "trust_required",
        "state file must reflect trust_required stall"
    );
    assert_eq!(state["is_ready"], false);
    assert_eq!(state["trust_gate_cleared"], false);
    assert!(state["seconds_since_update"].is_number());

    // 3. WorkerResolveTrust -> state file reflects recovery
    execute_tool("WorkerResolveTrust", &json!({"worker_id": worker_id}))
        .expect("WorkerResolveTrust should succeed");
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&state_path).expect("read state"))
            .expect("parse state");
    assert_eq!(
        state["status"], "spawning",
        "state file must show spawning after trust resolved"
    );
    assert_eq!(state["trust_gate_cleared"], true);

    // 4. Observe ready screen -> state file shows ready_for_prompt
    execute_tool(
        "WorkerObserve",
        &json!({"worker_id": worker_id, "screen_text": "Ready for input\n>"}),
    )
    .expect("WorkerObserve ready should succeed");
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&state_path).expect("read state"))
            .expect("parse state");
    assert_eq!(
        state["status"], "ready_for_prompt",
        "state file must show ready_for_prompt after ready screen"
    );
    assert_eq!(
        state["is_ready"], true,
        "is_ready must be true in state file at ready_for_prompt"
    );

    fs::remove_dir_all(&worktree).ok();
}

#[test]
fn stall_detect_and_resolve_trust_end_to_end() {
    // 1. Create worker WITHOUT trusted_roots so trust won't auto-resolve
    let created = execute_tool("WorkerCreate", &json!({"cwd": "/no/trusted/root/here"}))
        .expect("WorkerCreate should succeed");
    let created_output: serde_json::Value = serde_json::from_str(&created).expect("json");
    let worker_id = created_output["worker_id"]
        .as_str()
        .expect("worker_id")
        .to_string();
    assert_eq!(created_output["trust_auto_resolve"], false);

    // 2. Observe trust prompt screen text -> worker stalls at trust_required
    let stalled = execute_tool(
        "WorkerObserve",
        &json!({
            "worker_id": worker_id,
            "screen_text": "Do you trust the files in this folder?\n[Allow] [Deny]"
        }),
    )
    .expect("WorkerObserve should succeed");
    let stalled_output: serde_json::Value = serde_json::from_str(&stalled).expect("json");
    assert_eq!(
        stalled_output["status"], "trust_required",
        "worker should stall at trust_required when trust prompt seen without allowlist"
    );
    assert_eq!(stalled_output["trust_gate_cleared"], false);
    // 3. Clawhip calls WorkerResolveTrust to unblock
    let resolved = execute_tool("WorkerResolveTrust", &json!({"worker_id": worker_id}))
        .expect("WorkerResolveTrust should succeed");
    let resolved_output: serde_json::Value = serde_json::from_str(&resolved).expect("json");
    assert_eq!(
        resolved_output["status"], "spawning",
        "worker should return to spawning after trust resolved"
    );
    assert_eq!(resolved_output["trust_gate_cleared"], true);

    // 4. Ready screen text now advances worker normally
    let ready = execute_tool(
        "WorkerObserve",
        &json!({
            "worker_id": worker_id,
            "screen_text": "Ready for input\n>"
        }),
    )
    .expect("WorkerObserve should succeed after trust resolved");
    let ready_output: serde_json::Value = serde_json::from_str(&ready).expect("json");
    assert_eq!(
        ready_output["status"], "ready_for_prompt",
        "worker should reach ready_for_prompt after trust resolved and ready screen seen"
    );
}

#[test]
fn stall_detect_and_restart_recovery_end_to_end() {
    // Worker stalls at trust_required, clawhip restarts instead of resolving
    let created = execute_tool(
        "WorkerCreate",
        &json!({"cwd": "/no/trusted/root/restart-test"}),
    )
    .expect("WorkerCreate should succeed");
    let created_output: serde_json::Value = serde_json::from_str(&created).expect("json");
    let worker_id = created_output["worker_id"]
        .as_str()
        .expect("worker_id")
        .to_string();

    // Force trust_required
    let stalled = execute_tool(
        "WorkerObserve",
        &json!({
            "worker_id": worker_id,
            "screen_text": "trust this folder? [Yes] [No]"
        }),
    )
    .expect("WorkerObserve should succeed");
    let stalled_output: serde_json::Value = serde_json::from_str(&stalled).expect("json");
    assert_eq!(stalled_output["status"], "trust_required");

    // WorkerRestart resets the worker
    let restarted = execute_tool("WorkerRestart", &json!({"worker_id": worker_id}))
        .expect("WorkerRestart should succeed");
    let restarted_output: serde_json::Value = serde_json::from_str(&restarted).expect("json");
    assert_eq!(
        restarted_output["status"], "spawning",
        "restarted worker should be back at spawning"
    );
    assert_eq!(
        restarted_output["trust_gate_cleared"], false,
        "restart clears trust — next observe loop must re-acquire trust"
    );
}

#[test]
fn worker_terminate_on_unknown_id_returns_error() {
    let result = execute_tool(
        "WorkerTerminate",
        &json!({"worker_id": "worker_nonexistent_00000000"}),
    );
    assert!(result.is_err(), "terminating unknown worker should fail");
    assert!(
        result.unwrap_err().contains("worker not found"),
        "error should mention worker not found"
    );
}

#[test]
fn worker_restart_on_unknown_id_returns_error() {
    let result = execute_tool(
        "WorkerRestart",
        &json!({"worker_id": "worker_nonexistent_00000001"}),
    );
    assert!(result.is_err(), "restarting unknown worker should fail");
    assert!(
        result.unwrap_err().contains("worker not found"),
        "error should mention worker not found"
    );
}

#[test]
fn worker_observe_completion_success_finish_sets_finished_status() {
    let created = execute_tool(
        "WorkerCreate",
        &json!({"cwd": "/tmp/observe-completion-test", "trusted_roots": ["/tmp"]}),
    )
    .expect("WorkerCreate should succeed");
    let output: serde_json::Value = serde_json::from_str(&created).expect("json");
    let worker_id = output["worker_id"].as_str().expect("worker_id").to_string();

    let completed = execute_tool(
        "WorkerObserveCompletion",
        &json!({
            "worker_id": worker_id,
            "finish_reason": "end_turn",
            "tokens_output": 512
        }),
    )
    .expect("WorkerObserveCompletion should succeed");
    let completed_output: serde_json::Value = serde_json::from_str(&completed).expect("json");
    assert_eq!(completed_output["status"], "finished");
    assert_eq!(completed_output["prompt_in_flight"], false);
}

#[test]
fn worker_observe_completion_degraded_provider_sets_failed_status() {
    let created = execute_tool(
        "WorkerCreate",
        &json!({"cwd": "/tmp/observe-degraded-test", "trusted_roots": ["/tmp"]}),
    )
    .expect("WorkerCreate should succeed");
    let output: serde_json::Value = serde_json::from_str(&created).expect("json");
    let worker_id = output["worker_id"].as_str().expect("worker_id").to_string();

    // finish=unknown + 0 tokens = degraded provider classification
    let failed = execute_tool(
        "WorkerObserveCompletion",
        &json!({
            "worker_id": worker_id,
            "finish_reason": "unknown",
            "tokens_output": 0
        }),
    )
    .expect("WorkerObserveCompletion should succeed");
    let failed_output: serde_json::Value = serde_json::from_str(&failed).expect("json");
    assert_eq!(
        failed_output["status"], "failed",
        "finish=unknown + 0 tokens should classify as provider failure"
    );
    assert_eq!(failed_output["prompt_in_flight"], false);
    // last_error should be set with provider failure message
    assert!(
        !failed_output["last_error"].is_null(),
        "last_error should be populated for provider failure"
    );
}

#[test]
fn worker_tools_detect_misdelivery_and_arm_prompt_replay() {
    let created = execute_tool(
        "WorkerCreate",
        &json!({
            "cwd": "/tmp/repo/worker-misdelivery"
        }),
    )
    .expect("WorkerCreate should succeed");
    let created_output: serde_json::Value = serde_json::from_str(&created).expect("json");
    let worker_id = created_output["worker_id"]
        .as_str()
        .expect("worker id")
        .to_string();

    execute_tool(
        "WorkerObserve",
        &json!({
            "worker_id": worker_id,
            "screen_text": "Ready for input\n>"
        }),
    )
    .expect("worker should become ready");

    execute_tool(
        "WorkerSendPrompt",
        &json!({
            "worker_id": worker_id,
            "prompt": "Investigate flaky boot"
        }),
    )
    .expect("prompt send should succeed");

    let recovered = execute_tool(
        "WorkerObserve",
        &json!({
            "worker_id": worker_id,
            "screen_text": "% Investigate flaky boot\nzsh: command not found: Investigate"
        }),
    )
    .expect("misdelivery observe should succeed");
    let recovered_output: serde_json::Value = serde_json::from_str(&recovered).expect("json");
    assert_eq!(recovered_output["status"], "ready_for_prompt");
    assert_eq!(recovered_output["last_error"]["kind"], "prompt_delivery");
    assert_eq!(recovered_output["replay_prompt"], "Investigate flaky boot");
    assert_eq!(
        recovered_output["events"][3]["payload"]["observed_target"],
        "shell"
    );
    assert_eq!(
        recovered_output["events"][4]["payload"]["recovery_armed"],
        true
    );

    let replayed = execute_tool(
        "WorkerSendPrompt",
        &json!({
            "worker_id": worker_id
        }),
    )
    .expect("WorkerSendPrompt should replay recovered prompt");
    let replayed_output: serde_json::Value = serde_json::from_str(&replayed).expect("json");
    assert_eq!(replayed_output["status"], "running");
    assert_eq!(replayed_output["prompt_delivery_attempts"], 2);
    assert_eq!(replayed_output["prompt_in_flight"], true);
}

#[test]
fn global_tool_registry_denies_blocked_tool_before_dispatch() {
    // given
    let policy = permission_policy_for_mode(PermissionMode::ReadOnly);
    let registry = GlobalToolRegistry::builtin().with_enforcer(PermissionEnforcer::new(policy));

    // when
    let error = registry
        .execute(
            "write_file",
            &json!({
                "path": "blocked.txt",
                "content": "blocked"
            }),
        )
        .expect_err("write tool should be denied before dispatch");

    // then
    assert!(error.contains("requires workspace-write permission"));
}

#[test]
fn global_tool_registry_denies_prompt_mode_without_prompter_before_dispatch() {
    let policy = PermissionPolicy::new(PermissionMode::Prompt)
        .with_tool_requirement("bash", PermissionMode::DangerFullAccess);
    let registry = GlobalToolRegistry::builtin().with_enforcer(PermissionEnforcer::new(policy));

    let error = registry
        .execute(
            "bash",
            &json!({
                "command": "printf prompt-dispatch-denied"
            }),
        )
        .expect_err("prompt mode without a prompter should deny before dispatch");

    assert!(error.contains("requires approval to escalate from prompt to danger-full-access"));
}

#[test]
fn global_tool_registry_denies_non_bash_dangerous_builtin_without_prompter_before_dispatch() {
    let policy = PermissionPolicy::new(PermissionMode::Prompt)
        .with_tool_requirement("RemoteTrigger", PermissionMode::DangerFullAccess);
    let registry = GlobalToolRegistry::builtin().with_enforcer(PermissionEnforcer::new(policy));

    let error = registry
        .execute(
            "RemoteTrigger",
            &json!({
                "url": "not-a-url"
            }),
        )
        .expect_err("prompt mode should deny dangerous built-ins before dispatch");

    assert!(error.contains("requires approval to escalate from prompt to danger-full-access"));
}

#[test]
fn subagent_tool_executor_denies_blocked_tool_before_dispatch() {
    // given
    let policy = permission_policy_for_mode(PermissionMode::ReadOnly);
    let mut executor = SubagentToolExecutor::new(BTreeSet::from([String::from("write_file")]))
        .with_enforcer(PermissionEnforcer::new(policy));

    // when
    let error = executor
        .execute(
            "write_file",
            &json!({
                "path": "blocked.txt",
                "content": "blocked"
            })
            .to_string(),
        )
        .expect_err("subagent write tool should be denied before dispatch");

    // then
    assert!(error
        .to_string()
        .contains("requires workspace-write permission"));
}

#[test]
fn permission_mode_from_plugin_rejects_invalid_inputs() {
    let unknown_permission =
        permission_mode_from_plugin("admin").expect_err("unknown plugin permission should fail");
    assert!(unknown_permission.contains("unsupported plugin permission: admin"));

    let empty_permission =
        permission_mode_from_plugin("").expect_err("empty plugin permission should fail");
    assert!(empty_permission.contains("unsupported plugin permission: "));
}

#[test]
fn runtime_tools_extend_registry_definitions_permissions_and_search() {
    let registry = GlobalToolRegistry::builtin()
        .with_runtime_tools(vec![super::RuntimeToolDefinition {
            name: "mcp__demo__echo".to_string(),
            description: Some("Echo text from the demo MCP server".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "additionalProperties": false
            }),
            required_permission: runtime::PermissionMode::ReadOnly,
        }])
        .expect("runtime tools should register");

    let allowed = registry
        .normalize_allowed_tools(&["mcp__demo__echo".to_string()])
        .expect("runtime tool should be allow-listable")
        .expect("allow-list should be populated");
    assert!(allowed.contains("mcp__demo__echo"));

    let definitions = registry.definitions(Some(&allowed));
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0].name, "mcp__demo__echo");

    let permissions = registry
        .permission_specs(Some(&allowed))
        .expect("runtime tool permissions should resolve");
    assert_eq!(
        permissions,
        vec![(
            "mcp__demo__echo".to_string(),
            runtime::PermissionMode::ReadOnly
        )]
    );

    let search = registry.search(
        "demo echo",
        5,
        Some(vec!["pending-server".to_string()]),
        Some(runtime::McpDegradedReport::new(
            vec!["demo".to_string()],
            vec![runtime::McpFailedServer {
                server_name: "pending-server".to_string(),
                phase: runtime::McpLifecyclePhase::ToolDiscovery,
                error: runtime::McpErrorSurface::new(
                    runtime::McpLifecyclePhase::ToolDiscovery,
                    Some("pending-server".to_string()),
                    "tool discovery failed",
                    BTreeMap::new(),
                    true,
                ),
            }],
            vec!["mcp__demo__echo".to_string()],
            vec!["mcp__demo__echo".to_string()],
        )),
    );
    let output = serde_json::to_value(search).expect("search output should serialize");
    assert_eq!(output["matches"][0], "mcp__demo__echo");
    assert_eq!(output["pending_mcp_servers"][0], "pending-server");
    assert_eq!(
        output["mcp_degraded"]["failed_servers"][0]["phase"],
        "tool_discovery"
    );
}

#[test]
fn web_fetch_returns_prompt_aware_summary() {
    let server = TestServer::spawn(Arc::new(|request_line: &str| {
        assert!(request_line.starts_with("GET /page "));
        HttpResponse::html(
                200,
                "OK",
                "<html><head><title>Ignored</title></head><body><h1>Test Page</h1><p>Hello <b>world</b> from local server.</p></body></html>",
            )
    }));

    let result = execute_tool(
        "WebFetch",
        &json!({
            "url": format!("http://{}/page", server.addr()),
            "prompt": "Summarize this page"
        }),
    )
    .expect("WebFetch should succeed");

    let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(output["code"], 200);
    let summary = output["result"].as_str().expect("result string");
    assert!(summary.contains("Fetched"));
    assert!(summary.contains("Test Page"));
    assert!(summary.contains("Hello world from local server"));

    let titled = execute_tool(
        "WebFetch",
        &json!({
            "url": format!("http://{}/page", server.addr()),
            "prompt": "What is the page title?"
        }),
    )
    .expect("WebFetch title query should succeed");
    let titled_output: serde_json::Value = serde_json::from_str(&titled).expect("valid json");
    let titled_summary = titled_output["result"].as_str().expect("result string");
    assert!(titled_summary.contains("Title: Ignored"));
}

#[test]
fn web_fetch_supports_plain_text_and_rejects_invalid_url() {
    let server = TestServer::spawn(Arc::new(|request_line: &str| {
        assert!(request_line.starts_with("GET /plain "));
        HttpResponse::text(200, "OK", "plain text response")
    }));

    let result = execute_tool(
        "WebFetch",
        &json!({
            "url": format!("http://{}/plain", server.addr()),
            "prompt": "Show me the content"
        }),
    )
    .expect("WebFetch should succeed for text content");

    let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(output["url"], format!("http://{}/plain", server.addr()));
    assert!(output["result"]
        .as_str()
        .expect("result")
        .contains("plain text response"));

    let error = execute_tool(
        "WebFetch",
        &json!({
            "url": "not a url",
            "prompt": "Summarize"
        }),
    )
    .expect_err("invalid URL should fail");
    assert!(error.contains("relative URL without a base") || error.contains("invalid"));
}

#[test]
fn web_search_extracts_and_filters_results() {
    // Serialize env-var mutation so this test cannot race with the sibling
    // web_search_handles_generic_links_and_invalid_base_url test that also
    // sets CLAWD_WEB_SEARCH_BASE_URL. Without the lock, parallel test
    // runners can interleave the set/remove calls and cause assertion
    // failures on the wrong port.
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = TestServer::spawn(Arc::new(|request_line: &str| {
        assert!(request_line.contains("GET /search?q=rust+web+search "));
        HttpResponse::html(
            200,
            "OK",
            r#"
                <html><body>
                  <a class="result__a" href="https://docs.rs/reqwest">Reqwest docs</a>
                  <a class="result__a" href="https://example.com/blocked">Blocked result</a>
                </body></html>
                "#,
        )
    }));

    std::env::set_var(
        "CLAWD_WEB_SEARCH_BASE_URL",
        format!("http://{}/search", server.addr()),
    );
    let result = execute_tool(
        "WebSearch",
        &json!({
            "query": "rust web search",
            "allowed_domains": ["https://DOCS.rs/"],
            "blocked_domains": ["HTTPS://EXAMPLE.COM"]
        }),
    )
    .expect("WebSearch should succeed");
    std::env::remove_var("CLAWD_WEB_SEARCH_BASE_URL");

    let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(output["query"], "rust web search");
    let results = output["results"].as_array().expect("results array");
    let search_result = results
        .iter()
        .find(|item| item.get("content").is_some())
        .expect("search result block present");
    let content = search_result["content"].as_array().expect("content array");
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["title"], "Reqwest docs");
    assert_eq!(content[0]["url"], "https://docs.rs/reqwest");
}

#[test]
fn web_search_handles_generic_links_and_invalid_base_url() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = TestServer::spawn(Arc::new(|request_line: &str| {
        assert!(request_line.contains("GET /fallback?q=generic+links "));
        HttpResponse::html(
            200,
            "OK",
            r#"
                <html><body>
                  <a href="https://example.com/one">Example One</a>
                  <a href="https://example.com/one">Duplicate Example One</a>
                  <a href="https://docs.rs/tokio">Tokio Docs</a>
                </body></html>
                "#,
        )
    }));

    std::env::set_var(
        "CLAWD_WEB_SEARCH_BASE_URL",
        format!("http://{}/fallback", server.addr()),
    );
    let result = execute_tool(
        "WebSearch",
        &json!({
            "query": "generic links"
        }),
    )
    .expect("WebSearch fallback parsing should succeed");
    std::env::remove_var("CLAWD_WEB_SEARCH_BASE_URL");

    let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    let results = output["results"].as_array().expect("results array");
    let search_result = results
        .iter()
        .find(|item| item.get("content").is_some())
        .expect("search result block present");
    let content = search_result["content"].as_array().expect("content array");
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["url"], "https://example.com/one");
    assert_eq!(content[1]["url"], "https://docs.rs/tokio");

    std::env::set_var("CLAWD_WEB_SEARCH_BASE_URL", "://bad-base-url");
    let error = execute_tool("WebSearch", &json!({ "query": "generic links" }))
        .expect_err("invalid base URL should fail");
    std::env::remove_var("CLAWD_WEB_SEARCH_BASE_URL");
    assert!(error.contains("relative URL without a base") || error.contains("empty host"));
}

#[test]
fn pending_tools_preserve_multiple_streaming_tool_calls_by_index() {
    let mut events = Vec::new();
    let mut pending_tools = BTreeMap::new();

    push_output_block(
        OutputContentBlock::ToolUse {
            id: "tool-1".to_string(),
            name: "read_file".to_string(),
            input: json!({}),
        },
        1,
        &mut events,
        &mut pending_tools,
        true,
    );
    push_output_block(
        OutputContentBlock::ToolUse {
            id: "tool-2".to_string(),
            name: "grep_search".to_string(),
            input: json!({}),
        },
        2,
        &mut events,
        &mut pending_tools,
        true,
    );

    pending_tools
        .get_mut(&1)
        .expect("first tool pending")
        .2
        .push_str("{\"path\":\"src/main.rs\"}");
    pending_tools
        .get_mut(&2)
        .expect("second tool pending")
        .2
        .push_str("{\"pattern\":\"TODO\"}");

    assert_eq!(
        pending_tools.remove(&1),
        Some((
            "tool-1".to_string(),
            "read_file".to_string(),
            "{\"path\":\"src/main.rs\"}".to_string(),
        ))
    );
    assert_eq!(
        pending_tools.remove(&2),
        Some((
            "tool-2".to_string(),
            "grep_search".to_string(),
            "{\"pattern\":\"TODO\"}".to_string(),
        ))
    );
}

#[test]
fn tool_stop_without_start_diagnostic_ignores_text_block_stop() {
    let mut tool_block_indices = BTreeSet::new();
    track_tool_block_index(
        &OutputContentBlock::Text {
            text: String::new(),
        },
        0,
        &mut tool_block_indices,
    );

    assert!(!should_log_tool_stop_without_start(
        &mut tool_block_indices,
        0
    ));
}

#[test]
fn tool_stop_without_start_diagnostic_flags_tracked_tool_block_stop_once() {
    let mut tool_block_indices = BTreeSet::new();
    track_tool_block_index(
        &OutputContentBlock::ToolUse {
            id: "tool-1".to_string(),
            name: "read_file".to_string(),
            input: json!({}),
        },
        1,
        &mut tool_block_indices,
    );

    assert!(should_log_tool_stop_without_start(
        &mut tool_block_indices,
        1
    ));
    assert!(!should_log_tool_stop_without_start(
        &mut tool_block_indices,
        1
    ));
}

#[test]
fn empty_streaming_tool_input_normalizes_to_empty_object_on_stop() {
    assert_eq!(normalize_tool_input_string(String::new()), "{}");
    assert_eq!(normalize_tool_input_string("   ".to_string()), "{}");
    assert_eq!(
        normalize_tool_input_string(r#"{"path":"src/main.rs"}"#.to_string()),
        r#"{"path":"src/main.rs"}"#
    );
}

#[test]
fn debug_json_input_summary_includes_lengths_prefix_and_suffix() {
    let summary = debug_json_input_summary(r#"{"description":"abc","name":"Review""#, 12);

    assert!(summary.contains("input_bytes=36"));
    assert!(summary.contains("input_chars=36"));
    assert!(summary.contains("input_prefix="));
    assert!(summary.contains("descript"));
    assert!(summary.contains("input_suffix="));
    assert!(summary.contains("Review"));
}

#[test]
fn subagent_executor_accepts_empty_input_for_empty_object_tools() {
    let mut executor = SubagentToolExecutor::new(
        ["TaskList".to_string()]
            .into_iter()
            .collect::<BTreeSet<_>>(),
    );

    let output = executor
        .execute("TaskList", "")
        .expect("empty string should be treated as empty object JSON");
    let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid json");
    assert!(parsed["tasks"].is_array());
}

#[test]
fn todo_write_persists_and_returns_previous_state() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let path = temp_path("todos.json");
    std::env::set_var("CLAWD_TODO_STORE", &path);

    let first = execute_tool(
        "TodoWrite",
        &json!({
            "todos": [
                {"content": "Add tool", "activeForm": "Adding tool", "status": "in_progress"},
                {"content": "Run tests", "activeForm": "Running tests", "status": "pending"}
            ]
        }),
    )
    .expect("TodoWrite should succeed");
    let first_output: serde_json::Value = serde_json::from_str(&first).expect("valid json");
    assert_eq!(first_output["oldTodos"].as_array().expect("array").len(), 0);

    let second = execute_tool(
        "TodoWrite",
        &json!({
            "todos": [
                {"content": "Add tool", "activeForm": "Adding tool", "status": "completed"},
                {"content": "Run tests", "activeForm": "Running tests", "status": "completed"},
                {"content": "Verify", "activeForm": "Verifying", "status": "completed"}
            ]
        }),
    )
    .expect("TodoWrite should succeed");
    std::env::remove_var("CLAWD_TODO_STORE");
    let _ = std::fs::remove_file(path);

    let second_output: serde_json::Value = serde_json::from_str(&second).expect("valid json");
    assert_eq!(
        second_output["oldTodos"].as_array().expect("array").len(),
        2
    );
    assert_eq!(
        second_output["newTodos"].as_array().expect("array").len(),
        3
    );
    assert!(second_output["verificationNudgeNeeded"].is_null());
}

#[test]
fn todo_write_rejects_invalid_payloads_and_sets_verification_nudge() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let path = temp_path("todos-errors.json");
    std::env::set_var("CLAWD_TODO_STORE", &path);

    let empty =
        execute_tool("TodoWrite", &json!({ "todos": [] })).expect_err("empty todos should fail");
    assert!(empty.contains("todos must not be empty"));

    // Multiple in_progress items are now allowed for parallel workflows
    let _multi_active = execute_tool(
        "TodoWrite",
        &json!({
            "todos": [
                {"content": "One", "activeForm": "Doing one", "status": "in_progress"},
                {"content": "Two", "activeForm": "Doing two", "status": "in_progress"}
            ]
        }),
    )
    .expect("multiple in-progress todos should succeed");

    let blank_content = execute_tool(
        "TodoWrite",
        &json!({
            "todos": [
                {"content": "   ", "activeForm": "Doing it", "status": "pending"}
            ]
        }),
    )
    .expect_err("blank content should fail");
    assert!(blank_content.contains("todo content must not be empty"));

    let nudge = execute_tool(
        "TodoWrite",
        &json!({
            "todos": [
                {"content": "Write tests", "activeForm": "Writing tests", "status": "completed"},
                {"content": "Fix errors", "activeForm": "Fixing errors", "status": "completed"},
                {"content": "Ship branch", "activeForm": "Shipping branch", "status": "completed"}
            ]
        }),
    )
    .expect("completed todos should succeed");
    std::env::remove_var("CLAWD_TODO_STORE");
    let _ = fs::remove_file(path);

    let output: serde_json::Value = serde_json::from_str(&nudge).expect("valid json");
    assert_eq!(output["verificationNudgeNeeded"], true);
}

#[test]
fn skill_loads_local_skill_prompt() {
    let _guard = env_lock().lock().expect("env lock should acquire");
    let home = temp_path("skills-home");
    let skill_dir = home.join(".agents").join("skills").join("help");
    fs::create_dir_all(&skill_dir).expect("skill dir should exist");
    fs::write(
        skill_dir.join("SKILL.md"),
        "# help\n\nGuide on using oh-my-codex plugin\n",
    )
    .expect("skill file should exist");
    let original_home = std::env::var("HOME").ok();
    std::env::set_var("HOME", &home);

    let result = execute_tool(
        "Skill",
        &json!({
            "skill": "help",
            "args": "overview"
        }),
    )
    .expect("Skill should succeed");

    let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(output["skill"], "help");
    assert!(output["path"]
        .as_str()
        .expect("path")
        .ends_with("/help/SKILL.md"));
    assert!(output["prompt"]
        .as_str()
        .expect("prompt")
        .contains("Guide on using oh-my-codex plugin"));

    let dollar_result = execute_tool(
        "Skill",
        &json!({
            "skill": "$help"
        }),
    )
    .expect("Skill should accept $skill invocation form");
    let dollar_output: serde_json::Value =
        serde_json::from_str(&dollar_result).expect("valid json");
    assert_eq!(dollar_output["skill"], "$help");
    assert!(dollar_output["path"]
        .as_str()
        .expect("path")
        .ends_with("/help/SKILL.md"));

    if let Some(home) = original_home {
        std::env::set_var("HOME", home);
    } else {
        std::env::remove_var("HOME");
    }
    fs::remove_dir_all(home).expect("temp home should clean up");
}

#[test]
fn skill_resolves_project_local_skills_and_legacy_commands() {
    let _guard = env_lock().lock().expect("env lock should acquire");
    let root = temp_path("project-skills");
    let skill_dir = root.join(".claw").join("skills").join("plan");
    let command_dir = root.join(".claw").join("commands");
    fs::create_dir_all(&skill_dir).expect("skill dir should exist");
    fs::create_dir_all(&command_dir).expect("command dir should exist");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: plan\ndescription: Project planning guidance\n---\n\n# plan\n",
    )
    .expect("skill file should exist");
    fs::write(
        command_dir.join("handoff.md"),
        "---\nname: handoff\ndescription: Legacy handoff guidance\n---\n\n# handoff\n",
    )
    .expect("command file should exist");

    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&root).expect("set cwd");

    let skill_result = execute_tool("Skill", &json!({ "skill": "$plan" }))
        .expect("project-local skill should resolve");
    let skill_output: serde_json::Value = serde_json::from_str(&skill_result).expect("valid json");
    assert!(skill_output["path"]
        .as_str()
        .expect("path")
        .ends_with(".claw/skills/plan/SKILL.md"));

    let command_result = execute_tool("Skill", &json!({ "skill": "/handoff" }))
        .expect("legacy command should resolve");
    let command_output: serde_json::Value =
        serde_json::from_str(&command_result).expect("valid json");
    assert!(command_output["path"]
        .as_str()
        .expect("path")
        .ends_with(".claw/commands/handoff.md"));

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    fs::remove_dir_all(root).expect("temp project should clean up");
}

#[test]
fn skill_loads_project_local_claude_skill_prompt() {
    let _guard = env_lock().lock().expect("env lock should acquire");
    let root = temp_path("project-skills");
    let home = root.join("home");
    let workspace = root.join("workspace");
    let nested = workspace.join("nested");
    let skill_dir = workspace.join(".claude").join("skills").join("trace");
    fs::create_dir_all(&skill_dir).expect("skill dir should exist");
    fs::create_dir_all(&nested).expect("nested cwd should exist");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: trace\ndescription: Project-local trace helper\n---\n# trace\n",
    )
    .expect("skill file should exist");

    let original_home = std::env::var("HOME").ok();
    let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
    let original_codex_home = std::env::var("CODEX_HOME").ok();
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_var("HOME", &home);
    std::env::remove_var("CLAW_CONFIG_HOME");
    std::env::remove_var("CODEX_HOME");
    std::env::set_current_dir(&nested).expect("set cwd");

    let result = execute_tool("Skill", &json!({ "skill": "trace" }))
        .expect("project-local skill should resolve");

    let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert!(output["path"]
        .as_str()
        .expect("path")
        .ends_with(".claude/skills/trace/SKILL.md"));
    assert_eq!(output["description"], "Project-local trace helper");

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    match original_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
    match original_config_home {
        Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
        None => std::env::remove_var("CLAW_CONFIG_HOME"),
    }
    match original_codex_home {
        Some(value) => std::env::set_var("CODEX_HOME", value),
        None => std::env::remove_var("CODEX_HOME"),
    }
    fs::remove_dir_all(root).expect("temp tree should clean up");
}

#[test]
fn skill_loads_project_local_omc_and_agents_skill_prompts() {
    let _guard = env_lock().lock().expect("env lock should acquire");
    let root = temp_path("project-omc-skills");
    let home = root.join("home");
    let workspace = root.join("workspace");
    let nested = workspace.join("nested");
    let omc_skill_dir = workspace.join(".omc").join("skills").join("hud");
    let agents_skill_dir = workspace.join(".agents").join("skills").join("trace");
    fs::create_dir_all(&omc_skill_dir).expect("omc skill dir should exist");
    fs::create_dir_all(&agents_skill_dir).expect("agents skill dir should exist");
    fs::create_dir_all(&nested).expect("nested cwd should exist");
    fs::write(
        omc_skill_dir.join("SKILL.md"),
        "---\nname: hud\ndescription: Project-local OMC HUD helper\n---\n# hud\n",
    )
    .expect("omc skill file should exist");
    fs::write(
        agents_skill_dir.join("SKILL.md"),
        "---\nname: trace\ndescription: Project-local agents compatibility helper\n---\n# trace\n",
    )
    .expect("agents skill file should exist");

    let original_home = std::env::var("HOME").ok();
    let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
    let original_codex_home = std::env::var("CODEX_HOME").ok();
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_var("HOME", &home);
    std::env::remove_var("CLAW_CONFIG_HOME");
    std::env::remove_var("CODEX_HOME");
    std::env::set_current_dir(&nested).expect("set cwd");

    let omc_result =
        execute_tool("Skill", &json!({ "skill": "hud" })).expect("omc skill should resolve");
    let agents_result =
        execute_tool("Skill", &json!({ "skill": "trace" })).expect("agents skill should resolve");

    let omc_output: serde_json::Value = serde_json::from_str(&omc_result).expect("valid json");
    let agents_output: serde_json::Value =
        serde_json::from_str(&agents_result).expect("valid json");
    assert!(omc_output["path"]
        .as_str()
        .expect("path")
        .ends_with(".omc/skills/hud/SKILL.md"));
    assert_eq!(omc_output["description"], "Project-local OMC HUD helper");
    assert!(agents_output["path"]
        .as_str()
        .expect("path")
        .ends_with(".agents/skills/trace/SKILL.md"));
    assert_eq!(
        agents_output["description"],
        "Project-local agents compatibility helper"
    );

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    match original_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
    match original_config_home {
        Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
        None => std::env::remove_var("CLAW_CONFIG_HOME"),
    }
    match original_codex_home {
        Some(value) => std::env::set_var("CODEX_HOME", value),
        None => std::env::remove_var("CODEX_HOME"),
    }
    fs::remove_dir_all(root).expect("temp tree should clean up");
}

#[test]
fn skill_loads_learned_skill_from_claude_config_dir() {
    let _guard = env_lock().lock().expect("env lock should acquire");
    let root = temp_path("claude-config-learned-skill");
    let home = root.join("home");
    let claude_config_dir = root.join("claude-config");
    let learned_skill_dir = claude_config_dir
        .join("skills")
        .join("omc-learned")
        .join("learned");
    fs::create_dir_all(&learned_skill_dir).expect("learned skill dir should exist");
    fs::write(
        learned_skill_dir.join("SKILL.md"),
        "---\nname: learned\ndescription: Learned OMC skill\n---\n# learned\n",
    )
    .expect("learned skill file should exist");

    let original_home = std::env::var("HOME").ok();
    let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
    let original_codex_home = std::env::var("CODEX_HOME").ok();
    let original_claude_config_dir = std::env::var("CLAUDE_CONFIG_DIR").ok();
    std::env::set_var("HOME", &home);
    std::env::remove_var("CLAW_CONFIG_HOME");
    std::env::remove_var("CODEX_HOME");
    std::env::set_var("CLAUDE_CONFIG_DIR", &claude_config_dir);

    let result = execute_tool("Skill", &json!({ "skill": "learned" }))
        .expect("learned skill should resolve");

    let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert!(output["path"]
        .as_str()
        .expect("path")
        .ends_with("skills/omc-learned/learned/SKILL.md"));
    assert_eq!(output["description"], "Learned OMC skill");

    match original_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
    match original_config_home {
        Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
        None => std::env::remove_var("CLAW_CONFIG_HOME"),
    }
    match original_codex_home {
        Some(value) => std::env::set_var("CODEX_HOME", value),
        None => std::env::remove_var("CODEX_HOME"),
    }
    match original_claude_config_dir {
        Some(value) => std::env::set_var("CLAUDE_CONFIG_DIR", value),
        None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
    }
    fs::remove_dir_all(root).expect("temp tree should clean up");
}

#[test]
fn skill_loads_direct_skill_and_legacy_command_from_claude_config_dir() {
    let _guard = env_lock().lock().expect("env lock should acquire");
    let root = temp_path("claude-config-direct-skill");
    let home = root.join("home");
    let claude_config_dir = root.join("claude-config");
    let skill_dir = claude_config_dir.join("skills").join("statusline");
    let command_dir = claude_config_dir.join("commands");
    fs::create_dir_all(&skill_dir).expect("direct skill dir should exist");
    fs::create_dir_all(&command_dir).expect("command dir should exist");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: statusline\ndescription: Claude config skill\n---\n# statusline\n",
    )
    .expect("direct skill file should exist");
    fs::write(
        command_dir.join("doctor-check.md"),
        "---\nname: doctor-check\ndescription: Claude config command\n---\n# doctor-check\n",
    )
    .expect("direct command file should exist");

    let original_home = std::env::var("HOME").ok();
    let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
    let original_codex_home = std::env::var("CODEX_HOME").ok();
    let original_claude_config_dir = std::env::var("CLAUDE_CONFIG_DIR").ok();
    std::env::set_var("HOME", &home);
    std::env::remove_var("CLAW_CONFIG_HOME");
    std::env::remove_var("CODEX_HOME");
    std::env::set_var("CLAUDE_CONFIG_DIR", &claude_config_dir);

    let direct_skill =
        execute_tool("Skill", &json!({ "skill": "statusline" })).expect("direct skill");
    let direct_skill_output: serde_json::Value =
        serde_json::from_str(&direct_skill).expect("valid skill json");
    assert!(direct_skill_output["path"]
        .as_str()
        .expect("path")
        .ends_with("skills/statusline/SKILL.md"));
    assert_eq!(direct_skill_output["description"], "Claude config skill");

    let legacy_command =
        execute_tool("Skill", &json!({ "skill": "doctor-check" })).expect("direct command");
    let legacy_command_output: serde_json::Value =
        serde_json::from_str(&legacy_command).expect("valid command json");
    assert!(legacy_command_output["path"]
        .as_str()
        .expect("path")
        .ends_with("commands/doctor-check.md"));
    assert_eq!(
        legacy_command_output["description"],
        "Claude config command"
    );

    match original_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
    match original_config_home {
        Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
        None => std::env::remove_var("CLAW_CONFIG_HOME"),
    }
    match original_codex_home {
        Some(value) => std::env::set_var("CODEX_HOME", value),
        None => std::env::remove_var("CODEX_HOME"),
    }
    match original_claude_config_dir {
        Some(value) => std::env::set_var("CLAUDE_CONFIG_DIR", value),
        None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
    }
    fs::remove_dir_all(root).expect("temp tree should clean up");
}

#[test]
fn skill_loads_project_local_legacy_command_markdown() {
    let _guard = env_lock().lock().expect("env lock should acquire");
    let root = temp_path("project-legacy-command");
    let home = root.join("home");
    let workspace = root.join("workspace");
    let nested = workspace.join("nested");
    let command_dir = workspace.join(".claude").join("commands");
    fs::create_dir_all(&command_dir).expect("legacy command dir should exist");
    fs::create_dir_all(&nested).expect("nested cwd should exist");
    fs::write(
        command_dir.join("team.md"),
        "---\nname: team\ndescription: Legacy team workflow\n---\n# team\n",
    )
    .expect("legacy command file should exist");

    let original_home = std::env::var("HOME").ok();
    let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
    let original_codex_home = std::env::var("CODEX_HOME").ok();
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_var("HOME", &home);
    std::env::remove_var("CLAW_CONFIG_HOME");
    std::env::remove_var("CODEX_HOME");
    std::env::set_current_dir(&nested).expect("set cwd");

    let result = execute_tool("Skill", &json!({ "skill": "team" }))
        .expect("legacy command markdown should resolve");

    let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert!(output["path"]
        .as_str()
        .expect("path")
        .ends_with(".claude/commands/team.md"));
    assert_eq!(output["description"], "Legacy team workflow");

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    match original_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
    match original_config_home {
        Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
        None => std::env::remove_var("CLAW_CONFIG_HOME"),
    }
    match original_codex_home {
        Some(value) => std::env::set_var("CODEX_HOME", value),
        None => std::env::remove_var("CODEX_HOME"),
    }
    fs::remove_dir_all(root).expect("temp tree should clean up");
}

#[test]
fn tool_search_supports_keyword_and_select_queries() {
    let keyword = execute_tool(
        "ToolSearch",
        &json!({"query": "web current", "max_results": 3}),
    )
    .expect("ToolSearch should succeed");
    let keyword_output: serde_json::Value = serde_json::from_str(&keyword).expect("valid json");
    let matches = keyword_output["matches"].as_array().expect("matches");
    assert!(matches.iter().any(|value| value == "WebSearch"));

    let selected = execute_tool("ToolSearch", &json!({"query": "select:Agent,Skill"}))
        .expect("ToolSearch should succeed");
    let selected_output: serde_json::Value = serde_json::from_str(&selected).expect("valid json");
    assert_eq!(selected_output["matches"][0], "Agent");
    assert_eq!(selected_output["matches"][1], "Skill");

    let aliased = execute_tool("ToolSearch", &json!({"query": "AgentTool"}))
        .expect("ToolSearch should support tool aliases");
    let aliased_output: serde_json::Value = serde_json::from_str(&aliased).expect("valid json");
    assert_eq!(aliased_output["matches"][0], "Agent");
    assert_eq!(aliased_output["normalized_query"], "agent");

    let selected_with_alias =
        execute_tool("ToolSearch", &json!({"query": "select:AgentTool,Skill"}))
            .expect("ToolSearch alias select should succeed");
    let selected_with_alias_output: serde_json::Value =
        serde_json::from_str(&selected_with_alias).expect("valid json");
    assert_eq!(selected_with_alias_output["matches"][0], "Agent");
    assert_eq!(selected_with_alias_output["matches"][1], "Skill");
}

#[test]
fn agent_persists_handoff_metadata() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = temp_path("agent-store");
    std::env::set_var("CLAWD_AGENT_STORE", &dir);
    let captured = Arc::new(Mutex::new(None::<AgentJob>));
    let captured_for_spawn = Arc::clone(&captured);

    let manifest = execute_agent_with_spawn(
        AgentInput {
            description: "Audit the branch".to_string(),
            prompt: "Check tests and outstanding work.".to_string(),
            subagent_type: Some("Explore".to_string()),
            name: Some("ship-audit".to_string()),
            model: None,
            run_in_background: None,
        },
        move |job| {
            *captured_for_spawn
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(job);
            Ok(())
        },
    )
    .expect("Agent should succeed");
    std::env::remove_var("CLAWD_AGENT_STORE");

    assert_eq!(manifest.name, "ship-audit");
    assert_eq!(manifest.subagent_type.as_deref(), Some("Explore"));
    assert_eq!(manifest.status, "running");
    assert!(!manifest.created_at.is_empty());
    assert!(manifest.started_at.is_some());
    assert!(manifest.completed_at.is_none());
    let contents = std::fs::read_to_string(&manifest.output_file).expect("agent file exists");
    let manifest_contents =
        std::fs::read_to_string(&manifest.manifest_file).expect("manifest file exists");
    let manifest_json: serde_json::Value =
        serde_json::from_str(&manifest_contents).expect("manifest should be valid json");
    assert!(contents.contains("Audit the branch"));
    assert!(contents.contains("Check tests and outstanding work."));
    assert!(manifest_contents.contains("\"subagentType\": \"Explore\""));
    assert!(manifest_contents.contains("\"status\": \"running\""));
    assert_eq!(manifest_json["laneEvents"][0]["event"], "lane.started");
    assert_eq!(manifest_json["laneEvents"][0]["status"], "running");
    assert!(manifest_json["currentBlocker"].is_null());
    let captured_job = captured
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .expect("spawn job should be captured");
    assert_eq!(captured_job.prompt, "Check tests and outstanding work.");
    assert!(captured_job.allowed_tools.contains("read_file"));
    assert!(!captured_job.allowed_tools.contains("Agent"));
    assert_eq!(captured_job.parent_session_id, None);

    // Use execute_agent_with_spawn + noop closure to exercise the
    // normalization path without making real API calls (execute_agent
    // now runs the sub-agent synchronously).
    let normalized = execute_agent_with_spawn(
        AgentInput {
            description: "Verify the branch".to_string(),
            prompt: "Check tests.".to_string(),
            subagent_type: Some("explorer".to_string()),
            name: None,
            model: None,
            run_in_background: None,
        },
        |_job| Ok(()),
    )
    .expect("Agent should normalize built-in aliases");
    assert_eq!(normalized.subagent_type.as_deref(), Some("Explore"));

    let named = execute_agent_with_spawn(
        AgentInput {
            description: "Review the branch".to_string(),
            prompt: "Inspect diff.".to_string(),
            subagent_type: None,
            name: Some("Ship Audit!!!".to_string()),
            model: None,
            run_in_background: None,
        },
        |_job| Ok(()),
    )
    .expect("Agent should normalize explicit names");
    assert_eq!(named.name, "ship-audit");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn agent_system_prompt_uses_runtime_date_override() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous = std::env::var_os("CLAWD_AGENT_SYSTEM_DATE");
    std::env::set_var("CLAWD_AGENT_SYSTEM_DATE", "2026-05-03");

    let prompt = build_agent_system_prompt("Explore").expect("system prompt should build");

    match previous {
        Some(value) => std::env::set_var("CLAWD_AGENT_SYSTEM_DATE", value),
        None => std::env::remove_var("CLAWD_AGENT_SYSTEM_DATE"),
    }
    let joined = prompt.join("\n");
    assert!(joined.contains("Date: 2026-05-03"));
    assert!(joined.contains("You are a delegated sub-agent of type `Explore`"));
    assert!(!joined.contains("background sub-agent"));
}

#[test]
fn agent_runtime_session_uses_manifest_agent_id_for_llm_logs() {
    let session = new_agent_session("agent-test-123");
    assert_eq!(session.session_id, "agent-test-123");
}

#[test]
fn confirms_issue_08_subagent_default_model_is_not_hardcoded_anthropic() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _model = EnvVarGuard::set("CLAWD_AGENT_MODEL", "openai/gpt-4o-mini");
    let model = super::agent::resolve_agent_model(None);

    assert_eq!(model, "openai/gpt-4o-mini");
}

#[test]
fn subagent_inherits_active_parent_session_model_when_no_agent_default_is_set() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _agent_model = EnvVarGuard::remove("CLAWD_AGENT_MODEL");
    let _anthropic_model = EnvVarGuard::remove("ANTHROPIC_MODEL");
    let workspace = temp_path("parent-model-workspace");
    let agent_store = temp_path("parent-model-agent-store");
    fs::create_dir_all(&workspace).expect("workspace should exist");
    let _agent_store_env = EnvVarGuard::set(
        "CLAWD_AGENT_STORE",
        agent_store
            .to_str()
            .expect("agent store path should be utf8"),
    );
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&workspace).expect("set cwd");

    let parent_session_id = "parent-model-session";
    let session_store = SessionStore::from_cwd(&workspace).expect("session store should build");
    let parent_handle = session_store.create_handle(parent_session_id);
    let mut parent_session = Session::new().with_workspace_root(workspace.clone());
    parent_session.session_id = parent_session_id.to_string();
    parent_session.model = Some("openai/gpt-5.1".to_string());
    parent_session
        .save_to_path(&parent_handle.path)
        .expect("parent session should save");
    let captured = Arc::new(Mutex::new(None::<AgentJob>));
    let captured_for_spawn = Arc::clone(&captured);

    let manifest = with_active_tool_context(Some(parent_session_id), Some(&workspace), || {
        execute_agent_with_spawn(
            AgentInput {
                description: "Audit the active model".to_string(),
                prompt: "Confirm inherited model.".to_string(),
                subagent_type: None,
                name: None,
                model: None,
                run_in_background: None,
            },
            move |job| {
                *captured_for_spawn
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(job);
                Ok(())
            },
        )
    })
    .expect("agent should spawn");

    std::env::set_current_dir(original_dir).expect("restore cwd");
    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&agent_store);

    assert_eq!(manifest.model.as_deref(), Some("openai/gpt-5.1"));
    let captured_job = captured
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .expect("spawn job should be captured");
    assert_eq!(
        captured_job.manifest.model.as_deref(),
        Some("openai/gpt-5.1")
    );
}

#[test]
fn subagent_parent_model_takes_precedence_over_anthropic_model_env() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _agent_model = EnvVarGuard::remove("CLAWD_AGENT_MODEL");
    let _anthropic_model = EnvVarGuard::set("ANTHROPIC_MODEL", "claude-opus-4-6");
    let config_home = temp_path("parent-model-anthropic-config-home");
    let agent_store = temp_path("parent-model-anthropic-agent-store");
    let workspace = temp_path("parent-model-anthropic-workspace");
    fs::create_dir_all(&config_home).expect("config home should exist");
    fs::create_dir_all(&agent_store).expect("agent store should exist");
    fs::create_dir_all(&workspace).expect("workspace should exist");
    let _config_home = EnvVarGuard::set(
        "CLAW_CONFIG_HOME",
        config_home.to_str().expect("config home should be utf8"),
    );
    let _agent_store = EnvVarGuard::set(
        "CLAWD_AGENT_STORE",
        agent_store.to_str().expect("agent store should be utf8"),
    );
    let _cwd = CwdGuard::set(&workspace);

    let parent_session_id = "parent-model-anthropic-session";
    let session_store = SessionStore::from_cwd(&workspace).expect("session store should build");
    let parent_handle = session_store.create_handle(parent_session_id);
    let mut parent_session = Session::new().with_workspace_root(workspace.clone());
    parent_session.session_id = parent_session_id.to_string();
    parent_session.model = Some("openai/gpt-5.1".to_string());
    parent_session
        .save_to_path(&parent_handle.path)
        .expect("parent session should save");

    let manifest = with_active_tool_context(Some(parent_session_id), Some(&workspace), || {
        execute_agent_with_spawn(
            AgentInput {
                description: "Audit the active model".to_string(),
                prompt: "Confirm inherited model.".to_string(),
                subagent_type: None,
                name: None,
                model: None,
                run_in_background: None,
            },
            |_| Ok(()),
        )
    })
    .expect("agent should spawn");

    assert_eq!(manifest.model.as_deref(), Some("openai/gpt-5.1"));
    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&agent_store);
    let _ = std::fs::remove_dir_all(&config_home);
}

#[test]
fn subagent_configured_model_uses_active_workspace_root_not_process_cwd() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _agent_model = EnvVarGuard::remove("CLAWD_AGENT_MODEL");
    let _anthropic_model = EnvVarGuard::remove("ANTHROPIC_MODEL");
    let config_home = temp_path("agent-model-active-config-home");
    let agent_store = temp_path("agent-model-active-agent-store");
    let workspace = temp_path("agent-model-active-workspace");
    let process_cwd = temp_path("agent-model-process-cwd");
    fs::create_dir_all(&config_home).expect("config home should exist");
    fs::create_dir_all(&agent_store).expect("agent store should exist");
    fs::create_dir_all(workspace.join(".claw")).expect("active workspace config dir");
    fs::create_dir_all(process_cwd.join(".claw")).expect("process cwd config dir");
    fs::write(
        workspace.join(".claw").join("settings.json"),
        r#"{"model":"openai/active-workspace-model"}"#,
    )
    .expect("write active workspace settings");
    fs::write(
        process_cwd.join(".claw").join("settings.json"),
        r#"{"model":"claude-process-cwd-model"}"#,
    )
    .expect("write process cwd settings");
    let _config_home = EnvVarGuard::set(
        "CLAW_CONFIG_HOME",
        config_home.to_str().expect("config home should be utf8"),
    );
    let _agent_store = EnvVarGuard::set(
        "CLAWD_AGENT_STORE",
        agent_store.to_str().expect("agent store should be utf8"),
    );
    let _cwd = CwdGuard::set(&process_cwd);

    let parent_session_id = "agent-model-active-workspace-session";
    let session_store = SessionStore::from_cwd(&workspace).expect("session store should build");
    let parent_handle = session_store.create_handle(parent_session_id);
    let mut parent_session = Session::new().with_workspace_root(workspace.clone());
    parent_session.session_id = parent_session_id.to_string();
    parent_session.model = Some("openai/parent-session-model".to_string());
    parent_session
        .save_to_path(&parent_handle.path)
        .expect("parent session should save");

    let manifest = with_active_tool_context(Some(parent_session_id), Some(&workspace), || {
        execute_agent_with_spawn(
            AgentInput {
                description: "Audit the active workspace config".to_string(),
                prompt: "Confirm configured model.".to_string(),
                subagent_type: None,
                name: None,
                model: None,
                run_in_background: None,
            },
            |_| Ok(()),
        )
    })
    .expect("agent should spawn");

    assert_eq!(
        manifest.model.as_deref(),
        Some("openai/active-workspace-model")
    );
    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&process_cwd);
    let _ = std::fs::remove_dir_all(&agent_store);
    let _ = std::fs::remove_dir_all(&config_home);
}

#[test]
fn confirms_issue_09_subagent_session_has_persistence_for_tool_result_budgeting() {
    let session = new_agent_session("agent-issue-09");

    assert!(
        session.persistence_path().is_some(),
        "subagent sessions need persistence paths so large tool results can be externalized"
    );
}

#[test]
fn background_agent_captures_parent_session_and_formats_notification() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = temp_path("agent-session-notification");
    std::env::set_var("CLAWD_AGENT_STORE", &dir);
    let captured = Arc::new(Mutex::new(None::<AgentJob>));
    let captured_for_spawn = Arc::clone(&captured);

    let parent_workspace = dir.join("workspace");
    fs::create_dir_all(&parent_workspace).expect("parent workspace should exist");
    let _ = with_active_tool_context(Some("parent-session"), Some(&parent_workspace), || {
        execute_agent_with_spawn(
            AgentInput {
                description: "Background audit".to_string(),
                prompt: "Review the branch".to_string(),
                subagent_type: None,
                name: Some("audit".to_string()),
                model: None,
                run_in_background: Some(true),
            },
            move |job| {
                *captured_for_spawn
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(job);
                Ok(())
            },
        )
        .expect("background agent should be captured")
    });

    let captured_job = captured
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .expect("spawn job should be captured");
    assert_eq!(
        captured_job.parent_session_id.as_deref(),
        Some("parent-session")
    );
    assert_eq!(
        captured_job.parent_workspace_root.as_deref(),
        Some(parent_workspace.as_path())
    );

    enqueue_background_agent_notification(&captured_job, "completed", "review done");
    let notifications = drain_session_notifications("parent-session");
    assert_eq!(notifications.len(), 1);
    assert!(notifications[0].contains("Background agent finished."));
    assert!(notifications[0].contains("agentId:"));
    assert!(notifications[0].contains("status: completed"));
    assert!(notifications[0].contains("review done"));

    std::env::remove_var("CLAWD_AGENT_STORE");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn confirms_issue_10_background_agent_notification_is_bounded() {
    let parent_session = "parent-issue-10";
    let manifest = super::AgentOutput {
        agent_id: "agent-issue-10".to_string(),
        name: "issue-10".to_string(),
        description: "large result".to_string(),
        subagent_type: Some("general".to_string()),
        model: Some("claude-opus-4-6".to_string()),
        status: "running".to_string(),
        output_file: "/tmp/agent-issue-10.md".to_string(),
        manifest_file: "/tmp/agent-issue-10.json".to_string(),
        created_at: super::iso8601_now(),
        started_at: Some(super::iso8601_now()),
        completed_at: None,
        lane_events: Vec::new(),
        current_blocker: None,
        derived_state: "working".to_string(),
        error: None,
        result: None,
    };
    let job = AgentJob {
        manifest,
        prompt: "summarize".to_string(),
        system_prompt: Vec::new(),
        allowed_tools: BTreeSet::new(),
        parent_session_id: Some(parent_session.to_string()),
        parent_workspace_root: None,
    };
    let large_body = "x".repeat(128 * 1024);

    enqueue_background_agent_notification(&job, "completed", &large_body);
    let notifications = drain_session_notifications(parent_session);

    assert_eq!(notifications.len(), 1);
    assert!(
        notifications[0].chars().count() <= 8 * 1024,
        "background completion notifications should summarize or externalize large agent output"
    );
    assert!(notifications[0].contains("result_preview:"));
    assert!(!notifications[0].contains(&"x".repeat(4 * 1024)));
}

#[test]
#[allow(clippy::too_many_lines)]
fn agent_fake_runner_can_persist_completion_and_failure() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = temp_path("agent-runner");
    std::env::set_var("CLAWD_AGENT_STORE", &dir);

    let completed = execute_agent_with_spawn(
        AgentInput {
            description: "Complete the task".to_string(),
            prompt: "Do the work".to_string(),
            subagent_type: Some("Explore".to_string()),
            name: Some("complete-task".to_string()),
            model: Some("claude-sonnet-4-6".to_string()),
            run_in_background: None,
        },
        |job| {
            persist_agent_terminal_state(
                &job.manifest,
                "completed",
                Some("Finished successfully in commit abc1234"),
                None,
            )
        },
    )
    .expect("completed agent should succeed");

    let completed_manifest =
        std::fs::read_to_string(&completed.manifest_file).expect("completed manifest should exist");
    let completed_manifest_json: serde_json::Value =
        serde_json::from_str(&completed_manifest).expect("completed manifest json");
    let completed_output =
        std::fs::read_to_string(&completed.output_file).expect("completed output should exist");
    assert!(completed_manifest.contains("\"status\": \"completed\""));
    assert!(completed_output.contains("Finished successfully"));
    assert_eq!(
        completed_manifest_json["laneEvents"][0]["event"],
        "lane.started"
    );
    assert_eq!(
        completed_manifest_json["laneEvents"][1]["event"],
        "lane.finished"
    );
    assert_eq!(
        completed_manifest_json["laneEvents"][2]["event"],
        "lane.commit.created"
    );
    assert_eq!(
        completed_manifest_json["laneEvents"][2]["data"]["commit"],
        "abc1234"
    );
    assert!(completed_manifest_json["currentBlocker"].is_null());
    assert_eq!(
        completed_manifest_json["derivedState"],
        "finished_cleanable"
    );

    let failed = execute_agent_with_spawn(
        AgentInput {
            description: "Fail the task".to_string(),
            prompt: "Do the failing work".to_string(),
            subagent_type: Some("Verification".to_string()),
            name: Some("fail-task".to_string()),
            model: None,
            run_in_background: None,
        },
        |job| {
            persist_agent_terminal_state(
                &job.manifest,
                "failed",
                None,
                Some(String::from("tool failed: simulated failure")),
            )
        },
    )
    .expect("failed agent should still spawn");

    let failed_manifest =
        std::fs::read_to_string(&failed.manifest_file).expect("failed manifest should exist");
    let failed_manifest_json: serde_json::Value =
        serde_json::from_str(&failed_manifest).expect("failed manifest json");
    let failed_output =
        std::fs::read_to_string(&failed.output_file).expect("failed output should exist");
    assert!(failed_manifest.contains("\"status\": \"failed\""));
    assert!(failed_manifest.contains("simulated failure"));
    assert!(failed_output.contains("simulated failure"));
    assert!(failed_output.contains("failure_class: tool_runtime"));
    assert_eq!(
        failed_manifest_json["currentBlocker"]["failureClass"],
        "tool_runtime"
    );
    assert_eq!(
        failed_manifest_json["laneEvents"][1]["event"],
        "lane.blocked"
    );
    assert_eq!(
        failed_manifest_json["laneEvents"][2]["event"],
        "lane.failed"
    );
    assert_eq!(
        failed_manifest_json["laneEvents"][2]["failureClass"],
        "tool_runtime"
    );
    assert_eq!(failed_manifest_json["derivedState"], "truly_idle");

    let spawn_error = execute_agent_with_spawn(
        AgentInput {
            description: "Spawn error task".to_string(),
            prompt: "Never starts".to_string(),
            subagent_type: None,
            name: Some("spawn-error".to_string()),
            model: None,
            run_in_background: None,
        },
        |_| Err(String::from("thread creation failed")),
    )
    .expect_err("spawn errors should surface");
    assert!(spawn_error.contains("failed to spawn sub-agent"));
    let spawn_error_manifest = std::fs::read_dir(&dir)
        .expect("agent dir should exist")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .find_map(|path| {
            let contents = std::fs::read_to_string(&path).ok()?;
            contents
                .contains("\"name\": \"spawn-error\"")
                .then_some(contents)
        })
        .expect("failed manifest should still be written");
    let spawn_error_manifest_json: serde_json::Value =
        serde_json::from_str(&spawn_error_manifest).expect("spawn error manifest json");
    assert!(spawn_error_manifest.contains("\"status\": \"failed\""));
    assert!(spawn_error_manifest.contains("thread creation failed"));
    assert_eq!(
        spawn_error_manifest_json["currentBlocker"]["failureClass"],
        "infra"
    );
    assert_eq!(spawn_error_manifest_json["derivedState"], "truly_idle");

    std::env::remove_var("CLAWD_AGENT_STORE");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn agent_background_registers_in_task_registry() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = temp_path("agent-bg-registry");
    std::env::set_var("CLAWD_AGENT_STORE", &dir);

    let manifest = execute_agent_with_spawn(
        AgentInput {
            description: "Background task".to_string(),
            prompt: "Quick work".to_string(),
            subagent_type: None,
            name: Some("bg-test".to_string()),
            model: None,
            run_in_background: None,
        },
        |job| {
            // Simulate what spawn_agent_job does: register + complete.
            let registry = global_task_registry();
            registry.create_with_id(
                job.manifest.agent_id.clone(),
                &job.prompt,
                Some(&job.manifest.description),
            );
            registry
                .set_status(&job.manifest.agent_id, TaskStatus::Running)
                .unwrap();
            persist_agent_terminal_state(&job.manifest, "completed", Some("review done"), None)
                .unwrap();
            registry
                .append_output(&job.manifest.agent_id, "review done")
                .unwrap();
            registry
                .set_status(&job.manifest.agent_id, TaskStatus::Completed)
                .unwrap();
            Ok(())
        },
    )
    .expect("background agent should succeed");

    // TaskGet / TaskOutput should find the agent by its agentId.
    let registry = global_task_registry();
    let task = registry
        .get(&manifest.agent_id)
        .expect("agent should be in task registry");
    assert_eq!(task.status, TaskStatus::Completed);
    assert_eq!(task.output, "review done");

    // The returned manifest should reflect the completed state (read back from disk).
    assert_eq!(manifest.status, "completed");
    assert_eq!(manifest.result.as_deref(), Some("review done"));

    registry.remove(&manifest.agent_id);
    std::env::remove_var("CLAWD_AGENT_STORE");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn confirms_issue_11_task_registry_requires_session_scope() {
    let registry = global_task_registry();
    let task = with_active_tool_session(Some("session-a"), || {
        registry.create_with_id(
            "shared-task-id".to_string(),
            "prompt from session a",
            Some("session a task"),
        )
    });

    let visible_in_session_a =
        with_active_tool_session(Some("session-a"), || registry.get(&task.task_id).is_some());
    let visible_in_session_b =
        with_active_tool_session(Some("session-b"), || registry.get(&task.task_id).is_some());
    let visible_without_session = registry.get(&task.task_id).is_some();

    let _ = with_active_tool_session(Some("session-a"), || registry.remove(&task.task_id));

    assert!(visible_in_session_a);
    assert!(!visible_in_session_b);
    assert!(!visible_without_session);
}

#[test]
fn agent_background_launch_returns_async_contract() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = temp_path("agent-bg-contract");
    std::env::set_var("CLAWD_AGENT_STORE", &dir);

    let launched = execute_agent_with_mode(
        AgentInput {
            description: "Background review".to_string(),
            prompt: "Inspect the branch".to_string(),
            subagent_type: None,
            name: Some("bg-review".to_string()),
            model: None,
            run_in_background: Some(true),
        },
        |job| {
            let registry = global_task_registry();
            registry.create_with_id(
                job.manifest.agent_id.clone(),
                &job.prompt,
                Some(&job.manifest.description),
            );
            registry
                .set_status(&job.manifest.agent_id, TaskStatus::Running)
                .unwrap();
            Ok(())
        },
    )
    .expect("background agent launch should succeed");

    let AgentToolOutput::AsyncLaunched(launch) = launched else {
        panic!("expected async launch output");
    };
    assert_eq!(launch.status, "async_launched");
    assert_eq!(launch.description, "Background review");
    assert_eq!(launch.prompt, "Inspect the branch");
    assert!(Path::new(&launch.output_file).exists());

    let registry = global_task_registry();
    let task = registry
        .get(&launch.agent_id)
        .expect("background task should be registered");
    assert_eq!(task.status, TaskStatus::Running);
    assert_eq!(task.prompt, "Inspect the branch");

    registry.remove(&launch.agent_id);
    std::env::remove_var("CLAWD_AGENT_STORE");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn background_agent_spawn_error_marks_registry_failed() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = temp_path("agent-bg-spawn-error");
    fs::create_dir_all(&dir).expect("agent store should be created");
    let agent_id = "agent-bg-spawn-error".to_string();
    let session_id = "bg-spawn-error-parent-session";
    let output_file = dir.join("agent-bg-spawn-error.md");
    let manifest_file = dir.join("agent-bg-spawn-error.json");
    fs::write(&output_file, "initial output\n").expect("agent output should be seeded");
    let manifest = super::AgentOutput {
        agent_id: agent_id.clone(),
        name: "bg-spawn-error".to_string(),
        description: "Background spawn error regression".to_string(),
        subagent_type: Some("general".to_string()),
        model: Some("claude-opus-4-6".to_string()),
        status: "running".to_string(),
        output_file: output_file.display().to_string(),
        manifest_file: manifest_file.display().to_string(),
        created_at: super::iso8601_now(),
        started_at: Some(super::iso8601_now()),
        completed_at: None,
        lane_events: vec![runtime::LaneEvent::started(super::iso8601_now())],
        current_blocker: None,
        derived_state: "working".to_string(),
        error: None,
        result: None,
    };
    super::agent::write_agent_manifest(&manifest).expect("manifest should be seeded");
    let job = AgentJob {
        manifest,
        prompt: "Do background work".to_string(),
        system_prompt: Vec::new(),
        allowed_tools: BTreeSet::new(),
        parent_session_id: Some(session_id.to_string()),
        parent_workspace_root: None,
    };

    let launch_error = with_active_tool_session(Some(session_id), || {
        super::agent::spawn_agent_job_with_thread_spawner(job, |_builder, _body| {
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "thread creation failed",
            ))
        })
        .expect_err("thread spawn failure should surface")
    });

    assert!(launch_error.contains("thread creation failed"));
    let registry = global_task_registry();
    let task = with_active_tool_session(Some(session_id), || {
        registry
            .get(&agent_id)
            .expect("failed background agent should remain registered")
    });
    assert_eq!(task.status, TaskStatus::Failed);
    assert!(task.output.contains("thread creation failed"));

    let task_output = with_active_tool_session(Some(session_id), || {
        run_task_output(TaskOutputInput {
            task_id: agent_id.clone(),
            block: false,
            timeout_ms: 30_000,
        })
    })
    .expect("task output should succeed");
    let task_output_json: serde_json::Value =
        serde_json::from_str(&task_output).expect("task output json");
    assert_eq!(task_output_json["retrieval_status"], "success");
    assert_eq!(task_output_json["task"]["status"], "failed");
    assert!(task_output_json["output"]
        .as_str()
        .unwrap_or_default()
        .contains("thread creation failed"));

    let failed_manifest = super::agent::read_agent_manifest(&manifest_file.display().to_string())
        .expect("failed manifest should be readable");
    assert_eq!(failed_manifest.status, "failed");
    assert!(failed_manifest
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("thread creation failed"));
    let notifications = drain_session_notifications(session_id);
    assert_eq!(notifications.len(), 1);
    assert!(notifications[0].contains("status: failed"));
    assert!(notifications[0].contains("thread creation failed"));

    let _ = with_active_tool_session(Some(session_id), || registry.remove(&agent_id));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn confirms_issue_12_background_agent_execution_requires_concurrency_limit() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _limit = EnvVarGuard::set("CLAWD_AGENT_MAX_CONCURRENCY", "1");
    let dir = temp_path("agent-issue-12-cap");
    fs::create_dir_all(&dir).expect("agent store should be created");
    let held_slot =
        super::agent::BackgroundAgentSlot::acquire().expect("first slot should be available");
    let agent_id = "agent-issue-12-cap".to_string();
    let session_id = "issue-12-parent-session";
    let output_file = dir.join("agent-issue-12-cap.md");
    let manifest_file = dir.join("agent-issue-12-cap.json");
    fs::write(&output_file, "initial output\n").expect("agent output should be seeded");
    let manifest = super::AgentOutput {
        agent_id: agent_id.clone(),
        name: "issue-12-cap".to_string(),
        description: "Background cap regression".to_string(),
        subagent_type: Some("general".to_string()),
        model: Some("claude-opus-4-6".to_string()),
        status: "running".to_string(),
        output_file: output_file.display().to_string(),
        manifest_file: manifest_file.display().to_string(),
        created_at: super::iso8601_now(),
        started_at: Some(super::iso8601_now()),
        completed_at: None,
        lane_events: vec![runtime::LaneEvent::started(super::iso8601_now())],
        current_blocker: None,
        derived_state: "working".to_string(),
        error: None,
        result: None,
    };
    super::agent::write_agent_manifest(&manifest).expect("manifest should be seeded");
    let job = AgentJob {
        manifest,
        prompt: "Do background work".to_string(),
        system_prompt: Vec::new(),
        allowed_tools: BTreeSet::new(),
        parent_session_id: Some(session_id.to_string()),
        parent_workspace_root: None,
    };

    let launch_error = with_active_tool_session(Some(session_id), || {
        super::agent::spawn_agent_job(job)
            .expect_err("exhausted cap should reject before starting a background thread")
    });

    assert!(launch_error.contains("background agent concurrency limit reached"));
    let registry = global_task_registry();
    let task = with_active_tool_session(Some(session_id), || {
        registry
            .get(&agent_id)
            .expect("rejected background agent should be registered")
    });
    assert_eq!(task.status, TaskStatus::Failed);
    assert!(task
        .output
        .contains("background agent concurrency limit reached"));
    let failed_manifest = super::agent::read_agent_manifest(&manifest_file.display().to_string())
        .expect("failed manifest should be readable");
    assert_eq!(failed_manifest.status, "failed");
    assert!(failed_manifest
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("background agent concurrency limit reached"));
    let notifications = drain_session_notifications(session_id);
    assert_eq!(notifications.len(), 1);
    assert!(notifications[0].contains("status: failed"));
    assert!(notifications[0].contains("background agent concurrency limit reached"));

    let _ = with_active_tool_session(Some(session_id), || registry.remove(&agent_id));
    drop(held_slot);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn task_output_reports_not_ready_and_success_for_background_agents() {
    let registry = global_task_registry();

    let running = registry.create("Inspect branch", Some("Background review"));
    registry
        .set_status(&running.task_id, TaskStatus::Running)
        .expect("set status should succeed");

    let not_ready = run_task_output(TaskOutputInput {
        task_id: running.task_id.clone(),
        block: false,
        timeout_ms: 30_000,
    })
    .expect("non-blocking task output should succeed");
    let not_ready_json: serde_json::Value = serde_json::from_str(&not_ready).expect("json output");
    assert_eq!(not_ready_json["retrieval_status"], "not_ready");
    assert_eq!(not_ready_json["task"]["status"], "running");
    assert_eq!(not_ready_json["task"]["has_output"], false);

    registry.remove(&running.task_id);

    let completed = registry.create("Inspect branch", Some("Background review"));
    registry
        .set_status(&completed.task_id, TaskStatus::Running)
        .expect("set status should succeed");

    let registry_for_thread = registry.clone();
    let completed_task_id = completed.task_id.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(25));
        registry_for_thread
            .append_output(&completed_task_id, "review complete")
            .expect("append output should succeed");
        registry_for_thread
            .set_status(&completed_task_id, TaskStatus::Completed)
            .expect("set status should succeed");
    });

    let success = run_task_output(TaskOutputInput {
        task_id: completed.task_id.clone(),
        block: true,
        timeout_ms: 500,
    })
    .expect("blocking task output should succeed");
    let success_json: serde_json::Value = serde_json::from_str(&success).expect("json output");
    assert_eq!(success_json["retrieval_status"], "success");
    assert_eq!(success_json["task"]["status"], "completed");
    assert_eq!(success_json["output"], "review complete");
    assert_eq!(success_json["task"]["output"], "review complete");
    assert_eq!(success_json["task"]["has_output"], true);

    registry.remove(&completed.task_id);
}

#[test]
fn task_output_reports_timeout_for_running_task() {
    let registry = global_task_registry();
    let task = registry.create("Inspect branch", Some("Background review"));
    registry
        .set_status(&task.task_id, TaskStatus::Running)
        .expect("set status should succeed");

    let timeout = run_task_output(TaskOutputInput {
        task_id: task.task_id.clone(),
        block: true,
        timeout_ms: 10,
    })
    .expect("blocking task output should succeed");
    let timeout_json: serde_json::Value = serde_json::from_str(&timeout).expect("json output");
    assert_eq!(timeout_json["retrieval_status"], "timeout");
    assert_eq!(timeout_json["task"]["status"], "running");

    registry.remove(&task.task_id);
}

#[test]
fn task_output_input_uses_timeout_ms_and_accepts_legacy_timeout_alias() {
    let input: TaskOutputInput = serde_json::from_value(json!({
        "task_id": "agent-1",
        "block": true,
        "timeout_ms": 120_000
    }))
    .expect("timeout_ms field should deserialize");
    assert_eq!(input.timeout_ms, 120_000);

    let legacy_input: TaskOutputInput = serde_json::from_value(json!({
        "task_id": "agent-1",
        "block": true,
        "timeout": 120_000
    }))
    .expect("legacy timeout alias should deserialize");
    assert_eq!(legacy_input.timeout_ms, 120_000);
}

#[test]
fn agent_state_classification_covers_finished_and_specific_blockers() {
    assert_eq!(derive_agent_state("running", None, None, None), "working");
    assert_eq!(
        derive_agent_state("completed", Some("done"), None, None),
        "finished_cleanable"
    );
    assert_eq!(
        derive_agent_state("completed", None, None, None),
        "finished_pending_report"
    );
    assert_eq!(
        derive_agent_state("failed", None, Some("mcp handshake timed out"), None),
        "degraded_mcp"
    );
    assert_eq!(
        derive_agent_state(
            "failed",
            None,
            Some("background terminal still running"),
            None
        ),
        "blocked_background_job"
    );
    assert_eq!(
        derive_agent_state("failed", None, Some("merge conflict while rebasing"), None),
        "blocked_merge_conflict"
    );
    assert_eq!(
        derive_agent_state(
            "failed",
            None,
            Some("transport interrupted after partial progress"),
            None
        ),
        "interrupted_transport"
    );
}

#[test]
fn commit_provenance_is_extracted_from_agent_results() {
    let provenance = maybe_commit_provenance(Some("landed as commit deadbee with clean push"))
        .expect("commit provenance");
    assert_eq!(provenance.commit, "deadbee");
    assert_eq!(provenance.canonical_commit.as_deref(), Some("deadbee"));
    assert_eq!(provenance.lineage, vec!["deadbee".to_string()]);
}
#[test]
fn lane_failure_taxonomy_normalizes_common_blockers() {
    let cases = [
        (
            "prompt delivery failed in tmux pane",
            LaneFailureClass::PromptDelivery,
        ),
        (
            "trust prompt is still blocking startup",
            LaneFailureClass::TrustGate,
        ),
        (
            "branch stale against main after divergence",
            LaneFailureClass::BranchDivergence,
        ),
        (
            "compile failed after cargo check",
            LaneFailureClass::Compile,
        ),
        ("targeted tests failed", LaneFailureClass::Test),
        ("plugin bootstrap failed", LaneFailureClass::PluginStartup),
        ("mcp handshake timed out", LaneFailureClass::McpHandshake),
        (
            "mcp startup failed before listing tools",
            LaneFailureClass::McpStartup,
        ),
        (
            "gateway routing rejected the request",
            LaneFailureClass::GatewayRouting,
        ),
        (
            "tool failed: denied tool execution from hook",
            LaneFailureClass::ToolRuntime,
        ),
        ("thread creation failed", LaneFailureClass::Infra),
    ];

    for (message, expected) in cases {
        assert_eq!(classify_lane_failure(message), expected, "{message}");
    }
}

#[test]
fn lane_event_schema_serializes_to_canonical_names() {
    let cases = [
        (LaneEventName::Started, "lane.started"),
        (LaneEventName::Ready, "lane.ready"),
        (LaneEventName::PromptMisdelivery, "lane.prompt_misdelivery"),
        (LaneEventName::Blocked, "lane.blocked"),
        (LaneEventName::Red, "lane.red"),
        (LaneEventName::Green, "lane.green"),
        (LaneEventName::CommitCreated, "lane.commit.created"),
        (LaneEventName::PrOpened, "lane.pr.opened"),
        (LaneEventName::MergeReady, "lane.merge.ready"),
        (LaneEventName::Finished, "lane.finished"),
        (LaneEventName::Failed, "lane.failed"),
        (
            LaneEventName::BranchStaleAgainstMain,
            "branch.stale_against_main",
        ),
    ];

    for (event, expected) in cases {
        assert_eq!(
            serde_json::to_value(event).expect("serialize lane event"),
            json!(expected)
        );
    }
}

#[test]
fn agent_tool_subset_mapping_is_expected() {
    let general = allowed_tools_for_subagent("general-purpose");
    assert!(general.contains("bash"));
    assert!(general.contains("write_file"));
    assert!(!general.contains("Agent"));

    let explore = allowed_tools_for_subagent("Explore");
    assert!(explore.contains("read_file"));
    assert!(explore.contains("grep_search"));
    assert!(!explore.contains("bash"));

    let plan = allowed_tools_for_subagent("Plan");
    assert!(plan.contains("TodoWrite"));
    assert!(plan.contains("StructuredOutput"));
    assert!(!plan.contains("Agent"));

    let verification = allowed_tools_for_subagent("Verification");
    assert!(verification.contains("bash"));
    assert!(verification.contains("PowerShell"));
    assert!(!verification.contains("write_file"));
}

#[derive(Debug)]
struct MockSubagentApiClient {
    calls: usize,
    input_path: String,
}

impl runtime::ApiClient for MockSubagentApiClient {
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
        self.calls += 1;
        match self.calls {
            1 => {
                assert_eq!(request.messages.len(), 1);
                Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "read_file".to_string(),
                        input: json!({ "path": self.input_path }).to_string(),
                    },
                    AssistantEvent::MessageStop,
                ])
            }
            2 => {
                assert!(request.messages.len() >= 3);
                Ok(vec![
                    AssistantEvent::TextDelta("Scope: completed mock review".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
            _ => unreachable!("extra mock stream call"),
        }
    }
}

#[test]
fn subagent_runtime_executes_tool_loop_with_isolated_session() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_path("subagent-runtime");
    fs::create_dir_all(&root).expect("create subagent workspace");
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&root).expect("set subagent workspace cwd");
    let path = root.join("subagent-input.txt");
    std::fs::write(&path, "hello from child").expect("write input file");

    let mut runtime = ConversationRuntime::new(
        Session::new(),
        MockSubagentApiClient {
            calls: 0,
            input_path: path.display().to_string(),
        },
        SubagentToolExecutor::new(BTreeSet::from([String::from("read_file")])),
        agent_permission_policy(),
        vec![String::from("system prompt")],
    );

    let summary = runtime
        .run_turn("Inspect the delegated file", None)
        .expect("subagent loop should succeed");
    std::env::set_current_dir(original_dir).expect("restore cwd");

    assert_eq!(
        final_assistant_text(&summary),
        "Scope: completed mock review"
    );
    assert!(runtime
        .session()
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .any(|block| matches!(
            block,
            runtime::ContentBlock::ToolResult { output, .. }
                if output.contains("hello from child")
        )));

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn agent_rejects_blank_required_fields() {
    let missing_description = execute_tool(
        "Agent",
        &json!({
            "description": "  ",
            "prompt": "Inspect"
        }),
    )
    .expect_err("blank description should fail");
    assert!(missing_description.contains("description must not be empty"));

    let missing_prompt = execute_tool(
        "Agent",
        &json!({
            "description": "Inspect branch",
            "prompt": " "
        }),
    )
    .expect_err("blank prompt should fail");
    assert!(missing_prompt.contains("prompt must not be empty"));
}

#[test]
fn notebook_edit_replaces_inserts_and_deletes_cells() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_path("notebook-workspace");
    fs::create_dir_all(&root).expect("create notebook workspace");
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&root).expect("set notebook workspace cwd");
    let path = root.join("notebook.ipynb");
    std::fs::write(
            &path,
            r#"{
  "cells": [
    {"cell_type": "code", "id": "cell-a", "metadata": {}, "source": ["print(1)\n"], "outputs": [], "execution_count": null}
  ],
  "metadata": {"kernelspec": {"language": "python"}},
  "nbformat": 4,
  "nbformat_minor": 5
}"#,
        )
        .expect("write notebook");

    let replaced = execute_tool(
        "NotebookEdit",
        &json!({
            "notebook_path": path.display().to_string(),
            "cell_id": "cell-a",
            "new_source": "print(2)\n",
            "edit_mode": "replace"
        }),
    )
    .expect("NotebookEdit replace should succeed");
    let replaced_output: serde_json::Value = serde_json::from_str(&replaced).expect("json");
    assert_eq!(replaced_output["cell_id"], "cell-a");
    assert_eq!(replaced_output["cell_type"], "code");

    let inserted = execute_tool(
        "NotebookEdit",
        &json!({
            "notebook_path": path.display().to_string(),
            "cell_id": "cell-a",
            "new_source": "# heading\n",
            "cell_type": "markdown",
            "edit_mode": "insert"
        }),
    )
    .expect("NotebookEdit insert should succeed");
    let inserted_output: serde_json::Value = serde_json::from_str(&inserted).expect("json");
    assert_eq!(inserted_output["cell_type"], "markdown");
    let appended = execute_tool(
        "NotebookEdit",
        &json!({
            "notebook_path": path.display().to_string(),
            "new_source": "print(3)\n",
            "edit_mode": "insert"
        }),
    )
    .expect("NotebookEdit append should succeed");
    let appended_output: serde_json::Value = serde_json::from_str(&appended).expect("json");
    assert_eq!(appended_output["cell_type"], "code");

    let deleted = execute_tool(
        "NotebookEdit",
        &json!({
            "notebook_path": path.display().to_string(),
            "cell_id": "cell-a",
            "edit_mode": "delete"
        }),
    )
    .expect("NotebookEdit delete should succeed without new_source");
    let deleted_output: serde_json::Value = serde_json::from_str(&deleted).expect("json");
    assert!(deleted_output["cell_type"].is_null());
    assert_eq!(deleted_output["new_source"], "");

    let final_notebook: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read notebook"))
            .expect("valid notebook json");
    let cells = final_notebook["cells"].as_array().expect("cells array");
    assert_eq!(cells.len(), 2);
    assert_eq!(cells[0]["cell_type"], "markdown");
    assert!(cells[0].get("outputs").is_none());
    assert_eq!(cells[1]["cell_type"], "code");
    assert_eq!(cells[1]["source"][0], "print(3)\n");
    std::env::set_current_dir(original_dir).expect("restore cwd");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn notebook_edit_rejects_invalid_inputs() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_path("notebook-invalid-workspace");
    fs::create_dir_all(&root).expect("create notebook workspace");
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&root).expect("set notebook workspace cwd");
    let text_path = root.join("notebook.txt");
    fs::write(&text_path, "not a notebook").expect("write text file");
    let wrong_extension = execute_tool(
        "NotebookEdit",
        &json!({
            "notebook_path": text_path.display().to_string(),
            "new_source": "print(1)\n"
        }),
    )
    .expect_err("non-ipynb file should fail");
    assert!(wrong_extension.contains("Jupyter notebook"));
    let _ = fs::remove_file(&text_path);

    let empty_notebook = root.join("empty.ipynb");
    fs::write(
            &empty_notebook,
            r#"{"cells":[],"metadata":{"kernelspec":{"language":"python"}},"nbformat":4,"nbformat_minor":5}"#,
        )
        .expect("write empty notebook");

    let missing_source = execute_tool(
        "NotebookEdit",
        &json!({
            "notebook_path": empty_notebook.display().to_string(),
            "edit_mode": "insert"
        }),
    )
    .expect_err("insert without source should fail");
    assert!(missing_source.contains("new_source is required"));

    let missing_cell = execute_tool(
        "NotebookEdit",
        &json!({
            "notebook_path": empty_notebook.display().to_string(),
            "edit_mode": "delete"
        }),
    )
    .expect_err("delete on empty notebook should fail");
    assert!(missing_cell.contains("Notebook has no cells to edit"));
    std::env::set_current_dir(original_dir).expect("restore cwd");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn notebook_edit_rejects_paths_outside_active_workspace() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let workspace = temp_path("notebook-workspace-boundary");
    let outside = temp_path("notebook-outside");
    fs::create_dir_all(&workspace).expect("create workspace");
    fs::create_dir_all(&outside).expect("create outside dir");
    let outside_notebook = outside.join("outside.ipynb");
    fs::write(
        &outside_notebook,
        r#"{"cells":[{"cell_type":"code","id":"cell-a","metadata":{},"source":["print(1)\n"],"outputs":[],"execution_count":null}],"metadata":{"kernelspec":{"language":"python"}},"nbformat":4,"nbformat_minor":5}"#,
    )
    .expect("write outside notebook");

    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&workspace).expect("set workspace cwd");
    let error = execute_tool(
        "NotebookEdit",
        &json!({
            "notebook_path": outside_notebook.display().to_string(),
            "cell_id": "cell-a",
            "new_source": "print(2)\n",
            "edit_mode": "replace"
        }),
    )
    .expect_err("outside notebook edit should be rejected");
    std::env::set_current_dir(original_dir).expect("restore cwd");

    assert!(error.contains("escapes workspace boundary"));
    let outside_contents = fs::read_to_string(&outside_notebook).expect("read outside notebook");
    assert!(outside_contents.contains("print(1)"));
    let _ = fs::remove_dir_all(workspace);
    let _ = fs::remove_dir_all(outside);
}

#[test]
fn bash_tool_reports_success_exit_failure_timeout_and_background() {
    let success =
        execute_tool("bash", &json!({ "command": "printf 'hello'" })).expect("bash should succeed");
    let success_output: serde_json::Value = serde_json::from_str(&success).expect("json");
    assert_eq!(success_output["stdout"], "hello");
    assert_eq!(success_output["interrupted"], false);

    let failure = execute_tool("bash", &json!({ "command": "printf 'oops' >&2; exit 7" }))
        .expect("bash failure should still return structured output");
    let failure_output: serde_json::Value = serde_json::from_str(&failure).expect("json");
    assert_eq!(failure_output["returnCodeInterpretation"], "exit_code:7");
    assert!(failure_output["stderr"]
        .as_str()
        .expect("stderr")
        .contains("oops"));

    let timeout = execute_tool("bash", &json!({ "command": "sleep 1", "timeout": 10 }))
        .expect("bash timeout should return output");
    let timeout_output: serde_json::Value = serde_json::from_str(&timeout).expect("json");
    assert_eq!(timeout_output["interrupted"], true);
    assert_eq!(timeout_output["returnCodeInterpretation"], "timeout");
    assert!(timeout_output["stderr"]
        .as_str()
        .expect("stderr")
        .contains("Command exceeded timeout"));

    let background = execute_tool(
        "bash",
        &json!({ "command": "sleep 1", "run_in_background": true }),
    )
    .expect("bash background should succeed");
    let background_output: serde_json::Value = serde_json::from_str(&background).expect("json");
    assert!(background_output["backgroundTaskId"].as_str().is_some());
    assert_eq!(background_output["noOutputExpected"], true);
}

#[test]
fn bash_workspace_tests_are_blocked_when_branch_is_behind_main() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_path("workspace-test-preflight");
    let original_dir = std::env::current_dir().expect("cwd");
    init_git_repo(&root);
    run_git(&root, &["checkout", "-b", "feature/stale-tests"]);
    run_git(&root, &["checkout", "main"]);
    commit_file(
        &root,
        "hotfix.txt",
        "fix from main\n",
        "fix: unblock workspace tests",
    );
    run_git(&root, &["checkout", "feature/stale-tests"]);
    std::env::set_current_dir(&root).expect("set cwd");

    let output = execute_tool(
        "bash",
        &json!({ "command": "cargo test --workspace --all-targets" }),
    )
    .expect("preflight should return structured output");
    let output_json: serde_json::Value = serde_json::from_str(&output).expect("json");
    assert_eq!(
        output_json["returnCodeInterpretation"],
        "preflight_blocked:branch_divergence"
    );
    assert!(output_json["stderr"]
        .as_str()
        .expect("stderr")
        .contains("branch divergence detected before workspace tests"));
    assert_eq!(
        output_json["structuredContent"][0]["event"],
        "branch.stale_against_main"
    );
    assert_eq!(
        output_json["structuredContent"][0]["failureClass"],
        "branch_divergence"
    );
    assert_eq!(
        output_json["structuredContent"][0]["data"]["missingCommits"][0],
        "fix: unblock workspace tests"
    );

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn bash_targeted_tests_skip_branch_preflight() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_path("targeted-test-no-preflight");
    let original_dir = std::env::current_dir().expect("cwd");
    init_git_repo(&root);
    run_git(&root, &["checkout", "-b", "feature/targeted-tests"]);
    run_git(&root, &["checkout", "main"]);
    commit_file(
        &root,
        "hotfix.txt",
        "fix from main\n",
        "fix: only broad tests should block",
    );
    run_git(&root, &["checkout", "feature/targeted-tests"]);
    std::env::set_current_dir(&root).expect("set cwd");

    let output = execute_tool(
        "bash",
        &json!({ "command": "printf 'targeted ok'; cargo test -p runtime stale_branch" }),
    )
    .expect("targeted commands should still execute");
    let output_json: serde_json::Value = serde_json::from_str(&output).expect("json");
    assert_ne!(
        output_json["returnCodeInterpretation"],
        "preflight_blocked:branch_divergence"
    );

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn file_tools_cover_read_write_and_edit_behaviors() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_path("fs-suite");
    fs::create_dir_all(&root).expect("create root");
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&root).expect("set cwd");

    let write_create = execute_tool(
        "write_file",
        &json!({ "path": "nested/demo.txt", "content": "alpha\nbeta\nalpha\n" }),
    )
    .expect("write create should succeed");
    let write_create_output: serde_json::Value = serde_json::from_str(&write_create).expect("json");
    assert_eq!(write_create_output["type"], "create");
    assert!(root.join("nested/demo.txt").exists());

    let write_update = execute_tool(
        "write_file",
        &json!({ "path": "nested/demo.txt", "content": "alpha\nbeta\ngamma\n" }),
    )
    .expect("write update should succeed");
    let write_update_output: serde_json::Value = serde_json::from_str(&write_update).expect("json");
    assert_eq!(write_update_output["type"], "update");
    assert_eq!(write_update_output["originalFile"], "alpha\nbeta\nalpha\n");

    let read_full = execute_tool("read_file", &json!({ "path": "nested/demo.txt" }))
        .expect("read full should succeed");
    let read_full_output: serde_json::Value = serde_json::from_str(&read_full).expect("json");
    assert_eq!(read_full_output["file"]["content"], "alpha\nbeta\ngamma");
    assert_eq!(read_full_output["file"]["startLine"], 1);

    let read_slice = execute_tool(
        "read_file",
        &json!({ "path": "nested/demo.txt", "offset": 1, "limit": 1 }),
    )
    .expect("read slice should succeed");
    let read_slice_output: serde_json::Value = serde_json::from_str(&read_slice).expect("json");
    assert_eq!(read_slice_output["file"]["content"], "beta");
    assert_eq!(read_slice_output["file"]["startLine"], 2);

    let read_past_end = execute_tool(
        "read_file",
        &json!({ "path": "nested/demo.txt", "offset": 50 }),
    )
    .expect("read past EOF should succeed");
    let read_past_end_output: serde_json::Value =
        serde_json::from_str(&read_past_end).expect("json");
    assert_eq!(read_past_end_output["file"]["content"], "");
    assert_eq!(read_past_end_output["file"]["startLine"], 4);

    let read_error = execute_tool("read_file", &json!({ "path": "missing.txt" }))
        .expect_err("missing file should fail");
    assert!(!read_error.is_empty());

    let edit_once = execute_tool(
        "edit_file",
        &json!({ "path": "nested/demo.txt", "old_string": "alpha", "new_string": "omega" }),
    )
    .expect("single edit should succeed");
    let edit_once_output: serde_json::Value = serde_json::from_str(&edit_once).expect("json");
    assert_eq!(edit_once_output["replaceAll"], false);
    assert_eq!(
        fs::read_to_string(root.join("nested/demo.txt")).expect("read file"),
        "omega\nbeta\ngamma\n"
    );

    execute_tool(
        "write_file",
        &json!({ "path": "nested/demo.txt", "content": "alpha\nbeta\nalpha\n" }),
    )
    .expect("reset file");
    let edit_all = execute_tool(
        "edit_file",
        &json!({
            "path": "nested/demo.txt",
            "old_string": "alpha",
            "new_string": "omega",
            "replace_all": true
        }),
    )
    .expect("replace all should succeed");
    let edit_all_output: serde_json::Value = serde_json::from_str(&edit_all).expect("json");
    assert_eq!(edit_all_output["replaceAll"], true);
    assert_eq!(
        fs::read_to_string(root.join("nested/demo.txt")).expect("read file"),
        "omega\nbeta\nomega\n"
    );

    let edit_same = execute_tool(
        "edit_file",
        &json!({ "path": "nested/demo.txt", "old_string": "omega", "new_string": "omega" }),
    )
    .expect_err("identical old/new should fail");
    assert!(edit_same.contains("must differ"));

    let edit_missing = execute_tool(
        "edit_file",
        &json!({ "path": "nested/demo.txt", "old_string": "missing", "new_string": "omega" }),
    )
    .expect_err("missing substring should fail");
    assert!(edit_missing.contains("old_string not found"));

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn glob_and_grep_tools_cover_success_and_errors() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_path("search-suite");
    fs::create_dir_all(root.join("nested")).expect("create root");
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&root).expect("set cwd");

    fs::write(
        root.join("nested/lib.rs"),
        "fn main() {}\nlet alpha = 1;\nlet alpha = 2;\n",
    )
    .expect("write rust file");
    fs::write(root.join("nested/notes.txt"), "alpha\nbeta\n").expect("write txt file");

    let globbed = execute_tool("glob_search", &json!({ "pattern": "nested/*.rs" }))
        .expect("glob should succeed");
    let globbed_output: serde_json::Value = serde_json::from_str(&globbed).expect("json");
    assert_eq!(globbed_output["numFiles"], 1);
    assert!(globbed_output["filenames"][0]
        .as_str()
        .expect("filename")
        .ends_with("nested/lib.rs"));

    let glob_error = execute_tool("glob_search", &json!({ "pattern": "[" }))
        .expect_err("invalid glob should fail");
    assert!(!glob_error.is_empty());

    let grep_content = execute_tool(
        "grep_search",
        &json!({
            "pattern": "alpha",
            "path": "nested",
            "glob": "*.rs",
            "output_mode": "content",
            "-n": true,
            "head_limit": 1,
            "offset": 1
        }),
    )
    .expect("grep content should succeed");
    let grep_content_output: serde_json::Value = serde_json::from_str(&grep_content).expect("json");
    assert_eq!(grep_content_output["numFiles"], 0);
    assert!(grep_content_output["appliedLimit"].is_null());
    assert_eq!(grep_content_output["appliedOffset"], 1);
    assert!(grep_content_output["content"]
        .as_str()
        .expect("content")
        .contains("let alpha = 2;"));

    let grep_count = execute_tool(
        "grep_search",
        &json!({ "pattern": "alpha", "path": "nested", "output_mode": "count" }),
    )
    .expect("grep count should succeed");
    let grep_count_output: serde_json::Value = serde_json::from_str(&grep_count).expect("json");
    assert_eq!(grep_count_output["numMatches"], 3);

    let grep_error = execute_tool(
        "grep_search",
        &json!({ "pattern": "(alpha", "path": "nested" }),
    )
    .expect_err("invalid regex should fail");
    assert!(!grep_error.is_empty());

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn sleep_waits_and_reports_duration() {
    let started = std::time::Instant::now();
    let result = execute_tool("Sleep", &json!({"duration_ms": 20})).expect("Sleep should succeed");
    let elapsed = started.elapsed();
    let output: serde_json::Value = serde_json::from_str(&result).expect("json");
    assert_eq!(output["duration_ms"], 20);
    assert!(output["message"]
        .as_str()
        .expect("message")
        .contains("Slept for 20ms"));
    assert!(elapsed >= Duration::from_millis(15));
}

#[test]
fn given_excessive_duration_when_sleep_then_rejects_with_error() {
    let result = execute_tool("Sleep", &json!({"duration_ms": 999_999_999_u64}));
    let error = result.expect_err("excessive sleep should fail");
    assert!(error.contains("exceeds maximum allowed sleep"));
}

#[test]
fn given_zero_duration_when_sleep_then_succeeds() {
    let result =
        execute_tool("Sleep", &json!({"duration_ms": 0})).expect("0ms sleep should succeed");
    let output: serde_json::Value = serde_json::from_str(&result).expect("json");
    assert_eq!(output["duration_ms"], 0);
}

#[test]
fn brief_returns_sent_message_and_attachment_metadata() {
    let attachment = std::env::temp_dir().join(format!(
        "clawd-brief-{}.png",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    std::fs::write(&attachment, b"png-data").expect("write attachment");

    let result = execute_tool(
        "SendUserMessage",
        &json!({
            "message": "hello user",
            "attachments": [attachment.display().to_string()],
            "status": "normal"
        }),
    )
    .expect("SendUserMessage should succeed");

    let output: serde_json::Value = serde_json::from_str(&result).expect("json");
    assert_eq!(output["message"], "hello user");
    assert!(output["sentAt"].as_str().is_some());
    assert_eq!(output["attachments"][0]["isImage"], true);
    let _ = std::fs::remove_file(attachment);
}

#[test]
fn config_reads_and_writes_supported_values() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = std::env::temp_dir().join(format!(
        "clawd-config-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    let home = root.join("home");
    let cwd = root.join("cwd");
    std::fs::create_dir_all(home.join(".claw")).expect("home dir");
    std::fs::create_dir_all(cwd.join(".claw")).expect("cwd dir");
    std::fs::write(
        home.join(".claw").join("settings.json"),
        r#"{"verbose":false}"#,
    )
    .expect("write global settings");

    let original_home = std::env::var("HOME").ok();
    let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_var("HOME", &home);
    std::env::remove_var("CLAW_CONFIG_HOME");
    std::env::set_current_dir(&cwd).expect("set cwd");

    let get = execute_tool("Config", &json!({"setting": "verbose"})).expect("get config");
    let get_output: serde_json::Value = serde_json::from_str(&get).expect("json");
    assert_eq!(get_output["value"], false);

    let set = execute_tool(
        "Config",
        &json!({"setting": "permissions.defaultMode", "value": "plan"}),
    )
    .expect("set config");
    let set_output: serde_json::Value = serde_json::from_str(&set).expect("json");
    assert_eq!(set_output["operation"], "set");
    assert_eq!(set_output["newValue"], "plan");

    let invalid = execute_tool(
        "Config",
        &json!({"setting": "permissions.defaultMode", "value": "bogus"}),
    )
    .expect_err("invalid config value should error");
    assert!(invalid.contains("Invalid value"));

    let unknown =
        execute_tool("Config", &json!({"setting": "nope"})).expect("unknown setting result");
    let unknown_output: serde_json::Value = serde_json::from_str(&unknown).expect("json");
    assert_eq!(unknown_output["success"], false);

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    match original_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
    match original_config_home {
        Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
        None => std::env::remove_var("CLAW_CONFIG_HOME"),
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn enter_and_exit_plan_mode_round_trip_existing_local_override() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = std::env::temp_dir().join(format!(
        "clawd-plan-mode-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    let home = root.join("home");
    let cwd = root.join("cwd");
    std::fs::create_dir_all(home.join(".claw")).expect("home dir");
    std::fs::create_dir_all(cwd.join(".claw")).expect("cwd dir");
    std::fs::write(
        cwd.join(".claw").join("settings.local.json"),
        r#"{"permissions":{"defaultMode":"acceptEdits"}}"#,
    )
    .expect("write local settings");

    let original_home = std::env::var("HOME").ok();
    let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_var("HOME", &home);
    std::env::remove_var("CLAW_CONFIG_HOME");
    std::env::set_current_dir(&cwd).expect("set cwd");

    let enter = execute_tool("EnterPlanMode", &json!({})).expect("enter plan mode");
    let enter_output: serde_json::Value = serde_json::from_str(&enter).expect("json");
    assert_eq!(enter_output["changed"], true);
    assert_eq!(enter_output["managed"], true);
    assert_eq!(enter_output["previousLocalMode"], "acceptEdits");
    assert_eq!(enter_output["currentLocalMode"], "plan");

    let local_settings = std::fs::read_to_string(cwd.join(".claw").join("settings.local.json"))
        .expect("local settings after enter");
    assert!(local_settings.contains(r#""defaultMode": "plan""#));
    let state =
        std::fs::read_to_string(cwd.join(".claw").join("tool-state").join("plan-mode.json"))
            .expect("plan mode state");
    assert!(state.contains(r#""hadLocalOverride": true"#));
    assert!(state.contains(r#""previousLocalMode": "acceptEdits""#));

    let exit = execute_tool("ExitPlanMode", &json!({})).expect("exit plan mode");
    let exit_output: serde_json::Value = serde_json::from_str(&exit).expect("json");
    assert_eq!(exit_output["changed"], true);
    assert_eq!(exit_output["managed"], false);
    assert_eq!(exit_output["previousLocalMode"], "acceptEdits");
    assert_eq!(exit_output["currentLocalMode"], "acceptEdits");

    let local_settings = std::fs::read_to_string(cwd.join(".claw").join("settings.local.json"))
        .expect("local settings after exit");
    assert!(local_settings.contains(r#""defaultMode": "acceptEdits""#));
    assert!(!cwd
        .join(".claw")
        .join("tool-state")
        .join("plan-mode.json")
        .exists());

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    match original_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
    match original_config_home {
        Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
        None => std::env::remove_var("CLAW_CONFIG_HOME"),
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn exit_plan_mode_clears_override_when_enter_created_it_from_empty_local_state() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = std::env::temp_dir().join(format!(
        "clawd-plan-mode-empty-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    let home = root.join("home");
    let cwd = root.join("cwd");
    std::fs::create_dir_all(home.join(".claw")).expect("home dir");
    std::fs::create_dir_all(cwd.join(".claw")).expect("cwd dir");

    let original_home = std::env::var("HOME").ok();
    let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_var("HOME", &home);
    std::env::remove_var("CLAW_CONFIG_HOME");
    std::env::set_current_dir(&cwd).expect("set cwd");

    let enter = execute_tool("EnterPlanMode", &json!({})).expect("enter plan mode");
    let enter_output: serde_json::Value = serde_json::from_str(&enter).expect("json");
    assert_eq!(enter_output["previousLocalMode"], serde_json::Value::Null);
    assert_eq!(enter_output["currentLocalMode"], "plan");

    let exit = execute_tool("ExitPlanMode", &json!({})).expect("exit plan mode");
    let exit_output: serde_json::Value = serde_json::from_str(&exit).expect("json");
    assert_eq!(exit_output["changed"], true);
    assert_eq!(exit_output["currentLocalMode"], serde_json::Value::Null);

    let local_settings = std::fs::read_to_string(cwd.join(".claw").join("settings.local.json"))
        .expect("local settings after exit");
    let local_settings_json: serde_json::Value =
        serde_json::from_str(&local_settings).expect("valid settings json");
    assert_eq!(
        local_settings_json.get("permissions"),
        None,
        "permissions override should be removed on exit"
    );
    assert!(!cwd
        .join(".claw")
        .join("tool-state")
        .join("plan-mode.json")
        .exists());

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    match original_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
    match original_config_home {
        Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
        None => std::env::remove_var("CLAW_CONFIG_HOME"),
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn structured_output_echoes_input_payload() {
    let result = execute_tool("StructuredOutput", &json!({"ok": true, "items": [1, 2, 3]}))
        .expect("StructuredOutput should succeed");
    let output: serde_json::Value = serde_json::from_str(&result).expect("json");
    assert_eq!(output["data"], "Structured output provided successfully");
    assert_eq!(output["structured_output"]["ok"], true);
    assert_eq!(output["structured_output"]["items"][1], 2);
}

#[test]
fn given_empty_payload_when_structured_output_then_rejects_with_error() {
    let result = execute_tool("StructuredOutput", &json!({}));
    let error = result.expect_err("empty payload should fail");
    assert!(error.contains("must not be empty"));
}

#[test]
fn repl_executes_python_code() {
    let result = execute_tool(
        "REPL",
        &json!({"language": "python", "code": "print(1 + 1)", "timeout_ms": 500}),
    )
    .expect("REPL should succeed");
    let output: serde_json::Value = serde_json::from_str(&result).expect("json");
    assert_eq!(output["language"], "python");
    assert_eq!(output["exitCode"], 0);
    assert!(output["stdout"].as_str().expect("stdout").contains('2'));
}

#[test]
fn given_empty_code_when_repl_then_rejects_with_error() {
    let result = execute_tool("REPL", &json!({"language": "python", "code": "   "}));

    let error = result.expect_err("empty REPL code should fail");
    assert!(error.contains("code must not be empty"));
}

#[test]
fn given_unsupported_language_when_repl_then_rejects_with_error() {
    let result = execute_tool("REPL", &json!({"language": "ruby", "code": "puts 1"}));

    let error = result.expect_err("unsupported REPL language should fail");
    assert!(error.contains("unsupported REPL language: ruby"));
}

#[test]
fn given_timeout_ms_when_repl_blocks_then_returns_timeout_error() {
    let result = execute_tool(
        "REPL",
        &json!({
            "language": "python",
            "code": "import time\ntime.sleep(1)",
            "timeout_ms": 10
        }),
    );

    let error = result.expect_err("timed out REPL execution should fail");
    assert!(error.contains("REPL execution exceeded timeout of 10 ms"));
}

#[test]
fn powershell_runs_via_stub_shell() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = std::env::temp_dir().join(format!(
        "clawd-pwsh-bin-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create dir");
    let script = dir.join("pwsh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
while [ "$1" != "-Command" ] && [ $# -gt 0 ]; do shift; done
shift
printf 'pwsh:%s' "$1"
"#,
    )
    .expect("write script");
    std::process::Command::new("/bin/chmod")
        .arg("+x")
        .arg(&script)
        .status()
        .expect("chmod");
    let original_path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{}", dir.display(), original_path));

    let result = execute_tool(
        "PowerShell",
        &json!({"command": "Write-Output hello", "timeout": 1000}),
    )
    .expect("PowerShell should succeed");

    let background = execute_tool(
        "PowerShell",
        &json!({"command": "Write-Output hello", "run_in_background": true}),
    )
    .expect("PowerShell background should succeed");

    std::env::set_var("PATH", original_path);
    let _ = std::fs::remove_dir_all(dir);

    let output: serde_json::Value = serde_json::from_str(&result).expect("json");
    assert_eq!(output["stdout"], "pwsh:Write-Output hello");
    assert!(output["stderr"].as_str().expect("stderr").is_empty());

    let background_output: serde_json::Value = serde_json::from_str(&background).expect("json");
    assert!(background_output["backgroundTaskId"].as_str().is_some());
    assert_eq!(background_output["backgroundedByUser"], true);
    assert_eq!(background_output["assistantAutoBackgrounded"], false);
}

#[test]
fn powershell_errors_when_shell_is_missing() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let original_path = std::env::var("PATH").unwrap_or_default();
    let empty_dir = std::env::temp_dir().join(format!(
        "clawd-empty-bin-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    std::fs::create_dir_all(&empty_dir).expect("create empty dir");
    std::env::set_var("PATH", empty_dir.display().to_string());

    let err = execute_tool("PowerShell", &json!({"command": "Write-Output hello"}))
        .expect_err("PowerShell should fail when shell is missing");

    std::env::set_var("PATH", original_path);
    let _ = std::fs::remove_dir_all(empty_dir);

    assert!(err.contains("PowerShell executable not found"));
}

fn read_only_registry() -> super::GlobalToolRegistry {
    use runtime::permission_enforcer::PermissionEnforcer;
    use runtime::PermissionPolicy;

    let policy = mvp_tool_specs().into_iter().fold(
        PermissionPolicy::new(runtime::PermissionMode::ReadOnly),
        |policy, spec| policy.with_tool_requirement(spec.name, spec.required_permission),
    );
    let mut registry = super::GlobalToolRegistry::builtin();
    registry.set_enforcer(PermissionEnforcer::new(policy));
    registry
}

#[test]
fn given_read_only_enforcer_when_bash_then_denied() {
    let registry = read_only_registry();
    let err = registry
        .execute("bash", &json!({ "command": "echo hi" }))
        .expect_err("bash should be denied in read-only mode");
    assert!(
        err.contains("current mode is read-only"),
        "should cite active mode: {err}"
    );
}

#[test]
fn given_read_only_enforcer_when_write_file_then_denied() {
    let registry = read_only_registry();
    let err = registry
        .execute(
            "write_file",
            &json!({ "path": "/tmp/x.txt", "content": "x" }),
        )
        .expect_err("write_file should be denied in read-only mode");
    assert!(
        err.contains("current mode is read-only"),
        "should cite active mode: {err}"
    );
}

#[test]
fn given_read_only_enforcer_when_edit_file_then_denied() {
    let registry = read_only_registry();
    let err = registry
        .execute(
            "edit_file",
            &json!({ "path": "/tmp/x.txt", "old_string": "a", "new_string": "b" }),
        )
        .expect_err("edit_file should be denied in read-only mode");
    assert!(
        err.contains("current mode is read-only"),
        "should cite active mode: {err}"
    );
}

#[test]
fn given_read_only_enforcer_when_read_file_then_not_permission_denied() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_path("perm-read");
    fs::create_dir_all(&root).expect("create root");
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&root).expect("set test workspace cwd");
    let file = root.join("readable.txt");
    fs::write(&file, "content\n").expect("write test file");

    let registry = read_only_registry();
    let result = registry.execute("read_file", &json!({ "path": file.display().to_string() }));
    std::env::set_current_dir(original_dir).expect("restore cwd");
    assert!(result.is_ok(), "read_file should be allowed: {result:?}");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn given_read_only_enforcer_when_glob_search_then_not_permission_denied() {
    let registry = read_only_registry();
    let result = registry.execute("glob_search", &json!({ "pattern": "*.rs" }));
    assert!(
        result.is_ok(),
        "glob_search should be allowed in read-only mode: {result:?}"
    );
}

#[test]
fn given_no_enforcer_when_bash_then_executes_normally() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let registry = super::GlobalToolRegistry::builtin();
    let result = registry
        .execute("bash", &json!({ "command": "printf 'ok'" }))
        .expect("bash should succeed without enforcer");
    let output: serde_json::Value = serde_json::from_str(&result).expect("json");
    assert_eq!(output["stdout"], "ok");
}

#[test]
fn provider_runtime_client_chain_uses_only_primary_when_no_fallbacks_configured() {
    // given
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let original_anthropic = std::env::var_os("ANTHROPIC_API_KEY");
    std::env::set_var("ANTHROPIC_API_KEY", "anthropic-test-key");
    let fallback_config = ProviderFallbackConfig::default();

    // when
    let client = ProviderRuntimeClient::new_with_fallback_config(
        "claude-sonnet-4-6".to_string(),
        BTreeSet::new(),
        &fallback_config,
    )
    .expect("primary-only chain should construct");

    // then
    assert_eq!(client.chain.len(), 1);
    assert_eq!(client.chain[0].model, "claude-sonnet-4-6");

    match original_anthropic {
        Some(value) => std::env::set_var("ANTHROPIC_API_KEY", value),
        None => std::env::remove_var("ANTHROPIC_API_KEY"),
    }
}

#[test]
fn provider_runtime_client_chain_appends_configured_fallbacks_in_order() {
    // given
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let original_anthropic = std::env::var_os("ANTHROPIC_API_KEY");
    let original_xai = std::env::var_os("XAI_API_KEY");
    std::env::set_var("ANTHROPIC_API_KEY", "anthropic-test-key");
    std::env::set_var("XAI_API_KEY", "xai-test-key");
    let fallback_config =
        ProviderFallbackConfig::new(None, vec!["grok-3".to_string(), "grok-3-mini".to_string()]);

    // when
    let client = ProviderRuntimeClient::new_with_fallback_config(
        "claude-sonnet-4-6".to_string(),
        BTreeSet::new(),
        &fallback_config,
    )
    .expect("chain with fallbacks should construct");

    // then
    assert_eq!(client.chain.len(), 3);
    assert_eq!(client.chain[0].model, "claude-sonnet-4-6");
    assert_eq!(client.chain[1].model, "grok-3");
    assert_eq!(client.chain[2].model, "grok-3-mini");

    match original_anthropic {
        Some(value) => std::env::set_var("ANTHROPIC_API_KEY", value),
        None => std::env::remove_var("ANTHROPIC_API_KEY"),
    }
    match original_xai {
        Some(value) => std::env::set_var("XAI_API_KEY", value),
        None => std::env::remove_var("XAI_API_KEY"),
    }
}

#[test]
fn provider_runtime_client_chain_primary_override_replaces_constructor_model() {
    // given
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let original_anthropic = std::env::var_os("ANTHROPIC_API_KEY");
    let original_xai = std::env::var_os("XAI_API_KEY");
    std::env::set_var("ANTHROPIC_API_KEY", "anthropic-test-key");
    std::env::set_var("XAI_API_KEY", "xai-test-key");
    let fallback_config = ProviderFallbackConfig::new(
        Some("grok-3".to_string()),
        vec!["claude-sonnet-4-6".to_string()],
    );

    // when
    let client = ProviderRuntimeClient::new_with_fallback_config(
        "claude-haiku-4-5-20251213".to_string(),
        BTreeSet::new(),
        &fallback_config,
    )
    .expect("chain with primary override should construct");

    // then
    assert_eq!(client.chain.len(), 2);
    assert_eq!(client.chain[0].model, "grok-3");
    assert_eq!(client.chain[1].model, "claude-sonnet-4-6");

    match original_anthropic {
        Some(value) => std::env::set_var("ANTHROPIC_API_KEY", value),
        None => std::env::remove_var("ANTHROPIC_API_KEY"),
    }
    match original_xai {
        Some(value) => std::env::set_var("XAI_API_KEY", value),
        None => std::env::remove_var("XAI_API_KEY"),
    }
}

#[test]
fn provider_runtime_client_chain_skips_fallbacks_missing_credentials() {
    // given
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let original_anthropic = std::env::var_os("ANTHROPIC_API_KEY");
    let original_xai = std::env::var_os("XAI_API_KEY");
    std::env::set_var("ANTHROPIC_API_KEY", "anthropic-test-key");
    std::env::remove_var("XAI_API_KEY");
    let fallback_config = ProviderFallbackConfig::new(
        None,
        vec![
            "grok-3".to_string(),
            "claude-haiku-4-5-20251213".to_string(),
        ],
    );

    // when
    let client = ProviderRuntimeClient::new_with_fallback_config(
        "claude-sonnet-4-6".to_string(),
        BTreeSet::new(),
        &fallback_config,
    )
    .expect("chain construction should not fail when only some fallbacks are unavailable");

    // then
    assert_eq!(client.chain.len(), 2);
    assert_eq!(client.chain[0].model, "claude-sonnet-4-6");
    assert_eq!(client.chain[1].model, "claude-haiku-4-5-20251213");

    match original_anthropic {
        Some(value) => std::env::set_var("ANTHROPIC_API_KEY", value),
        None => std::env::remove_var("ANTHROPIC_API_KEY"),
    }
    if let Some(value) = original_xai {
        std::env::set_var("XAI_API_KEY", value);
    }
}

#[test]
fn provider_runtime_client_anthropic_entries_use_prompt_cache_and_cli_auth_path() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let original_api_key = std::env::var_os("ANTHROPIC_API_KEY");
    let original_auth_token = std::env::var_os("ANTHROPIC_AUTH_TOKEN");
    std::env::remove_var("ANTHROPIC_API_KEY");
    std::env::set_var("ANTHROPIC_AUTH_TOKEN", "test-bearer-token");

    let client = ProviderRuntimeClient::new_for_session(
        "claude-sonnet-4-6".to_string(),
        BTreeSet::new(),
        "agent-session-123",
    )
    .expect("anthropic client should construct from CLI-style auth");

    assert_eq!(client.chain.len(), 1);
    match &client.chain[0].client {
        ProviderClient::Anthropic(inner) => {
            assert!(inner.prompt_cache().is_some());
            assert_eq!(
                inner.auth_source(),
                &AuthSource::BearerToken("test-bearer-token".to_string())
            );
            let headers = inner.request_profile().header_pairs();
            assert!(
                headers.contains(&(
                    "X-Claude-Code-Session-Id".to_string(),
                    "agent-session-123".to_string()
                )),
                "subagent Anthropic requests should carry session affinity headers: {headers:?}"
            );
            assert!(
                headers.contains(&("x-app".to_string(), "cli".to_string())),
                "subagent Anthropic requests should match CLI relay headers: {headers:?}"
            );
            assert!(
                headers.contains(&(
                    "anthropic-dangerous-direct-browser-access".to_string(),
                    "true".to_string()
                )),
                "subagent Anthropic requests should match CLI relay headers: {headers:?}"
            );
        }
        other => panic!("expected anthropic provider entry, got {other:?}"),
    }

    match original_api_key {
        Some(value) => std::env::set_var("ANTHROPIC_API_KEY", value),
        None => std::env::remove_var("ANTHROPIC_API_KEY"),
    }
    match original_auth_token {
        Some(value) => std::env::set_var("ANTHROPIC_AUTH_TOKEN", value),
        None => std::env::remove_var("ANTHROPIC_AUTH_TOKEN"),
    }
}

#[test]
fn run_task_packet_creates_packet_backed_task() {
    let result = run_task_packet(TaskPacket {
        objective: "Ship packetized runtime task".to_string(),
        scope: "runtime/task system".to_string(),
        repo: "claw-code-parity".to_string(),
        branch_policy: "origin/main only".to_string(),
        acceptance_tests: vec![
            "cargo build --workspace".to_string(),
            "cargo test --workspace".to_string(),
        ],
        commit_policy: "single commit".to_string(),
        reporting_contract: "print build/test result and sha".to_string(),
        escalation_policy: "manual escalation".to_string(),
    })
    .expect("task packet should create a task");

    let output: serde_json::Value = serde_json::from_str(&result).expect("json");
    assert_eq!(output["status"], "created");
    assert_eq!(output["prompt"], "Ship packetized runtime task");
    assert_eq!(output["description"], "runtime/task system");
    assert_eq!(output["task_packet"]["repo"], "claw-code-parity");
    assert_eq!(
        output["task_packet"]["acceptance_tests"][1],
        "cargo test --workspace"
    );
}

struct TestServer {
    addr: SocketAddr,
    shutdown: Option<std::sync::mpsc::Sender<()>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl TestServer {
    fn spawn(handler: Arc<dyn Fn(&str) -> HttpResponse + Send + Sync + 'static>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        listener
            .set_nonblocking(true)
            .expect("set nonblocking listener");
        let addr = listener.local_addr().expect("local addr");
        let (tx, rx) = std::sync::mpsc::channel::<()>();

        let handle = thread::spawn(move || loop {
            if rx.try_recv().is_ok() {
                break;
            }

            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut buffer = [0_u8; 4096];
                    let size = stream.read(&mut buffer).expect("read request");
                    let request = String::from_utf8_lossy(&buffer[..size]).into_owned();
                    let request_line = request.lines().next().unwrap_or_default().to_string();
                    let response = handler(&request_line);
                    stream
                        .write_all(response.to_bytes().as_slice())
                        .expect("write response");
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("server accept failed: {error}"),
            }
        });

        Self {
            addr,
            shutdown: Some(tx),
            handle: Some(handle),
        }
    }

    fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            handle.join().expect("join test server");
        }
    }
}

struct HttpResponse {
    status: u16,
    reason: &'static str,
    content_type: &'static str,
    body: String,
}

impl HttpResponse {
    fn html(status: u16, reason: &'static str, body: &str) -> Self {
        Self {
            status,
            reason,
            content_type: "text/html; charset=utf-8",
            body: body.to_string(),
        }
    }

    fn text(status: u16, reason: &'static str, body: &str) -> Self {
        Self {
            status,
            reason,
            content_type: "text/plain; charset=utf-8",
            body: body.to_string(),
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        format!(
                "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                self.status,
                self.reason,
                self.content_type,
                self.body.len(),
                self.body
            )
            .into_bytes()
    }
}
