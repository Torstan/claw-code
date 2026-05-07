# Rust Crates Organization Design

## Context

The first implementation phase reorganizes Rust code under `rust/crates` without changing behavior. Repository exploration found the largest hotspots in `rusty-claude-cli/src/main.rs`, `tools/src/lib.rs`, `commands/src/lib.rs`, and several runtime/API files. The approved first phase intentionally limits code changes to the safer library-crate scope:

- `rust/crates/commands`
- `rust/crates/tools`

The CLI main flow, runtime crate, API crate, and provider code remain out of scope for this phase.

## Goals

- Improve names and module boundaries in `commands` and `tools`.
- Split very long Rust source files into focused Rust directory modules.
- Keep crate-root compatibility facades so existing callers do not learn the new internal layout.
- Increase small-scale reuse where it is clearly local and low risk.
- Preserve public API paths, user-facing behavior, JSON output, tool names, tool schemas, permissions, and slash-command text behavior.

## Non-Goals

- Do not rewrite CLI behavior, runtime orchestration, provider clients, prompt-cache behavior, MCP protocol behavior, or session persistence.
- Do not introduce new feature behavior.
- Do not introduce broad new abstractions while moving code.
- Do not rename public symbols unless the old path is preserved with a root re-export.

## Approved Approach

Use a compatibility facade plus coarse-grained module splits.

`commands/src/lib.rs` and `tools/src/lib.rs` stay as crate entrypoints. They should mainly declare modules and re-export the public API that current consumers already use. Internal modules can use `pub(crate)` for cross-module helpers when needed.

This phase prefers moving code into clearer homes over rewriting code. New helper extraction is allowed only for small, repeated, local patterns such as JSON dispatch helpers and permission checks.

## Commands Crate Design

`commands/src/lib.rs` becomes a facade over these modules:

- `registry.rs`: `CommandManifestEntry`, `CommandSource`, and `CommandRegistry`.
- `spec.rs`: `SlashCommandSpec`, the static slash-command table, and command categories.
- `parse.rs`: `SlashCommand`, parse validation, parse helper functions, and `SlashCommandParseError`.
- `help.rs`: slash-command help rendering, detail rendering, suggestions, and ranking helpers.
- `plugins.rs`: plugin slash-command handling and report rendering.
- `agents.rs`: agent discovery and report rendering.
- `skills.rs`: skill discovery, skill invocation resolution, skill install handling, and reports.
- `mcp.rs`: MCP command handling and report rendering.
- `simplify.rs`: keep the existing prompt-backed simplify command in place for now.

The crate root must continue to expose existing public items, including:

- `build_simplify_prompt`
- `classify_skills_slash_command`
- `handle_agents_slash_command`
- `handle_agents_slash_command_json`
- `handle_mcp_slash_command`
- `handle_mcp_slash_command_json`
- `handle_plugins_slash_command`
- `handle_skills_slash_command`
- `handle_skills_slash_command_json`
- `render_slash_command_help`
- `render_slash_command_help_filtered`
- `resolve_skill_invocation`
- `resume_supported_slash_commands`
- `slash_command_specs`
- `validate_slash_command_input`
- `SkillSlashDispatch`
- `SlashCommand`

## Tools Crate Design

`tools/src/lib.rs` becomes a facade over these modules:

- `registry.rs`: `ToolManifestEntry`, `ToolSource`, `ToolRegistry`, `ToolSpec`, `GlobalToolRegistry`, and `RuntimeToolDefinition`.
- `specs.rs`: `mvp_tool_specs()`, built-in tool schema definitions, permission requirements, and `is_background_task_tool_name`.
- `dispatch.rs`: `execute_tool`, permission enforcement, JSON deserialization dispatch, `render_tool_result_for_model`, `from_value`, and `to_pretty_json`.
- `basic.rs`: bash, read/write/edit file, glob, grep, sleep, brief, and other small direct handlers.
- `web.rs`: web fetch and web search implementation.
- `skill.rs`: skill resolution and execution.
- `notebook.rs`: notebook edit implementation.
- `config.rs`: config tool and plan-mode state helpers.
- `repl.rs`: REPL-like subprocess tool.
- `tasks.rs`: task tool handlers.
- `workers.rs`: worker tool handlers.
- `team_cron.rs`: team and cron tool handlers.
- `mcp.rs`: MCP resource/auth/tool handlers.
- `lsp.rs`: LSP tool handler.
- `tool_search.rs`: `ToolSearchOutput`, deferred tool search, scoring, and canonicalization.
- `agent/mod.rs`: agent orchestration. If visibility costs remain low, split further into `manifest.rs`, `runtime.rs`, `provider.rs`, `stream.rs`, and `lane.rs`; otherwise keep the first implementation coarse in `agent/mod.rs`.

