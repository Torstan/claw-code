# Rust Risk Fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the 17 confirmed Rust bugs in `docs/rust-risk-test-confirmation.md` while ignoring Python code.

**Architecture:** The repair is split by crate and write set so workers can run in parallel without editing the same files. Runtime fixes define shared safety primitives, API fixes handle provider wire contracts, tools fixes handle subagent lifecycle/resource behavior, and CLI fixes handle user-facing integration.

**Tech Stack:** Rust workspace under `rust/`, Cargo tests, existing crates `runtime`, `api`, `tools`, and `rusty-claude-cli`.

---

## Execution Model

Run commands from `/mnt/d/ginobili/code/claw-code/rust`.

Parallel wave:

- Task 1: runtime permission, sandbox, and debug logging.
- Task 2: runtime file operations and workspace search helpers.
- Task 3: API provider protocol and retry budget.
- Task 4: tools agent lifecycle, task scope, notifications, and background concurrency.
- Task 6: CLI provider token limits, debug-log coverage, and parallel Agent cap.

Dependency wave:

- Task 5 depends on Task 2 because tools dispatch should call the workspace-scoped runtime helpers added there.
- Task 7 runs after all implementation tasks and performs integration verification plus final review.

Every worker must keep existing unrelated untracked files untouched. Workers are not alone in the codebase: do not revert edits made by another worker, and adapt to already-landed changes.

## File Ownership

- Task 1 owns `rust/crates/runtime/src/permissions.rs`, `rust/crates/runtime/src/sandbox.rs`, and `rust/crates/runtime/src/agent_debug.rs`.
- Task 2 owns `rust/crates/runtime/src/file_ops.rs`.
- Task 3 owns `rust/crates/api/src/providers/openai_compat.rs` and `rust/crates/api/src/providers/anthropic.rs`.
- Task 4 owns `rust/crates/tools/src/agent/mod.rs`, `rust/crates/tools/src/registries.rs`, `rust/crates/tools/src/tests.rs`, and `rust/crates/runtime/src/task_registry.rs`.
- Task 5 owns `rust/crates/tools/src/dispatch.rs`, `rust/crates/tools/src/lib.rs`, and the issue 2 test in `rust/crates/tools/src/tests.rs`.
- Task 6 owns `rust/crates/rusty-claude-cli/src/provider_client.rs`, `rust/crates/rusty-claude-cli/src/tool_executor.rs`, and CLI tests in those files.
- Task 7 owns no production files. It may adjust tests only when integration reveals an assertion that was a structural proxy for the old bug and the replacement assertion is stronger.

## Shared Rules

- Before production edits, run the relevant ignored confirmation command and record that it fails for the expected issue.
- Promote fixed confirmation tests by removing the `#[ignore = "..."]` attribute from the fixed tests.
- Do not weaken behavioral assertions. When replacing a structural proxy test, assert the intended behavior directly.
- Use `cargo fmt -p <crate> --check` before committing task changes.
- Commit each task separately with the listed commit message or a more precise one.

---

### Task 1: Runtime Permission, Sandbox, And Debug Logging

**Issues:** 1, 3, and runtime side of 16.

**Files:**

- Modify: `rust/crates/runtime/src/permissions.rs`
- Modify: `rust/crates/runtime/src/sandbox.rs`
- Modify: `rust/crates/runtime/src/agent_debug.rs`

- [ ] **Step 1: Verify the existing confirmation failures**

Run:

```bash
cargo test -p runtime confirms_issue_01 -- --ignored
cargo test -p runtime confirms_issue_03 -- --ignored
```

Expected: both commands fail because prompt mode allows without prompting and `filesystem_active` is true without enforced filesystem isolation.

- [ ] **Step 2: Add a runtime debug-log redaction regression test**

In `rust/crates/runtime/src/agent_debug.rs`, inside the existing `#[cfg(test)] mod tests`, add this test. Use the existing temp-dir pattern from `agent_debug_log_writes_to_configured_directory`.

```rust
#[test]
fn agent_debug_log_redacts_secret_shaped_values() {
    let dir = std::env::temp_dir().join(format!(
        "clawd-agent-debug-redaction-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("debug dir should be created");
    let previous = std::env::var_os(AGENT_DEBUG_ENV_VAR);
    std::env::set_var(AGENT_DEBUG_ENV_VAR, &dir);

    agent_debug_log(
        "test.secret",
        "api_key=sk-ant-secret-value\nAuthorization: Bearer sk-openai-secret-value",
    );

    match previous {
        Some(value) => std::env::set_var(AGENT_DEBUG_ENV_VAR, value),
        None => std::env::remove_var(AGENT_DEBUG_ENV_VAR),
    }

    let contents =
        std::fs::read_to_string(dir.join(AGENT_DEBUG_FILE_NAME)).expect("debug log file");
    let _ = std::fs::remove_dir_all(&dir);

    assert!(!contents.contains("sk-ant-secret-value"));
    assert!(!contents.contains("sk-openai-secret-value"));
    assert!(contents.contains("[REDACTED_SECRET]"));
}
```

Run:

```bash
cargo test -p runtime agent_debug_log_redacts_secret_shaped_values
```

Expected: fail because `agent_debug_log` currently writes details verbatim.

- [ ] **Step 3: Fix prompt mode authorization**

In `rust/crates/runtime/src/permissions.rs`, add an explicit helper near `impl PermissionPolicy`:

```rust
fn mode_satisfies_requirement(current: PermissionMode, required: PermissionMode) -> bool {
    match current {
        PermissionMode::Allow => true,
        PermissionMode::Prompt => matches!(
            required,
            PermissionMode::ReadOnly | PermissionMode::WorkspaceWrite | PermissionMode::Prompt
        ),
        _ => current >= required && required != PermissionMode::Prompt,
    }
}
```

Replace both occurrences of:

```rust
current_mode >= required_mode
```

inside `authorize_with_context` with:

```rust
Self::mode_satisfies_requirement(current_mode, required_mode)
```

Keep the existing prompt block below it:

```rust
if current_mode == PermissionMode::Prompt
    || (current_mode == PermissionMode::WorkspaceWrite
        && required_mode == PermissionMode::DangerFullAccess)
{
    let reason = Some(format!(
        "tool '{tool_name}' requires approval to escalate from {} to {}",
        current_mode.as_str(),
        required_mode.as_str()
    ));
    return Self::prompt_or_deny(
        tool_name,
        input,
        current_mode,
        required_mode,
        reason,
        prompter,
    );
}
```

Remove the `#[ignore = "..."]` attributes from the two issue 1 tests.

- [ ] **Step 4: Fix sandbox filesystem status truthfulness**

In `rust/crates/runtime/src/sandbox.rs`, replace the current `filesystem_active` calculation:

```rust
let filesystem_active =
    request.enabled && request.filesystem_mode != FilesystemIsolationMode::Off;
```

with:

```rust
let filesystem_requested =
    request.enabled && request.filesystem_mode != FilesystemIsolationMode::Off;
let filesystem_active = false;
```

After the existing allow-list empty-mount fallback check, add:

```rust
if filesystem_requested {
    fallback_reasons.push(
        "filesystem isolation unavailable (no enforced mount boundary is configured)".to_string(),
    );
}
```

Keep `filesystem_mode` and `allowed_mounts` in `SandboxStatus` unchanged so callers can still see what was requested. Remove the `#[ignore = "..."]` attribute from the issue 3 test.

- [ ] **Step 5: Redact and bound debug-log details centrally**

In `rust/crates/runtime/src/agent_debug.rs`, add `use regex::Regex;` with the other imports.

Add constants near the existing debug constants:

```rust
const DEBUG_DETAIL_MAX_CHARS: usize = 16 * 1024;
const DEBUG_DETAIL_TRUNCATION_MARKER: &str = "[truncated debug detail]";
const REDACTED_SECRET: &str = "[REDACTED_SECRET]";
```

Add these helpers below `agent_debug_enabled`:

```rust
fn secret_token_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"(?i)\bsk-[a-z0-9][a-z0-9_-]{6,}\b")
            .expect("secret token regex should compile")
    })
}

fn authorization_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"(?i)(authorization\s*[:=]\s*bearer\s+)[^\s\"']+")
            .expect("authorization regex should compile")
    })
}

fn key_value_secret_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r#"(?i)\b((?:api[_-]?key|token|secret|password)\s*[:=]\s*)[^\s,"'}\]]+"#)
            .expect("key-value secret regex should compile")
    })
}

fn redact_secret_shaped_values(detail: &str) -> String {
    let redacted = secret_token_regex().replace_all(detail, REDACTED_SECRET);
    let redacted = authorization_regex().replace_all(&redacted, "${1}[REDACTED_SECRET]");
    key_value_secret_regex()
        .replace_all(&redacted, "${1}[REDACTED_SECRET]")
        .into_owned()
}

fn bound_debug_detail(detail: &str) -> String {
    let redacted = redact_secret_shaped_values(detail);
    if redacted.chars().count() <= DEBUG_DETAIL_MAX_CHARS {
        return redacted;
    }

    let mut bounded = redacted
        .chars()
        .take(DEBUG_DETAIL_MAX_CHARS)
        .collect::<String>();
    let original_chars = redacted.chars().count();
    bounded.push_str(&format!(
        "\n{DEBUG_DETAIL_TRUNCATION_MARKER} original_chars={original_chars}"
    ));
    bounded
}
```

Inside `agent_debug_log`, replace:

```rust
let detail = detail.as_ref();
```

with:

```rust
let detail = bound_debug_detail(detail.as_ref());
let detail = detail.as_str();
```

- [ ] **Step 6: Verify runtime task 1**

Run:

```bash
cargo test -p runtime confirms_issue_01
cargo test -p runtime confirms_issue_03
cargo test -p runtime agent_debug_log_redacts_secret_shaped_values
cargo test -p runtime
cargo fmt -p runtime --check
```

Expected: all commands pass.

- [ ] **Step 7: Commit task 1**

Run:

```bash
git add rust/crates/runtime/src/permissions.rs rust/crates/runtime/src/sandbox.rs rust/crates/runtime/src/agent_debug.rs
git commit -m "fix(runtime): harden permissions sandbox and debug logs"
```

---

### Task 2: Runtime File Operations And Workspace Search Helpers

**Issues:** 13, 14, 15, and runtime helper side of 2.

**Files:**

- Modify: `rust/crates/runtime/src/file_ops.rs`

- [ ] **Step 1: Verify the existing confirmation failures**

Run:

```bash
cargo test -p runtime confirms_issue_13 -- --ignored
cargo test -p runtime confirms_issue_14 -- --ignored
cargo test -p runtime confirms_issue_15 -- --ignored
```

Expected: all three commands fail for the known behavior.

- [ ] **Step 2: Add workspace-scoped search tests**

In `rust/crates/runtime/src/file_ops.rs`, inside `#[cfg(test)] mod tests`, extend the import list to include the scoped search helpers after adding them in the implementation step:

```rust
glob_search_in_workspace, grep_search_in_workspace,
```

Add tests:

```rust
#[test]
fn workspace_scoped_search_rejects_outside_absolute_paths() {
    let workspace = temp_path("workspace-search-root");
    let outside = temp_path("workspace-search-outside");
    std::fs::create_dir_all(&workspace).expect("workspace should create");
    std::fs::create_dir_all(&outside).expect("outside should create");
    let outside_file = outside.join("secret.txt");
    std::fs::write(&outside_file, "needle\n").expect("outside file should write");

    let glob_error = glob_search_in_workspace(
        "**/*.txt",
        Some(outside.to_string_lossy().as_ref()),
        &workspace,
    )
    .expect_err("outside glob base should be rejected");
    assert_eq!(glob_error.kind(), std::io::ErrorKind::PermissionDenied);

    let grep_error = grep_search_in_workspace(
        &GrepSearchInput {
            pattern: "needle".to_string(),
            path: Some(outside.to_string_lossy().into_owned()),
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
            multiline: Some(false),
        },
        &workspace,
    )
    .expect_err("outside grep base should be rejected");
    assert_eq!(grep_error.kind(), std::io::ErrorKind::PermissionDenied);

    let _ = std::fs::remove_dir_all(workspace);
    let _ = std::fs::remove_dir_all(outside);
}
```

