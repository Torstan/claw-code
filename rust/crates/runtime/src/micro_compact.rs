use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::session::{
    CompactionMarkerKind, ContentBlock, ConversationMessage, MessageRole, Session,
};
use crate::snip_compact::SNIP_CLEARED_TOOL_RESULT_SENTINEL;

pub const MICROCOMPACT_CLEARED_SENTINEL: &str = "[Old tool result content cleared by microcompact]";

const CLEARED_PREFIX: &str = "[Cleared: ";

const COMPACTABLE_TOOLS: &[&str] = &[
    "bash",
    "read_file",
    "write_file",
    "edit_file",
    "glob_search",
    "grep_search",
];

#[must_use]
pub(crate) fn is_compactable_tool_name(tool_name: &str) -> bool {
    COMPACTABLE_TOOLS.contains(&tool_name)
}

pub(crate) fn is_cleared_sentinel(text: &str) -> bool {
    text == MICROCOMPACT_CLEARED_SENTINEL
        || text == SNIP_CLEARED_TOOL_RESULT_SENTINEL
        || (text.starts_with(CLEARED_PREFIX) && text.ends_with(']'))
}

pub(crate) fn build_tool_input_index(messages: &[ConversationMessage]) -> HashMap<&str, &str> {
    let mut index = HashMap::new();
    for msg in messages {
        for block in &msg.blocks {
            if let ContentBlock::ToolUse { id, input, .. } = block {
                index.insert(id.as_str(), input.as_str());
            }
        }
    }
    index
}

pub(crate) fn recoverable_sentinel(
    tool_name: &str,
    tool_input: Option<&str>,
    output: &str,
) -> String {
    let line_count = output.lines().count();
    let detail = tool_input.and_then(|input| extract_tool_detail(tool_name, input));

    match (tool_name, detail) {
        ("bash", Some(cmd)) => format!("[Cleared: bash output ({line_count} lines) from `{cmd}`, re-run to reproduce]"),
        ("bash", None) => format!("[Cleared: bash output ({line_count} lines), re-run to reproduce]"),
        ("read_file", Some(path)) => format!("[Cleared: content of {path} ({line_count} lines), use read_file to reload]"),
        ("read_file", None) => format!("[Cleared: read_file output ({line_count} lines), use read_file to reload]"),
        ("grep_search", Some(info)) => format!("[Cleared: grep results for {info} ({line_count} lines), re-run grep_search to refresh]"),
        ("grep_search", None) => format!("[Cleared: grep_search output ({line_count} lines), re-run to refresh]"),
        ("glob_search", Some(pat)) => format!("[Cleared: glob results for {pat} ({line_count} lines), re-run glob_search to refresh]"),
        ("glob_search", None) => format!("[Cleared: glob_search output ({line_count} lines), re-run to refresh]"),
        ("edit_file", Some(path)) => format!("[Cleared: edit_file result for {path}]"),
        ("write_file", Some(path)) => format!("[Cleared: write_file result for {path}]"),
        (name, _) => format!("[Cleared: {name} output ({line_count} lines)]"),
    }
}

fn extract_tool_detail(tool_name: &str, input_json: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(input_json).ok()?;
    match tool_name {
        "bash" => {
            let cmd = parsed.get("command")?.as_str()?;
            let truncated = truncate_to_char_boundary(cmd, 80);
            Some(truncated.to_string())
        }
        "read_file" | "edit_file" | "write_file" => {
            let path = parsed
                .get("file_path")
                .or_else(|| parsed.get("path"))?
                .as_str()?;
            Some(path.to_string())
        }
        "grep_search" => {
            let pattern = parsed.get("pattern")?.as_str()?;
            let path = parsed.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            Some(format!("\"{pattern}\" in {path}"))
        }
        "glob_search" => {
            let pattern = parsed.get("pattern")?.as_str()?;
            Some(format!("\"{pattern}\""))
        }
        _ => None,
    }
}

fn truncate_to_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicroCompactionConfig {
    pub trigger_count: usize,
    pub keep_recent: usize,
    pub gap_threshold_minutes: u64,
}

