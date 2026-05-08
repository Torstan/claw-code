# Rust Risk Test Confirmation Report

## Summary

This report confirms 17 Rust implementation risks with ignored regression tests. The tests assert desired behavior and are expected to fail against the current implementation when run with `-- --ignored`.

All Cargo commands below were run from `rust/`, because this repository's Cargo workspace lives under `/mnt/d/ginobili/code/claw-code/rust`.

## Confirmation Matrix

| # | Issue | Crate | Test | Status | Command |
|---:|---|---|---|---|---|
| 1 | Prompt mode bypasses approval | runtime | `confirms_issue_01_prompt_mode_requires_prompter_for_dangerous_tools` / `confirms_issue_01_prompt_mode_denies_without_prompter` | confirmed_by_failing_test | `cargo test -p runtime confirms_issue -- --ignored` |
| 2 | File tools bypass workspace boundary | tools | `confirms_issue_02_file_tool_dispatch_rejects_outside_absolute_paths` | confirmed_by_failing_test | `cargo test -p tools confirms_issue -- --ignored` |
| 3 | Filesystem sandbox reports active without isolation | runtime | `confirms_issue_03_filesystem_sandbox_requires_enforced_mount_boundary` | confirmed_by_failing_test | `cargo test -p runtime confirms_issue -- --ignored` |
| 4 | Local OpenAI-compatible endpoints require API key | api | `confirms_issue_04_openai_base_url_without_key_builds_unauthenticated_client` | confirmed_by_failing_test | `cargo test -p api confirms_issue -- --ignored` |
| 5 | OpenAI tool result wire format is invalid | api | `confirms_issue_05_tool_result_wire_payload_omits_is_error` / `confirms_issue_05_orphan_tool_messages_are_dropped` | confirmed_by_failing_test | `cargo test -p api confirms_issue -- --ignored` |
| 6 | Non-streaming OpenAI `tool_calls: null` fails | api | `confirms_issue_06_non_streaming_tool_calls_null_deserializes_as_empty` | confirmed_by_failing_test | `cargo test -p api confirms_issue -- --ignored` |
| 7 | CLI always requests 64k output tokens | rusty-claude-cli | `confirms_issue_07_cli_uses_model_specific_max_tokens` | confirmed_by_failing_test | `cargo test -p rusty-claude-cli confirms_issue -- --ignored` |
| 8 | Subagent default model ignores parent provider | tools | `confirms_issue_08_subagent_default_model_is_not_hardcoded_anthropic` | confirmed_by_failing_test | `cargo test -p tools confirms_issue -- --ignored` |
| 9 | Subagent sessions cannot externalize large tool results | tools | `confirms_issue_09_subagent_session_has_persistence_for_tool_result_budgeting` | confirmed_by_failing_test | `cargo test -p tools confirms_issue -- --ignored` |
| 10 | Background agent notifications inject unbounded results | tools | `confirms_issue_10_background_agent_notification_is_bounded` | confirmed_by_failing_test | `cargo test -p tools confirms_issue -- --ignored` |
| 11 | Task registry is process-wide and non-persistent | tools/runtime | `confirms_issue_11_task_registry_requires_session_scope` | confirmed_by_structural_test | `cargo test -p tools confirms_issue -- --ignored` |
| 12 | Agent execution has no concurrency limit | tools | `confirms_issue_12_background_agent_execution_requires_concurrency_limit` | confirmed_by_structural_test | `cargo test -p tools confirms_issue -- --ignored` |
| 13 | `edit_file` does not require unique match | runtime | `confirms_issue_13_edit_file_requires_unique_match_when_not_replace_all` | confirmed_by_failing_test | `cargo test -p runtime confirms_issue -- --ignored` |
| 14 | Patch output is full-file replacement | runtime | `confirms_issue_14_structured_patch_is_localized` | confirmed_by_failing_test | `cargo test -p runtime confirms_issue -- --ignored` |
| 15 | Multiline grep does not match across lines | runtime | `confirms_issue_15_multiline_grep_matches_across_lines_in_content_mode` | confirmed_by_failing_test | `cargo test -p runtime confirms_issue -- --ignored` |
| 16 | Debug logging writes full inputs and outputs | rusty-claude-cli/runtime | `confirms_issue_16_tool_debug_log_redacts_secret_shaped_values` | confirmed_by_failing_test | `cargo test -p rusty-claude-cli confirms_issue -- --ignored` |
| 17 | Provider retry/fallback can stall for minutes | api | `confirms_issue_17_default_retry_budget_exceeds_fast_fallback_window` | confirmed_by_structural_test | `cargo test -p api confirms_issue -- --ignored` |

