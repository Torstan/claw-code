# Rust Risk Test Confirmation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add explicit ignored Rust regression tests that confirm the 17 reviewed risks, run them, and write an evidence-backed root cause report.

**Architecture:** Keep confirmation tests close to the module that owns each behavior. Tests assert desired correct behavior and are marked `#[ignore = "known issue confirmation"]`, so they fail only when explicitly run with `-- --ignored`. The final report maps each failing test to root cause, impact, and fix direction.

**Tech Stack:** Rust workspace, Cargo unit tests, ignored regression tests, Markdown documentation.

---

## File Structure

- Modify `rust/crates/runtime/src/permissions.rs`: issue 1 permission-mode test.
- Modify `rust/crates/runtime/src/file_ops.rs`: issues 13, 14, 15 file operation tests.
- Modify `rust/crates/runtime/src/sandbox.rs`: issue 3 sandbox structural test.
- Modify `rust/crates/api/src/providers/openai_compat.rs`: issues 4, 5, 6 OpenAI-compatible provider tests.
- Modify `rust/crates/api/src/providers/anthropic.rs`: issue 17 retry duration policy test for Anthropic defaults.
- Modify `rust/crates/tools/src/tests.rs`: issues 2, 8, 9, 10, 11, and 12 tool/subagent/task confirmation tests.
- Modify `rust/crates/rusty-claude-cli/src/provider_client.rs`: issue 7 CLI max token test.
- Modify `rust/crates/rusty-claude-cli/src/tool_executor.rs`: issue 16 debug logging confirmation test.
- Create `docs/rust-risk-test-confirmation.md`: final investigation report with test evidence and root cause analysis.

Do not change production behavior to make the new tests pass. The known-issue tests should fail when explicitly run.

---

### Task 1: Runtime Known-Issue Tests

**Files:**
- Modify: `rust/crates/runtime/src/permissions.rs`
- Modify: `rust/crates/runtime/src/file_ops.rs`
- Modify: `rust/crates/runtime/src/sandbox.rs`

- [ ] **Step 1: Add issue 1 permission tests**

Append these tests inside `#[cfg(test)] mod tests` in `rust/crates/runtime/src/permissions.rs`:

```rust
#[test]
#[ignore = "known issue confirmation: prompt mode currently bypasses approval"]
fn confirms_issue_01_prompt_mode_requires_prompter_for_dangerous_tools() {
    let policy = PermissionPolicy::new(PermissionMode::Prompt)
        .with_tool_requirement("bash", PermissionMode::DangerFullAccess);
    let mut prompter = RecordingPrompter {
        seen: Vec::new(),
        allow: true,
    };

    let outcome = policy.authorize("bash", r#"{"command":"rm -rf /tmp/example"}"#, Some(&mut prompter));

    assert_eq!(outcome, PermissionOutcome::Allow);
    assert_eq!(
        prompter.seen.len(),
        1,
        "Prompt mode must ask before allowing danger-full-access tools"
    );
}

#[test]
#[ignore = "known issue confirmation: prompt mode currently allows without prompter"]
fn confirms_issue_01_prompt_mode_denies_without_prompter() {
    let policy = PermissionPolicy::new(PermissionMode::Prompt)
        .with_tool_requirement("bash", PermissionMode::DangerFullAccess);

    assert!(matches!(
        policy.authorize("bash", r#"{"command":"rm -rf /tmp/example"}"#, None),
        PermissionOutcome::Deny { reason } if reason.contains("requires approval")
    ));
}
```

- [ ] **Step 2: Run issue 1 tests and confirm failure**

Run:

```bash
cargo test -p runtime confirms_issue_01 -- --ignored
```

Expected: both tests fail against the current implementation because prompt mode returns `Allow` before invoking the prompter.

- [ ] **Step 3: Add issue 13, 14, and 15 file operation tests**

Keep the existing import list in `rust/crates/runtime/src/file_ops.rs` test module in this shape:

```rust
use super::{
    edit_file, expand_braces, glob_search, grep_search, is_symlink_escape, read_file,
    read_file_in_workspace, write_file, GrepSearchInput, MAX_WRITE_SIZE,
};
```

Append these tests inside the same test module:

```rust
#[test]
#[ignore = "known issue confirmation: edit_file currently edits first duplicate match"]
fn confirms_issue_13_edit_file_requires_unique_match_when_not_replace_all() {
    let path = temp_path("issue-13-duplicate-edit.txt");
    write_file(path.to_string_lossy().as_ref(), "alpha\nbeta\nalpha\n")
        .expect("initial file should write");

    let result = edit_file(path.to_string_lossy().as_ref(), "alpha", "omega", false);

    assert!(
        result.is_err(),
        "non-replace-all edit should reject ambiguous duplicate old_string matches"
    );
}

#[test]
#[ignore = "known issue confirmation: structured patch currently emits full-file replacement"]
fn confirms_issue_14_structured_patch_is_localized() {
    let path = temp_path("issue-14-patch.txt");
    write_file(path.to_string_lossy().as_ref(), "one\ntwo\nthree\nfour\n")
        .expect("initial file should write");

    let output = edit_file(path.to_string_lossy().as_ref(), "two", "TWO", false)
        .expect("edit should execute");
    let changed_lines = output
        .structured_patch
        .iter()
        .flat_map(|hunk| hunk.lines.iter())
        .filter(|line| line.starts_with('+') || line.starts_with('-'))
        .count();

    assert!(
        changed_lines <= 4,
        "single-line edits should not serialize the entire file as removed and re-added"
    );
}

#[test]
#[ignore = "known issue confirmation: multiline grep content mode currently scans line-by-line"]
fn confirms_issue_15_multiline_grep_matches_across_lines_in_content_mode() {
    let dir = temp_path("issue-15-grep");
    std::fs::create_dir_all(&dir).expect("directory should create");
    let file = dir.join("sample.txt");
    write_file(file.to_string_lossy().as_ref(), "first\nsecond\nthird\n")
        .expect("file should write");

    let result = grep_search(&GrepSearchInput {
        pattern: "first\\nsecond".to_string(),
        path: Some(dir.to_string_lossy().into_owned()),
        glob: Some("**/*.txt".to_string()),
        output_mode: Some("content".to_string()),
        before: None,
        after: None,
        context_short: None,
        context: None,
        line_numbers: Some(true),
        case_insensitive: Some(false),
        file_type: None,
        head_limit: Some(10),
        offset: Some(0),
        multiline: Some(true),
    })
    .expect("grep should run");

    assert!(
        result.content.unwrap_or_default().contains("first"),
        "multiline content mode should return a match spanning newline characters"
    );
}
```

- [ ] **Step 4: Run runtime file tests and confirm failure**

Run:

```bash
cargo test -p runtime confirms_issue_13 -- --ignored
cargo test -p runtime confirms_issue_14 -- --ignored
cargo test -p runtime confirms_issue_15 -- --ignored
```

Expected: issue 13, 14, and 15 tests fail against current behavior.

- [ ] **Step 5: Add issue 3 sandbox structural test**

Append this test inside `#[cfg(test)] mod tests` in `rust/crates/runtime/src/sandbox.rs`:

```rust
#[test]
#[ignore = "known issue confirmation: filesystem sandbox reports active without mount isolation"]
fn confirms_issue_03_filesystem_sandbox_requires_enforced_mount_boundary() {
    let request = SandboxConfig::default().resolve_request(
        Some(true),
        Some(false),
        Some(false),
        Some(FilesystemIsolationMode::WorkspaceOnly),
        None,
    );
    let status = super::resolve_sandbox_status_for_request(&request, Path::new("/workspace"));

    assert!(
        !status.filesystem_active,
        "filesystem_active must not be true unless filesystem isolation is actually enforced"
    );
}
```

- [ ] **Step 6: Run issue 3 test and confirm failure**

Run:

```bash
cargo test -p runtime confirms_issue_03 -- --ignored
```

Expected: test fails because `filesystem_active` is true for workspace-only mode even when no filesystem boundary is enforced.

- [ ] **Step 7: Run default runtime tests**

Run:

```bash
cargo test -p runtime
```

Expected: default runtime tests pass because the new confirmation tests are ignored.

- [ ] **Step 8: Commit runtime tests**

Run:

```bash
git add rust/crates/runtime/src/permissions.rs rust/crates/runtime/src/file_ops.rs rust/crates/runtime/src/sandbox.rs
git commit -m "test(runtime): add ignored risk confirmation tests"
```

---

### Task 2: API Provider Known-Issue Tests

**Files:**
- Modify: `rust/crates/api/src/providers/openai_compat.rs`
- Modify: `rust/crates/api/src/providers/anthropic.rs`

- [ ] **Step 1: Add issue 4, 5, and 6 OpenAI-compatible tests**

Append these tests inside `#[cfg(test)] mod tests` in `rust/crates/api/src/providers/openai_compat.rs`:

