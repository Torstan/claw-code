# Rust Crates Organization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reorganize `rust/crates/commands` and `rust/crates/tools` into clearer Rust directory modules while preserving public APIs and behavior.

**Architecture:** Keep `commands/src/lib.rs` and `tools/src/lib.rs` as compatibility facades. Move existing code unchanged into focused internal modules first, then make only small local helper extractions needed for compilation and reuse. Do not touch `rusty-claude-cli`, `runtime`, or `api` during implementation; stop and report the exact compiler error if a change outside `commands` or `tools` appears necessary.

**Tech Stack:** Rust 2021 workspace, Cargo, serde/serde_json, existing `commands`, `tools`, `runtime`, `plugins`, and `api` crates.

---

## File Structure

### Commands Crate

- Modify: `rust/crates/commands/src/lib.rs`
- Keep: `rust/crates/commands/src/simplify.rs`
- Create: `rust/crates/commands/src/registry.rs`
- Create: `rust/crates/commands/src/spec.rs`
- Create: `rust/crates/commands/src/help.rs`
- Create: `rust/crates/commands/src/parse.rs`
- Create: `rust/crates/commands/src/definition.rs`
- Create: `rust/crates/commands/src/shared_args.rs`
- Create: `rust/crates/commands/src/plugins.rs`
- Create: `rust/crates/commands/src/agents.rs`
- Create: `rust/crates/commands/src/skills.rs`
- Create: `rust/crates/commands/src/mcp.rs`
- Create: `rust/crates/commands/tests/public_api.rs`

### Tools Crate

- Modify: `rust/crates/tools/src/lib.rs`
- Keep: `rust/crates/tools/src/lane_completion.rs`
- Keep: `rust/crates/tools/src/pdf_extract.rs`
- Create: `rust/crates/tools/src/registries.rs`
- Create: `rust/crates/tools/src/registry.rs`
- Create: `rust/crates/tools/src/specs.rs`
- Create: `rust/crates/tools/src/dispatch.rs`
- Create: `rust/crates/tools/src/basic.rs`
- Create: `rust/crates/tools/src/web.rs`
- Create: `rust/crates/tools/src/todo.rs`
- Create: `rust/crates/tools/src/skill.rs`
- Create: `rust/crates/tools/src/notebook.rs`
- Create: `rust/crates/tools/src/config.rs`
- Create: `rust/crates/tools/src/repl.rs`
- Create: `rust/crates/tools/src/tasks.rs`
- Create: `rust/crates/tools/src/workers.rs`
- Create: `rust/crates/tools/src/team_cron.rs`
- Create: `rust/crates/tools/src/mcp.rs`
- Create: `rust/crates/tools/src/lsp.rs`
- Create: `rust/crates/tools/src/tool_search.rs`
- Create: `rust/crates/tools/src/agent/mod.rs`
- Create: `rust/crates/tools/tests/public_api.rs`

### Ownership For Parallel Work

- A commands worker owns only `rust/crates/commands/**`.
- A tools worker owns only `rust/crates/tools/**`.
- A verification worker can run read-only checks after either worker reports a completed patch.
- No worker may edit unrelated dirty files already present in the repository.

---

## Task 1: Add Public API Guard Tests

**Files:**
- Create: `rust/crates/commands/tests/public_api.rs`
- Create: `rust/crates/tools/tests/public_api.rs`

- [ ] **Step 1: Add commands public API guard**

Create `rust/crates/commands/tests/public_api.rs`:

```rust
use commands::{
    classify_skills_slash_command, render_slash_command_help, render_slash_command_help_filtered,
    resolve_skill_invocation, resume_supported_slash_commands, slash_command_specs,
    validate_slash_command_input, SkillSlashDispatch, SlashCommand,
};

#[test]
fn crate_root_exports_slash_command_api() {
    let specs = slash_command_specs();
    assert!(specs.iter().any(|spec| spec.name == "skills"));
    assert!(resume_supported_slash_commands()
        .iter()
        .any(|spec| spec.name == "help"));

    assert!(matches!(
        validate_slash_command_input("/skills list").expect("parse should succeed"),
        Some(SlashCommand::Skills { args: Some(args) }) if args == "list"
    ));

    assert_eq!(
        classify_skills_slash_command(Some("list")),
        SkillSlashDispatch::Local
    );
    assert_eq!(
        classify_skills_slash_command(Some("help overview")),
        SkillSlashDispatch::Invoke("$help overview".to_string())
    );

    let help = render_slash_command_help();
    assert!(help.contains("/skills"));
    assert!(render_slash_command_help_filtered(&["skills"]).contains("Slash commands"));
}

#[test]
fn missing_skill_resolution_reports_unknown_skill() {
    let cwd = std::env::current_dir().expect("current dir should be available");
    let error = resolve_skill_invocation(&cwd, Some("definitely-missing-skill-name"))
        .expect_err("missing skill should be rejected");

    assert!(error.contains("Unknown skill: definitely-missing-skill-name"));
    assert!(error.contains("Usage: /skills"));
}
```

