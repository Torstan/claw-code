# P1 Refactoring: Module Structure Proposal

## Overview
This document proposes a phased approach to refactor `main.rs` (13,140 lines) and `tools/lib.rs` (9,523 lines) into maintainable modules.

## Phase 1: Extract File Operations Module

### Target: `tools/src/file_ops.rs`
Extract file-related functions from `lib.rs`:
- `run_read_file` (line 2035)
- `run_write_file` (line 2040)
- `run_edit_file` (line 2045)
- `run_glob_search` (line 2058)
- `run_grep_search` (line 2063)
- Related input structs: `ReadFileInput`, `WriteFileInput`, `EditFileInput`, `GlobSearchInputValue`

**Estimated impact**: ~200 lines extracted

### Implementation Steps
1. Create `tools/src/file_ops.rs`
2. Move functions and types
3. Add `pub mod file_ops;` to `lib.rs`
4. Re-export public items: `pub use file_ops::*;`
5. Run tests to verify no breakage

## Phase 2: Extract Terminal Operations Module

### Target: `tools/src/terminal_ops.rs`
Extract terminal/process-related functions:
- Terminal execution functions
- Process management functions
- Background task handling
- Related input/output structs

**Estimated impact**: ~300 lines extracted

## Phase 3: Extract Browser Operations Module

### Target: `tools/src/browser_ops.rs`
Extract browser and vision-related functions:
- Browser navigation and interaction
- Vision analysis
- Screenshot handling
- Related input/output structs

**Estimated impact**: ~400 lines extracted

## Phase 4: Main.rs Modularization

### Approach
Unlike `tools/lib.rs`, `main.rs` contains tightly coupled code. We'll use a different strategy:

1. **Extract standalone utilities first**
   - Git operations → `cli/git_utils.rs`
   - Config helpers → `cli/config_utils.rs`
   - Suggestion/completion → `cli/suggestions.rs`

2. **Extract command handlers**
   - OAuth flow → `cli/oauth.rs`
   - Doctor command → `cli/doctor.rs`
   - Export command → `cli/export.rs`

3. **Keep core REPL in main.rs**
   - Entry point
   - Argument parsing
   - REPL loop
   - Runtime initialization

**Target**: Reduce `main.rs` from 13,140 lines to ~2,000-3,000 lines

## Success Criteria

- ✅ All existing tests pass
- ✅ No behavioral changes
- ✅ Compilation time improves (incremental builds)
- ✅ Code navigation easier (smaller files)
- ✅ No clippy warnings introduced

## Timeline

- Phase 1 (file_ops): 1-2 days
- Phase 2 (terminal_ops): 1-2 days  
- Phase 3 (browser_ops): 1-2 days
- Phase 4 (main.rs): 1 week
- **Total**: ~2 weeks

## Notes

- Each phase = 1 PR
- Maintain backward compatibility
- Keep git history clean
- Run full test suite after each phase