Run:

```bash
cargo test -p runtime workspace_scoped_search_rejects_outside_absolute_paths
```

Expected: fail because `glob_search_in_workspace` and `grep_search_in_workspace` do not exist.

- [ ] **Step 3: Require unique matches for non-replace-all edits**

In `edit_file`, replace:

```rust
if !original_file.contains(old_string) {
    return Err(io::Error::new(
        io::ErrorKind::NotFound,
        "old_string not found in file",
    ));
}

let updated = if replace_all {
    original_file.replace(old_string, new_string)
} else {
    original_file.replacen(old_string, new_string, 1)
};
```

with:

```rust
let match_count = original_file.matches(old_string).count();
if match_count == 0 {
    return Err(io::Error::new(
        io::ErrorKind::NotFound,
        "old_string not found in file",
    ));
}
if !replace_all && match_count != 1 {
    return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "old_string matched {match_count} times; expected exactly one match when replace_all is false"
        ),
    ));
}

let updated = if replace_all {
    original_file.replace(old_string, new_string)
} else {
    original_file.replacen(old_string, new_string, 1)
};
```

Remove the `#[ignore = "..."]` attribute from the issue 13 test.

- [ ] **Step 4: Replace full-file patch output with localized hunks**

Replace `make_patch` with this line-based localized implementation:

```rust
fn make_patch(original: &str, updated: &str) -> Vec<StructuredPatchHunk> {
    if original == updated {
        return Vec::new();
    }

    let original_lines = original.lines().collect::<Vec<_>>();
    let updated_lines = updated.lines().collect::<Vec<_>>();
    let mut prefix_len = 0usize;
    while prefix_len < original_lines.len()
        && prefix_len < updated_lines.len()
        && original_lines[prefix_len] == updated_lines[prefix_len]
    {
        prefix_len += 1;
    }

    let mut suffix_len = 0usize;
    while suffix_len < original_lines.len().saturating_sub(prefix_len)
        && suffix_len < updated_lines.len().saturating_sub(prefix_len)
        && original_lines[original_lines.len() - 1 - suffix_len]
            == updated_lines[updated_lines.len() - 1 - suffix_len]
    {
        suffix_len += 1;
    }

    let old_change_end = original_lines.len().saturating_sub(suffix_len);
    let new_change_end = updated_lines.len().saturating_sub(suffix_len);
    let context_before_start = prefix_len.saturating_sub(2);
    let context_after_end = (old_change_end + 2).min(original_lines.len());
    let context_after_len = context_after_end.saturating_sub(old_change_end);

    let mut lines = Vec::new();
    for line in &original_lines[context_before_start..prefix_len] {
        lines.push(format!(" {line}"));
    }
    for line in &original_lines[prefix_len..old_change_end] {
        lines.push(format!("-{line}"));
    }
    for line in &updated_lines[prefix_len..new_change_end] {
        lines.push(format!("+{line}"));
    }
    for line in &original_lines[old_change_end..context_after_end] {
        lines.push(format!(" {line}"));
    }

    let old_lines = context_after_end.saturating_sub(context_before_start);
    let new_lines = prefix_len.saturating_sub(context_before_start)
        + new_change_end.saturating_sub(prefix_len)
        + context_after_len;

    vec![StructuredPatchHunk {
        old_start: context_before_start.saturating_add(1),
        old_lines,
        new_start: context_before_start.saturating_add(1),
        new_lines,
        lines,
    }]
}
```

Remove the `#[ignore = "..."]` attribute from the issue 14 test.

- [ ] **Step 5: Implement multiline content-mode grep against full file content**

Add `use std::collections::BTreeSet;` near the imports.

Add helpers near `matches_optional_filters`:

```rust
fn matched_line_indices(regex: &regex::Regex, contents: &str, multiline: bool) -> (Vec<usize>, usize) {
    if !multiline {
        let lines = contents.lines().collect::<Vec<_>>();
        let matched = lines
            .iter()
            .enumerate()
            .filter_map(|(index, line)| regex.is_match(line).then_some(index))
            .collect::<Vec<_>>();
        return (matched.clone(), matched.len());
    }

    let line_starts = line_start_offsets(contents);
    let mut lines = BTreeSet::new();
    let mut matches = 0usize;
    for found in regex.find_iter(contents) {
        matches += 1;
        let start_line = line_index_for_offset(&line_starts, found.start());
        let end_offset = found.end().saturating_sub(1);
        let end_line = line_index_for_offset(&line_starts, end_offset);
        for line in start_line..=end_line {
            lines.insert(line);
        }
    }
    (lines.into_iter().collect(), matches)
}

fn line_start_offsets(contents: &str) -> Vec<usize> {
    let mut offsets = vec![0usize];
    for (index, ch) in contents.char_indices() {
        if ch == '\n' && index + 1 < contents.len() {
            offsets.push(index + 1);
        }
    }
    offsets
}

fn line_index_for_offset(line_starts: &[usize], offset: usize) -> usize {
    match line_starts.binary_search(&offset) {
        Ok(index) => index,
        Err(index) => index.saturating_sub(1),
    }
}
```

In `grep_search`, replace the current line-by-line `matched_lines` loop with:

```rust
let lines: Vec<&str> = file_contents.lines().collect();
let (matched_lines, match_count) =
    matched_line_indices(&regex, &file_contents, input.multiline.unwrap_or(false));
total_matches += match_count;

if matched_lines.is_empty() {
    continue;
}
```

Keep the existing content rendering loop over `matched_lines`. Remove the `#[ignore = "..."]` attribute from the issue 15 test.

- [ ] **Step 6: Add workspace-scoped glob and grep helpers**

Below `edit_file_in_workspace`, add:

```rust
/// Run a glob search with workspace boundary enforcement.
pub fn glob_search_in_workspace(
    pattern: &str,
    path: Option<&str>,
    workspace_root: &Path,
) -> io::Result<GlobSearchOutput> {
    let canonical_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let base_path = path
        .map(normalize_path)
        .transpose()?
        .unwrap_or_else(|| canonical_root.clone());
    validate_workspace_boundary(&base_path, &canonical_root)?;

    if Path::new(pattern).is_absolute() {
        let resolved_pattern = normalize_path_allow_missing(pattern)?;
        validate_workspace_boundary(&resolved_pattern, &canonical_root)?;
        return glob_search(pattern, None);
    }

    glob_search(pattern, Some(base_path.to_string_lossy().as_ref()))
}

/// Run a grep search with workspace boundary enforcement.
pub fn grep_search_in_workspace(
    input: &GrepSearchInput,
    workspace_root: &Path,
) -> io::Result<GrepSearchOutput> {
    let canonical_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let base_path = input
        .path
        .as_deref()
        .map(normalize_path)
        .transpose()?
        .unwrap_or_else(|| canonical_root.clone());
    validate_workspace_boundary(&base_path, &canonical_root)?;

    let mut scoped = input.clone();
    scoped.path = Some(base_path.to_string_lossy().into_owned());
    grep_search(&scoped)
}
```

- [ ] **Step 7: Verify runtime task 2**

Run:

```bash
cargo test -p runtime confirms_issue_13
cargo test -p runtime confirms_issue_14
cargo test -p runtime confirms_issue_15
cargo test -p runtime workspace_scoped_search_rejects_outside_absolute_paths
cargo test -p runtime
cargo fmt -p runtime --check
```

Expected: all commands pass.

- [ ] **Step 8: Commit task 2**

Run:

```bash
git add rust/crates/runtime/src/file_ops.rs
git commit -m "fix(runtime): harden file edits and search"
```

---

### Task 3: API Provider Protocol And Retry Budget

**Issues:** 4, 5, 6, and 17.

**Files:**

- Modify: `rust/crates/api/src/providers/openai_compat.rs`
- Modify: `rust/crates/api/src/providers/anthropic.rs`

- [ ] **Step 1: Verify the existing confirmation failures**

Run:

```bash
cargo test -p api confirms_issue_04 -- --ignored
cargo test -p api confirms_issue_05 -- --ignored
cargo test -p api confirms_issue_06 -- --ignored
cargo test -p api confirms_issue_17 -- --ignored
```

Expected: all commands fail for the known provider issues.

- [ ] **Step 2: Allow unauthenticated local OpenAI-compatible endpoints**

In `OpenAiCompatClient`, change:

```rust
api_key: String,
```

to:

```rust
api_key: Option<String>,
```

Update `new`:

```rust
pub fn new(api_key: impl Into<String>, config: OpenAiCompatConfig) -> Self {
    Self::new_with_optional_api_key(Some(api_key.into()), config)
}

fn new_with_optional_api_key(api_key: Option<String>, config: OpenAiCompatConfig) -> Self {
    Self {
        http: build_http_client_or_default(),
        api_key,
        config,
        base_url: read_base_url(config),
        max_retries: DEFAULT_MAX_RETRIES,
        initial_backoff: DEFAULT_INITIAL_BACKOFF,
        max_backoff: DEFAULT_MAX_BACKOFF,
    }
}
```

Add:

```rust
fn is_local_openai_compatible_base_url(base_url: &str) -> bool {
    let normalized = base_url.trim().to_ascii_lowercase();
    normalized.starts_with("http://localhost:")
        || normalized == "http://localhost"
        || normalized.starts_with("http://localhost/")
        || normalized.starts_with("http://127.")
        || normalized.starts_with("http://[::1]")
}
```

Update `from_env`:

```rust
pub fn from_env(config: OpenAiCompatConfig) -> Result<Self, ApiError> {
    let api_key = read_env_non_empty(config.api_key_env)?;
    if api_key.is_none() {
        let base_url = read_base_url(config);
        if !is_local_openai_compatible_base_url(&base_url) {
            return Err(ApiError::missing_credentials(
                config.provider_name,
                config.credential_env_vars(),
            ));
        }
    }
    Ok(Self::new_with_optional_api_key(api_key, config))
}
```

Update `send_raw_request` so bearer auth is conditional:

```rust
let request_builder = self
    .http
    .post(&request_url)
    .header("content-type", "application/json")
    .json(&build_chat_completion_request(request, self.config()));
let request_builder = if let Some(api_key) = &self.api_key {
    request_builder.bearer_auth(api_key)
} else {
    request_builder
};
request_builder.send().await.map_err(ApiError::from)
```

Remove the `#[ignore = "..."]` attribute from the issue 4 test.

- [ ] **Step 3: Fix OpenAI tool result payload shape and orphan sanitizer**

In `translate_message`, replace the `InputContentBlock::ToolResult` arm:

```rust
InputContentBlock::ToolResult {
    tool_use_id,
    content,
    is_error,
    ..
} => Some(json!({
    "role": "tool",
    "tool_call_id": tool_use_id,
    "content": flatten_tool_result_content(content),
    "is_error": is_error,
})),
```

with:

```rust
InputContentBlock::ToolResult {
    tool_use_id,
    content,
    ..
} => Some(json!({
    "role": "tool",
    "tool_call_id": tool_use_id,
    "content": flatten_tool_result_content(content),
})),
```

In `sanitize_tool_message_pairing`, remove the special case that lets tool messages through when the preceding non-tool message is not `assistant`. Replace the pairing block with:

```rust
let paired = preceding
    .filter(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
    .and_then(|m| m.get("tool_calls").and_then(|tc| tc.as_array()))
    .is_some_and(|tool_calls| {
        tool_calls
            .iter()
            .any(|tc| tc.get("id").and_then(|v| v.as_str()) == Some(tool_call_id))
    });
if !paired {
    drop_indices.insert(i);
}
```

Remove the `#[ignore = "..."]` attributes from the two issue 5 tests.

- [ ] **Step 4: Accept `tool_calls: null` in non-streaming OpenAI responses**

In `ChatMessage`, replace:

```rust
#[serde(default)]
tool_calls: Vec<ResponseToolCall>,
```

with:

```rust
#[serde(default, deserialize_with = "deserialize_null_as_empty_vec")]
tool_calls: Vec<ResponseToolCall>,
```