- [ ] **Step 2: Add tools public API guard**

Create `rust/crates/tools/tests/public_api.rs`:

```rust
use serde_json::json;

use runtime::PermissionMode;
use tools::{
    execute_tool, is_background_task_tool_name, mvp_tool_specs, render_tool_result_for_model,
    GlobalToolRegistry, RuntimeToolDefinition, ToolManifestEntry, ToolRegistry, ToolSearchOutput,
    ToolSource,
};

fn assert_tool_search_output_type<T>() {}

#[test]
fn crate_root_exports_tool_api() {
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
    assert_tool_search_output_type::<ToolSearchOutput>();
}

#[test]
fn unsupported_tool_error_stays_user_visible() {
    let error = execute_tool("DefinitelyMissingTool", &json!({}))
        .expect_err("unsupported tool should return an error");
    assert_eq!(error, "unsupported tool: DefinitelyMissingTool");
}
```

- [ ] **Step 3: Run commands guard test**

Run from `rust/`:

```bash
cargo test -p commands --test public_api
```

Expected: PASS. If it fails before refactoring, fix the test to match the current public API before moving code.

- [ ] **Step 4: Run tools guard test**

Run from `rust/`:

```bash
cargo test -p tools --test public_api
```

Expected: PASS. If it fails before refactoring, fix the test to match the current public API before moving code.

- [ ] **Step 5: Commit guard tests**

Run from the repository root:

```bash
git add rust/crates/commands/tests/public_api.rs rust/crates/tools/tests/public_api.rs
git commit -m "test: guard commands and tools public APIs"
```

---

## Task 2: Split Commands Registry, Specs, Help, And Parse

**Files:**
- Modify: `rust/crates/commands/src/lib.rs`
- Create: `rust/crates/commands/src/registry.rs`
- Create: `rust/crates/commands/src/spec.rs`
- Create: `rust/crates/commands/src/help.rs`
- Create: `rust/crates/commands/src/parse.rs`

- [ ] **Step 1: Move registry types**

Move these existing items unchanged from `rust/crates/commands/src/lib.rs` into `rust/crates/commands/src/registry.rs`:

- `CommandManifestEntry`
- `CommandSource`
- `CommandRegistry`
- `impl CommandRegistry`

