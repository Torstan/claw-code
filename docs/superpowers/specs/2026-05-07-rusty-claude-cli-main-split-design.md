# Rusty Claude CLI Main Split Design

## Context

`rust/crates/rusty-claude-cli` is a binary crate. Its stable surface is the
`claw` executable and the behavior covered by integration tests. The current
`src/main.rs` is about 13,700 lines and owns many unrelated responsibilities:
CLI entry and error formatting, argument parsing, one-shot prompt dispatch,
REPL handling, managed sessions, status and doctor reports, runtime/plugin/MCP
construction, provider streaming, tool execution, tool display formatting, and
unit tests.

The approved approach is conservative mechanical splitting. This phase should
move code into focused private modules while preserving behavior. It should not
redesign slash-command handling, parser semantics, provider routing, runtime
construction, or public crate shape.

## Goals

- Keep `main.rs` as a thin binary entrypoint.
- Split the oversized `main.rs` into focused sibling modules under
  `rust/crates/rusty-claude-cli/src/`.
- Use clearer module names based on responsibility.
- Preserve CLI behavior, output contracts, exit codes, JSON envelopes, session
  compatibility, tool rendering, and runtime behavior.
- Increase reuse only where it falls out naturally from moving shared helpers.

## Non-Goals

- Do not add `src/lib.rs` or create a public library API.
- Do not rewrite argument parsing.
- Do not unify CLI, REPL, and resume slash-command dispatch in this pass.
- Do not rename behavior-sensitive symbols unless needed for module visibility.
- Do not rename `AnthropicRuntimeClient` in this pass, even though the name is
  stale, because the approved scope prioritizes safe movement over cleanup.
- Do not change tool output formatting, ANSI rendering, JSON shapes, tool
  schemas, permission semantics, prompt-cache behavior, or MCP protocol behavior.

## Architecture

Keep `src/main.rs` as the binary entrypoint. It should contain module
declarations, `main()`, top-level error formatting, and `run()` action dispatch.
This keeps process startup, JSON error-envelope detection, and exit behavior
easy to audit.

Add private sibling modules:

- `args.rs`: `CliAction`, `CliOutputFormat`, `LocalHelpTopic`, `parse_args`,
  and CLI option helpers.
- `doctor.rs`: diagnostic checks and doctor report rendering.
- `status.rs`: status, sandbox, git summary types, and renderers.
- `sessions.rs`: managed session path/reference/list/delete helpers.
- `resume.rs`: `resume_session`, `run_resume_command`, and resume command
  outcome handling.
- `repl.rs`: `run_repl`, `LiveCli`, REPL command handling, and prompt history.
- `runtime_bridge.rs`: runtime/plugin/MCP construction glue, `BuiltRuntime`,
  hook progress reporting, and permission prompting.
- `provider_client.rs`: current `AnthropicRuntimeClient` provider-streaming
  implementation and API error formatting.
- `tool_display.rs`: tool call/result formatting, truncation, and JSON display
  helpers.
- `tool_executor.rs`: `CliToolExecutor`, parallel execution, debug logging, and
  permission policy.
- `reports.rs`: small text report formatters shared by commands.

Use `pub(crate)` only where cross-module access or existing unit tests require
it. Avoid making internals `pub`.

## Data Flow

Startup remains unchanged:

1. `main()` collects process context and calls `run()`.
2. `run()` calls `args::parse_args()`.
3. `run()` dispatches on `CliAction`.

One-shot prompt and REPL flows keep the current runtime path:

1. `args` produces `CliAction`.
2. `run()` dispatches to `repl::run_repl`, `resume::resume_session`,
   `doctor::run_doctor`, `status::print_status_snapshot`, and similar module
   entrypoints.
3. `repl::LiveCli` uses `runtime_bridge::build_runtime`.
4. `runtime_bridge` constructs
   `ConversationRuntime<provider_client::AnthropicRuntimeClient,
   tool_executor::CliToolExecutor>`.
5. `provider_client` streams API events and calls `tool_display` helpers for
   live output.
6. `tool_executor` runs tool calls and calls `tool_display` for results.
7. `sessions` owns session reference resolution and list/delete helpers.

The tests currently inside `main.rs` may stay in `main.rs` with imports adjusted
or move in small groups next to their modules. Choose the lower-risk option per
module during implementation.

## Error Handling And Behavior Preservation

No intentional behavior changes are allowed. Preserve:

- `main()` stderr behavior, JSON error-envelope detection, and exit code `1`.
- `--output-format json` contracts for local commands.
- `parse_args` outcomes, including current side effects through model/config/tool
  resolution.
- stdin behavior for piped no-arg prompt versus prompt-mode stdin merging.
- broad-CWD preflight and stale-base preflight timing.
- session lookup compatibility for `.jsonl`, legacy `.json`,
  `latest`/`last`/`recent`, and legacy flat session lookup.
- compact output cleanliness.
- tool stream rendering, ANSI output, truncation, and result summaries.
- MCP degraded/pending behavior and runtime tool wrappers.
- provider streaming fallback, prompt-cache diagnostics, and user-visible API
  error formatting.

## Testing

Use targeted checks while splitting:

```bash
cd rust
cargo check -p rusty-claude-cli
cargo test -p rusty-claude-cli
```

On finishing coding, run the required Rust verification sequence from `rust/` in
order:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Before committing code, run the repository test wrapper from the repository
root:

```bash
./scripts/run_all_tests.sh
```

Key regression surfaces include JSON output contracts, resume slash commands,
compact stdout cleanliness, CLI flags/config defaults, system prompt
attachments, and the mock parity harness.

## Risks

- `parse_args` has hidden config/tool-resolution side effects. Moving it must
  preserve those effects.
- CLI, REPL, and resume slash-command paths duplicate related behavior. This
  split should not try to unify them yet.
- Tool display strings and ANSI output are user-facing and indirectly tested.
- Existing unit tests import many private symbols from `main.rs`; splitting may
  require careful `pub(crate)` exposure.
- The workspace contains unrelated modified and untracked files. Implementation
  commits must stage only files touched for this refactor.

## Acceptance Criteria

- `main.rs` is a thin entrypoint and dispatch layer.
- The implementation currently concentrated in `main.rs` is moved into focused
  private modules with minimal behavior-preserving visibility changes.
- No `src/lib.rs` is added for `rusty-claude-cli`.
- Existing `claw` CLI behavior and integration-test contracts are preserved.
- The required verification commands pass before code is committed.