Remove the `#[ignore = "..."]` attribute from the issue 6 test.

- [ ] **Step 5: Add a fast default retry budget for fallback-friendly behavior**

In `rust/crates/api/src/providers/anthropic.rs`, change the default retry constants:

```rust
const DEFAULT_INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(1);
const DEFAULT_MAX_RETRIES: u32 = 2;
```

Update `default_retry_policy_matches_exponential_schedule` so it expects:

```rust
assert_eq!(
    client.backoff_for_attempt(1).expect("attempt 1"),
    Duration::from_millis(250)
);
assert_eq!(
    client.backoff_for_attempt(2).expect("attempt 2"),
    Duration::from_millis(500)
);
assert_eq!(
    client.backoff_for_attempt(3).expect("attempt 3"),
    Duration::from_secs(1)
);
assert_eq!(
    client.backoff_for_attempt(8).expect("attempt 8"),
    Duration::from_secs(1)
);
```

Remove the `#[ignore = "..."]` attribute from the issue 17 test.

- [ ] **Step 6: Verify API task 3**

Run:

```bash
cargo test -p api confirms_issue_04
cargo test -p api confirms_issue_05
cargo test -p api confirms_issue_06
cargo test -p api confirms_issue_17
cargo test -p api
cargo fmt -p api --check
```

Expected: all commands pass.

- [ ] **Step 7: Commit task 3**

Run:

```bash
git add rust/crates/api/src/providers/openai_compat.rs rust/crates/api/src/providers/anthropic.rs
git commit -m "fix(api): harden provider protocol and fallback timing"
```

---

### Task 4: Tools Agent Lifecycle, Task Scope, Notifications, And Background Concurrency

**Issues:** 8, 9, 10, 11, and tools side of 12.

**Files:**

- Modify: `rust/crates/tools/src/agent/mod.rs`
- Modify: `rust/crates/tools/src/tests.rs`
- Modify: `rust/crates/runtime/src/task_registry.rs`

- [ ] **Step 1: Verify the existing confirmation failures**

Run:

```bash
cargo test -p tools confirms_issue_08 -- --ignored
cargo test -p tools confirms_issue_09 -- --ignored
cargo test -p tools confirms_issue_10 -- --ignored
cargo test -p tools confirms_issue_11 -- --ignored
cargo test -p tools confirms_issue_12 -- --ignored
```

Expected: all commands fail for the known tools lifecycle issues.

- [ ] **Step 2: Replace the hardcoded subagent default model**

In `rust/crates/tools/src/agent/mod.rs`, replace:

```rust
const DEFAULT_AGENT_MODEL: &str = "claude-opus-4-6";
```

with:

```rust
const FALLBACK_AGENT_MODEL: &str = "claude-sonnet-4-6";
const AGENT_MODEL_ENV_VAR: &str = "CLAWD_AGENT_MODEL";
```

Add:

```rust
fn configured_agent_model() -> Option<String> {
    std::env::var(AGENT_MODEL_ENV_VAR)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            let cwd = std::env::current_dir().ok()?;
            ConfigLoader::new(cwd)
                .load()
                .ok()?
                .model()
                .map(ToOwned::to_owned)
        })
        .or_else(|| {
            std::env::var("ANTHROPIC_MODEL")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
}
```

Replace `resolve_agent_model` with:

```rust
pub(crate) fn resolve_agent_model(model: Option<&str>) -> String {
    model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(ToOwned::to_owned)
        .or_else(configured_agent_model)
        .unwrap_or_else(|| FALLBACK_AGENT_MODEL.to_string())
}
```

In `build_agent_runtime`, replace `DEFAULT_AGENT_MODEL` with `FALLBACK_AGENT_MODEL`. Remove the `#[ignore = "..."]` attribute from the issue 8 test and update the assertion to also cover the env override:

```rust
let _guard = env_lock()
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner);
let previous = std::env::var_os("CLAWD_AGENT_MODEL");
std::env::set_var("CLAWD_AGENT_MODEL", "openai/gpt-4o-mini");
let model = super::agent::resolve_agent_model(None);
match previous {
    Some(value) => std::env::set_var("CLAWD_AGENT_MODEL", value),
    None => std::env::remove_var("CLAWD_AGENT_MODEL"),
}
assert_eq!(model, "openai/gpt-4o-mini");
```

- [ ] **Step 3: Give subagent sessions persistence paths**

Replace `new_agent_session` with:

```rust
pub(crate) fn new_agent_session(agent_id: &str) -> Session {
    let persistence_path = agent_store_dir()
        .unwrap_or_else(|_| {
            std::env::current_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                .join(".clawd-agents")
        })
        .join(format!("{agent_id}.session.jsonl"));
    let mut session = Session::new().with_persistence_path(persistence_path);
    session.session_id = agent_id.to_string();
    session
}
```

Remove the `#[ignore = "..."]` attribute from the issue 9 test.

- [ ] **Step 4: Bound background agent notifications**

Add near the other agent constants:

```rust
const BACKGROUND_NOTIFICATION_MAX_CHARS: usize = 8 * 1024;
const BACKGROUND_NOTIFICATION_BODY_PREVIEW_CHARS: usize = 2 * 1024;
```

Replace the body append block in `enqueue_background_agent_notification` with:

```rust
if !body.trim().is_empty() {
    let compressed = compress_summary_text(body.trim());
    let preview = compressed
        .chars()
        .take(BACKGROUND_NOTIFICATION_BODY_PREVIEW_CHARS)
        .collect::<String>();
    let truncated = compressed.chars().count() > BACKGROUND_NOTIFICATION_BODY_PREVIEW_CHARS;
    let _ = std::fmt::Write::write_fmt(
        &mut message,
        format_args!(
            "\n{detail_label}_preview:\n{}{}",
            preview,
            if truncated {
                "\n[full output is available in output_file]"
            } else {
                ""
            }
        ),
    );
}
if message.chars().count() > BACKGROUND_NOTIFICATION_MAX_CHARS {
    message = message
        .chars()
        .take(BACKGROUND_NOTIFICATION_MAX_CHARS)
        .collect::<String>();
    message.push_str("\n[notification truncated; full output is available in output_file]");
}
```