```rust
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

    fn unset(key: &'static str) -> Self {
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

#[test]
#[ignore = "known issue confirmation: local OpenAI-compatible endpoints currently require API key"]
fn confirms_issue_04_openai_base_url_without_key_builds_unauthenticated_client() {
    let _guard = env_lock();
    let _api_key = EnvVarGuard::unset("OPENAI_API_KEY");
    let _base_url = EnvVarGuard::set("OPENAI_BASE_URL", "http://127.0.0.1:11434/v1");

    let client = OpenAiCompatClient::from_env(OpenAiCompatConfig::openai());

    assert!(
        client.is_ok(),
        "local OpenAI-compatible endpoints configured by OPENAI_BASE_URL should not require an API key"
    );
}

#[test]
#[ignore = "known issue confirmation: tool result wire payload currently contains non-standard fields"]
fn confirms_issue_05_tool_result_wire_payload_omits_is_error() {
    let request = MessageRequest {
        model: "openai/gpt-4o".to_string(),
        max_tokens: 64,
        messages: vec![InputMessage {
            role: "user".to_string(),
            content: vec![InputContentBlock::ToolResult {
                tool_use_id: "call_1".to_string(),
                content: vec![ToolResultContentBlock::Text {
                    text: "tool failed".to_string(),
                }],
                is_error: true,
                cache_control: None,
            }],
        }],
        ..Default::default()
    };

    let payload = build_chat_completion_request(&request, OpenAiCompatConfig::openai());
    let serialized = serde_json::to_string(&payload).expect("payload should serialize");

    assert!(
        !serialized.contains("\"is_error\""),
        "OpenAI Chat Completions tool messages must not include Anthropic-only is_error"
    );
}

#[test]
#[ignore = "known issue confirmation: orphan tool messages currently survive sanitizer"]
fn confirms_issue_05_orphan_tool_messages_are_dropped() {
    let request = MessageRequest {
        model: "openai/gpt-4o".to_string(),
        max_tokens: 64,
        messages: vec![
            InputMessage::user_text("hello"),
            InputMessage {
                role: "user".to_string(),
                content: vec![InputContentBlock::ToolResult {
                    tool_use_id: "missing_call".to_string(),
                    content: vec![ToolResultContentBlock::Text {
                        text: "orphan".to_string(),
                    }],
                    is_error: false,
                    cache_control: None,
                }],
            },
        ],
        ..Default::default()
    };

    let payload = build_chat_completion_request(&request, OpenAiCompatConfig::openai());
    let tool_messages = payload["messages"]
        .as_array()
        .expect("messages should be array")
        .iter()
        .filter(|message| message["role"] == json!("tool"))
        .count();

    assert_eq!(tool_messages, 0, "orphan tool messages should be removed from OpenAI payloads");
}

#[test]
#[ignore = "known issue confirmation: non-streaming tool_calls null currently fails deserialization"]
fn confirms_issue_06_non_streaming_tool_calls_null_deserializes_as_empty() {
    let body = r#"{
        "id": "chatcmpl_null_tools",
        "object": "chat.completion",
        "created": 1,
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "hello",
                "tool_calls": null
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 1,
            "completion_tokens": 1,
            "total_tokens": 2
        }
    }"#;

    let parsed = serde_json::from_str::<super::ChatCompletionResponse>(body);

    assert!(
        parsed.is_ok(),
        "non-streaming OpenAI responses should tolerate tool_calls:null"
    );
}
```

- [ ] **Step 2: Run OpenAI-compatible confirmation tests**

Run:

```bash
cargo test -p api confirms_issue_04 -- --ignored
cargo test -p api confirms_issue_05 -- --ignored
cargo test -p api confirms_issue_06 -- --ignored
```

Expected: all three issue groups fail against current implementation.

- [ ] **Step 3: Add issue 17 Anthropic retry policy test**

Append this test inside `#[cfg(test)] mod tests` in `rust/crates/api/src/providers/anthropic.rs`:

```rust
#[test]
#[ignore = "known issue confirmation: default retry budget is too long for fast fallback"]
fn confirms_issue_17_default_retry_budget_exceeds_fast_fallback_window() {
    let client = AnthropicClient::from_auth(AuthSource::ApiKey("test".to_string()));
    let first = client.backoff_for_attempt(1).expect("attempt 1 backoff");
    let second = client.backoff_for_attempt(2).expect("attempt 2 backoff");
    let third = client.backoff_for_attempt(3).expect("attempt 3 backoff");
    let minimum_without_jitter = first + second + third;

    assert!(
        minimum_without_jitter <= Duration::from_secs(3),
        "provider fallback should not wait through a long retry budget before trying the next provider"
    );
}
```

