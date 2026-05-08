# Rust Risk Fix Design

Date: 2026-05-08

## Purpose

Fix the 17 confirmed Rust implementation bugs documented in
`docs/rust-risk-test-confirmation.md`. Python code is out of scope.

The repair will use subagent-driven development with parallel workers wherever
the write sets can be kept independent. Each worker owns one subsystem group,
runs the relevant ignored confirmation tests until they pass, runs default
regression tests for the touched crate, and then goes through spec-compliance
and code-quality review before integration is considered complete.

## Scope

In scope:

- Runtime permission, sandbox status, file operation, search, patch, and debug
  logging fixes.
- API provider protocol and fallback behavior fixes.
- Tools crate subagent lifecycle, registry, notification, and concurrency fixes.
- CLI max-token selection and debug logging integration fixes.
- Updating the existing ignored confirmation tests so they become normal
  regression tests or otherwise run as part of the verification path once the
  corresponding production behavior is fixed.

Out of scope:

- Python code.
- Unrelated refactors.
- Broad redesign of provider abstractions or the agent runtime beyond what is
  needed to close the confirmed bugs.
- Changes to unrelated untracked files in the working tree.

## Confirmed Bug Map

| Issue | Area | Expected Repair |
|---:|---|---|
| 1 | runtime permissions | Prompt mode must ask a prompter for dangerous tools and deny when no prompter is available. |
| 2 | tools/runtime file dispatch | File tools must enforce the active workspace boundary for absolute and relative paths. |
| 3 | runtime sandbox | Sandbox status must not claim filesystem isolation unless a boundary is actually enforced. |
| 4 | api OpenAI-compatible | Local loopback OpenAI-compatible base URLs may run without an API key and omit authorization. |
| 5 | api OpenAI tool messages | OpenAI tool result payloads must omit Anthropic-only fields and drop orphan tool messages. |
| 6 | api OpenAI non-streaming | Non-streaming `tool_calls: null` must deserialize as an empty list. |
| 7 | CLI provider client | CLI token limits must be provider/model aware instead of always requesting 64k. |
| 8 | tools subagents | Default subagent model must inherit or use configured parent context instead of hardcoded Anthropic. |
| 9 | tools subagents | Subagent sessions must have persistence paths for large tool-result externalization. |
| 10 | tools background notifications | Background agent notifications must be bounded and reference full persisted output when needed. |
| 11 | tools/runtime task registry | Task registry operations must be session scoped and not leak through a process-wide id namespace. |
| 12 | tools/CLI agent execution | Background and parallel agent execution must have a configurable concurrency cap. |
| 13 | runtime edit_file | Non-`replace_all` edits must fail unless `old_string` matches exactly once. |
| 14 | runtime patch output | Edit patch output must emit localized hunks rather than full-file replacement. |
| 15 | runtime grep | Multiline content-mode grep must match across line boundaries and report useful line context. |
| 16 | runtime/CLI debug logging | Debug logs must redact secret-shaped values and bound large details before writing. |
| 17 | api fallback | Retry/fallback policy must avoid long primary-provider stalls before trying fallback providers. |

## Parallel Workstreams

### Worker 1: Runtime Security And File Correctness

Owned files are expected to be under `rust/crates/runtime/src/`, with any tools
integration for workspace-bound file dispatch coordinated with Worker 3.

Primary issues: 1, 2 runtime side, 3, 13, 14, 15, 16 runtime side.

Design constraints:

- Replace implicit permission ordering for prompt mode with explicit handling:
  prompt first when a dangerous action requires user approval; deny if prompt
  mode has no prompter.
- Ensure workspace-scoped file helpers are the path used for tool-facing
  read/write/edit/search calls.
- Keep sandbox reporting truthful. If the implementation only sets advisory
  environment variables, report filesystem isolation as inactive rather than
  overstating protection.
- For `edit_file`, count occurrences before writing. With `replace_all=false`,
  exactly one match is required.
- Generate localized patch hunks using an existing workspace diff helper if one
  exists; otherwise add the smallest internal helper needed for line-level
  localized output.
- For multiline grep, evaluate the regex against the full file content and map
  match ranges back to line numbers and context lines.
- Centralize debug-log redaction in the runtime logger so call sites inherit the
  protection.

### Worker 2: API Provider Protocol And Fallback

Owned files are expected to be under `rust/crates/api/src/providers/`.

Primary issues: 4, 5, 6, 17.

Design constraints:

- Treat loopback or explicitly local OpenAI-compatible base URLs as optionally
  unauthenticated. When no key is present, build the client and omit the
  `Authorization` header.
- Strip Anthropic-only fields from OpenAI tool messages.
- Make tool-message pairing validation independent of whether the preceding
  non-tool message is assistant, user, or system.