impl Default for MicroCompactionConfig {
    fn default() -> Self {
        Self {
            trigger_count: 6,
            keep_recent: 2,
            gap_threshold_minutes: 60,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicroCompactionResult {
    pub compacted_session: Session,
    pub cleared_tool_result_count: usize,
    pub estimated_tokens_freed: usize,
}

#[must_use]
pub fn micro_compact_session(
    session: &Session,
    config: MicroCompactionConfig,
) -> MicroCompactionResult {
    maybe_micro_compact_session(session, config).unwrap_or_else(|| no_op_result(session))
}

#[must_use]
pub(crate) fn maybe_micro_compact_session(
    session: &Session,
    config: MicroCompactionConfig,
) -> Option<MicroCompactionResult> {
    let compactable_indices = collect_compactable_tool_result_indices(session);
    let time_triggered = is_time_based_triggered(session, config, current_time_millis());
    let count_triggered = compactable_indices.len() > config.trigger_count;
    if !time_triggered && !count_triggered {
        return None;
    }

    let clear_until = compactable_indices.len().saturating_sub(config.keep_recent);
    if clear_until == 0 {
        return None;
    }

    let mut compacted_session = session.clone();
    let mut cleared_tool_result_count = 0;
    let mut estimated_tokens_freed = 0;
    let tool_input_index = build_tool_input_index(&session.messages);

    for message_index in compactable_indices.into_iter().take(clear_until) {
        let Some(message) = compacted_session.messages.get_mut(message_index) else {
            continue;
        };

        for block in &mut message.blocks {
            let ContentBlock::ToolResult {
                tool_use_id,
                tool_name,
                output,
                ..
            } = block
            else {
                continue;
            };
            if is_cleared_sentinel(output) {
                continue;
            }
            let tool_input = tool_input_index.get(tool_use_id.as_str()).copied();
            let sentinel = recoverable_sentinel(tool_name, tool_input, output);
            estimated_tokens_freed += estimate_output_tokens(output);
            *output = sentinel;
            cleared_tool_result_count += 1;
        }
    }

    if cleared_tool_result_count == 0 {
        return None;
    }

    insert_microcompact_marker(
        &mut compacted_session.messages,
        cleared_tool_result_count,
        estimated_tokens_freed,
    );
    compacted_session.updated_at_ms = current_time_millis();

    Some(MicroCompactionResult {
        compacted_session,
        cleared_tool_result_count,
        estimated_tokens_freed,
    })
}

fn no_op_result(session: &Session) -> MicroCompactionResult {
    MicroCompactionResult {
        compacted_session: session.clone(),
        cleared_tool_result_count: 0,
        estimated_tokens_freed: 0,
    }
}

fn collect_compactable_tool_result_indices(session: &Session) -> Vec<usize> {
    session
        .messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            if message.role != MessageRole::Tool {
                return None;
            }

            message.blocks.iter().find_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_name, output, ..
                } if is_compactable_tool_name(tool_name) && !is_cleared_sentinel(output) => {
                    Some(index)
                }
                ContentBlock::Text { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. } => None,
            })
        })
        .collect()
}

fn is_time_based_triggered(session: &Session, config: MicroCompactionConfig, now_ms: u64) -> bool {
    let Some(last_assistant_timestamp_ms) = session
        .messages
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::Assistant)
        .map(|message| message.timestamp_ms)
    else {
        return false;
    };

    now_ms.saturating_sub(last_assistant_timestamp_ms)
        >= config.gap_threshold_minutes.saturating_mul(60_000)
}

fn insert_microcompact_marker(
    messages: &mut Vec<ConversationMessage>,
    cleared_tool_result_count: usize,
    estimated_tokens_freed: usize,
) {
    let marker = ConversationMessage::system_text(format!(
        "Microcompact cleared {cleared_tool_result_count} tool results (~{estimated_tokens_freed} estimated tokens freed)."
    ))
    .with_compaction_marker(CompactionMarkerKind::MicrocompactBoundary);
    let insert_at = messages
        .iter()
        .take_while(|message| message.role == MessageRole::System)
        .count();
    messages.insert(insert_at, marker);
}

fn estimate_output_tokens(output: &str) -> usize {
    output.len().div_ceil(4).max(1)
}

fn current_time_millis() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}
