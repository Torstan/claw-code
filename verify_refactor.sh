#!/bin/bash
# Verification script for refactoring correctness

set -e

echo "=== Refactoring Verification ==="
echo

# 1. Extract all public exports from lib.rs
echo "1. Checking public API surface..."
grep -E "^pub (fn|struct|enum|trait|type|const|static|mod|use)" rust/crates/tools/src/lib.rs | sort > /tmp/lib_public_before.txt || true
PUB_COUNT=$(wc -l < /tmp/lib_public_before.txt)
echo "   Public items: $PUB_COUNT"

# 2. Extract all tool registration names
echo "2. Checking tool registrations..."
grep -E 'register_tool\(' rust/crates/tools/src/lib.rs | sed 's/.*register_tool("\([^"]*\)".*/\1/' | sort > /tmp/tools_before.txt || true
TOOL_COUNT=$(wc -l < /tmp/tools_before.txt)
echo "   Registered tools: $TOOL_COUNT"

# 3. Count functions by category
echo "3. Counting functions..."
TOTAL_FN=$(grep -c "^fn \|^pub fn \|^async fn \|^pub async fn " rust/crates/tools/src/lib.rs || echo 0)
RUN_FN=$(grep -c "^fn run_\|^pub fn run_" rust/crates/tools/src/lib.rs || echo 0)
echo "   Total functions: $TOTAL_FN"
echo "   Tool runner functions (run_*): $RUN_FN"

# 4. List all modules
echo "4. Checking module structure..."
grep "^pub mod \|^mod " rust/crates/tools/src/lib.rs | sort
echo

# 5. Check file_ops module
if [ -f rust/crates/tools/src/file_ops.rs ]; then
    echo "5. Verifying file_ops module..."
    FILE_OPS_FN=$(grep -c "^pub fn " rust/crates/tools/src/file_ops.rs || echo 0)
    echo "   file_ops.rs public functions: $FILE_OPS_FN"
fi

echo
echo "=== Baseline captured ===" 
echo "Files saved:"
echo "  - /tmp/lib_public_before.txt (public API)"
echo "  - /tmp/tools_before.txt (tool names)"