- [ ] **Step 4: Run issue 17 API test**

Run:

```bash
cargo test -p api confirms_issue_17 -- --ignored
```

Expected: test fails because the first three default backoffs already exceed the desired fast-fallback window.

- [ ] **Step 5: Run default API tests**

Run:

```bash
cargo test -p api
```

Expected: default API tests pass because the confirmation tests are ignored.

- [ ] **Step 6: Commit API tests**

Run:

```bash
git add rust/crates/api/src/providers/openai_compat.rs rust/crates/api/src/providers/anthropic.rs
git commit -m "test(api): add ignored provider risk confirmations"
```

---

### Task 3: Tools And Subagent Known-Issue Tests

**Files:**
- Modify: `rust/crates/tools/src/tests.rs`

- [ ] **Step 1: Add issue 2 dispatch boundary test**

Append this test to `rust/crates/tools/src/tests.rs`:

```rust
#[test]
#[ignore = "known issue confirmation: tool dispatch currently bypasses workspace boundary helpers"]
fn confirms_issue_02_file_tool_dispatch_rejects_outside_absolute_paths() {
    let outside_dir = temp_path("issue-02-outside-dir");
    fs::create_dir_all(&outside_dir).expect("outside dir should create");
    let outside_file = outside_dir.join("secret.txt");
    fs::write(&outside_file, "alpha\nneedle\n").expect("outside file should write");
    let outside_write = outside_dir.join("created-by-tool.txt");

    let outcomes = vec![
        (
            "read_file",
            execute_tool("read_file", &json!({
                "path": outside_file.display().to_string()
            }))
            .is_err(),
        ),
        (
            "write_file",
            execute_tool("write_file", &json!({
                "path": outside_write.display().to_string(),
                "content": "created outside workspace"
            }))
            .is_err(),
        ),
        (
            "edit_file",
            execute_tool("edit_file", &json!({
                "path": outside_file.display().to_string(),
                "old_string": "alpha",
                "new_string": "omega"
            }))
            .is_err(),
        ),
        (
            "glob_search",
            execute_tool("glob_search", &json!({
                "pattern": "**/*.txt",
                "path": outside_dir.display().to_string()
            }))
            .is_err(),
        ),
        (
            "grep_search",
            execute_tool("grep_search", &json!({
                "pattern": "needle",
                "path": outside_dir.display().to_string(),
                "glob": "**/*.txt",
                "output_mode": "content"
            }))
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
```

- [ ] **Step 2: Add issue 8 subagent default model test**

Append this test:

```rust
#[test]
#[ignore = "known issue confirmation: subagent default model currently ignores parent provider"]
fn confirms_issue_08_subagent_default_model_is_not_hardcoded_anthropic() {
    let model = super::agent::resolve_agent_model(None);

    assert_ne!(
        model, "claude-opus-4-6",
        "subagents without explicit model should inherit parent/provider model or use configurable default"
    );
}
```

- [ ] **Step 3: Add issue 9 subagent persistence test**

Append this test:

```rust
#[test]
#[ignore = "known issue confirmation: subagent sessions currently lack persistence path"]
fn confirms_issue_09_subagent_session_has_persistence_for_tool_result_budgeting() {
    let session = new_agent_session("agent-issue-09");

    assert!(
        session.persistence_path().is_some(),
        "subagent sessions need persistence paths so large tool results can be externalized"
    );
}
```

- [ ] **Step 4: Add issue 10 notification size test**

Append this test:

```rust
#[test]
#[ignore = "known issue confirmation: background agent notifications currently include unbounded body"]
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
    };
    let large_body = "x".repeat(128 * 1024);

    enqueue_background_agent_notification(&job, "completed", &large_body);
    let notifications = drain_session_notifications(parent_session);

    assert_eq!(notifications.len(), 1);
    assert!(
        notifications[0].len() < 8 * 1024,
        "background completion notifications should summarize or externalize large agent output"
    );
}
```

- [ ] **Step 5: Add issue 11 task registry isolation test**

Append this test:

```rust
#[test]
#[ignore = "known issue confirmation: global task registry is not session-scoped"]
fn confirms_issue_11_task_registry_requires_session_scope() {
    let registry = global_task_registry();
    let task = registry.create_with_id(
        "shared-task-id".to_string(),
        "prompt from session a",
        Some("session a task"),
    );

    let visible_from_global_lookup = registry.get(&task.task_id).is_some();

    assert!(
        !visible_from_global_lookup,
        "task lookup should require a session namespace instead of process-global task ids"
    );
}
```

