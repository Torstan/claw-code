# Rusty Claude CLI Main Split Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split `rust/crates/rusty-claude-cli/src/main.rs` into focused private modules while preserving the `claw` binary behavior exactly.

**Architecture:** Keep `main.rs` as the binary entrypoint and dispatch layer. Move existing code mechanically into private sibling modules under `rust/crates/rusty-claude-cli/src/`, using `pub(crate)` only where cross-module access requires it. Do not rename core private symbols such as `AnthropicRuntimeClient` or `STUB_COMMANDS`, and do not add `src/lib.rs`.

**Tech Stack:** Rust workspace, Cargo, `rusty-claude-cli` binary crate, `api`, `runtime`, `commands`, `plugins`, `tools`, `compat-harness`, `serde_json`, `tokio`, `rustyline`, `crossterm`.

---

## File Structure

Create these files under `rust/crates/rusty-claude-cli/src/`:

- `args.rs`: `CliAction`, `CliOutputFormat`, `LocalHelpTopic`, `parse_args`, and CLI parsing helpers.
- `help.rs`: `print_help_to`, `print_help`, `render_repl_help`, `STUB_COMMANDS`, and slash-command completion candidates.
- `sessions.rs`: `SessionHandle`, `ManagedSessionSummary`, session directory/reference/list/delete/backup helpers.
- `status.rs`: `StatusContext`, `StatusUsage`, `GitWorkspaceSummary`, status/sandbox/config/memory/diff/git renderers and helpers.
- `doctor.rs`: `DiagnosticLevel`, `DiagnosticCheck`, `DoctorReport`, doctor checks, `run_doctor`.
- `auth.rs`: `default_oauth_config`, login/logout, OAuth callback/browser helpers, CLI auth source resolution.
- `mcp_runtime.rs`: `RuntimeMcpState`, MCP runtime tool request DTOs, wrapper tool definitions, MCP permission helpers.
- `runtime_host.rs`: `RuntimePluginState`, `RuntimePluginStateBuildOutput`, `BuiltRuntime`, runtime/plugin construction, hook progress, permission prompting, internal prompt progress.
- `tool_display.rs`: tool call/result formatting, truncation, stream-event debug formatting, response-to-event helpers, prompt-cache event helper.
- `tool_executor.rs`: `CliToolExecutor`, tool execution, parallel execution, `permission_policy`.
- `provider_client.rs`: `AnthropicRuntimeClient`, provider streaming, auth-source use, message conversion, API error formatting.
- `resume.rs`: `resume_session`, `ResumeCommandOutcome`, `run_resume_command`.
- `repl.rs`: `run_repl`, `LiveCli`, `HookAbortMonitor`, REPL command handling.

Modify:

- `rust/crates/rusty-claude-cli/src/main.rs`: keep module declarations, imports used by `main()`/`run()`, `main()`, `read_piped_stdin()`, `merge_prompt_with_stdin()`, `run()`, and any tiny glue that directly serves dispatch.

Keep unchanged:

- `rust/crates/rusty-claude-cli/src/input.rs`
- `rust/crates/rusty-claude-cli/src/init.rs`
- `rust/crates/rusty-claude-cli/src/render.rs`
- `rust/crates/rusty-claude-cli/Cargo.toml`

Ownership for parallel implementation:

- Worker A owns `args.rs`, `help.rs`, `sessions.rs`, `status.rs`, `doctor.rs`, and related removals from `main.rs`.
- Worker B owns `auth.rs`, `mcp_runtime.rs`, `runtime_host.rs`, `provider_client.rs`, `tool_display.rs`, `tool_executor.rs`, `resume.rs`, `repl.rs`, and related removals from `main.rs`.
- A verification worker may run read-only checks after patches land.
- Workers are not alone in the codebase. Do not revert edits made by others; adjust imports and visibility to accommodate them.