## Ignored Test Evidence

- `cargo test -p runtime confirms_issue -- --ignored`: failed as expected, `0 passed; 6 failed`.
- `cargo test -p api confirms_issue -- --ignored`: failed as expected, `0 passed; 5 failed`.
- `cargo test -p tools confirms_issue -- --ignored`: failed as expected, `0 passed; 6 failed`.
- `cargo test -p rusty-claude-cli confirms_issue -- --ignored`: failed as expected, `0 passed; 2 failed`.

## Default Test Health

- `cargo test -p runtime`: passed, unit tests `481 passed; 6 ignored`, integration tests `12 passed`.
- `cargo test -p api`: passed, unit tests `122 passed; 5 ignored`, integration tests passed.
- `cargo test -p tools`: passed, unit tests `114 passed; 6 ignored`, public API tests `3 passed`.
- `cargo test -p rusty-claude-cli`: passed, unit tests `181 passed; 2 ignored`, integration tests passed.

Additional checks:

- `cargo fmt -p tools --check`: passed.
- `cargo clippy -p tools --tests --no-deps -- -D warnings`: passed.
- CLI code-quality reviewer verified `cargo clippy -p rusty-claude-cli --tests -- -D warnings`: passed.

## Detailed Findings

### Issue 1: Prompt Mode Bypasses Approval

**Test evidence:** `cargo test -p runtime confirms_issue -- --ignored` fails because `confirms_issue_01_prompt_mode_requires_prompter_for_dangerous_tools` records zero prompt calls, and `confirms_issue_01_prompt_mode_denies_without_prompter` receives an allow outcome instead of a denial.

**Root cause:** `rust/crates/runtime/src/permissions.rs:175` checks authorization using ordered permission modes before prompt handling. `Prompt` currently satisfies the comparison against `DangerFullAccess`, so the prompt branch is bypassed.

**Impact:** Interactive prompt mode cannot be trusted to gate dangerous tools.

**Suggested fix:** Remove prompt mode from automatic capability ordering. Handle `Prompt` before rank comparisons, or replace derived ordering with an explicit capability-rank function.

### Issue 2: File Tools Bypass Workspace Boundary

**Test evidence:** `cargo test -p tools confirms_issue -- --ignored` fails because `read_file`, `write_file`, `edit_file`, `glob_search`, and `grep_search` all accept absolute outside paths.

**Root cause:** `rust/crates/tools/src/dispatch.rs` routes file tools directly to runtime helpers that accept normalized absolute paths. The workspace-aware helpers in `rust/crates/runtime/src/file_ops.rs` are not used at the dispatch boundary.

**Impact:** A workspace-scoped tool policy can be bypassed by passing absolute paths.

**Suggested fix:** Thread the active workspace root into file-tool dispatch and call the workspace-scoped runtime helpers for read/write/edit/search operations.

### Issue 3: Filesystem Sandbox Reports Active Without Enforcing Isolation

**Test evidence:** `cargo test -p runtime confirms_issue -- --ignored` fails because `filesystem_active` is true for workspace-only mode even though no mount boundary is enforced.

**Root cause:** `rust/crates/runtime/src/sandbox.rs:166` sets `filesystem_active` from the requested filesystem mode. `build_linux_sandbox_command` at `rust/crates/runtime/src/sandbox.rs:211` only sets environment variables and namespace flags; it does not bind, chroot, remount, or otherwise enforce filesystem isolation.

**Impact:** Status reporting can overstate sandbox protection and mislead callers into trusting a filesystem boundary that does not exist.

**Suggested fix:** Either implement real filesystem isolation for workspace/allow-list modes or report filesystem isolation as inactive when only advisory environment variables are set.

### Issue 4: Local OpenAI-Compatible Endpoints Require API Key

**Test evidence:** `cargo test -p api confirms_issue -- --ignored` fails because `OpenAiCompatClient::from_env(OpenAiCompatConfig::openai())` rejects `OPENAI_BASE_URL=http://127.0.0.1:11434/v1` when `OPENAI_API_KEY` is unset.

**Root cause:** `OpenAiCompatClient::from_env` requires a provider key env var before building a client, even for local unauthenticated OpenAI-compatible endpoints.

**Impact:** Users cannot configure local OpenAI-compatible servers, such as local model gateways, without providing a dummy API key.

**Suggested fix:** Treat local loopback/base-url overrides as optionally unauthenticated and omit `Authorization` when no key is configured.

### Issue 5: OpenAI Tool Result Wire Format Is Invalid

