use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use api::{InputContentBlock, MessageRequest};
use mock_anthropic_service::{MockAnthropicService, SCENARIO_PREFIX};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn project_attachments_stay_in_dynamic_system_blocks() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let base_url = server.base_url();
    let workspace = unique_temp_dir("system-prompt-attachments");
    let config_home = workspace.join("config-home");
    let home = workspace.join("home");
    fs::create_dir_all(&workspace).expect("workspace should exist");
    fs::create_dir_all(&config_home).expect("config home should exist");
    fs::create_dir_all(&home).expect("home should exist");
    seed_dirty_git_workspace(&workspace);

    let prompt = format!("{SCENARIO_PREFIX}streaming_text");
    let output = run_claw(
        &workspace,
        &config_home,
        &home,
        &base_url,
        &[
            "--model",
            "sonnet",
            "--permission-mode",
            "read-only",
            "--compact",
            &prompt,
        ],
    );

    assert!(
        output.status.success(),
        "claw run should succeed\nstdout:\n{}\n\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let request = latest_message_request(&runtime, &server);
    let system = request
        .system
        .as_ref()
        .expect("system blocks should be present");
    assert!(
        system
            .iter()
            .any(|block| block.text.contains("# Project attachments")),
        "project attachments should remain in dynamic system blocks: {system:#?}"
    );
    assert!(
        system
            .iter()
            .any(|block| block.text.contains("__SYSTEM_PROMPT_ATTACHMENT_BOUNDARY__")),
        "attachment boundary marker should remain in serialized system blocks: {system:#?}"
    );

    assert_eq!(
        user_text_messages(&request)
            .into_iter()
            .filter(|text| text.contains("# Project attachments"))
            .count(),
        0,
        "project attachments should not be injected into request messages"
    );

    fs::remove_dir_all(&workspace).expect("workspace cleanup should succeed");
}

#[test]
fn attachment_context_does_not_reorder_request_messages() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let base_url = server.base_url();
    let workspace = unique_temp_dir("attachment-message-order");
    let config_home = workspace.join("config-home");
    let home = workspace.join("home");
    fs::create_dir_all(&workspace).expect("workspace should exist");
    fs::create_dir_all(&config_home).expect("config home should exist");
    fs::create_dir_all(&home).expect("home should exist");
    seed_dirty_git_workspace(&workspace);

    let prompt = format!("{SCENARIO_PREFIX}streaming_text");
    let output = run_claw(
        &workspace,
        &config_home,
        &home,
        &base_url,
        &[
            "--model",
            "sonnet",
            "--permission-mode",
            "read-only",
            "--compact",
            &prompt,
        ],
    );

    assert!(
        output.status.success(),
        "claw run should succeed\nstdout:\n{}\n\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let request = latest_message_request(&runtime, &server);
    let user_texts = user_text_messages(&request);
    assert!(
        user_texts.len() == 1,
        "expected only the final user prompt in request messages, got {user_texts:#?}"
    );
    let last_user_text = user_texts.last().expect("last user message should exist");
    assert!(
        last_user_text.contains(&prompt),
        "final user prompt should remain the last user message: {user_texts:#?}"
    );
    assert!(
        !last_user_text.contains("# Project attachments"),
        "attachment context should not be merged into the final user prompt: {user_texts:#?}"
    );
    assert!(
        request
            .system
            .as_ref()
            .is_some_and(|blocks| blocks.iter().all(|block| !block
                .text
                .contains("Raw project attachments are omitted by default"))),
        "omission note should not be injected in current prompt assembly: {:?}",
        request.system
    );

    fs::remove_dir_all(&workspace).expect("workspace cleanup should succeed");
}

