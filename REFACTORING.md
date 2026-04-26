# Refactoring Plan

## P0: Critical Error Handling (In Progress)

### Completed
- ✅ Fixed `panic!` in `stale_branch.rs` test helper (commit 5035ef3)

### Remaining P0 Tasks
1. **Mutex Lock Poisoning** (10 occurrences)
   - Files: `team_cron_registry.rs`, `mcp_tool_bridge.rs`
   - Current: `.expect("...lock poisoned")`
   - Solution: Use `.unwrap_or_else(PoisonError::into_inner)` or migrate to `parking_lot::Mutex`

2. **Critical Path unwrap/expect** (281 occurrences in main.rs)
   - Priority areas:
     - Runtime initialization (lines 3409, 3442, 3450)
     - Progress tracking (lines 6719, 6744, 6769, 6801, 6814)
     - OAuth flow (lines 9562-9587)
   - Solution: Return `Result<T, E>` instead of panicking

## P1: Code Organization

### main.rs Refactoring (13,140 lines → ~500 lines)

Target module structure:
```
src/
├── main.rs (entry point, ~500 lines)
├── cli/
│   ├── mod.rs
│   ├── parser.rs (parse_args, parse_*_args functions)
│   ├── slash_commands.rs (slash command handling)
│   └── suggestions.rs (suggest_*, levenshtein_distance)
├── config/
│   ├── mod.rs
│   ├── model.rs (resolve_model_alias, config_model_for_current_dir)
│   └── permission.rs (permission_mode_*, parse_permission_mode_arg)
├── session/
│   ├── mod.rs
│   ├── resume.rs (resume_session, parse_resume_args)
│   └── export.rs (parse_export_args)
├── repl/
│   ├── mod.rs
│   ├── runtime.rs (run_repl, BuiltRuntime)
│   └── progress.rs (InternalPromptProgress)
├── oauth/
│   ├── mod.rs
│   ├── login.rs (run_login, wait_for_oauth_callback)
│   └── logout.rs (run_logout)
├── doctor/
│   ├── mod.rs
│   └── checks.rs (check_*_health, render_doctor_report)
└── git/
    ├── mod.rs
    └── status.rs (parse_git_*, run_git_capture_in)
```

### tools/lib.rs Refactoring (9,523 lines)

Split by tool category:
```
tools/
├── lib.rs (re-exports, ~200 lines)
├── file_ops.rs (read_file, write_file, search_files, patch)
├── terminal.rs (terminal, process)
├── browser.rs (browser_*, vision)
├── delegation.rs (delegate_task)
├── memory.rs (memory, session_search)
├── skills.rs (skill_*, skills_list)
├── messaging.rs (send_message)
└── misc.rs (todo, clarify, cronjob, etc.)
```

## Implementation Strategy

### Phase 1: Error Handling (P0)
1. Fix remaining panic! calls in tests
2. Replace `.expect("lock poisoned")` with proper handling
3. Add `Result` returns to critical paths
4. **Target**: 1 PR, ~200 lines changed

### Phase 2: Extract Modules (P1)
1. Create module structure
2. Move functions in logical groups (50-100 functions per PR)
3. Update imports and visibility
4. **Target**: 5-8 PRs, ~2000 lines moved per PR

### Phase 3: Testing & Validation
1. Ensure all tests pass after each PR
2. Run clippy and rustfmt
3. Verify no behavioral changes

## Benefits

- **Maintainability**: Easier to navigate and understand
- **Testing**: Isolated modules are easier to test
- **Collaboration**: Multiple developers can work on different modules
- **Performance**: Faster compilation (incremental builds)
- **Safety**: Better error handling reduces panics

## Timeline

- P0 (Error Handling): 1-2 days
- P1 (Module Extraction): 1-2 weeks
- P2 (Testing & Polish): 3-5 days

## Notes

- All refactoring must maintain backward compatibility
- No behavioral changes in this phase
- Focus on structure, not algorithm changes
- Keep git history clean with atomic commits
