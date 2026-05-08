# Rust Risk Test Confirmation Design

## Goal

Confirm the 17 Rust implementation risks identified in code review by adding targeted Rust regression tests, analyzing root causes, and producing an evidence-backed investigation document.

This work does not fix the bugs. It creates reproducible proof, maps each failure to code paths, and prepares the repository for prioritized fixes.

## Scope

In scope:

- Rust workspace only: `rust/crates/runtime`, `rust/crates/tools`, `rust/crates/api`, and `rust/crates/rusty-claude-cli`.
- The 17 issues listed in the review response.
- New Rust tests that demonstrate expected correct behavior and currently fail when explicitly run.
- A final investigation document with test commands, observed failures, root cause analysis, impact, and suggested fixes.

Out of scope:

- Python code and scripts.
- Fixing production implementation logic.
- Broad refactors unrelated to making tests possible.
- Live network calls to real providers.
- Destructive filesystem or shell operations outside temporary directories.

## Testing Strategy

Use ignored failing regression tests as the default mechanism.

Each test should assert the desired correct behavior. Because the current implementation is buggy, the test should fail when run with `-- --ignored`. Marking these tests ignored keeps the default workspace test suite usable while still preserving direct proof for each issue.

Naming convention:

- `confirms_issue_01_prompt_mode_requires_approval`
- `confirms_issue_02_file_tools_enforce_workspace_boundary`
- Continue the `confirms_issue_XX_*` pattern for all 17 issues.

Execution convention:

```bash
cargo test -p <crate> confirms_issue_XX -- --ignored
```

For issues where a single crate cannot safely reproduce the full behavior, use the narrowest safe structural test. For example, sandbox tests should validate that current status/build behavior does not enforce filesystem isolation rather than running a destructive command.

## Issue Coverage Plan

### 1. Prompt Mode Bypasses Approval

Add a runtime permission test that sets active mode to `Prompt`, registers a dangerous tool requirement, and asserts that authorization calls the prompter or denies without one. Current behavior allows before reaching the prompt branch because `Prompt` sorts above `DangerFullAccess`.

Expected location: `rust/crates/runtime/src/permissions.rs`.

### 2. File Tools Bypass Workspace Boundary

Add tools/runtime tests using temporary workspace and outside files. Assert `read_file`, `write_file`, `edit_file`, `glob_search`, and `grep_search` should reject absolute outside paths when called through the tool execution surface under workspace-scoped expectations. Current dispatch calls unsafe path functions directly.

Expected locations: `rust/crates/tools/src/tests.rs`, `rust/crates/runtime/src/file_ops.rs`.

### 3. Filesystem Sandbox Reports Active Without Enforcing Isolation

Add a sandbox structural test proving that `filesystem_active` can be true while `build_linux_sandbox_command` only sets environment variables and does not add mount/chroot/bind arguments. Avoid destructive execution. The expected correct behavior is either real filesystem isolation or a non-active status when isolation is not enforced.

Expected location: `rust/crates/runtime/src/sandbox.rs`.

### 4. OpenAI-Compatible Local Endpoints Require API Key

Add an API test with `OPENAI_BASE_URL` set and no `OPENAI_API_KEY`, expecting provider creation to succeed for local unauthenticated endpoints and to omit `Authorization`. Current `OpenAiCompatClient::from_env` rejects missing credentials and `send_raw_request` always attaches bearer auth.

Expected locations: `rust/crates/api/src/providers/openai_compat.rs`, `rust/crates/api/src/client.rs`.

### 5. OpenAI Tool Result Wire Format Is Invalid

Add a request-building test for tool results that asserts the Chat Completions payload does not include `is_error`, and orphaned tool messages are dropped unless paired with an immediately preceding assistant `tool_calls` entry. Current payload includes `is_error` and sanitizer preserves some orphaned tool messages.

Expected location: `rust/crates/api/src/providers/openai_compat.rs`.

### 6. Non-Streaming OpenAI `tool_calls: null` Fails Deserialization

Add a non-streaming response normalization test with `"tool_calls": null`, expecting it to deserialize as an empty vector. Current non-streaming `ChatMessage` only uses `#[serde(default)]`; the streaming path already has a null-as-empty deserializer.

Expected location: `rust/crates/api/src/providers/openai_compat.rs`.

### 7. CLI Always Requests 64k Output Tokens

Add a CLI/provider-client test proving model-specific limits should be used for non-Anthropic and small-context models. Current `max_tokens_for_model(_model)` ignores the model and always returns `64_000`.

Expected location: `rust/crates/rusty-claude-cli/src/provider_client.rs`.

### 8. Subagent Default Model Ignores Parent Provider

Add a tools test that constructs an agent input without an explicit model under a non-Anthropic parent-model expectation and asserts the default should inherit parent/provider context or be configurable. Current `DEFAULT_AGENT_MODEL` hardcodes `claude-opus-4-6`.

Expected location: `rust/crates/tools/src/agent/mod.rs`.

### 9. Subagent Sessions Cannot Externalize Large Tool Results

Add a tools/runtime test that creates a subagent session and large tool output, expecting large output externalization to occur. Current `new_agent_session` has no persistence path, so `stabilize_tool_result_output` returns the original output.

