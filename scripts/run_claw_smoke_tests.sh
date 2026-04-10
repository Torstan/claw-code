#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_ROOT="$ROOT/rust"
BINARY="$RUST_ROOT/target/debug/claw"

if [[ ! -x "$BINARY" ]]; then
  echo "claw binary not found at $BINARY; run the build-claw suite first" >&2
  exit 1
fi

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/claw-smoke.XXXXXX")"
HOME_DIR="$TMP_DIR/home"
CONFIG_DIR="$TMP_DIR/config"
CODEX_DIR="$TMP_DIR/codex"
WORKSPACE_DIR="$TMP_DIR/workspace"

cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

mkdir -p "$HOME_DIR" "$CONFIG_DIR" "$CODEX_DIR" "$WORKSPACE_DIR"

run_claw() {
  env -i \
    PATH="$PATH" \
    HOME="$HOME_DIR" \
    CLAW_CONFIG_HOME="$CONFIG_DIR" \
    CODEX_HOME="$CODEX_DIR" \
    TERM="${TERM:-dumb}" \
    "$BINARY" "$@"
}

assert_contains() {
  local output="$1"
  local needle="$2"
  local label="$3"
  if [[ "$output" != *"$needle"* ]]; then
    echo "smoke assertion failed: $label" >&2
    echo "expected to find: $needle" >&2
    echo "actual output:" >&2
    printf '%s\n' "$output" >&2
    exit 1
  fi
}

version_output="$(run_claw --version)"
assert_contains "$version_output" "Claw Code" "version banner"
assert_contains "$version_output" "Version" "version details"

help_output="$(run_claw help)"
assert_contains "$help_output" "Usage:" "help usage"
assert_contains "$help_output" "claw doctor" "help doctor entry"

status_json="$(cd "$RUST_ROOT" && run_claw --output-format json status)"
assert_contains "$status_json" '"kind": "status"' "status kind"
assert_contains "$status_json" '"workspace"' "status workspace payload"

sandbox_json="$(cd "$RUST_ROOT" && run_claw --output-format json sandbox)"
assert_contains "$sandbox_json" '"kind": "sandbox"' "sandbox kind"
assert_contains "$sandbox_json" '"filesystem_mode"' "sandbox payload"

agents_json="$(cd "$RUST_ROOT" && run_claw --output-format json agents)"
assert_contains "$agents_json" '"kind": "agents"' "agents kind"
assert_contains "$agents_json" '"count": 0' "agents empty count"

mcp_json="$(cd "$RUST_ROOT" && run_claw --output-format json mcp)"
assert_contains "$mcp_json" '"kind": "mcp"' "mcp kind"
assert_contains "$mcp_json" '"configured_servers": 0' "mcp empty list"

skills_json="$(cd "$RUST_ROOT" && run_claw --output-format json skills)"
assert_contains "$skills_json" '"kind": "skills"' "skills kind"
assert_contains "$skills_json" '"total": 0' "skills empty summary"

doctor_json="$(cd "$RUST_ROOT" && run_claw --output-format json doctor)"
assert_contains "$doctor_json" '"kind": "doctor"' "doctor kind"
assert_contains "$doctor_json" '"has_failures": false' "doctor no failures"

system_prompt_json="$(cd "$RUST_ROOT" && run_claw --output-format json system-prompt --cwd .. --date 2026-04-04)"
assert_contains "$system_prompt_json" '"kind": "system-prompt"' "system prompt kind"
assert_contains "$system_prompt_json" '"message"' "system prompt payload"

init_json="$(cd "$WORKSPACE_DIR" && run_claw --output-format json init)"
assert_contains "$init_json" '"kind": "init"' "init kind"
[[ -f "$WORKSPACE_DIR/CLAUDE.md" ]] || { echo "init did not create CLAUDE.md" >&2; exit 1; }
[[ -f "$WORKSPACE_DIR/.claw.json" ]] || { echo "init did not create .claw.json" >&2; exit 1; }

echo "claw CLI smoke tests passed"