- Reuse the null-as-empty deserializer for non-streaming `ChatMessage.tool_calls`.
- Add a bounded fast-fallback policy so retryable primary-provider errors do not
  consume minutes before a configured fallback provider can be tried.

### Worker 3: Tools Agent Lifecycle And Resource Control

Owned files are expected to be under `rust/crates/tools/src/`.

Primary issues: 2 tools dispatch side, 8, 9, 10, 11, 12 tools side.

Design constraints:

- Thread the active workspace root/session context through file-tool dispatch so
  absolute paths cannot bypass workspace policy.
- Resolve the default subagent model from parent request/session configuration
  when no explicit model is provided.
- Create subagent sessions with persistence paths under the agent store.
- Bound background notification text. Include status, agent id, and a stable
  reference to full output instead of injecting large bodies.
- Scope task registry access by session or workspace namespace. Avoid tests and
  runtime behavior that depend on process-global task ids.
- Add a configurable concurrency cap for background agent execution. Excess work
  should queue or be rejected with a clear error, depending on the existing local
  execution pattern.

### Worker 4: CLI Integration

Owned files are expected to be under `rust/crates/rusty-claude-cli/src/`.

Primary issues: 7, 16 CLI side, 12 CLI side if the parallel executor owns part
of the concurrency behavior.

Design constraints:

- Replace the hardcoded `64_000` token limit with provider/model-aware limits,
  preferring existing API crate logic if it is already available without
  introducing a dependency cycle.
- Ensure CLI debug logging benefits from runtime redaction and does not pass
  avoidably large or secret-shaped raw details when a structured summary is
  sufficient.
- Apply the same agent concurrency cap semantics to CLI parallel execution if
  that path can spawn background agents independently of the tools crate.

## Coordination Rules

- Workers are not alone in the codebase. They must not revert edits made by
  other workers and must adapt to already-landed changes.
- Each worker should keep edits inside its owned crate unless a documented
  integration point requires a small cross-crate change.
- Cross-crate API changes must be narrow and explicit. Prefer adding small
  context/config parameters over broad global state.
- Existing ignored confirmation tests are the behavioral contract. Production
  fixes should make those tests pass without weakening their assertions.
- Once a bug is fixed, its confirmation test should be promoted into the normal
  regression suite unless there is a concrete reason to keep it ignored.

## Testing Strategy

Each worker must run the smallest relevant failing confirmation command first
with `-- --ignored`, because the confirmation tests currently live outside the
default suite. After the production fix, the corresponding confirmation tests
should be promoted into the normal regression path and re-run without
`-- --ignored`, followed by the crate default tests.

Required worker checks:

- Runtime worker: reproduce with
  `cargo test -p runtime confirms_issue -- --ignored`, then verify with
  `cargo test -p runtime confirms_issue` and `cargo test -p runtime`.
- API worker: reproduce with
  `cargo test -p api confirms_issue -- --ignored`, then verify with
  `cargo test -p api confirms_issue` and `cargo test -p api`.
- Tools worker: reproduce with
  `cargo test -p tools confirms_issue -- --ignored`, then verify with
  `cargo test -p tools confirms_issue` and `cargo test -p tools`.
- CLI worker: reproduce with
  `cargo test -p rusty-claude-cli confirms_issue -- --ignored`, then verify with
  `cargo test -p rusty-claude-cli confirms_issue` and
  `cargo test -p rusty-claude-cli`.

Final integration checks:

- Re-run all four `confirms_issue` filters from `rust/` and confirm they pass.
- Re-run all four crate default test suites.
- Run formatting and clippy checks for touched crates.

## Review Strategy

For each worker result:

1. The implementer performs a self-review and reports changed files, tests run,
   and any concerns.
2. A spec-compliance reviewer checks the implementation against this design and
   `docs/rust-risk-test-confirmation.md`.
3. A code-quality reviewer checks correctness, maintainability, test strength,
   and cross-worker conflicts.
4. The controller integrates only reviewed changes and runs the relevant
   verification commands locally.

After all worker groups are complete, a final reviewer reviews the combined
diff for regressions, inconsistent abstractions, missed tests, and concurrency
or session-scope conflicts across crates.

## Acceptance Criteria

- All 17 confirmed bugs have production fixes.
- Existing confirmation tests pass without weakening the behavioral assertions.
- Default tests for `runtime`, `api`, `tools`, and `rusty-claude-cli` pass.
- Formatting and clippy checks pass for touched crates.
- Debug logs no longer write secret-shaped values in the confirmed CLI/runtime
  path.
- File-tool and task-registry fixes do not introduce new process-global leakage.
- The final combined diff has passed both spec-compliance and code-quality
  review.