**Test evidence:** `cargo test -p api confirms_issue -- --ignored` fails because serialized OpenAI tool messages contain `is_error`, and orphan tool messages are preserved after a user turn.

**Root cause:** `translate_message` in `rust/crates/api/src/providers/openai_compat.rs:881` serializes Anthropic-only `is_error` into OpenAI `role:"tool"` messages. `sanitize_tool_message_pairing` in `rust/crates/api/src/providers/openai_compat.rs:952` only drops orphan tool messages after an assistant turn, so orphans after user/system turns survive.

**Impact:** OpenAI-compatible backends can reject requests with invalid tool-result payloads or orphaned tool messages.

**Suggested fix:** Strip Anthropic-only fields from OpenAI payloads and enforce tool-result pairing regardless of whether the preceding non-tool message is assistant, user, or system.

### Issue 6: Non-Streaming OpenAI `tool_calls: null` Fails Deserialization

**Test evidence:** `cargo test -p api confirms_issue -- --ignored` fails because a non-streaming response with `"tool_calls": null` does not deserialize.

**Root cause:** `ChatMessage.tool_calls` in `rust/crates/api/src/providers/openai_compat.rs:671` uses `#[serde(default)]`, which handles missing fields but not explicit `null`. The streaming delta path already has a null-as-empty deserializer.

**Impact:** Valid OpenAI-compatible responses from providers that emit `tool_calls: null` can fail after the model has responded successfully.

**Suggested fix:** Reuse the null-as-empty vector deserializer on the non-streaming `ChatMessage.tool_calls` field.

### Issue 7: CLI Always Requests 64k Output Tokens

**Test evidence:** `cargo test -p rusty-claude-cli confirms_issue -- --ignored` fails because `max_tokens_for_model("openai/gpt-4o-mini")` returns `64000`.

**Root cause:** `rust/crates/rusty-claude-cli/src/provider_client.rs:27` ignores the `model` argument and returns `64_000` for every provider/model.

**Impact:** OpenAI-compatible or small local models can receive unsupported output-token requests, causing avoidable provider errors.

**Suggested fix:** Reuse the API crate's model-aware max-token logic or add conservative provider/model-specific limits in the CLI path.

### Issue 8: Subagent Default Model Ignores Parent Provider

**Test evidence:** `cargo test -p tools confirms_issue -- --ignored` fails because `resolve_agent_model(None)` returns `claude-opus-4-6`.

**Root cause:** `DEFAULT_AGENT_MODEL` is hardcoded to `claude-opus-4-6` in `rust/crates/tools/src/agent/mod.rs:21`, and `resolve_agent_model` falls back to that constant.

**Impact:** Subagents can unexpectedly route to Anthropic even when the parent session is using another provider.

**Suggested fix:** Inherit the parent/provider model when no explicit subagent model is supplied, or make the default configurable through runtime settings.

### Issue 9: Subagent Sessions Cannot Externalize Large Tool Results

**Test evidence:** `cargo test -p tools confirms_issue -- --ignored` fails because `new_agent_session("agent-issue-09").persistence_path()` is `None`.

**Root cause:** `new_agent_session` in `rust/crates/tools/src/agent/mod.rs:451` creates `Session::new()` and only sets `session_id`; it does not call `with_persistence_path`.

**Impact:** Runtime tool-result budgeting cannot persist large subagent tool outputs, so large outputs remain inline.

**Suggested fix:** Create subagent sessions with a persistence path in the agent store so large tool results can be externalized.

### Issue 10: Background Agent Notifications Inject Unbounded Results

**Test evidence:** `cargo test -p tools confirms_issue -- --ignored` fails because a 128 KiB background agent body is injected into the parent-session notification.

**Root cause:** `enqueue_background_agent_notification` in `rust/crates/tools/src/agent/mod.rs:331` appends the full result body to the notification string without size bounds, summarization, or persisted-output handoff.

**Impact:** Large background agent results can bloat parent-session context and degrade or break subsequent model requests.

**Suggested fix:** Bound notification size and put full results in the agent output file or persisted tool-result storage, with only a summary/reference injected into the parent session.

### Issue 11: Task Registry Is Process-Wide And Non-Persistent

**Test evidence:** `cargo test -p tools confirms_issue -- --ignored` fails because a task created in the global registry can be read back through process-global lookup without a session namespace.

**Root cause:** `global_task_registry` in `rust/crates/tools/src/registries.rs:32` returns a single process-wide `TaskRegistry`; the registry keys tasks only by task id.

**Impact:** Task ids are not scoped to a session or persisted storage boundary, so tasks can collide, leak across sessions, and disappear on process restart.