#[test]
fn cacheable_system_blocks_are_split_by_scope_and_ttl() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let base_url = server.base_url();
    let workspace = unique_temp_dir("system-prompt-cache-scope");
    let config_home = workspace.join("config-home");
    let home = workspace.join("home");
    fs::create_dir_all(&workspace).expect("workspace should exist");
    fs::create_dir_all(&config_home).expect("config home should exist");
    fs::create_dir_all(&home).expect("home should exist");
    seed_dirty_git_workspace(&workspace);

    let prompt = format!("{SCENARIO_PREFIX}streaming_text");
    let output = run_claw(
        &workspace,
        &config_home,
        &home,
        &base_url,
        &[
            "--model",
            "sonnet",
            "--permission-mode",
            "read-only",
            "--compact",
            &prompt,
        ],
    );

    assert!(
        output.status.success(),
        "claw run should succeed\nstdout:\n{}\n\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let request = latest_message_request(&runtime, &server);
    let system = request.system.expect("system blocks should be present");
    assert_eq!(
        system.len(),
        2,
        "expected static and dynamic system blocks, got {system:#?}"
    );

    assert!(system[0].text.contains("You are an interactive agent"));
    assert!(system[0].text.contains("# Claude instructions"));
    assert!(system[0].text.contains("# Project summary"));
    assert!(!system[0].text.contains("# Runtime config"));
    assert!(!system[0].text.contains("# Project attachments"));

    assert!(system[1].text.contains("# Environment context"));
    assert!(system[1].text.contains("# Runtime config"));
    assert!(system[1].text.contains("# Project attachments"));
    assert!(system[1]
        .text
        .contains("__SYSTEM_PROMPT_ATTACHMENT_BOUNDARY__"));

    let system_json = serde_json::to_value(&system).expect("system blocks should serialize");
    assert_eq!(system_json[0]["cache_control"]["type"], "ephemeral");
    assert!(
        system_json[0]["cache_control"].get("ttl").is_none(),
        "static block should not set an explicit ttl in current payload"
    );
    assert!(
        system_json[0]["cache_control"].get("scope").is_none(),
        "static block should not set an explicit scope in current payload"
    );

    assert!(
        system_json[1]["cache_control"].is_null(),
        "dynamic block should stay uncached to preserve marker budget"
    );

    fs::remove_dir_all(&workspace).expect("workspace cleanup should succeed");
}

fn latest_message_request(
    runtime: &tokio::runtime::Runtime,
    server: &MockAnthropicService,
) -> MessageRequest {
    let captured = runtime.block_on(server.captured_requests());
    let request = captured
        .iter()
        .rev()
        .find(|request| request.path == "/v1/messages")
        .expect("expected a /v1/messages request");
    serde_json::from_str(&request.raw_body).expect("captured request body should deserialize")
}

fn user_text_messages(request: &MessageRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .filter(|message| message.role == "user")
        .filter_map(|message| {
            let text = message
                .content
                .iter()
                .filter_map(|block| match block {
                    InputContentBlock::Text { text, .. } => Some(text.as_str()),
                    InputContentBlock::ToolUse { .. } | InputContentBlock::ToolResult { .. } => {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            (!text.is_empty()).then_some(text)
        })
        .collect()
}

fn seed_dirty_git_workspace(workspace: &Path) {
    git(&["init", "--quiet", "-b", "main"], workspace);
    git(&["config", "user.email", "tests@example.com"], workspace);
    git(&["config", "user.name", "Rusty Claude Tests"], workspace);
    fs::write(
        workspace.join("CLAUDE.md"),
        "Follow repository instructions.\n",
    )
    .expect("CLAUDE.md should write");
    fs::write(workspace.join("tracked.txt"), "alpha\n").expect("tracked file should write");
    git(&["add", "CLAUDE.md", "tracked.txt"], workspace);
    git(&["commit", "-m", "init", "--quiet"], workspace);
    fs::write(workspace.join("tracked.txt"), "alpha\nbeta\n").expect("tracked file should update");
}

fn git(args: &[&str], workspace: &Path) {
    let status = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .status()
        .expect("git command should run");
    assert!(status.success(), "git command should succeed: git {args:?}");
}

fn run_claw(cwd: &Path, config_home: &Path, home: &Path, base_url: &str, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_claw"));
    command
        .current_dir(cwd)
        .env_clear()
        .env("ANTHROPIC_API_KEY", "test-system-prompt-key")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("CLAW_CONFIG_HOME", config_home)
        .env("HOME", home)
        .env("NO_COLOR", "1")
        .env("PATH", "/usr/bin:/bin")
        .args(args);
    command.output().expect("claw should launch")
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_millis();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "claw-system-prompt-{label}-{}-{millis}-{counter}",
        std::process::id()
    ))
}