- [ ] **Step 6: Add issue 12 background spawn structural test**

Append this test:

```rust
#[test]
#[ignore = "known issue confirmation: background agent spawn is unbounded"]
fn confirms_issue_12_background_agent_execution_requires_concurrency_limit() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let agent_store = temp_path("issue-12-agent-store");
    let original_agent_store = std::env::var_os("CLAWD_AGENT_STORE");
    std::env::set_var("CLAWD_AGENT_STORE", &agent_store);

    let input = AgentInput {
        description: "bounded background work".to_string(),
        prompt: "do bounded work".to_string(),
        subagent_type: None,
        name: None,
        model: Some("claude-opus-4-6".to_string()),
        run_in_background: Some(true),
    };
    let spawned = Arc::new(Mutex::new(0usize));
    let spawned_for_closure = Arc::clone(&spawned);

    let output = execute_agent_with_mode(input, move |_job| {
        *spawned_for_closure.lock().expect("spawn count lock") += 1;
        Ok(())
    });

    match original_agent_store {
        Some(value) => std::env::set_var("CLAWD_AGENT_STORE", value),
        None => std::env::remove_var("CLAWD_AGENT_STORE"),
    }
    let _ = fs::remove_dir_all(&agent_store);
    let output = output.expect("agent launch should return");

    assert!(matches!(output, AgentToolOutput::AsyncLaunched(_)));
    assert_eq!(
        *spawned.lock().expect("spawn count lock"),
        0,
        "background launches should enter a bounded queue instead of spawning immediately"
    );
}
```

- [ ] **Step 7: Run tools confirmation tests**

Run:

```bash
cargo test -p tools confirms_issue_02 -- --ignored
cargo test -p tools confirms_issue_08 -- --ignored
cargo test -p tools confirms_issue_09 -- --ignored
cargo test -p tools confirms_issue_10 -- --ignored
cargo test -p tools confirms_issue_11 -- --ignored
cargo test -p tools confirms_issue_12 -- --ignored
```

Expected: all tools confirmation tests fail against current implementation.

- [ ] **Step 8: Run default tools tests**

Run:

```bash
cargo test -p tools
```

Expected: default tools tests pass because the confirmation tests are ignored.

- [ ] **Step 9: Commit tools tests**

Run:

```bash
git add rust/crates/tools/src/tests.rs
git commit -m "test(tools): add ignored agent risk confirmations"
```

---