## Task 1: Prepare Baseline And Module Shells

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/main.rs`
- Create: all new module files listed above

- [ ] **Step 1: Confirm baseline status for target files**

Run:

```bash
git status --short rust/crates/rusty-claude-cli/src/main.rs rust/crates/rusty-claude-cli/src
```

Expected: no unrelated staged changes in `rust/crates/rusty-claude-cli/src/**`. If target files are already modified, inspect them and preserve the existing edits.

- [ ] **Step 2: Add module declarations to `main.rs`**

At the top of `rust/crates/rusty-claude-cli/src/main.rs`, keep existing modules and add the new private modules:

```rust
mod args;
mod auth;
mod doctor;
mod help;
mod init;
mod input;
mod mcp_runtime;
mod provider_client;
mod render;
mod repl;
mod resume;
mod runtime_host;
mod sessions;
mod status;
mod tool_display;
mod tool_executor;
```

- [ ] **Step 3: Create empty module files**

Create these files with a single temporary module comment so Cargo can resolve declarations:

```rust
// Module populated during the main.rs split.
```

Files:

```text
rust/crates/rusty-claude-cli/src/args.rs
rust/crates/rusty-claude-cli/src/auth.rs
rust/crates/rusty-claude-cli/src/doctor.rs
rust/crates/rusty-claude-cli/src/help.rs
rust/crates/rusty-claude-cli/src/mcp_runtime.rs
rust/crates/rusty-claude-cli/src/provider_client.rs
rust/crates/rusty-claude-cli/src/repl.rs
rust/crates/rusty-claude-cli/src/resume.rs
rust/crates/rusty-claude-cli/src/runtime_host.rs
rust/crates/rusty-claude-cli/src/sessions.rs
rust/crates/rusty-claude-cli/src/status.rs
rust/crates/rusty-claude-cli/src/tool_display.rs
rust/crates/rusty-claude-cli/src/tool_executor.rs
```

- [ ] **Step 4: Run baseline check**

Run from `rust/`:

```bash
cargo check -p rusty-claude-cli
```

Expected: PASS. Empty modules should not change behavior.

- [ ] **Step 5: Commit module shells**

Run from the repository root:

```bash
git add rust/crates/rusty-claude-cli/src/main.rs rust/crates/rusty-claude-cli/src/args.rs rust/crates/rusty-claude-cli/src/auth.rs rust/crates/rusty-claude-cli/src/doctor.rs rust/crates/rusty-claude-cli/src/help.rs rust/crates/rusty-claude-cli/src/mcp_runtime.rs rust/crates/rusty-claude-cli/src/provider_client.rs rust/crates/rusty-claude-cli/src/repl.rs rust/crates/rusty-claude-cli/src/resume.rs rust/crates/rusty-claude-cli/src/runtime_host.rs rust/crates/rusty-claude-cli/src/sessions.rs rust/crates/rusty-claude-cli/src/status.rs rust/crates/rusty-claude-cli/src/tool_display.rs rust/crates/rusty-claude-cli/src/tool_executor.rs
git commit -m "refactor(cli): add rusty claude cli module shells"
```

Expected: one commit containing only module declarations and empty module files.

## Task 2: Move CLI Argument Parsing

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/main.rs`
- Modify: `rust/crates/rusty-claude-cli/src/args.rs`

- [ ] **Step 1: Move CLI action types and parser helpers into `args.rs`**

Move these existing items unchanged from `main.rs` into `args.rs`:

```text
CliAction
LocalHelpTopic
CliOutputFormat
impl CliOutputFormat
parse_args
parse_local_help_action
is_help_flag
parse_single_word_command_alias
bare_slash_command_guidance
join_optional_args
parse_direct_slash_cli_action
normalized_prompt_slash_args
prompt_slash_turn_policy
format_prompt_slash_command_input
format_prompt_slash_command_metadata
format_prompt_slash_skill_listing
format_unknown_option
format_unknown_direct_slash_command
format_unknown_slash_command
omc_compatibility_note_for_unknown_slash_command
render_suggestion_line
suggest_slash_commands
suggest_closest_term
ranked_suggestions
levenshtein_distance
resolve_model_alias
resolve_model_alias_with_config
config_alias_for_current_dir
normalize_allowed_tools
current_tool_registry
parse_permission_mode_arg
permission_mode_from_label
permission_mode_from_resolved
default_permission_mode
config_permission_mode_for_current_dir
config_model_for_current_dir
resolve_repl_model
provider_label
format_connected_line
filter_tool_specs
filter_tool_specs_for_request
parse_system_prompt_args
parse_export_args
parse_resume_args
resume_command_can_absorb_token
looks_like_slash_command_token
```

Keep function bodies exactly as they are today.

- [ ] **Step 2: Add imports to `args.rs`**

Use this import block, then delete unused imports after `cargo check` identifies them:

```rust
use std::collections::BTreeSet;
use std::env;
use std::io::{IsTerminal, Read};
use std::path::PathBuf;

use api::{ProviderKind, ToolDefinition};
use commands::{
    classify_skills_slash_command, resolve_skill_invocation, slash_command_specs,
    validate_slash_command_input, SkillSlashDispatch, SlashCommand,
};
use runtime::{ConfigLoader, PermissionMode, ResolvedPermissionMode, TurnExecutionPolicy};
use serde_json::Value;
use tools::GlobalToolRegistry;

use crate::{AllowedToolSet, CLI_OPTION_SUGGESTIONS, DEFAULT_DATE, DEFAULT_MODEL};
```

- [ ] **Step 3: Set visibility for cross-module callers**

Apply `pub(crate)` to these moved items without changing variant names, function names, signatures, or bodies:

```text
CliAction
LocalHelpTopic
CliOutputFormat
parse_args
resolve_model_alias
resolve_model_alias_with_config
default_permission_mode
permission_mode_from_label
resolve_repl_model
format_connected_line
filter_tool_specs
filter_tool_specs_for_request
format_prompt_slash_command_input
format_prompt_slash_command_metadata
format_prompt_slash_skill_listing
format_unknown_slash_command
```

If tests still import additional moved helpers from `main.rs`, expose those helpers as `pub(crate)` without changing names, signatures, or bodies.

- [ ] **Step 4: Import parser items in `main.rs`**

Add:

```rust
use args::{parse_args, CliAction, CliOutputFormat};
```

Remove the moved item definitions from `main.rs`.

- [ ] **Step 5: Run focused check**

Run from `rust/`:

```bash
cargo check -p rusty-claude-cli
```

Expected: PASS. If errors are only missing visibility/imports, fix with `pub(crate)` and imports. Do not change parser behavior.

- [ ] **Step 6: Commit args split**

Run from the repository root:

```bash
git add rust/crates/rusty-claude-cli/src/main.rs rust/crates/rusty-claude-cli/src/args.rs
git commit -m "refactor(cli): move argument parsing out of main"
```

## Task 3: Move Help, Sessions, Status, Doctor, And Auth

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/main.rs`
- Modify: `rust/crates/rusty-claude-cli/src/help.rs`
- Modify: `rust/crates/rusty-claude-cli/src/sessions.rs`
- Modify: `rust/crates/rusty-claude-cli/src/status.rs`
- Modify: `rust/crates/rusty-claude-cli/src/doctor.rs`
- Modify: `rust/crates/rusty-claude-cli/src/auth.rs`

- [ ] **Step 1: Move help and completion code into `help.rs`**

Move these existing items unchanged:

```text
STUB_COMMANDS
slash_command_completion_candidates_with_sessions
render_repl_help
render_help_topic
print_help_topic
print_help_to
print_help
```

Use this initial import block:

```rust
use std::collections::BTreeSet;
use std::io::{self, Write};

use commands::{render_slash_command_help_filtered, resume_supported_slash_commands, slash_command_specs};
use serde_json::json;

use crate::args::{resolve_model_alias, CliOutputFormat, LocalHelpTopic};
use crate::{LATEST_SESSION_REFERENCE, PRIMARY_SESSION_EXTENSION, VERSION};
```

Apply `pub(crate)` to these moved items without changing names, signatures, values, or bodies:

```text
STUB_COMMANDS
slash_command_completion_candidates_with_sessions
render_repl_help
print_help_topic
print_help
```

- [ ] **Step 2: Move session helpers into `sessions.rs`**

Move these existing items unchanged:

```text
SessionHandle
ManagedSessionSummary
sessions_dir
create_managed_session_handle
resolve_session_reference
resolve_managed_session_path
is_managed_session_file
collect_sessions_from_dir
sort_managed_session_summaries
session_created_at_from_id
session_counter_from_id
parse_session_id_components
list_managed_sessions
latest_managed_session
delete_managed_session
confirm_session_deletion
format_missing_session_reference
format_no_managed_sessions
render_session_list
format_session_modified_age
write_session_clear_backup
session_clear_backup_path
```

Use this initial import block:

```rust
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use runtime::Session;

use crate::{LATEST_SESSION_REFERENCE, PRIMARY_SESSION_EXTENSION, LEGACY_SESSION_EXTENSION};
```

Apply crate visibility to the moved session types and helpers:

```rust
#[derive(Debug, Clone)]
pub(crate) struct SessionHandle {
    pub(crate) id: String,
    pub(crate) path: PathBuf,
}
```

Also apply `pub(crate)` to:

```text
ManagedSessionSummary
create_managed_session_handle
resolve_session_reference
list_managed_sessions
delete_managed_session
confirm_session_deletion
render_session_list
write_session_clear_backup
```

For `ManagedSessionSummary`, make fields `pub(crate)` only when callers outside `sessions.rs` need direct access.

- [ ] **Step 3: Move status, config, memory, diff, and git helpers into `status.rs`**

Move these existing items unchanged:

```text
StatusContext
StatusUsage
GitWorkspaceSummary
impl GitWorkspaceSummary
dump_manifests
print_bootstrap_plan
run_worker_state
print_system_prompt
print_version
version_json_value
format_model_report
format_model_switch_report
format_permissions_report
format_permissions_switch_report
format_cost_report
format_resume_report
render_resume_usage
format_compact_report
format_auto_compaction_notice
parse_git_status_metadata
parse_git_status_branch
parse_git_workspace_summary
resolve_git_branch_for
run_git_capture_in
find_git_root_in
parse_git_status_metadata_for
detect_broad_cwd
enforce_broad_cwd_policy
run_stale_base_preflight
print_status_snapshot
status_json_value
status_context
format_status_report
format_sandbox_report
format_commit_preflight_report
format_commit_skipped_report
print_sandbox_status_snapshot
sandbox_json_value
render_config_report
render_config_json
render_memory_report
render_memory_json
init_claude_md
run_init
init_json_value
normalize_permission_mode
render_diff_report
render_diff_report_for
render_diff_json_for
run_git_diff_command_in
render_teleport_report
render_last_tool_debug_report
indent_block
validate_no_args
format_bughunter_report
format_ultraplan_report
format_pr_report
format_issue_report
git_output
git_status_ok
command_exists
write_temp_text_file
parse_history_count
format_history_timestamp
civil_from_days
render_prompt_history_report
collect_session_prompt_history
recent_user_context
truncate_for_prompt
sanitize_generated_message
parse_titled_body
render_version_report
render_export_text
default_export_filename
resolve_export_path
summarize_tool_payload_for_markdown
run_export
render_session_markdown
short_tool_id
```

Use this initial import block:

```rust
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

use compat_harness::{extract_manifest, UpstreamPaths};
use runtime::{
    check_base_commit, compact_session_with_memory, format_stale_base_warning, format_usd,
    load_system_prompt, partial_compact_session, pricing_for_model, ConfigLoader, ConfigSource,
    ContentBlock, MessageRole, PartialCompactMode, PermissionMode, ProjectContext, Session,
    TokenUsage, UsageTracker,
};
use serde_json::json;

use crate::args::{default_permission_mode, permission_mode_from_label, CliOutputFormat};
use crate::init::initialize_repo;
use crate::{BUILD_TARGET, DEFAULT_DATE, GIT_SHA, VERSION};
```

Expose all moved functions and types used by `main.rs`, `doctor.rs`, `resume.rs`, `repl.rs`, `runtime_host.rs`, and tests as `pub(crate)`.

- [ ] **Step 4: Move doctor code into `doctor.rs`**

Move these existing items unchanged:

```text
DiagnosticLevel
impl DiagnosticLevel
DiagnosticCheck
impl DiagnosticCheck
DoctorReport
impl DoctorReport
render_diagnostic_check
render_doctor_report
run_doctor
check_auth_health
check_config_health
check_workspace_health
check_sandbox_health
check_system_health
```

Use this initial import block:

```rust
use std::env;
use std::path::Path;

use api::oauth_token_is_expired;
use runtime::{load_oauth_credentials, ConfigLoader, ProjectContext};
use serde_json::{json, Map, Value};

use crate::args::CliOutputFormat;
use crate::status::{
    parse_git_status_metadata, parse_git_workspace_summary, status_context, GitWorkspaceSummary,
    StatusContext,
};
use crate::{BUILD_TARGET, DEFAULT_DATE, GIT_SHA, VERSION};
```

Apply `pub(crate)` to `DoctorReport`, `render_doctor_report`, and `run_doctor` without changing names, signatures, or bodies.

- [ ] **Step 5: Move OAuth/auth code into `auth.rs`**

Move these existing items unchanged:

```text
default_oauth_config
run_login
emit_login_browser_open_failure
run_logout
open_browser
wait_for_oauth_callback
resolve_cli_auth_source
resolve_cli_auth_source_for_cwd
load_runtime_oauth_config_for
```

Use this initial import block:

```rust
use std::env;
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;

use api::{resolve_startup_auth_source, AnthropicClient, AuthSource};
use runtime::{
    clear_oauth_credentials, generate_pkce_pair, generate_state, parse_oauth_callback_request_target,
    save_oauth_credentials, ConfigLoader, OAuthAuthorizationRequest, OAuthConfig,
    OAuthTokenExchangeRequest,
};
use serde_json::json;

use crate::args::CliOutputFormat;
use crate::DEFAULT_OAUTH_CALLBACK_PORT;
```

Apply `pub(crate)` to these moved auth helpers without changing names, signatures, or bodies:

```text
default_oauth_config
run_login
run_logout
resolve_cli_auth_source
```

- [ ] **Step 6: Import moved functions in `main.rs`**

Add imports needed by `run()`:

```rust
use auth::{run_login, run_logout};
use doctor::run_doctor;
use help::{print_help, print_help_topic};
use resume::resume_session;
use status::{
    dump_manifests, enforce_broad_cwd_policy, print_bootstrap_plan,
    print_sandbox_status_snapshot, print_status_snapshot, print_system_prompt, print_version,
    run_export, run_init, run_stale_base_preflight, run_worker_state,
};
```

If `resume.rs` has not been populated yet, delay the `resume::resume_session` import until Task 6 and keep the function in `main.rs`.

- [ ] **Step 7: Run focused checks**

Run from `rust/`:

```bash
cargo check -p rusty-claude-cli
cargo test -p rusty-claude-cli --test output_format_contract
cargo test -p rusty-claude-cli --test resume_slash_commands
```

Expected: all PASS. Failures should be import/visibility mistakes only. Do not alter output text or JSON shapes.

- [ ] **Step 8: Commit low-coupling split**

Run from the repository root:

```bash
git add rust/crates/rusty-claude-cli/src/main.rs rust/crates/rusty-claude-cli/src/help.rs rust/crates/rusty-claude-cli/src/sessions.rs rust/crates/rusty-claude-cli/src/status.rs rust/crates/rusty-claude-cli/src/doctor.rs rust/crates/rusty-claude-cli/src/auth.rs
git commit -m "refactor(cli): move local reports and session helpers"
```

## Task 4: Move MCP Runtime And Runtime Host Glue

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/main.rs`
- Modify: `rust/crates/rusty-claude-cli/src/mcp_runtime.rs`
- Modify: `rust/crates/rusty-claude-cli/src/runtime_host.rs`

- [ ] **Step 1: Move MCP runtime state into `mcp_runtime.rs`**

Move these existing items unchanged:

```text
RuntimeMcpState
ToolSearchRequest
McpToolRequest
ListMcpResourcesRequest
ReadMcpResourceRequest
impl RuntimeMcpState
build_runtime_mcp_state
mcp_runtime_tool_definition
mcp_wrapper_tool_definitions
permission_mode_for_mcp_tool
mcp_annotation_flag
run_mcp_serve
```

Use this initial import block:

```rust
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use runtime::{McpServer, McpServerManager, McpServerSpec, McpTool, PermissionMode, ToolError};
use serde::Deserialize;
use serde_json::json;
use tools::{execute_tool, mvp_tool_specs, RuntimeToolDefinition};

use crate::{RuntimePluginStateBuildOutput, VERSION};
```

If `RuntimePluginStateBuildOutput` is moved to `runtime_host.rs` in the same task, import it as:

```rust
use crate::runtime_host::RuntimePluginStateBuildOutput;
```

Apply `pub(crate)` to these moved MCP runtime items without changing names, signatures, fields, or bodies:

```text
RuntimeMcpState
ToolSearchRequest
McpToolRequest
ListMcpResourcesRequest
ReadMcpResourceRequest
build_runtime_mcp_state
mcp_wrapper_tool_definitions
run_mcp_serve
```

- [ ] **Step 2: Move runtime host glue into `runtime_host.rs`**

Move these existing items unchanged:

```text
RuntimePluginStateBuildOutput
RuntimePluginState
BuiltRuntime
impl BuiltRuntime
impl Deref for BuiltRuntime
impl DerefMut for BuiltRuntime
impl Drop for BuiltRuntime
build_system_prompt
build_runtime_plugin_state
build_runtime_plugin_state_with_loader
build_plugin_manager
resolve_plugin_path
runtime_hook_config_from_plugin_hooks
InternalPromptProgressState
InternalPromptProgressEvent
InternalPromptProgressShared
InternalPromptProgressReporter
InternalPromptProgressRun
impl InternalPromptProgressReporter
impl InternalPromptProgressRun
impl Drop for InternalPromptProgressRun
format_internal_prompt_progress_line
describe_tool_progress
build_runtime
build_runtime_with_plugin_state
CliHookProgressReporter
impl runtime::HookProgressReporter for CliHookProgressReporter
CliPermissionPrompter
impl CliPermissionPrompter
impl runtime::PermissionPrompter for CliPermissionPrompter
permission_policy
build_runtime_plugin_state_with_loader
```

Use this initial import block:

```rust
use std::env;
use std::io::{self, Write};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use plugins::{PluginHooks, PluginManager, PluginManagerConfig, PluginRegistry};
use runtime::{ConversationRuntime, PermissionMode, PermissionPolicy, Session};
use tools::{GlobalToolRegistry, RuntimeToolDefinition};

use crate::args::filter_tool_specs_for_request;
use crate::mcp_runtime::{build_runtime_mcp_state, RuntimeMcpState};
use crate::provider_client::AnthropicRuntimeClient;
use crate::status::truncate_for_prompt;
use crate::tool_display::{extract_tool_path, first_visible_line, summarize_tool_payload, truncate_for_summary};
use crate::tool_executor::CliToolExecutor;
use crate::{AllowedToolSet, DEFAULT_DATE, INTERNAL_PROGRESS_HEARTBEAT_INTERVAL};
```

Move the existing type alias unchanged and make it visible to sibling modules:

```rust
pub(crate) type RuntimePluginStateBuildOutput = (
    Option<Arc<Mutex<RuntimeMcpState>>>,
    Vec<RuntimeToolDefinition>,
);
```

Apply `pub(crate)` to these moved runtime-host items without changing names, signatures, fields, or bodies:

```text
BuiltRuntime
RuntimePluginState
InternalPromptProgressReporter
InternalPromptProgressRun
CliPermissionPrompter
build_runtime
build_runtime_with_plugin_state
build_runtime_plugin_state_with_loader
build_plugin_manager
```

- [ ] **Step 3: Break the temporary circular dependency deliberately**

`runtime_host.rs` needs `CliToolExecutor` and `AnthropicRuntimeClient`; those modules are moved in later tasks. Until they move, keep `CliToolExecutor` and `AnthropicRuntimeClient` in `main.rs`, and import them as:

```rust
use crate::{AnthropicRuntimeClient, CliToolExecutor};
```

After Tasks 5 and 6, change those imports to:

```rust
use crate::provider_client::AnthropicRuntimeClient;
use crate::tool_executor::CliToolExecutor;
```

- [ ] **Step 4: Run focused check**

Run from `rust/`:

```bash
cargo check -p rusty-claude-cli
```

Expected: PASS.

- [ ] **Step 5: Commit runtime glue split**

Run from the repository root:

```bash
git add rust/crates/rusty-claude-cli/src/main.rs rust/crates/rusty-claude-cli/src/mcp_runtime.rs rust/crates/rusty-claude-cli/src/runtime_host.rs
git commit -m "refactor(cli): move runtime host glue"
```

## Task 5: Move Tool Display And Tool Executor

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/main.rs`
- Modify: `rust/crates/rusty-claude-cli/src/tool_display.rs`
- Modify: `rust/crates/rusty-claude-cli/src/tool_executor.rs`
- Modify: `rust/crates/rusty-claude-cli/src/runtime_host.rs`

- [ ] **Step 1: Move tool display and stream-event helpers into `tool_display.rs`**

Move these existing items unchanged:

```text
format_tool_call_start
format_tool_result
DISPLAY_TRUNCATION_NOTICE
READ_DISPLAY_MAX_LINES
READ_DISPLAY_MAX_CHARS
TOOL_OUTPUT_DISPLAY_MAX_LINES
TOOL_OUTPUT_DISPLAY_MAX_CHARS
extract_tool_path
format_search_start
format_patch_preview
format_bash_call
first_visible_line
format_bash_result
format_read_result
format_write_result
format_structured_patch_preview
format_edit_result
format_glob_result
format_grep_result
format_generic_tool_result
summarize_tool_payload
truncate_for_summary
truncate_output_for_display
render_thinking_block_summary
push_output_block
track_tool_block_index
should_log_tool_stop_without_start
response_to_events
normalize_tool_input_json
normalize_tool_input_string
validate_tool_input_json
debug_json_value_summary
debug_json_input_summary
debug_labeled_json_input_summary
should_log_stream_event_for_tool_diagnostics
should_log_streamed_tool_input_delta
stream_event_debug_summary
pending_tools_debug_summary
json_debug_string
json_debug_suffix
push_prompt_cache_record
prompt_cache_record_to_runtime_event
final_assistant_text
collect_tool_uses
collect_tool_results
collect_prompt_cache_events
```

Use this initial import block:

```rust
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

use api::{ContentBlockDelta, MessageResponse, OutputContentBlock, ProviderClient as ApiProviderClient, StreamEvent as ApiStreamEvent};
use runtime::{AssistantEvent, ContentBlock, MessageRole, PromptCacheEvent, RuntimeError};
use serde_json::{json, Value};

use crate::render::TerminalRenderer;
```

Expose every moved item needed by `provider_client.rs`, `runtime_host.rs`, `tool_executor.rs`, `status.rs`, `repl.rs`, and tests as `pub(crate)`.

- [ ] **Step 2: Move tool executor into `tool_executor.rs`**

Move these existing items unchanged:

```text
CliToolExecutor
impl CliToolExecutor
cli_agent_debug_log
describe_parallel_invocation
impl ToolExecutor for CliToolExecutor
permission_policy
```

Use this initial import block:

```rust
use std::sync::{Arc, Mutex};
use std::time::Instant;

use runtime::{active_tool_session_id, with_active_tool_session, PermissionMode, PermissionPolicy, ToolError, ToolExecutor, ToolInvocation};
use serde_json::Value;
use tools::{GlobalToolRegistry, ToolSearchOutput};

use crate::mcp_runtime::{ListMcpResourcesRequest, McpToolRequest, ReadMcpResourceRequest, RuntimeMcpState, ToolSearchRequest};
use crate::render::TerminalRenderer;
use crate::tool_display::{
    debug_json_input_summary, format_tool_result, normalize_tool_input_json,
};
use crate::AllowedToolSet;
```

Apply `pub(crate)` to `CliToolExecutor` and `permission_policy` without changing names, signatures, fields, or bodies.

- [ ] **Step 3: Update runtime host imports**

In `runtime_host.rs`, replace temporary imports:

```rust
use crate::{AnthropicRuntimeClient, CliToolExecutor};
```

with:

```rust
use crate::provider_client::AnthropicRuntimeClient;
use crate::tool_executor::{permission_policy, CliToolExecutor};
```

If `provider_client.rs` has not been populated yet, only switch `CliToolExecutor` now and leave `AnthropicRuntimeClient` temporary until Task 6.

- [ ] **Step 4: Run focused checks**

Run from `rust/`:

```bash
cargo check -p rusty-claude-cli
cargo test -p rusty-claude-cli --test compact_output
```

Expected: PASS. Compact output failures indicate display behavior changed; revert the behavior change and keep moving code mechanically.

- [ ] **Step 5: Commit tool split**

Run from the repository root:

```bash
git add rust/crates/rusty-claude-cli/src/main.rs rust/crates/rusty-claude-cli/src/tool_display.rs rust/crates/rusty-claude-cli/src/tool_executor.rs rust/crates/rusty-claude-cli/src/runtime_host.rs
git commit -m "refactor(cli): move tool display and execution"
```

## Task 6: Move Provider Client, Resume, And REPL

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/main.rs`
- Modify: `rust/crates/rusty-claude-cli/src/provider_client.rs`
- Modify: `rust/crates/rusty-claude-cli/src/resume.rs`
- Modify: `rust/crates/rusty-claude-cli/src/repl.rs`
- Modify: `rust/crates/rusty-claude-cli/src/runtime_host.rs`

- [ ] **Step 1: Move provider client into `provider_client.rs`**

Move these existing items unchanged:

```text
AnthropicRuntimeClient
impl AnthropicRuntimeClient
impl ApiClient for AnthropicRuntimeClient
impl AnthropicRuntimeClient::consume_stream
impl AnthropicRuntimeClient::non_streaming_fallback
request_ends_with_tool_result
format_user_visible_api_error
format_context_window_blocked_error
convert_messages
apply_tool_cache_controls
```

Use this initial import block:

```rust
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::time::Instant;

use api::{
    apply_message_cache_controls, build_system_blocks_with_cache_controls, detect_provider_kind,
    log_prompt_cache_block_diagnostics, summarize_prompt_cache_controls, AnthropicClient,
    CacheControl, ContentBlockDelta, ContextManagement, InputContentBlock, InputMessage,
    MessageRequest, OutputConfig, OutputContentBlock, PromptCache,
    ProviderClient as ApiProviderClient, ProviderKind, StreamEvent as ApiStreamEvent,
    ThinkingConfig, ToolChoice, ToolDefinition, ToolResultContentBlock,
};
use runtime::{agent_debug_log, ApiClient, ApiRequest, AssistantEvent, ContentBlock, ConversationMessage, MessageRole, RuntimeError};
use tools::{render_tool_result_for_model, GlobalToolRegistry};

use crate::args::filter_tool_specs_for_request;
use crate::auth::resolve_cli_auth_source;
use crate::render::{MarkdownStreamState, TerminalRenderer};
use crate::runtime_host::InternalPromptProgressReporter;
use crate::tool_display::{
    collect_prompt_cache_events, collect_tool_results, collect_tool_uses,
    debug_json_value_summary, debug_labeled_json_input_summary, final_assistant_text,
    format_tool_call_start, json_debug_string,
    json_debug_suffix, normalize_tool_input_string, pending_tools_debug_summary,
    push_output_block, push_prompt_cache_record, response_to_events,
    should_log_stream_event_for_tool_diagnostics, should_log_streamed_tool_input_delta,
    should_log_tool_stop_without_start, stream_event_debug_summary, track_tool_block_index,
    validate_tool_input_json,
};
use crate::{AllowedToolSet, POST_TOOL_STALL_TIMEOUT};
```

Do not rename `AnthropicRuntimeClient`.

- [ ] **Step 2: Move resume command execution into `resume.rs`**

Move these existing items unchanged:

```text
resume_session
ResumeCommandOutcome
run_resume_command
```

Use this initial import block:

```rust
use std::fs;
use std::path::{Path, PathBuf};

use commands::{
    classify_skills_slash_command, handle_agents_slash_command, handle_mcp_slash_command,
    handle_mcp_slash_command_json, handle_skills_slash_command, handle_skills_slash_command_json,
    SkillSlashDispatch, SlashCommand,
};
use runtime::{compact_session_with_memory, partial_compact_session, CompactionConfig, Session, UsageTracker};
use serde_json::json;

use crate::args::{default_permission_mode, CliOutputFormat};
use crate::doctor::render_doctor_report;
use crate::help::{render_repl_help, STUB_COMMANDS};
use crate::sessions::{list_managed_sessions, render_session_list, resolve_session_reference, write_session_clear_backup};
use crate::status::{
    collect_session_prompt_history, format_compact_report, format_cost_report,
    format_status_report, init_claude_md, init_json_value, parse_history_count,
    render_config_json, render_config_report, render_diff_json_for, render_diff_report_for,
    render_export_text, render_memory_json, render_memory_report, render_prompt_history_report,
    render_version_report, resolve_export_path, sandbox_json_value, status_context,
    status_json_value, version_json_value, StatusUsage,
};
```

Apply `pub(crate)` to `ResumeCommandOutcome`, `resume_session`, and `run_resume_command` without changing names, signatures, fields, or bodies.

- [ ] **Step 3: Move REPL into `repl.rs`**

Move these existing items unchanged:

```text
run_repl
LiveCli
PromptHistoryEntry
HookAbortMonitor
impl HookAbortMonitor
impl LiveCli
```

Use this initial import block:

```rust
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use commands::{
    build_simplify_prompt, handle_agents_slash_command, handle_agents_slash_command_json,
    handle_mcp_slash_command, handle_mcp_slash_command_json, handle_plugins_slash_command,
    handle_skills_slash_command, handle_skills_slash_command_json, resolve_skill_invocation,
    SkillSlashDispatch, SlashCommand,
};
use runtime::{partial_compact_session, CompactionConfig, ContentBlock, MessageRole, PartialCompactMode, PermissionMode, Session, TurnExecutionPolicy, UsageTracker};
use serde_json::json;

use crate::args::{default_permission_mode, format_connected_line, permission_mode_from_label, resolve_model_alias_with_config, resolve_repl_model, CliOutputFormat};
use crate::help::{render_repl_help, slash_command_completion_candidates_with_sessions};
use crate::input;
use crate::runtime_host::{build_plugin_manager, build_runtime, BuiltRuntime, CliPermissionPrompter, InternalPromptProgressRun};
use crate::sessions::{confirm_session_deletion, create_managed_session_handle, delete_managed_session, list_managed_sessions, render_session_list, resolve_session_reference, SessionHandle};
use crate::status::{
    collect_session_prompt_history, enforce_broad_cwd_policy, format_bughunter_report,
    format_commit_preflight_report, format_commit_skipped_report, format_compact_report,
    format_cost_report, format_issue_report, format_model_report, format_model_switch_report,
    format_permissions_report, format_permissions_switch_report, format_pr_report,
    format_resume_report, format_status_report, format_ultraplan_report, git_output,
    normalize_permission_mode, parse_git_status_branch, parse_git_workspace_summary,
    render_config_report, render_diff_report, render_export_text, render_last_tool_debug_report,
    render_memory_report, render_prompt_history_report, render_resume_usage,
    render_teleport_report, resolve_export_path, resolve_git_branch_for, run_stale_base_preflight,
    status_context, validate_no_args, StatusUsage,
};
use crate::tool_display::{collect_prompt_cache_events, collect_tool_results, collect_tool_uses, final_assistant_text};
use crate::AllowedToolSet;
```

Apply `pub(crate)` to `run_repl`, `LiveCli`, and `PromptHistoryEntry` without changing names, signatures, fields, or bodies. Keep the existing `#[derive(Debug, Clone)]` on `PromptHistoryEntry`.

- [ ] **Step 4: Update `main.rs` imports and remove moved code**

`main.rs` should import:

```rust
use args::{parse_args, CliAction, CliOutputFormat};
use auth::{run_login, run_logout};
use doctor::run_doctor;
use help::{print_help, print_help_topic};
use repl::{run_repl, LiveCli};
use resume::resume_session;
use status::{
    dump_manifests, enforce_broad_cwd_policy, print_bootstrap_plan,
    print_sandbox_status_snapshot, print_status_snapshot, print_system_prompt, print_version,
    run_export, run_init, run_stale_base_preflight, run_worker_state,
};
```

Keep in `main.rs`:

```text
DEFAULT_MODEL
max_tokens_for_model
DEFAULT_DATE
DEFAULT_OAUTH_CALLBACK_PORT
VERSION
BUILD_TARGET
GIT_SHA
INTERNAL_PROGRESS_HEARTBEAT_INTERVAL
POST_TOOL_STALL_TIMEOUT
PRIMARY_SESSION_EXTENSION
LEGACY_SESSION_EXTENSION
LATEST_SESSION_REFERENCE
SESSION_REFERENCE_ALIASES
CLI_OPTION_SUGGESTIONS
AllowedToolSet
main
read_piped_stdin
merge_prompt_with_stdin
run
```

- [ ] **Step 5: Run focused checks**

Run from `rust/`:

```bash
cargo check -p rusty-claude-cli
cargo test -p rusty-claude-cli --test cli_flags_and_config_defaults
cargo test -p rusty-claude-cli --test system_prompt_attachments
cargo test -p rusty-claude-cli --test mock_parity_harness
```

Expected: all PASS. If the mock parity harness is slow, let it finish; provider/tool behavior must remain unchanged.

- [ ] **Step 6: Commit provider, resume, and REPL split**

Run from the repository root:

```bash
git add rust/crates/rusty-claude-cli/src/main.rs rust/crates/rusty-claude-cli/src/provider_client.rs rust/crates/rusty-claude-cli/src/resume.rs rust/crates/rusty-claude-cli/src/repl.rs rust/crates/rusty-claude-cli/src/runtime_host.rs
git commit -m "refactor(cli): move provider resume and repl code"
```

## Task 7: Move Or Rehome Unit Tests

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/main.rs`
- Modify: relevant new module files under `rust/crates/rusty-claude-cli/src/`

- [ ] **Step 1: Inventory in-file tests**

Run:

```bash
rg --line-number "#\\[test\\]|mod tests|use super::" rust/crates/rusty-claude-cli/src/main.rs
```

Expected: output identifies the current `#[cfg(test)] mod tests` and `sandbox_report_tests`.

- [ ] **Step 2: Keep broad integration-like unit tests in `main.rs` if lower risk**

If tests import many helpers from multiple modules, leave the test module in `main.rs` and replace its `use super::{...}` block with explicit crate-module imports. Example pattern:

```rust
use crate::args::{parse_args, resolve_model_alias, resolve_model_alias_with_config, CliAction, CliOutputFormat, LocalHelpTopic};
use crate::help::{slash_command_completion_candidates_with_sessions, STUB_COMMANDS};
use crate::runtime_host::{build_runtime_plugin_state_with_loader, build_runtime_with_plugin_state, InternalPromptProgressEvent, InternalPromptProgressState};
use crate::sessions::{create_managed_session_handle, resolve_session_reference};
use crate::status::{format_bughunter_report, format_commit_preflight_report, format_commit_skipped_report, format_compact_report};
use crate::tool_display::{debug_json_input_summary, format_tool_call_start, format_tool_result, normalize_tool_input_string, response_to_events};
use crate::tool_executor::{permission_policy, CliToolExecutor};
```

Add imports as compiler errors require. Do not change assertions.

- [ ] **Step 3: Move narrow tests next to modules when simple**

For tests that depend on one module only, move them into that module's `#[cfg(test)] mod tests`. Example for tool display:

```rust
#[cfg(test)]
mod tests {
    use super::{format_tool_call_start, format_tool_result, normalize_tool_input_string};

    #[test]
    fn empty_tool_input_normalizes_to_object() {
        assert_eq!(normalize_tool_input_string(String::new()), "{}");
    }
}
```

Only move existing test bodies. Do not invent new behavior tests during this refactor.

- [ ] **Step 4: Run unit and integration tests for the crate**

Run from `rust/`:

```bash
cargo test -p rusty-claude-cli
```

Expected: PASS.

- [ ] **Step 5: Commit test import cleanup**

Run from the repository root:

```bash
git add rust/crates/rusty-claude-cli/src/main.rs rust/crates/rusty-claude-cli/src/args.rs rust/crates/rusty-claude-cli/src/help.rs rust/crates/rusty-claude-cli/src/sessions.rs rust/crates/rusty-claude-cli/src/status.rs rust/crates/rusty-claude-cli/src/doctor.rs rust/crates/rusty-claude-cli/src/auth.rs rust/crates/rusty-claude-cli/src/mcp_runtime.rs rust/crates/rusty-claude-cli/src/runtime_host.rs rust/crates/rusty-claude-cli/src/provider_client.rs rust/crates/rusty-claude-cli/src/repl.rs rust/crates/rusty-claude-cli/src/resume.rs rust/crates/rusty-claude-cli/src/tool_display.rs rust/crates/rusty-claude-cli/src/tool_executor.rs
git commit -m "test(cli): update unit tests for module split"
```

## Task 8: Final Rust Verification And Code Commit

**Files:**
- Modify only files already touched under `rust/crates/rusty-claude-cli/src/`

- [ ] **Step 1: Confirm `main.rs` is thin**

Run:

```bash
wc -l rust/crates/rusty-claude-cli/src/main.rs
rg --line-number "^(struct|enum|impl|fn|const|type) " rust/crates/rusty-claude-cli/src/main.rs
```

Expected: `main.rs` is much shorter than the original 13,743 lines and contains only constants/types needed by dispatch, `main`, stdin helpers, and `run`.

- [ ] **Step 2: Format all Rust code**

Run from `rust/`:

```bash
cargo fmt --all
```

Expected: PASS with no output or only rustfmt normal completion.

- [ ] **Step 3: Run clippy**

Run from `rust/`:

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: PASS. Fix only warnings introduced by module movement, such as unused imports or visibility warnings.

- [ ] **Step 4: Run workspace tests**

Run from `rust/`:

```bash
cargo test --workspace
```

Expected: PASS.

- [ ] **Step 5: Run repository test wrapper before code commit**

Run from repository root:

```bash
./scripts/run_all_tests.sh
```

Expected: PASS.

- [ ] **Step 6: Inspect staged and unstaged changes**

Run:

```bash
git status --short
git diff --stat
```

Expected: only files related to `rusty-claude-cli` module split and this plan are modified. Unrelated pre-existing dirty files should remain unstaged.

- [ ] **Step 7: Commit final cleanup if there are uncommitted code changes**

Run from repository root:

```bash
git add rust/crates/rusty-claude-cli/src/main.rs rust/crates/rusty-claude-cli/src/args.rs rust/crates/rusty-claude-cli/src/auth.rs rust/crates/rusty-claude-cli/src/doctor.rs rust/crates/rusty-claude-cli/src/help.rs rust/crates/rusty-claude-cli/src/mcp_runtime.rs rust/crates/rusty-claude-cli/src/provider_client.rs rust/crates/rusty-claude-cli/src/repl.rs rust/crates/rusty-claude-cli/src/resume.rs rust/crates/rusty-claude-cli/src/runtime_host.rs rust/crates/rusty-claude-cli/src/sessions.rs rust/crates/rusty-claude-cli/src/status.rs rust/crates/rusty-claude-cli/src/tool_display.rs rust/crates/rusty-claude-cli/src/tool_executor.rs
git commit -m "refactor(cli): finish rusty claude cli main split"
```

Expected: commit succeeds. If all code changes were already committed by earlier tasks, skip this commit.

## Task 9: Plan Self-Review Before Execution

**Files:**
- Read: `docs/superpowers/specs/2026-05-07-rusty-claude-cli-main-split-design.md`
- Read: `docs/superpowers/plans/2026-05-07-rusty-claude-cli-main-split.md`

- [ ] **Step 1: Verify spec coverage**

Check that every spec requirement maps to a task:

```text
Thin main.rs: Tasks 1, 6, 8
Focused sibling modules: Tasks 1-6
No src/lib.rs: File Structure and Task 8
No core private renames: Tasks 2-6
Behavior preservation: Tasks 3, 5, 6, 8
Verification sequence: Task 8
Run ./scripts/run_all_tests.sh before code commit: Task 8 Step 5
Unrelated dirty files not staged: Tasks 1 and 8
```

Expected: all requirements are covered.

- [ ] **Step 2: Scan for placeholders**

Run:

```bash
rg --line-number "[T]BD|[T]ODO|[i]mplement later|[S]imilar to|[a]ppropriate error handling|[W]rite tests for the above" docs/superpowers/plans/2026-05-07-rusty-claude-cli-main-split.md
```

Expected: no matches.

- [ ] **Step 3: Confirm execution strategy before coding**

Ask the user which execution approach to use:

```text
Plan complete and saved to `docs/superpowers/plans/2026-05-07-rusty-claude-cli-main-split.md`. Two execution options:

1. Subagent-Driven (recommended) - I dispatch a fresh subagent per task, review between tasks, fast iteration

2. Inline Execution - Execute tasks in this session using executing-plans, batch execution with checkpoints

Which approach?
```