At the top of `registry.rs`, use:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandManifestEntry {
    pub name: String,
    pub source: CommandSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSource {
    Builtin,
    InternalOnly,
    FeatureGated,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandRegistry {
    entries: Vec<CommandManifestEntry>,
}
```

- [ ] **Step 2: Move slash command specs**

Move `SlashCommandSpec` and `SLASH_COMMAND_SPECS` unchanged into `rust/crates/commands/src/spec.rs`.

Add this public accessor in `spec.rs`:

```rust
#[must_use]
pub fn slash_command_specs() -> &'static [SlashCommandSpec] {
    SLASH_COMMAND_SPECS
}
```

- [ ] **Step 3: Move help rendering**

Move these existing items unchanged into `rust/crates/commands/src/help.rs`:

- `find_slash_command_spec`
- `command_root_name`
- `slash_command_usage`
- `slash_command_detail_lines`
- `render_slash_command_help_detail`
- `resume_supported_slash_commands`
- `slash_command_category`
- `format_slash_command_help_line`
- `levenshtein_distance`
- `suggest_slash_commands`
- `render_slash_command_help_filtered`
- `render_slash_command_help`

At the top of `help.rs`, use:

```rust
use crate::spec::{slash_command_specs, SlashCommandSpec};
```

- [ ] **Step 4: Move slash command parser**

Move these existing items unchanged into `rust/crates/commands/src/parse.rs`:

- `SlashCommand`
- `SlashCommandParseError`
- `impl SlashCommandParseError`
- `impl fmt::Display for SlashCommandParseError`
- `impl std::error::Error for SlashCommandParseError`
- `impl SlashCommand`
- `validate_slash_command_input`
- `validate_no_args`
- `parse_compact_args`
- `optional_single_arg`
- `require_remainder`
- `parse_permissions_mode`
- `parse_clear_args`
- `parse_config_section`
- `parse_session_command`
- `parse_mcp_command`
- `parse_plugin_command`
- `parse_list_or_help_args`
- `parse_skills_args`
- `usage_error`
- `command_error`
- `remainder_after_command`

At the top of `parse.rs`, use:

```rust
use std::fmt;

use runtime::PartialCompactMode;

use crate::help::render_slash_command_help_detail;
```

- [ ] **Step 5: Replace commands crate root with facade imports for these modules**

Update the top of `rust/crates/commands/src/lib.rs` so these modules and re-exports exist:

```rust
mod agents;
mod definition;
mod help;
mod mcp;
mod parse;
mod plugins;
mod registry;
mod shared_args;
mod simplify;
mod skills;
mod spec;

pub use agents::{handle_agents_slash_command, handle_agents_slash_command_json};
pub use help::{
    render_slash_command_help, render_slash_command_help_detail,
    render_slash_command_help_filtered, resume_supported_slash_commands, suggest_slash_commands,
};
pub use mcp::{handle_mcp_slash_command, handle_mcp_slash_command_json};
pub use parse::{validate_slash_command_input, SlashCommand, SlashCommandParseError};
pub use plugins::{handle_plugins_slash_command, PluginsCommandResult};
pub use registry::{CommandManifestEntry, CommandRegistry, CommandSource};
pub use simplify::build_simplify_prompt;
pub use skills::{
    classify_skills_slash_command, handle_skills_slash_command,
    handle_skills_slash_command_json, resolve_skill_invocation, resolve_skill_path,
    SkillSlashDispatch,
};
pub use spec::{slash_command_specs, SlashCommandSpec};
```

- [ ] **Step 6: Compile commands**

Run from `rust/`:

```bash
cargo test -p commands --test public_api
```

Expected: PASS. Fix only module imports, `pub(crate)` visibility, and removed duplicate definitions.

---

## Task 3: Split Commands Family Modules

**Files:**
- Modify: `rust/crates/commands/src/lib.rs`
- Modify: `rust/crates/commands/src/parse.rs`
- Modify: `rust/crates/commands/src/help.rs`
- Create: `rust/crates/commands/src/definition.rs`
- Create: `rust/crates/commands/src/shared_args.rs`
- Create: `rust/crates/commands/src/plugins.rs`
- Create: `rust/crates/commands/src/agents.rs`
- Create: `rust/crates/commands/src/skills.rs`
- Create: `rust/crates/commands/src/mcp.rs`

- [ ] **Step 1: Move shared definition discovery types**

Move these existing items unchanged into `rust/crates/commands/src/definition.rs`:

- `DefinitionSource`
- `DefinitionScope`
- `impl DefinitionScope`
- `impl DefinitionSource`
- `discover_definition_roots`
- `discover_skill_roots`
- `push_unique_root`
- `push_unique_skill_root`
- `definition_source_id`
- `definition_source_json`
- `config_source_id`
- `config_source_json`

At the top of `definition.rs`, use:

```rust
use std::env;
use std::path::{Path, PathBuf};

use runtime::ConfigSource;
use serde_json::{json, Value};
```

Make the shared types and helpers `pub(crate)` instead of private:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DefinitionSource {
    ProjectClaw,
    ProjectCodex,
    ProjectClaude,
    UserClawConfigHome,
    UserCodexHome,
    UserClaw,
    UserCodex,
    UserClaude,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DefinitionScope {
    Project,
    UserConfigHome,
    UserHome,
}
```

- [ ] **Step 2: Move shared argument helpers**

Move these existing items unchanged into `rust/crates/commands/src/shared_args.rs`:

- `normalize_optional_args`
- `is_help_arg`
- `help_path_from_args`

The file content should be:

```rust
pub(crate) fn normalize_optional_args(args: Option<&str>) -> Option<&str> {
    args.map(str::trim).filter(|value| !value.is_empty())
}

pub(crate) fn is_help_arg(arg: &str) -> bool {
    matches!(arg, "help" | "-h" | "--help")
}

pub(crate) fn help_path_from_args(args: &str) -> Option<Vec<&str>> {
    let parts = args.split_whitespace().collect::<Vec<_>>();
    let help_index = parts.iter().position(|part| is_help_arg(part))?;
    Some(parts[..help_index].to_vec())
}
```

- [ ] **Step 3: Move plugin command handling**

Move these existing items unchanged into `rust/crates/commands/src/plugins.rs`:

- `PluginsCommandResult`
- `handle_plugins_slash_command`
- `render_plugins_report`
- `render_plugin_install_report`
- `resolve_plugin_target`

At the top of `plugins.rs`, use:

```rust
use plugins::{PluginError, PluginManager, PluginSummary};
```

- [ ] **Step 4: Move agent command handling**

Move these existing items unchanged into `rust/crates/commands/src/agents.rs`:

- `AgentSummary`
- `handle_agents_slash_command`
- `handle_agents_slash_command_json`
- `load_agents_from_roots`
- `parse_toml_string`
- `render_agents_report`
- `render_agents_report_json`
- `agent_detail`
- `agent_summary_json`
- `render_agents_usage`
- `render_agents_usage_json`

At the top of `agents.rs`, use:

```rust
use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use crate::definition::{definition_source_json, discover_definition_roots, DefinitionSource};
use crate::shared_args::{help_path_from_args, is_help_arg, normalize_optional_args};
```

- [ ] **Step 5: Move skill command handling**

Move these existing items unchanged into `rust/crates/commands/src/skills.rs`:

- `SkillSlashDispatch`
- `SkillSummary`
- `SkillOrigin`
- `SkillRoot`
- `InstalledSkill`
- `SkillInstallSource`
- `handle_skills_slash_command`
- `handle_skills_slash_command_json`
- `classify_skills_slash_command`
- `resolve_skill_invocation`
- `resolve_skill_path`
- `load_skills_from_roots`
- `install_skill`
- `install_skill_into`
- `default_skill_install_root`
- `resolve_skill_install_source`
- `derive_skill_install_name`
- `sanitize_skill_invocation_name`
- `copy_directory_contents`
- `impl SkillInstallSource`
- `parse_skill_frontmatter`
- `unquote_frontmatter_value`
- `render_skills_report`
- `render_skills_report_json`
- `render_skill_install_report`
- `render_skill_install_report_json`
- `skill_origin_id`
- `skill_origin_json`
- `skill_summary_json`
- `render_skills_usage`
- `render_skills_usage_json`

At the top of `skills.rs`, use:

```rust
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::definition::{
    definition_source_json, discover_skill_roots, push_unique_skill_root, DefinitionSource,
};
use crate::shared_args::{help_path_from_args, is_help_arg, normalize_optional_args};
```

- [ ] **Step 6: Move MCP command handling**

Move these existing items unchanged into `rust/crates/commands/src/mcp.rs`:

- `handle_mcp_slash_command`
- `handle_mcp_slash_command_json`
- `render_mcp_report_for`
- `render_mcp_report_json_for`
- `render_mcp_summary_report`
- `render_mcp_summary_report_json`
- `render_mcp_server_report`
- `render_mcp_server_report_json`
- `render_mcp_usage`
- `render_mcp_usage_json`
- `config_source_label`
- `mcp_transport_label`
- `mcp_server_summary`
- `format_optional_list`
- `format_optional_keys`
- `format_mcp_oauth`
- `mcp_transport_json`
- `mcp_oauth_json`
- `mcp_server_details_json`
- `mcp_server_json`

At the top of `mcp.rs`, use:

```rust
use std::collections::BTreeMap;
use std::path::Path;

use runtime::{
    ConfigLoader, ConfigSource, McpOAuthConfig, McpServerConfig, ScopedMcpServerConfig,
};
use serde_json::{json, Value};

use crate::definition::config_source_json;
use crate::shared_args::{help_path_from_args, is_help_arg, normalize_optional_args};
```

- [ ] **Step 7: Leave session compaction handling in lib**

Keep `handle_slash_command` in `lib.rs` for this phase. It uses `Session`, `CompactionConfig`, `compact_session_with_memory`, and `partial_compact_session` and is not a priority split. Ensure it imports `SlashCommand` from `parse` through the crate root.

- [ ] **Step 8: Run commands checks**

Run from `rust/`:

```bash
cargo test -p commands
cargo test -p rusty-claude-cli --test resume_slash_commands
```

Expected: PASS.

- [ ] **Step 9: Commit commands split**

Run from the repository root:

```bash
git add rust/crates/commands
git commit -m "refactor: split commands crate modules"
```

---

## Task 4: Split Tools Registries, Specs, And Dispatch

**Files:**
- Modify: `rust/crates/tools/src/lib.rs`
- Create: `rust/crates/tools/src/registries.rs`
- Create: `rust/crates/tools/src/registry.rs`
- Create: `rust/crates/tools/src/specs.rs`
- Create: `rust/crates/tools/src/dispatch.rs`

- [ ] **Step 1: Move global runtime registries**

Move these existing items unchanged into `rust/crates/tools/src/registries.rs`:

- `global_lsp_registry`
- `global_mcp_registry`
- `global_team_registry`
- `global_cron_registry`
- `global_task_registry`
- `global_worker_registry`
- `agent_debug_log`

At the top of `registries.rs`, use:

```rust
use runtime::{
    lsp_client::LspRegistry, mcp_tool_bridge::McpToolRegistry, task_registry::TaskRegistry,
    team_cron_registry::{CronRegistry, TeamRegistry}, worker_boot::WorkerRegistry,
};
```

Make each helper `pub(crate)` so tool modules can use them.

- [ ] **Step 2: Move tool registry types**

Move these existing items unchanged into `rust/crates/tools/src/registry.rs`:

- `ToolManifestEntry`
- `ToolSource`
- `ToolRegistry`
- `impl ToolRegistry`
- `ToolSpec`
- `GlobalToolRegistry`
- `RuntimeToolDefinition`
- `impl GlobalToolRegistry`
- `normalize_tool_name`
- `permission_mode_from_plugin`

At the top of `registry.rs`, use:

```rust
use std::collections::{BTreeMap, BTreeSet};

use plugins::PluginTool;
use runtime::{permission_enforcer::PermissionEnforcer, PermissionMode};
use serde_json::Value;

use crate::dispatch::execute_tool_with_enforcer;
use crate::specs::mvp_tool_specs;
use crate::tool_search::execute_tool_search;
```

- [ ] **Step 3: Move built-in tool specs**

Move `mvp_tool_specs` and `is_background_task_tool_name` unchanged into `rust/crates/tools/src/specs.rs`.

At the top of `specs.rs`, use:

```rust
use runtime::PermissionMode;
use serde_json::json;

use crate::registry::ToolSpec;
```

- [ ] **Step 4: Move dispatch entrypoints**

Move these existing items unchanged into `rust/crates/tools/src/dispatch.rs`:

- `enforce_permission_check`
- `execute_tool`
- `render_tool_result_for_model`
- `execute_tool_with_enforcer`
- `maybe_enforce_permission_check`
- `from_value`
- `to_pretty_json`
- `io_to_string`

At the top of `dispatch.rs`, use:

```rust
use runtime::permission_enforcer::{EnforcementResult, PermissionEnforcer};
use serde::Deserialize;
use serde_json::Value;
```

Mark these helpers `pub(crate)`:

```rust
pub(crate) fn from_value<T: for<'de> Deserialize<'de>>(input: &Value) -> Result<T, String> {
    serde_json::from_value(input.clone()).map_err(|error| error.to_string())
}

pub(crate) fn to_pretty_json<T: serde::Serialize>(value: T) -> Result<String, String> {
    serde_json::to_string_pretty(&value).map_err(|error| error.to_string())
}
```

Also change the moved `execute_tool_with_enforcer` function from private `fn` to `pub(crate) fn`; keep its existing match body unchanged.

- [ ] **Step 5: Replace tools crate root with facade imports for these modules**

Update `rust/crates/tools/src/lib.rs` to start with:

```rust
mod agent;
mod basic;
mod config;
mod dispatch;
mod lsp;
mod mcp;
mod notebook;
mod registries;
mod registry;
mod repl;
mod skill;
mod specs;
mod tasks;
mod team_cron;
mod todo;
mod tool_search;
mod web;
mod workers;

pub mod lane_completion;
pub mod pdf_extract;

pub use dispatch::{execute_tool, enforce_permission_check, render_tool_result_for_model};
pub use registry::{
    GlobalToolRegistry, RuntimeToolDefinition, ToolManifestEntry, ToolRegistry, ToolSource,
};
pub use specs::{is_background_task_tool_name, mvp_tool_specs};
pub use tool_search::ToolSearchOutput;
```

- [ ] **Step 6: Run tools public API guard**

Run from `rust/`:

```bash
cargo test -p tools --test public_api
```

Expected: PASS after import and visibility fixes.

---

## Task 5: Split Tools Direct Handler Modules

**Files:**
- Modify: `rust/crates/tools/src/dispatch.rs`
- Create: `rust/crates/tools/src/basic.rs`
- Create: `rust/crates/tools/src/tasks.rs`
- Create: `rust/crates/tools/src/workers.rs`
- Create: `rust/crates/tools/src/team_cron.rs`
- Create: `rust/crates/tools/src/lsp.rs`
- Create: `rust/crates/tools/src/mcp.rs`
- Create: `rust/crates/tools/src/todo.rs`

- [ ] **Step 1: Move basic file and shell handlers**

Move these existing items unchanged into `rust/crates/tools/src/basic.rs`:

- `ReadFileInput`
- `WriteFileInput`
- `EditFileInput`
- `GlobSearchInputValue`
- `SleepInput`
- `BriefInput`
- `BriefStatus`
- `PowerShellInput`
- `AskUserQuestionInput`
- `RemoteTriggerInput`
- `TestingPermissionInput`
- `SleepOutput`
- `BriefOutput`
- `ResolvedAttachment`
- `run_ask_user_question`
- `run_bash`
- `workspace_test_branch_preflight`
- `is_workspace_test_command`
- `normalize_shell_command`
- `resolve_main_ref`
- `git_ref_exists`
- `git_stdout`
- `branch_divergence_output`
- `run_read_file`
- `run_write_file`
- `run_edit_file`
- `run_glob_search`
- `run_grep_search`
- `run_sleep`
- `run_brief`
- `run_powershell`
- `run_structured_output`
- `run_testing_permission`
- `execute_sleep`
- `execute_brief`
- `resolve_attachment`
- `is_image_path`
- `execute_structured_output`
- `execute_powershell`
- `detect_powershell_shell`
- `command_exists`
- `execute_shell_command`
- `iso8601_now`

Expose only run functions needed by `dispatch.rs` as `pub(crate)`.

- [ ] **Step 2: Move task handlers**

Move task DTOs and task handlers into `rust/crates/tools/src/tasks.rs`:

- `TaskCreateInput`
- `TaskIdInput`
- `TaskOutputInput`
- `TaskUpdateInput`
- `TaskOutputPayload`
- `default_task_output_block`
- `default_task_output_timeout_ms`
- `run_task_create`
- `run_task_packet`
- `run_task_get`
- `run_task_list`
- `run_task_stop`
- `run_task_update`
- `run_task_output`
- `task_output_payload`

At the top of `tasks.rs`, use:

```rust
use std::time::{Duration, Instant};

use runtime::{task_registry::TaskStatus, TaskPacket};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::dispatch::to_pretty_json;
use crate::registries::{agent_debug_log, global_task_registry};
```

- [ ] **Step 3: Move worker handlers**

Move worker DTOs and handlers into `rust/crates/tools/src/workers.rs`:

- `WorkerCreateInput`
- `WorkerIdInput`
- `WorkerObserveCompletionInput`
- `WorkerObserveInput`
- `WorkerSendPromptInput`
- `default_auto_recover_prompt_misdelivery`
- `run_worker_create`
- `run_worker_get`
- `run_worker_observe`
- `run_worker_resolve_trust`
- `run_worker_await_ready`
- `run_worker_send_prompt`
- `run_worker_restart`
- `run_worker_terminate`
- `run_worker_observe_completion`

- [ ] **Step 4: Move team and cron handlers**

Move these items into `rust/crates/tools/src/team_cron.rs`:

- `TeamCreateInput`
- `TeamDeleteInput`
- `CronCreateInput`
- `CronDeleteInput`
- `run_team_create`
- `run_team_delete`
- `run_cron_create`
- `run_cron_delete`
- `run_cron_list`

- [ ] **Step 5: Move LSP and MCP handlers**

Move LSP items into `rust/crates/tools/src/lsp.rs`:

- `LspInput`
- `run_lsp`

Move MCP items into `rust/crates/tools/src/mcp.rs`:

- `McpResourceInput`
- `McpAuthInput`
- `McpToolInput`
- `run_list_mcp_resources`
- `run_read_mcp_resource`
- `run_mcp_auth`
- `run_mcp_tool`

- [ ] **Step 6: Move todo handlers**

Move todo DTOs and handlers into `rust/crates/tools/src/todo.rs`:

- `TodoWriteInput`
- `TodoItem`
- `TodoStatus`
- `TodoWriteOutput`
- `run_todo_write`
- `execute_todo_write`
- `validate_todos`
- `todo_store_path`

- [ ] **Step 7: Update dispatch imports and match arms**

In `rust/crates/tools/src/dispatch.rs`, import each run function from its new module. Keep the match arm strings unchanged:

```rust
use crate::basic::{
    run_ask_user_question, run_bash, run_brief, run_edit_file, run_glob_search, run_grep_search,
    run_powershell, run_read_file, run_sleep, run_structured_output, run_testing_permission,
    run_write_file,
};
use crate::lsp::run_lsp;
use crate::mcp::{run_list_mcp_resources, run_mcp_auth, run_mcp_tool, run_read_mcp_resource};
use crate::tasks::{
    run_task_create, run_task_get, run_task_list, run_task_output, run_task_packet, run_task_stop,
    run_task_update,
};
use crate::team_cron::{run_cron_create, run_cron_delete, run_cron_list, run_team_create, run_team_delete};
use crate::todo::run_todo_write;
use crate::workers::{
    run_worker_await_ready, run_worker_create, run_worker_get, run_worker_observe,
    run_worker_observe_completion, run_worker_resolve_trust, run_worker_restart,
    run_worker_send_prompt, run_worker_terminate,
};
```

- [ ] **Step 8: Run focused tools check**

Run from `rust/`:

```bash
cargo test -p tools --test public_api
cargo test -p tools --lib
```

Expected: PASS after import and visibility fixes.

---

## Task 6: Split Tools Web, Skill, Notebook, Config, REPL, And Search

**Files:**
- Modify: `rust/crates/tools/src/dispatch.rs`
- Create: `rust/crates/tools/src/web.rs`
- Create: `rust/crates/tools/src/skill.rs`
- Create: `rust/crates/tools/src/notebook.rs`
- Create: `rust/crates/tools/src/config.rs`
- Create: `rust/crates/tools/src/repl.rs`
- Create: `rust/crates/tools/src/tool_search.rs`

- [ ] **Step 1: Move web handlers**

Move these items into `rust/crates/tools/src/web.rs`:

- `WebFetchInput`
- `WebSearchInput`
- `WebFetchOutput`
- `WebSearchOutput`
- `WebSearchResultItem`
- `SearchHit`
- `run_web_fetch`
- `run_web_search`
- `execute_web_fetch`
- `execute_web_search`
- `build_http_client`
- `normalize_fetch_url`
- `build_search_url`
- `normalize_fetched_content`
- `summarize_web_fetch`
- `extract_title`
- `html_to_text`
- `decode_html_entities`
- `collapse_whitespace`
- `preview_text`
- `extract_search_hits`
- `extract_search_hits_from_generic_links`
- `extract_quoted_value`
- `decode_duckduckgo_redirect`
- `html_entity_decode_url`
- `host_matches_list`
- `normalize_domain_filter`
- `dedupe_hits`

- [ ] **Step 2: Move skill handlers**

Move these items into `rust/crates/tools/src/skill.rs`:

- `SkillInput`
- `SkillOutput`
- `run_skill`
- `execute_skill`
- `resolve_skill_path`
- `resolve_skill_path_from_compat_roots`
- `SkillLookupOrigin`
- `SkillLookupRoot`
- `skill_lookup_roots`
- `push_project_skill_lookup_roots`
- `push_home_skill_lookup_roots`
- `push_prefixed_skill_lookup_roots`
- `push_skill_lookup_root`
- `resolve_skill_path_in_root`
- `resolve_skill_path_in_skills_dir`
- `resolve_skill_path_in_legacy_commands_dir`
- `skill_frontmatter_name_matches`
- `parse_skill_name`
- `parse_skill_frontmatter_value`
- `parse_skill_description`

- [ ] **Step 3: Move notebook handlers**

Move these items into `rust/crates/tools/src/notebook.rs`:

- `NotebookEditInput`
- `NotebookCellType`
- `NotebookEditMode`
- `NotebookEditOutput`
- `run_notebook_edit`
- `execute_notebook_edit`
- `require_notebook_source`
- `build_notebook_cell`
- `cell_kind`
- `resolve_cell_index`
- `source_lines`
- `format_notebook_edit_mode`
- `make_cell_id`

- [ ] **Step 4: Move config handlers**

Move these items into `rust/crates/tools/src/config.rs`:

- `ConfigInput`
- `EnterPlanModeInput`
- `ExitPlanModeInput`
- `ConfigValue`
- `ConfigOutput`
- `PlanModeState`
- `PlanModeOutput`
- `PERMISSION_DEFAULT_MODE_PATH`
- `run_config`
- `run_enter_plan_mode`
- `run_exit_plan_mode`
- `execute_config`
- `execute_enter_plan_mode`
- `execute_exit_plan_mode`
- `ConfigScope`
- `ConfigSettingSpec`
- `ConfigKind`
- `supported_config_setting`
- `normalize_config_value`
- `config_file_for_scope`
- `config_home_dir`
- `read_json_object`
- `write_json_object`
- `get_nested_value`
- `set_nested_value`
- `remove_nested_value`
- `plan_mode_state_file`
- `read_plan_mode_state`
- `write_plan_mode_state`
- `clear_plan_mode_state`
- `iso8601_timestamp`

- [ ] **Step 5: Move REPL handlers**

Move these items into `rust/crates/tools/src/repl.rs`:

- `ReplInput`
- `ReplOutput`
- `ReplRuntime`
- `run_repl`
- `execute_repl`
- `resolve_repl_runtime`
- `detect_first_command`

- [ ] **Step 6: Move tool search**

Move these items into `rust/crates/tools/src/tool_search.rs`:

- `ToolSearchInput`
- `ToolSearchOutput`
- `SearchableToolSpec`
- `run_tool_search`
- `execute_tool_search`
- `deferred_tool_specs`
- `search_tool_specs`
- `normalize_tool_search_query`
- `canonical_tool_token`

- [ ] **Step 7: Update dispatch imports**

In `rust/crates/tools/src/dispatch.rs`, add imports:

```rust
use crate::config::{run_config, run_enter_plan_mode, run_exit_plan_mode};
use crate::notebook::run_notebook_edit;
use crate::repl::run_repl;
use crate::skill::run_skill;
use crate::tool_search::run_tool_search;
use crate::web::{run_web_fetch, run_web_search};
```

- [ ] **Step 8: Run focused tools check**

Run from `rust/`:

```bash
cargo test -p tools --test public_api
cargo test -p tools --lib
```

Expected: PASS.

---

## Task 7: Split Tools Agent Module

**Files:**
- Modify: `rust/crates/tools/src/dispatch.rs`
- Create: `rust/crates/tools/src/agent/mod.rs`

- [ ] **Step 1: Move agent DTOs and public model rendering helpers**

Move these items into `rust/crates/tools/src/agent/mod.rs`:

- `AgentInput`
- `AgentOutput`
- `AsyncAgentLaunchOutput`
- `AgentToolOutput`
- `AgentJob`
- `DEFAULT_AGENT_MODEL`
- `FALLBACK_AGENT_SYSTEM_DATE`
- `DEFAULT_AGENT_MAX_ITERATIONS`
- `run_agent`
- `render_async_agent_launch_for_model`
- `render_completed_agent_result_for_model`

Expose `run_agent`, `render_async_agent_launch_for_model`, and `render_completed_agent_result_for_model` as `pub(crate)` by changing only their visibility; keep their moved bodies unchanged.

- [ ] **Step 2: Move agent execution and persistence**

Move these items unchanged into `agent/mod.rs`:

- `execute_agent`
- `execute_agent_with_mode`
- `execute_agent_with_spawn`
- `build_async_agent_launch_output`
- `spawn_agent_job`
- `enqueue_background_agent_notification`
- `run_agent_job`
- `build_agent_runtime`
- `new_agent_session`
- `build_agent_system_prompt`
- `agent_system_date`
- `resolve_agent_model`
- `allowed_tools_for_subagent`
- `agent_permission_policy`
- `write_agent_manifest`
- `read_agent_manifest`
- `persist_agent_terminal_state`
- `derive_agent_state`
- `maybe_commit_provenance`
- `extract_commit_sha`
- `current_git_branch`
- `append_agent_output`
- `format_agent_terminal_output`
- `classify_lane_blocker`
- `classify_lane_failure`
- `ProviderEntry`
- `ProviderRuntimeClient`
- `impl ProviderRuntimeClient`
- `build_provider_entry`
- `resolve_subagent_auth_source`
- `load_subagent_oauth_config_for`
- `default_subagent_oauth_config`
- `load_provider_fallback_config`
- `impl ApiClient for ProviderRuntimeClient`
- `stream_with_provider`
- `SubagentToolExecutor`
- `impl SubagentToolExecutor`
- `impl ToolExecutor for SubagentToolExecutor`
- `tool_specs_for_allowed_tools`
- `tool_definitions_for_request_tools`
- `normalize_tool_input_json`
- `normalize_tool_input_string`
- `debug_json_value_summary`
- `debug_json_input_summary`
- `debug_labeled_json_input_summary`
- `should_log_stream_event_for_tool_diagnostics`
- `stream_event_debug_summary`
- `pending_tools_debug_summary`
- `json_debug_string`
- `json_debug_suffix`
- `convert_messages`
- `apply_tool_cache_controls`
- `push_output_block`
- `track_tool_block_index`
- `should_log_tool_stop_without_start`
- `response_to_events`
- `push_prompt_cache_record`
- `prompt_cache_record_to_runtime_event`
- `final_assistant_text`
- `agent_store_dir`
- `make_agent_id`
- `slugify_agent_name`
- `normalize_subagent_type`
- `iso8601_now`

- [ ] **Step 3: Update dispatch model rendering**

In `rust/crates/tools/src/dispatch.rs`, import:

```rust
use crate::agent::{render_async_agent_launch_for_model, render_completed_agent_result_for_model, run_agent};
```

Keep `render_tool_result_for_model` behavior unchanged.

- [ ] **Step 4: Run focused tools check**

Run from `rust/`:

```bash
cargo test -p tools --test public_api
cargo test -p tools --lib
```

Expected: PASS.

- [ ] **Step 5: Commit tools split**

Run from the repository root:

```bash
git add rust/crates/tools
git commit -m "refactor: split tools crate modules"
```

---

## Task 8: Full Verification And Cleanup

**Files:**
- Modify only files already touched in `rust/crates/commands/**` and `rust/crates/tools/**`

- [ ] **Step 1: Format**

Run from `rust/`:

```bash
cargo fmt --all
```

Expected: command exits 0.

- [ ] **Step 2: Run workspace clippy**

Run from `rust/`:

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: command exits 0. Fix only warnings caused by this refactor.

- [ ] **Step 3: Run workspace tests**

Run from `rust/`:

```bash
cargo test --workspace
```

Expected: command exits 0.

- [ ] **Step 4: Run full repository test wrapper**

Run from the repository root:

```bash
./scripts/run_all_tests.sh
```

Expected: command exits 0.

- [ ] **Step 5: Inspect final diff**

Run from the repository root:

```bash
git status --short
git diff --stat
```

Expected: changed tracked files are limited to `rust/crates/commands/**`, `rust/crates/tools/**`, and committed plan/test files from this work. Existing unrelated untracked files can remain visible in `git status`; do not add them.

- [ ] **Step 6: Commit verification cleanup**

If formatting or clippy produced changes after the previous commits, run:

```bash
git add rust/crates/commands rust/crates/tools
git commit -m "chore: finalize rust crate organization"
```

Expected: commit contains only formatting or import cleanup for this refactor.

---

## Self-Review Notes

- Spec coverage: Tasks 2 and 3 cover `commands`; Tasks 4 through 7 cover `tools`; Task 8 covers the required verification commands from the approved spec.
- Public compatibility: Task 1 creates root API guard tests before moving code.
- Scope control: No task edits `rusty-claude-cli`, `runtime`, or `api`; compile errors that appear to require outside edits must be reported before any outside file is changed.
- Parallel execution: Commands and tools tasks have disjoint write sets after Task 1 and can be assigned to separate subagents. Verification is read-only and can run after each implementation worker finishes.