**Suggested fix:** Add a session/workspace namespace to task registry operations and persist task state if tasks must survive process boundaries.

### Issue 12: Agent Execution Has No Concurrency Limit

**Test evidence:** `cargo test -p tools confirms_issue -- --ignored` fails because `execute_agent_with_mode` invokes the background spawn closure immediately; the spawned count is `1` instead of entering a bounded queue.

**Root cause:** `execute_agent_with_spawn` in `rust/crates/tools/src/agent/mod.rs:74` calls the provided spawn function directly for background work. The CLI parallel executor also spawns one OS thread per parallel Agent invocation.

**Impact:** A large batch of agent calls can create unbounded concurrent work and OS threads.

**Suggested fix:** Introduce a bounded agent work queue or semaphore and apply a configurable concurrency cap to background and parallel Agent execution.

### Issue 13: `edit_file` Does Not Require Unique Match

**Test evidence:** `cargo test -p runtime confirms_issue -- --ignored` fails because editing a file with duplicate `old_string` values succeeds when `replace_all=false`.

**Root cause:** `edit_file` in `rust/crates/runtime/src/file_ops.rs:258` uses `replacen(old_string, new_string, 1)` for non-replace-all edits.

**Impact:** Ambiguous edits can silently modify the first occurrence instead of forcing the caller to provide a unique context.

**Suggested fix:** Count matches before editing and return an error when `replace_all=false` and the match count is not exactly one.

### Issue 14: Patch Output Is Full-File Replacement

**Test evidence:** `cargo test -p runtime confirms_issue -- --ignored` fails because a one-line edit emits too many added/removed structured patch lines.

**Root cause:** `make_patch` in `rust/crates/runtime/src/file_ops.rs:518` emits every original line as removed and every updated line as added instead of computing localized hunks.

**Impact:** Tool results for small edits are noisy and can consume unnecessary context.

**Suggested fix:** Generate a real line diff with localized hunks, or use an existing diff crate/helper already accepted in the workspace.

### Issue 15: Multiline Grep Does Not Match Across Lines In Content Mode

**Test evidence:** `cargo test -p runtime confirms_issue -- --ignored` fails because content mode does not return a match for `first\nsecond`.

**Root cause:** `grep_search` in `rust/crates/runtime/src/file_ops.rs:351` builds a regex with `dot_matches_new_line` when multiline is true, but content mode still evaluates `regex.is_match(line)` one line at a time.

**Impact:** Multiline search advertises cross-line matching but misses cross-line content-mode matches.

**Suggested fix:** When multiline is enabled, match against the full file content and map match ranges back to output lines/context.

### Issue 16: Debug Logging Writes Full Inputs And Outputs

**Test evidence:** `cargo test -p rusty-claude-cli confirms_issue -- --ignored` fails because the debug log contains `sk-ant-secret-value`.

**Root cause:** `agent_debug_log` in `rust/crates/runtime/src/agent_debug.rs:59` writes detail lines verbatim, and CLI/tool call sites pass normalized inputs and full outputs into debug logging.

**Impact:** Secret-shaped tool inputs/outputs can be written to disk when debug logging is enabled.

**Suggested fix:** Redact secret-shaped values and bound large details before writing debug logs. Apply redaction at the shared runtime debug logger or all call sites.

### Issue 17: Provider Retry/Fallback Can Stall For Minutes

**Test evidence:** `cargo test -p api confirms_issue -- --ignored` fails because the first three Anthropic default backoffs already exceed a 3-second fast-fallback window.

**Root cause:** `backoff_for_attempt` in `rust/crates/api/src/providers/anthropic.rs:846` uses exponential retry delays through the provider's retry budget. Provider fallback is only considered after the primary provider's stream call returns a retryable error.

**Impact:** Fallback providers may not be tried until after a long primary retry budget is exhausted.

**Suggested fix:** Add a fast primary cutoff or shared fallback policy that limits primary retry time before trying configured fallback providers.

## Suggested Fix Priority

1. Permission and workspace isolation issues: 1, 2, 3, 16.
2. Provider protocol correctness issues: 4, 5, 6, 7, 17.
3. Agent/task lifecycle and resource-control issues: 8, 9, 10, 11, 12.
4. Editing/search correctness issues: 13, 14, 15.

## Implementation Notes

- The implementation intentionally does not fix production behavior. The expected confirmation result is failing ignored tests plus passing default tests.
- Commit `847d5ac` contains both API and tools tests due to parallel-worker commit interleaving. A follow-up commit `a30f866` hardens the tools ignored tests by cleaning global task-registry state before the intentional failure and removing a clippy warning.
