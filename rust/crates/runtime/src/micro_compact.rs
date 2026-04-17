use std::time::{SystemTime, UNIX_EPOCH};

use crate::session::{
    CompactionMarkerKind, ContentBlock, ConversationMessage, MessageRole, Session,
};

pub const MICROCOMPACT_CLEARED_SENTINEL: &str = "[Old tool result content cleared by microcompact]";

const COMPACTABLE_TOOLS: &[&str] = &[
    "bash",
    "read_file",
    "write_file",
    "edit_file",
    "glob_search",
    "grep_search",
];

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
    let compactable_indices = collect_compactable_tool_result_indices(session);
    let time_triggered = is_time_based_triggered(session, config, current_time_millis());
    let count_triggered = compactable_indices.len() > config.trigger_count;
    if !time_triggered && !count_triggered {
        return no_op_result(session);
    }

    let keep_recent = config.keep_recent.max(1);
    let clear_until = compactable_indices.len().saturating_sub(keep_recent);
    if clear_until == 0 {
        return no_op_result(session);
    }

    let mut compacted_session = session.clone();
    let mut cleared_tool_result_count = 0;
    let mut estimated_tokens_freed = 0;

    for message_index in compactable_indices.into_iter().take(clear_until) {
        let Some(message) = compacted_session.messages.get_mut(message_index) else {
            continue;
        };

        for block in &mut message.blocks {
            let ContentBlock::ToolResult { output, .. } = block else {
                continue;
            };
            if output == MICROCOMPACT_CLEARED_SENTINEL {
                continue;
            }
            estimated_tokens_freed += estimate_output_tokens(output);
            *output = MICROCOMPACT_CLEARED_SENTINEL.to_string();
            cleared_tool_result_count += 1;
        }
    }

    if cleared_tool_result_count == 0 {
        return no_op_result(session);
    }

    insert_microcompact_marker(
        &mut compacted_session.messages,
        cleared_tool_result_count,
        estimated_tokens_freed,
    );
    compacted_session.updated_at_ms = current_time_millis();

    MicroCompactionResult {
        compacted_session,
        cleared_tool_result_count,
        estimated_tokens_freed,
    }
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
                ContentBlock::ToolResult { tool_name, .. }
                    if COMPACTABLE_TOOLS.contains(&tool_name.as_str()) =>
                {
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