Expected locations: `rust/crates/tools/src/agent/mod.rs`, `rust/crates/runtime/src/tool_result_budget.rs`.

### 10. Background Agent Notifications Inject Unbounded Results

Add a tools/runtime test that enqueues a large background agent result and asserts the injected parent-session notification should be summarized or externalized. Current notification includes full body text and `ConversationRuntime` pushes it into session as a user message.

Expected locations: `rust/crates/tools/src/agent/mod.rs`, `rust/crates/runtime/src/conversation.rs`.

### 11. Task Registry Is Process-Wide And Non-Persistent

Add task registry tests showing tasks are not scoped by session and disappear outside process memory. The test should assert desired isolation by session ID or registry namespace. Current global registry is a single `OnceLock<TaskRegistry>` with no session dimension.

Expected locations: `rust/crates/tools/src/registries.rs`, `rust/crates/runtime/src/task_registry.rs`.

### 12. Agent Execution Has No Concurrency Limit

Add structural tests around `execute_many` and background agent spawning that expect a configurable concurrency cap or bounded queue. Current implementation spawns one OS thread per Agent invocation and one OS thread per background agent.

Expected locations: `rust/crates/rusty-claude-cli/src/tool_executor.rs`, `rust/crates/tools/src/agent/mod.rs`.

### 13. `edit_file` Does Not Require Unique Match

Add a runtime file-ops test where `old_string` appears twice and `replace_all=false`. Expected correct behavior is an error requiring a more specific match. Current behavior silently replaces the first occurrence.

Expected location: `rust/crates/runtime/src/file_ops.rs`.

### 14. Patch Output Is Full-File Replacement, Not A Real Diff

Add a file-ops test changing one line in a multi-line file and asserting the structured patch should contain a localized hunk. Current `make_patch` emits every original line as removed and every updated line as added.

Expected location: `rust/crates/runtime/src/file_ops.rs`.

### 15. Multiline Grep Does Not Match Across Lines In Content Mode

Add a grep test with `multiline=true`, `output_mode="content"`, and a pattern spanning newline characters. Expected behavior is a match. Current content mode still evaluates the regex line by line.

Expected location: `rust/crates/runtime/src/file_ops.rs`.

### 16. Debug Logging Writes Full Tool Inputs And Outputs

Add a CLI tool executor or runtime debug-log test that executes a tool with secret-shaped input/output under `CLAWD_AGENT_DEBUG`, expecting redaction or bounded truncation. Current debug log records full normalized input and output.

Expected locations: `rust/crates/rusty-claude-cli/src/tool_executor.rs`, `rust/crates/runtime/src/agent_debug.rs`.

### 17. Provider Retry/Fallback Can Stall For Minutes

Add API/tools tests with retry policies and fake retryable failures, expecting fallback or failure within a bounded configured time. Current Anthropic retry policy allows up to 8 retries with exponential backoff, and subagent provider fallback waits for a provider's internal retry exhaustion before trying the next provider.

Expected locations: `rust/crates/api/src/providers/anthropic.rs`, `rust/crates/tools/src/agent/mod.rs`.

## Final Investigation Document

After tests are added and run, write a separate investigation report. Recommended path:

`docs/rust-risk-test-confirmation.md`

The report should include:

- Summary table for all 17 issues.
- Test name and command for each issue.
- Observed current result, including failing assertion or command output summary.
- Root cause with file and line references.
- Impact assessment.
- Suggested fix direction.
- Suggested fix priority.

The report should distinguish three statuses:

- `confirmed_by_failing_test`: ignored test fails against current implementation.
- `confirmed_by_structural_test`: test safely proves an invariant mismatch without dangerous execution.
- `not_confirmed`: only allowed if a proposed test cannot be written safely; explain why.

The target is to avoid `not_confirmed` unless there is a hard safety or tooling blocker.

## Verification Commands

Use focused commands during implementation:

```bash
cargo test -p runtime confirms_issue -- --ignored
cargo test -p tools confirms_issue -- --ignored
cargo test -p api confirms_issue -- --ignored
cargo test -p rusty-claude-cli confirms_issue -- --ignored
```

Also run existing focused tests for any crate touched by test scaffolding:

```bash
cargo test -p runtime
cargo test -p tools
cargo test -p api
cargo test -p rusty-claude-cli
```

If the full crate tests are too slow, record the exact focused commands that were run and why broader tests were deferred.

## Implementation Boundaries

Tests may expose private helpers by placing regression tests in the same module where needed. If a helper must be made `pub(crate)` only for testing, keep the visibility change minimal and document it in the investigation report.

Do not change runtime behavior to make tests pass. The expected state after this work is:

- Default non-ignored tests still pass, unless an unrelated existing failure is discovered and documented.
- Issue confirmation tests fail only when explicitly run with `-- --ignored`.
- The investigation report is complete enough to drive a later fix plan.

## Risks

- Some issues span multiple crates, so tests may need small test-only adapters.
- Several production files are large. Keep tests close to the smallest module that owns the behavior.
- Tests that involve shell, filesystem, or provider behavior must use temporary directories and fake/local servers only.
- Ignored failing tests should be clearly named so they are not mistaken for flaky tests.

## Approval Gate

Once this specification is reviewed and approved, the next step is to create an implementation plan using the writing-plans workflow. Implementation should not start until that plan is approved.