Update `background_agent_captures_parent_session_and_formats_notification` to assert `review done` still appears. Remove the `#[ignore = "..."]` attribute from the issue 10 test.

- [ ] **Step 5: Scope task registry operations by active session**

In `rust/crates/runtime/src/task_registry.rs`, add:

```rust
use crate::active_tool_session_id;
```

Add to `impl TaskRegistry`:

```rust
fn current_scope() -> String {
    active_tool_session_id().unwrap_or_else(|| "global".to_string())
}

fn scoped_key(task_id: &str) -> String {
    format!("{}\u{1f}{task_id}", Self::current_scope())
}
```

In `insert_task`, change:

```rust
inner.tasks.insert(task_id, task.clone());
```

to:

```rust
inner.tasks.insert(Self::scoped_key(&task_id), task.clone());
```

In every method that looks up or mutates by `task_id` (`get`, `stop`, `update`, `output`, `append_output`, `set_status`, `assign_team`, `wait_for_terminal`, and `remove`), compute:

```rust
let key = Self::scoped_key(task_id);
```

and use `key` for the `HashMap` access while preserving the user-visible `task.task_id`.

In `list`, filter by the current scope prefix:

```rust
let scope_prefix = format!("{}\u{1f}", Self::current_scope());
inner
    .tasks
    .iter()
    .filter(|(key, _)| key.starts_with(&scope_prefix))
    .map(|(_, task)| task)
    .filter(|t| status_filter.map_or(true, |s| t.status == s))
    .cloned()
    .collect()
```

In `rust/crates/tools/src/agent/mod.rs`, wrap all task registry operations for background agents in the captured parent session:

```rust
let parent_session_id = job.parent_session_id.clone();
with_active_tool_session(parent_session_id.as_deref(), || {
    registry.create_with_id(
        agent_id.clone(),
        &job.prompt,
        Some(&job.manifest.description),
    );
    registry
        .set_status(&agent_id, TaskStatus::Running)
        .map_err(|e| e.clone())
})
```

Also wrap `append_output` and `set_status` calls inside the background thread with `with_active_tool_session(job.parent_session_id.as_deref(), || { ... })`.

To make `with_active_tool_session` available in `agent/mod.rs`, add it to the runtime import list in `rust/crates/tools/src/lib.rs` next to `active_tool_session_id`, and add it to the `use super::{ ... }` list at the top of `rust/crates/tools/src/agent/mod.rs`.

Replace the issue 11 proxy test with a direct session isolation test and remove the ignore:

```rust
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
```

- [ ] **Step 6: Add a background agent concurrency cap**

In `rust/crates/tools/src/agent/mod.rs`, add:

```rust
const DEFAULT_AGENT_CONCURRENCY_LIMIT: usize = 4;
const AGENT_CONCURRENCY_ENV_VAR: &str = "CLAWD_AGENT_MAX_CONCURRENCY";

fn agent_concurrency_limit() -> usize {
    std::env::var(AGENT_CONCURRENCY_ENV_VAR)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_AGENT_CONCURRENCY_LIMIT)
}

fn background_agent_slots() -> &'static std::sync::Mutex<usize> {
    static SLOTS: std::sync::OnceLock<std::sync::Mutex<usize>> = std::sync::OnceLock::new();
    SLOTS.get_or_init(|| std::sync::Mutex::new(0))
}

struct BackgroundAgentSlot;

impl BackgroundAgentSlot {
    fn acquire() -> Result<Self, String> {
        let limit = agent_concurrency_limit();
        let mut active = background_agent_slots()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *active >= limit {
            return Err(format!(
                "background agent concurrency limit reached: active={active} limit={limit}"
            ));
        }
        *active += 1;
        Ok(Self)
    }
}

impl Drop for BackgroundAgentSlot {
    fn drop(&mut self) {
        let mut active = background_agent_slots()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *active = active.saturating_sub(1);
    }
}
```

At the start of the spawned background thread closure, acquire a slot before `run_agent_job`:

```rust
let slot = match BackgroundAgentSlot::acquire() {
    Ok(slot) => slot,
    Err(error) => {
        let _ = persist_agent_terminal_state(
            &job.manifest,
            "failed",
            None,
            Some(error.clone()),
        );
        let _ = with_active_tool_session(job.parent_session_id.as_deref(), || {
            registry.append_output(&job.manifest.agent_id, &error).ok();
            registry.set_status(&job.manifest.agent_id, TaskStatus::Failed).ok();
        });
        enqueue_background_agent_notification(&job, "failed", &error);
        return;
    }
};
let _slot = slot;
```

Replace the issue 12 structural proxy test with a direct cap test and remove the ignore:

```rust
#[test]
fn confirms_issue_12_background_agent_execution_requires_concurrency_limit() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _limit = EnvVarGuard::set("CLAWD_AGENT_MAX_CONCURRENCY", "1");

    let first_slot = super::agent::BackgroundAgentSlot::acquire()
        .expect("first slot should be available");
    let second = super::agent::BackgroundAgentSlot::acquire();
    drop(first_slot);

    assert!(
        second.is_err(),
        "background agent work must be capped instead of allowing unbounded concurrent execution"
    );
}
```

If `BackgroundAgentSlot` remains private, keep the test in `tools/src/tests.rs` working by marking the struct and `acquire` as `pub(crate)`.

Before adding that test, extend the existing `EnvVarGuard` in `rust/crates/tools/src/tests.rs` with a string setter:

```rust
fn set(key: &'static str, value: &str) -> Self {
    let previous = std::env::var_os(key);
    std::env::set_var(key, value);
    Self { key, previous }
}
```

- [ ] **Step 7: Verify tools task 4**

Run:

```bash
cargo test -p tools confirms_issue_08
cargo test -p tools confirms_issue_09
cargo test -p tools confirms_issue_10
cargo test -p tools confirms_issue_11
cargo test -p tools confirms_issue_12
cargo test -p tools
cargo fmt -p tools --check
```

Expected: all commands pass.

- [ ] **Step 8: Commit task 4**

