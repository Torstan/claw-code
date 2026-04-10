#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_ROOT="$ROOT/rust"

ALL_SUITES=(
  build-claw
  claw-cli-smoke
  rust-fmt
  rust-test-workspace
  mock-parity-harness
  rust-clippy
  docs-source-of-truth
)

RUST_PROFILE=(
  build-claw
  claw-cli-smoke
  rust-fmt
  rust-test-workspace
  mock-parity-harness
  rust-clippy
)

selected_suites=()
profile="all"
list_only=0
fail_fast=0

suite_description() {
  case "$1" in
    build-claw) echo "build the Rust CLI binary (claw)" ;;
    claw-cli-smoke) echo "exercise the compiled claw binary through local smoke checks" ;;
    rust-fmt) echo "verify Rust formatting across the workspace" ;;
    rust-test-workspace) echo "run all Rust workspace tests" ;;
    mock-parity-harness) echo "run the deterministic mock parity harness end-to-end test" ;;
    rust-clippy) echo "run Clippy across the Rust workspace" ;;
    docs-source-of-truth) echo "check docs and metadata for stale branding and source-of-truth drift" ;;
    *) return 1 ;;
  esac
}

suite_category() {
  case "$1" in
    build-claw|claw-cli-smoke|rust-fmt|rust-test-workspace|mock-parity-harness|rust-clippy) echo "rust" ;;
    docs-source-of-truth) echo "docs" ;;
    *) return 1 ;;
  esac
}

print_usage() {
  cat <<'EOF'
Usage: ./scripts/run_all_tests.sh [--profile all|rust] [--suite NAME ...] [--list] [--fail-fast]

Options:
  --profile PROFILE   Choose the predefined suite group. Defaults to all.
  --suite NAME        Run only the named suite. May be repeated.
  --list              List available suites and exit.
  --fail-fast         Stop after the first failing suite.
  --help, -h          Show this help text.
EOF
}

is_known_suite() {
  local suite="$1"
  local known
  for known in "${ALL_SUITES[@]}"; do
    if [[ "$known" == "$suite" ]]; then
      return 0
    fi
  done
  return 1
}

append_suite_once() {
  local suite="$1"
  local existing
  for existing in "${selected_suites[@]}"; do
    if [[ "$existing" == "$suite" ]]; then
      return 0
    fi
  done
  selected_suites+=("$suite")
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --suite)
      [[ $# -ge 2 ]] || { echo "--suite requires a value" >&2; exit 2; }
      if ! is_known_suite "$2"; then
        echo "unknown suite: $2" >&2
        exit 2
      fi
      append_suite_once "$2"
      shift 2
      ;;
    --profile)
      [[ $# -ge 2 ]] || { echo "--profile requires a value" >&2; exit 2; }
      case "$2" in
        all|rust) profile="$2" ;;
        *) echo "unknown profile: $2" >&2; exit 2 ;;
      esac
      shift 2
      ;;
    --list)
      list_only=1
      shift
      ;;
    --fail-fast)
      fail_fast=1
      shift
      ;;
    --help|-h)
      print_usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      print_usage >&2
      exit 2
      ;;
  esac
done

if [[ $list_only -eq 1 ]]; then
  for suite in "${ALL_SUITES[@]}"; do
    printf '%s\t%s\t%s\n' "$suite" "$(suite_category "$suite")" "$(suite_description "$suite")"
  done
  exit 0
fi

if [[ ${#selected_suites[@]} -eq 0 ]]; then
  case "$profile" in
    all) selected_suites=("${ALL_SUITES[@]}") ;;
    rust) selected_suites=("${RUST_PROFILE[@]}") ;;
  esac
fi

run_in() {
  local cwd="$1"
  shift
  printf '    cwd: %s\n' "$cwd"
  printf '    cmd:'
  printf ' %q' "$@"
  printf '\n'
  (
    cd "$cwd"
    "$@"
  )
}

run_suite() {
  local suite="$1"
  case "$suite" in
    build-claw)
      run_in "$RUST_ROOT" cargo build -p rusty-claude-cli --bin claw
      ;;
    claw-cli-smoke)
      run_in "$ROOT" ./scripts/run_claw_smoke_tests.sh
      ;;
    rust-fmt)
      run_in "$RUST_ROOT" cargo fmt --all --check
      ;;
    rust-test-workspace)
      run_in "$RUST_ROOT" cargo test --workspace
      ;;
    mock-parity-harness)
      run_in "$RUST_ROOT" cargo test -p rusty-claude-cli --test mock_parity_harness -- --nocapture
      ;;
    rust-clippy)
      run_in "$RUST_ROOT" cargo clippy --workspace
      ;;
    docs-source-of-truth)
      run_in "$ROOT" python3 .github/scripts/check_doc_source_of_truth.py
      ;;
    *)
      echo "unknown suite: $suite" >&2
      return 2
      ;;
  esac
}

duration_seconds() {
  local start_ns="$1"
  local end_ns="$2"
  awk -v start="$start_ns" -v end="$end_ns" 'BEGIN { printf "%.2f", (end - start) / 1000000000 }'
}

names=()
statuses=()
durations=()
descriptions=()
passed=0
failed=0
total_duration="0.00"

for suite in "${selected_suites[@]}"; do
  description="$(suite_description "$suite")"
  printf '==> [%s] %s\n' "$suite" "$description"
  start_ns="$(date +%s%N)"
  if run_suite "$suite"; then
    status="PASS"
    passed=$((passed + 1))
  else
    exit_code=$?
    status="FAIL($exit_code)"
    failed=$((failed + 1))
  fi
  end_ns="$(date +%s%N)"
  duration="$(duration_seconds "$start_ns" "$end_ns")"
  total_duration="$(awk -v total="$total_duration" -v add="$duration" 'BEGIN { printf "%.2f", total + add }')"
  printf '<== [%s] %s (%ss)\n' "$suite" "$status" "$duration"
  names+=("$suite")
  statuses+=("$status")
  durations+=("$duration")
  descriptions+=("$description")
  if [[ $fail_fast -eq 1 && "$status" != PASS ]]; then
    break
  fi
done

printf '\nTest Summary\n\n'
for idx in "${!names[@]}"; do
  printf '%-10s %-22s %7ss  %s\n' "${statuses[$idx]}" "${names[$idx]}" "${durations[$idx]}" "${descriptions[$idx]}"
done
printf '\nPassed: %d\n' "$passed"
printf 'Failed: %d\n' "$failed"
printf 'Total duration: %ss\n' "$total_duration"

if [[ $failed -gt 0 ]]; then
  exit 1
fi