### Task 4: CLI Known-Issue Tests

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/provider_client.rs`
- Modify: `rust/crates/rusty-claude-cli/src/tool_executor.rs`

- [ ] **Step 1: Add issue 7 provider max token test**

Add this test module at the end of `rust/crates/rusty-claude-cli/src/provider_client.rs`:

```rust
#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "known issue confirmation: CLI provider currently hardcodes max_tokens to 64000"]
    fn confirms_issue_07_cli_uses_model_specific_max_tokens() {
        let gpt_max = super::max_tokens_for_model("openai/gpt-4o-mini");
        let unknown_local_max = super::max_tokens_for_model("local-small-model");

        assert_ne!(
            gpt_max, 64_000,
            "OpenAI-compatible models should not all request Anthropic-sized output budgets"
        );
        assert!(
            unknown_local_max <= 16_384,
            "unknown local models should use a conservative output budget"
        );
    }
}
```

- [ ] **Step 2: Add issue 16 debug log redaction test**

Add this test module at the end of `rust/crates/rusty-claude-cli/src/tool_executor.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env lock")
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &std::path::Path) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value.as_os_str());
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

    fn debug_temp_dir(name: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should move forward")
            .as_nanos();
        std::env::temp_dir().join(format!("clawd-cli-tool-executor-{name}-{unique}"))
    }

    #[test]
    #[ignore = "known issue confirmation: tool debug logs currently include full input/output"]
    fn confirms_issue_16_tool_debug_log_redacts_secret_shaped_values() {
        let _lock = env_lock();
        let debug_dir = debug_temp_dir("issue-16");
        std::fs::create_dir_all(&debug_dir).expect("debug dir should create");
        let _debug_env = EnvVarGuard::set("CLAWD_AGENT_DEBUG", &debug_dir);
        cli_agent_debug_log(
            "tool.execute.done",
            "tool_name=bash\nok=true\noutput=ANTHROPIC_API_KEY=sk-ant-secret-value",
        );

        let log_path = debug_dir.join("clawd-agent-debug.log");
        let log = std::fs::read_to_string(&log_path).expect("debug log should exist");
        let leaked_secret = log.contains("sk-ant-secret-value");
        let _ = std::fs::remove_dir_all(&debug_dir);

        assert!(
            !leaked_secret,
            "debug logs must redact secret-shaped values before writing to disk"
        );
    }
}
```

- [ ] **Step 3: Run CLI confirmation tests**

Run:

```bash
cargo test -p rusty-claude-cli confirms_issue_07 -- --ignored
cargo test -p rusty-claude-cli confirms_issue_16 -- --ignored
```

Expected: issue 7 and 16 fail.

- [ ] **Step 4: Run default CLI tests**

Run:

```bash
cargo test -p rusty-claude-cli
```

Expected: default CLI tests pass because the confirmation tests are ignored.

- [ ] **Step 5: Commit CLI tests**

Run:

```bash
git add rust/crates/rusty-claude-cli/src/provider_client.rs rust/crates/rusty-claude-cli/src/tool_executor.rs
git commit -m "test(cli): add ignored risk confirmations"
```

---

### Task 5: Execute Confirmation Matrix

**Files:**
- No source edits expected.
- Capture command outcomes for `docs/rust-risk-test-confirmation.md`.

- [ ] **Step 1: Run all ignored confirmation tests by crate**

Run:

```bash
cargo test -p runtime confirms_issue -- --ignored
cargo test -p api confirms_issue -- --ignored
cargo test -p tools confirms_issue -- --ignored
cargo test -p rusty-claude-cli confirms_issue -- --ignored
```

Expected: these commands return non-zero because the confirmation tests assert desired behavior against known-buggy implementation.

- [ ] **Step 2: Capture concise failure evidence**

For each issue, capture:

- Test name.
- Crate.
- Command.
- Failure assertion summary.
- Whether the failure confirms behavior directly or structurally.

Keep each failure summary short enough for the final report to be readable.

- [ ] **Step 3: Run default crate tests**

Run:

```bash
cargo test -p runtime
cargo test -p api
cargo test -p tools
cargo test -p rusty-claude-cli
```

Expected: default tests pass. If a pre-existing default failure appears, include the failing command and failure summary in the report under a separate "Default Test Health" section.

---

### Task 6: Write Investigation Report

**Files:**
- Create: `docs/rust-risk-test-confirmation.md`

- [ ] **Step 1: Create report with complete issue table**

Create `docs/rust-risk-test-confirmation.md` with this structure:

```markdown
# Rust Risk Test Confirmation Report

## Summary

This report confirms 17 Rust implementation risks with ignored regression tests. The tests assert desired behavior and are expected to fail against the current implementation when run with `-- --ignored`.

## Confirmation Matrix