Existing public root exports must remain available to consumers such as `rusty-claude-cli` and `compat-harness`, especially:

- `execute_tool`
- `is_background_task_tool_name`
- `mvp_tool_specs`
- `render_tool_result_for_model`
- `GlobalToolRegistry`
- `RuntimeToolDefinition`
- `ToolManifestEntry`
- `ToolRegistry`
- `ToolSource`
- `ToolSearchOutput`

Existing public modules `lane_completion` and `pdf_extract` remain available. They are not priority split targets for the first implementation phase.

## Data Flow And Visibility

The `commands` crate keeps the same caller data flow. Callers still enter through crate-root public functions and types. Internally, parsing, spec lookup, help rendering, and command-family handlers can call each other through `pub(crate)` helpers.

The `tools` crate keeps the same caller data flow. `GlobalToolRegistry` remains the tool definition/search/execution entrypoint. `execute_tool` remains the direct execution entrypoint. The dispatch layer maps tool names and JSON inputs to concrete module handlers. Concrete modules preserve existing DTOs, handler behavior, output JSON, and error text.

Root facades should avoid re-exporting new internals unless a current external caller needs them.

## Error Handling

Error handling stays behavior-preserving:

- Do not introduce a new shared error enum for this refactor.
- Preserve user-visible error strings wherever possible.
- Keep current `Result<_, String>`, `std::io::Result<_>`, `runtime::ConfigError`, and plugin error boundaries.
- Local changes required by module visibility or clippy are allowed if they do not alter behavior.

## Reuse

Allowed low-risk reuse:

- Centralize JSON dispatch helpers such as `from_value` and `to_pretty_json`.
- Centralize permission enforcement used by tool dispatch.
- Centralize empty JSON input normalization if it can be done without broad churn.
- Keep tool-name constants near `specs.rs` where useful, but do not force every string match to convert in phase one.

Avoid broad rewrites of report formatting, command parsing, tool schemas, provider streaming, agent orchestration, or file persistence.

## Risks

- `commands` is a library crate. Public item paths and signatures must remain stable through `lib.rs` re-exports.
- `tools` is consumed by `rusty-claude-cli` and `compat-harness`. Public root exports must remain stable.
- Tool names, schemas, permissions, and ordering from `mvp_tool_specs()` are user-facing and cache-sensitive.
- CLI output and slash-command help text may be implicitly tested even when there are no snapshots.
- Moving tests may require careful `pub(crate)` exposure rather than making internals public.
- The current workspace contains unrelated untracked and modified files. Implementation commits must add only files touched for this refactor.

## Verification

During implementation, use focused checks before the full suite:

```bash
cd rust
cargo test -p commands
cargo test -p tools --lib
cargo test -p compat-harness
cargo test -p rusty-claude-cli
```

On finishing coding, run the required Rust verification sequence from `rust/`:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Before committing code, run the full repository test wrapper from the repository root:

```bash
./scripts/run_all_tests.sh
```

## Acceptance Criteria

- `commands/src/lib.rs` and `tools/src/lib.rs` are compatibility facades over clearer internal modules.
- `commands` and `tools` no longer concentrate most implementation in one oversized `lib.rs`.
- Existing public imports used by `rusty-claude-cli`, `compat-harness`, and tests continue to compile.
- Tool schemas, tool names, permission requirements, slash-command help behavior, and JSON output remain unchanged except for formatting changes caused by `cargo fmt`.
- The required verification commands pass.