Run:

```bash
git add rust/crates/tools/src/agent/mod.rs rust/crates/tools/src/tests.rs rust/crates/runtime/src/task_registry.rs
git commit -m "fix(tools): scope and bound agent lifecycle state"
```

---

### Task 5: Tools File Dispatch Workspace Boundary

**Issues:** tools dispatch side of 2.

**Files:**

- Modify: `rust/crates/tools/src/lib.rs`
- Modify: `rust/crates/tools/src/dispatch.rs`
- Modify: `rust/crates/tools/src/tests.rs`

- [ ] **Step 1: Verify the existing confirmation failure**

Run:

```bash
cargo test -p tools confirms_issue_02 -- --ignored
```

Expected: fail because absolute paths outside the current workspace are accepted.

- [ ] **Step 2: Import workspace-scoped runtime helpers**

In `rust/crates/tools/src/lib.rs`, update the runtime import list by replacing:

```rust
enqueue_session_notification, execute_bash, glob_search, grep_search, load_system_prompt,
```

with:

```rust
enqueue_session_notification, execute_bash, glob_search_in_workspace,
grep_search_in_workspace, load_system_prompt,
```

and replacing:

```rust
read_file,
```

with:

```rust
read_file_in_workspace,
```

and replacing:

```rust
write_file, ApiClient,
```

with:

```rust
write_file_in_workspace, ApiClient,
```

Also add `edit_file_in_workspace` to the runtime import list near `dedupe_superseded_commit_events`.

- [ ] **Step 3: Route file tools through workspace-scoped helpers**

In `rust/crates/tools/src/dispatch.rs`, add:

```rust
fn active_workspace_root() -> Result<std::path::PathBuf, String> {
    std::env::current_dir().map_err(|error| error.to_string())
}
```

Replace the file runners:

```rust
fn run_read_file(input: ReadFileInput) -> Result<String, String> {
    let workspace = active_workspace_root()?;
    to_pretty_json(
        read_file_in_workspace(&input.path, input.offset, input.limit, &workspace)
            .map_err(io_to_string)?,
    )
}

fn run_write_file(input: WriteFileInput) -> Result<String, String> {
    let workspace = active_workspace_root()?;
    to_pretty_json(
        write_file_in_workspace(&input.path, &input.content, &workspace)
            .map_err(io_to_string)?,
    )
}

fn run_edit_file(input: EditFileInput) -> Result<String, String> {
    let workspace = active_workspace_root()?;
    to_pretty_json(
        edit_file_in_workspace(
            &input.path,
            &input.old_string,
            &input.new_string,
            input.replace_all.unwrap_or(false),
            &workspace,
        )
        .map_err(io_to_string)?,
    )
}

fn run_glob_search(input: GlobSearchInputValue) -> Result<String, String> {
    let workspace = active_workspace_root()?;
    to_pretty_json(
        glob_search_in_workspace(&input.pattern, input.path.as_deref(), &workspace)
            .map_err(io_to_string)?,
    )
}

fn run_grep_search(input: GrepSearchInput) -> Result<String, String> {
    let workspace = active_workspace_root()?;
    to_pretty_json(grep_search_in_workspace(&input, &workspace).map_err(io_to_string)?)
}
```

Remove the `#[ignore = "..."]` attribute from the issue 2 test.

- [ ] **Step 4: Verify tools task 5**

Run:

```bash
cargo test -p tools confirms_issue_02
cargo test -p tools
cargo fmt -p tools --check
```

Expected: all commands pass.

- [ ] **Step 5: Commit task 5**

Run:

```bash
git add rust/crates/tools/src/lib.rs rust/crates/tools/src/dispatch.rs rust/crates/tools/src/tests.rs
git commit -m "fix(tools): enforce workspace file dispatch"
```

---

### Task 6: CLI Provider Limits, Debug Coverage, And Parallel Agent Cap

**Issues:** 7, CLI side of 16, and CLI side of 12.

**Files:**

- Modify: `rust/crates/rusty-claude-cli/src/provider_client.rs`
- Modify: `rust/crates/rusty-claude-cli/src/tool_executor.rs`

- [ ] **Step 1: Verify the existing confirmation failures**

Run:

```bash
cargo test -p rusty-claude-cli confirms_issue_07 -- --ignored
cargo test -p rusty-claude-cli confirms_issue_16 -- --ignored
```

Expected: both commands fail for the known CLI issues.

- [ ] **Step 2: Replace the hardcoded CLI output-token budget**

In `rust/crates/rusty-claude-cli/src/provider_client.rs`, replace:

```rust
fn max_tokens_for_model(_model: &str) -> u32 {
    64_000
}
```

with:

```rust
fn max_tokens_for_model(model: &str) -> u32 {
    let canonical = api::resolve_model_alias(model);
    if matches!(
        canonical.as_str(),
        "claude-opus-4-6" | "claude-sonnet-4-6" | "claude-haiku-4-5-20251213"
    ) {
        return api::max_tokens_for_model(&canonical);
    }
    if canonical.starts_with("grok-") {
        return api::max_tokens_for_model(&canonical);
    }
    16_384
}
```

Remove the `#[ignore = "..."]` attribute from the issue 7 test.

- [ ] **Step 3: Promote CLI debug-log redaction confirmation**

After Task 1 has centralized redaction in `runtime::agent_debug_log`, remove the `#[ignore = "..."]` attribute from `confirms_issue_16_tool_debug_log_redacts_secret_shaped_values` in `rust/crates/rusty-claude-cli/src/tool_executor.rs`.

Keep the assertion:

```rust
assert!(
    !log.contains("sk-ant-secret-value"),
    "debug logs must redact secret-shaped values before writing to disk"
);
```

Add:

```rust
assert!(log.contains("[REDACTED_SECRET]"));
```

- [ ] **Step 4: Cap CLI parallel Agent thread batches**

In `rust/crates/rusty-claude-cli/src/tool_executor.rs`, add:

```rust
const DEFAULT_PARALLEL_AGENT_LIMIT: usize = 4;
const PARALLEL_AGENT_LIMIT_ENV_VAR: &str = "CLAWD_AGENT_MAX_CONCURRENCY";

fn parallel_agent_limit() -> usize {
    std::env::var(PARALLEL_AGENT_LIMIT_ENV_VAR)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_PARALLEL_AGENT_LIMIT)
}
```

Refactor the thread-spawn block in `execute_many` into a helper:

```rust
fn execute_parallel_chunk(
    invocations: &[ToolInvocation],
    allowed_tools: Option<AllowedToolSet>,
    tool_registry: GlobalToolRegistry,
    mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    session_id: Option<String>,
) -> Vec<Result<String, ToolError>> {
    let handles = invocations
        .iter()
        .map(|invocation| {
            let invocation = invocation.clone();
            let allowed_tools = allowed_tools.clone();
            let tool_registry = tool_registry.clone();
            let mcp_state = mcp_state.clone();
            let session_id = session_id.clone();
            std::thread::spawn(move || {
                let invocation_started_at = Instant::now();
                cli_agent_debug_log(
                    "tool.execute_many.worker.begin",
                    format!(
                        "tool_name={} description={}",
                        invocation.tool_name,
                        describe_parallel_invocation(&invocation)
                    ),
                );
                let worker = CliToolExecutor {
                    renderer: TerminalRenderer::new(),
                    emit_output: false,
                    allowed_tools,
                    tool_registry,
                    mcp_state,
                };
                let result = with_active_tool_session(session_id.as_deref(), || {
                    worker.execute_raw(&invocation.tool_name, &invocation.input)
                });
                cli_agent_debug_log(
                    "tool.execute_many.worker.done",
                    format!(
                        "tool_name={} description={} ok={} elapsed_us={}",
                        invocation.tool_name,
                        describe_parallel_invocation(&invocation),
                        result.is_ok(),
                        invocation_started_at.elapsed().as_micros()
                    ),
                );
                result
            })
        })
        .collect::<Vec<_>>();

    handles
        .into_iter()
        .map(|handle| match handle.join() {
            Ok(result) => result,
            Err(_) => Err(ToolError::new("tool execution thread panicked")),
        })
        .collect()
}
```

In `execute_many`, replace the single unbounded spawn block with chunked execution:

```rust
let parallel_limit = parallel_agent_limit();
let mut results = Vec::with_capacity(invocations.len());
for chunk in invocations.chunks(parallel_limit) {
    results.extend(execute_parallel_chunk(
        chunk,
        allowed_tools.clone(),
        tool_registry.clone(),
        mcp_state.clone(),
        session_id.clone(),
    ));
}
```

Add a unit test in the same test module:

```rust
#[test]
fn parallel_agent_limit_uses_env_override() {
    let _lock = env_lock();
    let previous = std::env::var_os("CLAWD_AGENT_MAX_CONCURRENCY");
    std::env::set_var("CLAWD_AGENT_MAX_CONCURRENCY", "2");
    assert_eq!(parallel_agent_limit(), 2);
    match previous {
        Some(value) => std::env::set_var("CLAWD_AGENT_MAX_CONCURRENCY", value),
        None => std::env::remove_var("CLAWD_AGENT_MAX_CONCURRENCY"),
    }
}
```

- [ ] **Step 5: Verify CLI task 6**

Run:

```bash
cargo test -p rusty-claude-cli confirms_issue_07
cargo test -p rusty-claude-cli confirms_issue_16
cargo test -p rusty-claude-cli parallel_agent_limit_uses_env_override
cargo test -p rusty-claude-cli
cargo fmt -p rusty-claude-cli --check
```

Expected: all commands pass.

- [ ] **Step 6: Commit task 6**

Run:

```bash
git add rust/crates/rusty-claude-cli/src/provider_client.rs rust/crates/rusty-claude-cli/src/tool_executor.rs
git commit -m "fix(cli): bound provider and agent execution defaults"
```

---

### Task 7: Integration Verification And Final Review

**Issues:** all 17.

**Files:**

- Review only unless a previous task left an integration break.

- [ ] **Step 1: Confirm no ignored confirmation tests remain**

Run:

```bash
rg -n "#\\[ignore = .*known issue confirmation|confirms_issue_.*-- --ignored|known issue confirmation" crates
```

Expected: no remaining ignored known-issue confirmation tests for the 17 fixed bugs.

- [ ] **Step 2: Run confirmation filters**

Run:

```bash
cargo test -p runtime confirms_issue
cargo test -p api confirms_issue
cargo test -p tools confirms_issue
cargo test -p rusty-claude-cli confirms_issue
```

Expected: all confirmation filters pass.

- [ ] **Step 3: Run default crate tests**

Run:

```bash
cargo test -p runtime
cargo test -p api
cargo test -p tools
cargo test -p rusty-claude-cli
```

Expected: all default tests pass.

- [ ] **Step 4: Run formatting and clippy for touched crates**

Run:

```bash
cargo fmt -p runtime --check
cargo fmt -p api --check
cargo fmt -p tools --check
cargo fmt -p rusty-claude-cli --check
cargo clippy -p runtime --tests --no-deps -- -D warnings
cargo clippy -p api --tests --no-deps -- -D warnings
cargo clippy -p tools --tests --no-deps -- -D warnings
cargo clippy -p rusty-claude-cli --tests --no-deps -- -D warnings
```

Expected: all commands pass.

- [ ] **Step 5: Run final code review**

Dispatch a final reviewer with this scope:

```text
Review the combined Rust changes for the 17 confirmed bugs in docs/rust-risk-test-confirmation.md and the design in docs/superpowers/specs/2026-05-08-rust-risk-fix-design.md. Focus on security boundary regressions, provider protocol correctness, task/session leakage, concurrency caps, debug-log redaction, and whether the promoted regression tests still assert the intended behavior. Report findings with file/line references. Do not modify files.
```

Expected: no blocking findings. If there are findings, fix them in the owning task area, re-run the relevant tests, and commit the fix.

- [ ] **Step 6: Commit final integration fixes if any**

If Step 5 required follow-up edits, run:

```bash
git add rust/crates
git commit -m "fix: integrate rust risk repairs"
```

If Step 5 required no edits, do not create an empty commit.