| # | Issue | Crate | Test | Status | Command |
|---:|---|---|---|---|---|
| 1 | Prompt mode bypasses approval | runtime | confirms_issue_01_prompt_mode_requires_prompter_for_dangerous_tools | confirmed_by_failing_test | cargo test -p runtime confirms_issue_01 -- --ignored |
| 2 | File tools bypass workspace boundary | tools | confirms_issue_02_file_tool_dispatch_rejects_outside_absolute_paths | confirmed_by_failing_test | cargo test -p tools confirms_issue_02 -- --ignored |
| 3 | Filesystem sandbox reports active without isolation | runtime | confirms_issue_03_filesystem_sandbox_requires_enforced_mount_boundary | confirmed_by_failing_test | cargo test -p runtime confirms_issue_03 -- --ignored |
| 4 | Local OpenAI-compatible endpoints require API key | api | confirms_issue_04_openai_base_url_without_key_builds_unauthenticated_client | confirmed_by_failing_test | cargo test -p api confirms_issue_04 -- --ignored |
| 5 | OpenAI tool result wire format is invalid | api | confirms_issue_05_tool_result_wire_payload_omits_is_error | confirmed_by_failing_test | cargo test -p api confirms_issue_05 -- --ignored |
| 6 | Non-streaming OpenAI tool_calls null fails | api | confirms_issue_06_non_streaming_tool_calls_null_deserializes_as_empty | confirmed_by_failing_test | cargo test -p api confirms_issue_06 -- --ignored |
| 7 | CLI always requests 64k output tokens | rusty-claude-cli | confirms_issue_07_cli_uses_model_specific_max_tokens | confirmed_by_failing_test | cargo test -p rusty-claude-cli confirms_issue_07 -- --ignored |
| 8 | Subagent default model ignores parent provider | tools | confirms_issue_08_subagent_default_model_is_not_hardcoded_anthropic | confirmed_by_failing_test | cargo test -p tools confirms_issue_08 -- --ignored |
| 9 | Subagent sessions cannot externalize large tool results | tools | confirms_issue_09_subagent_session_has_persistence_for_tool_result_budgeting | confirmed_by_failing_test | cargo test -p tools confirms_issue_09 -- --ignored |
| 10 | Background agent notifications inject unbounded results | tools | confirms_issue_10_background_agent_notification_is_bounded | confirmed_by_failing_test | cargo test -p tools confirms_issue_10 -- --ignored |
| 11 | Task registry is process-wide and non-persistent | tools/runtime | confirms_issue_11_task_registry_requires_session_scope | confirmed_by_structural_test | cargo test -p tools confirms_issue_11 -- --ignored |
| 12 | Agent execution has no concurrency limit | tools | confirms_issue_12_background_agent_execution_requires_concurrency_limit | confirmed_by_structural_test | cargo test -p tools confirms_issue_12 -- --ignored |
| 13 | edit_file does not require unique match | runtime | confirms_issue_13_edit_file_requires_unique_match_when_not_replace_all | confirmed_by_failing_test | cargo test -p runtime confirms_issue_13 -- --ignored |
| 14 | Patch output is full-file replacement | runtime | confirms_issue_14_structured_patch_is_localized | confirmed_by_failing_test | cargo test -p runtime confirms_issue_14 -- --ignored |
| 15 | Multiline grep does not match across lines | runtime | confirms_issue_15_multiline_grep_matches_across_lines_in_content_mode | confirmed_by_failing_test | cargo test -p runtime confirms_issue_15 -- --ignored |
| 16 | Debug logging writes full inputs and outputs | rusty-claude-cli/runtime | confirms_issue_16_tool_debug_log_redacts_secret_shaped_values | confirmed_by_failing_test | cargo test -p rusty-claude-cli confirms_issue_16 -- --ignored |
| 17 | Provider retry/fallback can stall for minutes | api | confirms_issue_17_default_retry_budget_exceeds_fast_fallback_window | confirmed_by_structural_test | cargo test -p api confirms_issue_17 -- --ignored |

## Default Test Health

Include the outcome of `cargo test -p runtime`, `cargo test -p api`, `cargo test -p tools`, and `cargo test -p rusty-claude-cli`.

## Detailed Findings

Every issue section includes `Test evidence`, `Root cause`, `Impact`, and `Suggested fix`.
```

- [ ] **Step 2: Add detailed finding sections**

Add all 17 detailed finding sections. Use the failure summaries captured in Task 5 for the `Test evidence` sentences, and use these root-cause anchors:

```markdown
### Issue 1: Prompt Mode Bypasses Approval

**Test evidence:** `cargo test -p runtime confirms_issue_01 -- --ignored` fails because the prompter was not called and authorization returned `Allow`.

**Root cause:** `PermissionMode` derives `Ord`, and `Prompt` is ordered above `DangerFullAccess`. `PermissionPolicy::authorize_with_context` checks `current_mode >= required_mode` before the prompt branch, so `Prompt` satisfies the comparison and bypasses approval.

**Impact:** Interactive prompt mode cannot be trusted to gate dangerous tools.

**Suggested fix:** Remove derived permission ordering for authorization decisions. Handle `Prompt` before capability comparisons, or use an explicit capability-rank function that excludes prompt mode from automatic allow decisions.

### Issue 2: File Tools Bypass Workspace Boundary

**Root cause:** `tools/src/dispatch.rs` routes `read_file`, `write_file`, `edit_file`, `glob_search`, and `grep_search` directly to runtime helpers that accept absolute paths. The workspace-aware helpers in `runtime/src/file_ops.rs` are not used at the tool dispatch boundary.

### Issue 3: Filesystem Sandbox Reports Active Without Enforcing Isolation

**Root cause:** `resolve_sandbox_status_for_request` sets `filesystem_active` from the requested mode, but `build_linux_sandbox_command` only sets environment variables and namespace flags; it does not bind, chroot, remount, or otherwise enforce a filesystem boundary.

### Issue 4: Local OpenAI-Compatible Endpoints Require API Key

**Root cause:** `OpenAiCompatClient::from_env` always requires a provider API key env var, even when `OPENAI_BASE_URL` points at a local unauthenticated endpoint.

### Issue 5: OpenAI Tool Result Wire Format Is Invalid

**Root cause:** `translate_message` serializes Anthropic-only `is_error` into OpenAI `role:"tool"` messages, and `sanitize_tool_message_pairing` preserves orphan tool messages when the preceding non-tool message is `user` or `system`.

### Issue 6: Non-Streaming OpenAI `tool_calls: null` Fails Deserialization

**Root cause:** Non-streaming `ChatMessage.tool_calls` has `#[serde(default)]` but does not use the null-as-empty deserializer used by the streaming delta path.

### Issue 7: CLI Always Requests 64k Output Tokens

**Root cause:** `rusty-claude-cli/src/provider_client.rs::max_tokens_for_model` ignores its `model` argument and returns `64_000` for every provider.

### Issue 8: Subagent Default Model Ignores Parent Provider

**Root cause:** `tools/src/agent/mod.rs::resolve_agent_model` falls back to `DEFAULT_AGENT_MODEL`, which is hardcoded to `claude-opus-4-6`.

### Issue 9: Subagent Sessions Cannot Externalize Large Tool Results

**Root cause:** `new_agent_session` builds `Session::new()` and only sets `session_id`; it does not call `with_persistence_path`, so runtime tool-result persistence has no destination.

### Issue 10: Background Agent Notifications Inject Unbounded Results

**Root cause:** `enqueue_background_agent_notification` appends the full agent result body into a parent-session notification string without size bounds, summarization, or persisted-output handoff.

### Issue 11: Task Registry Is Process-Wide And Non-Persistent

**Root cause:** `tools/src/registries.rs::global_task_registry` stores a single `OnceLock<TaskRegistry>`, and `TaskRegistry` keys tasks only by `task_id` in process memory.

### Issue 12: Agent Execution Has No Concurrency Limit

**Root cause:** Background agent execution calls the spawn closure immediately from `execute_agent_with_spawn`, and CLI parallel execution creates one OS thread per parallel Agent invocation without a bounded queue.

### Issue 13: `edit_file` Does Not Require Unique Match

**Root cause:** `runtime/src/file_ops.rs::edit_file` uses `replacen(old_string, new_string, 1)` when `replace_all=false`, so duplicate matches silently edit the first occurrence.

### Issue 14: Patch Output Is Full-File Replacement

**Root cause:** `make_patch` emits all original lines as removed and all updated lines as added instead of computing localized hunks.

### Issue 15: Multiline Grep Does Not Match Across Lines In Content Mode

**Root cause:** `grep_search` builds a regex with `dot_matches_new_line`, but content mode still checks `regex.is_match(line)` one line at a time.

### Issue 16: Debug Logging Writes Full Inputs And Outputs

**Root cause:** `agent_debug_log` writes every detail line as provided, and CLI/tool call sites pass normalized input and full output strings without redaction or truncation.

### Issue 17: Provider Retry/Fallback Can Stall For Minutes

**Root cause:** The Anthropic default retry schedule backs off exponentially through eight attempts, and provider fallback only occurs after `ProviderClient::stream_message` returns a retryable error from the primary provider.
```

- [ ] **Step 3: Add priority recommendation**

Add a final section:

```markdown
## Suggested Fix Priority

1. Permission and workspace isolation issues: 1, 2, 3, 16.
2. Provider protocol correctness issues: 4, 5, 6, 7, 17.
3. Agent/task lifecycle and resource-control issues: 8, 9, 10, 11, 12.
4. Editing/search correctness issues: 13, 14, 15.
```

- [ ] **Step 4: Self-check report**

Run:

```bash
rg -n 'TB''D|TO''DO|FIX''ME|PLACE''HOLDER|REPLACE''_ME' docs/rust-risk-test-confirmation.md
```

Expected: no matches. If there are matches, replace those instructional lines with actual evidence before committing.

- [ ] **Step 5: Commit report**

Run:

```bash
git add docs/rust-risk-test-confirmation.md
git commit -m "docs: document rust risk confirmation results"
```

---

### Task 7: Final Verification And Handoff

**Files:**
- No source edits expected unless Task 6 self-check found report gaps.

- [ ] **Step 1: Check git status**

Run:

```bash
git status --short
```

Expected: no tracked modified files from this plan. Existing unrelated untracked files may remain.

- [ ] **Step 2: Summarize commits**

Run:

```bash
git log --oneline -6
```

Expected: recent commits include the runtime, API, tools, CLI, and report commits from this plan.

- [ ] **Step 3: Prepare user-facing completion summary**

Summarize:

- Which ignored confirmation tests were added.
- Which commands were run.
- Which default test suites passed or had pre-existing failures.
- Where the final report lives.

Do not claim the bugs are fixed. The output of this plan is proof and documentation.
