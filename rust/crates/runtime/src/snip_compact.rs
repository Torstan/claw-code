use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::compact::{estimate_message_tokens, estimate_session_tokens};
use crate::micro_compact::{
    build_tool_input_index, is_cleared_sentinel, is_compactable_tool_name, recoverable_sentinel,
};
use crate::session::{
    CompactionMarkerKind, ContentBlock, ConversationMessage, MessageRole, Session,
};

pub const SNIP_CLEARED_ASSISTANT_TEXT_SENTINEL: &str =
    "[Older assistant text cleared by snip compact]";
pub const SNIP_CLEARED_TOOL_RESULT_SENTINEL: &str =
    "[Older tool result content cleared by snip compact]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnipCompactionConfig {
    pub trigger_threshold_tokens: usize,
    pub target_tokens: usize,
    pub protected_recent_messages: usize,
    pub min_candidate_tokens: usize,
}

impl SnipCompactionConfig {
    #[must_use]
    pub fn for_auto_compaction_threshold(auto_compaction_threshold_tokens: usize) -> Self {
        let threshold = auto_compaction_threshold_tokens.max(1);
        Self {
            trigger_threshold_tokens: threshold.saturating_mul(4) / 5,
            target_tokens: threshold.saturating_mul(3) / 5,
            protected_recent_messages: 4,
            min_candidate_tokens: 20,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnipCompactionResult {
    pub compacted_session: Session,
    pub snipped_message_ids: Vec<String>,
    pub estimated_tokens_freed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SnipCandidateKind {
    ToolResult,
    AssistantText,
}

#[must_use]
pub fn snip_compact_session(
    session: &Session,
    config: SnipCompactionConfig,
) -> SnipCompactionResult {
    let estimated_tokens = estimate_session_tokens(session);
    if estimated_tokens < config.trigger_threshold_tokens
        || estimated_tokens <= config.target_tokens
    {
        return no_op_result(session);
    }

    let candidate_indices = collect_snip_candidate_indices(session, config);
    if candidate_indices.is_empty() {
        return no_op_result(session);
    }

    let mut compacted_session = session.clone();
    let mut current_tokens = estimated_tokens;
    let mut estimated_tokens_freed = 0;
    let mut snipped_message_ids = Vec::new();
    let tool_input_index = build_tool_input_index(&session.messages);

    for message_index in candidate_indices {
        if current_tokens <= config.target_tokens {
            break;
        }

        let Some(message) = compacted_session.messages.get_mut(message_index) else {
            continue;
        };
        let original_tokens = estimate_message_tokens(message);
        let new_tokens = rewrite_message_for_snip(message, &tool_input_index);
        if new_tokens >= original_tokens {
            continue;
        }

        let freed = original_tokens - new_tokens;
        current_tokens = current_tokens.saturating_sub(freed);
        estimated_tokens_freed += freed;
        snipped_message_ids.push(message.message_id.clone());
    }

    if snipped_message_ids.is_empty() {
        return no_op_result(session);
    }

    insert_snip_marker(
        &mut compacted_session.messages,
        &snipped_message_ids,
        estimated_tokens_freed,
    );
    compacted_session.updated_at_ms = current_time_millis();

    SnipCompactionResult {
        compacted_session,
        snipped_message_ids,
        estimated_tokens_freed,
    }
}

fn no_op_result(session: &Session) -> SnipCompactionResult {
    SnipCompactionResult {
        compacted_session: session.clone(),
        snipped_message_ids: Vec::new(),
        estimated_tokens_freed: 0,
    }
}

fn collect_snip_candidate_indices(session: &Session, config: SnipCompactionConfig) -> Vec<usize> {
    let protected_start = session
        .messages
        .len()
        .saturating_sub(config.protected_recent_messages);
    let mut candidates = session
        .messages
        .iter()
        .enumerate()
        .take(protected_start)
        .filter_map(|(index, message)| {
            let estimated_tokens = estimate_message_tokens(message);
            if estimated_tokens < config.min_candidate_tokens {
                return None;
            }

            if is_snippable_tool_result(message) {
                Some((index, SnipCandidateKind::ToolResult, estimated_tokens))
            } else if is_snippable_assistant_text(message) {
                Some((index, SnipCandidateKind::AssistantText, estimated_tokens))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.1
            .cmp(&right.1)
            .then_with(|| right.2.cmp(&left.2))
            .then_with(|| left.0.cmp(&right.0))
    });
    candidates.into_iter().map(|(index, _, _)| index).collect()
}

fn is_snippable_assistant_text(message: &ConversationMessage) -> bool {
    message.role == MessageRole::Assistant
        && !message.blocks.is_empty()
        && message
            .blocks
            .iter()
            .all(|block| matches!(block, ContentBlock::Text { .. }))
        && message.blocks.iter().any(|block| {
            matches!(
                block,
                ContentBlock::Text { text } if text != SNIP_CLEARED_ASSISTANT_TEXT_SENTINEL
            )
        })
}

fn is_snippable_tool_result(message: &ConversationMessage) -> bool {
    if message.role != MessageRole::Tool {
        return false;
    }

    message.blocks.iter().any(is_snippable_tool_result_block)
}

fn is_snippable_tool_result_block(block: &ContentBlock) -> bool {
    matches!(
        block,
        ContentBlock::ToolResult {
            tool_name,
            output,
            is_error,
            ..
        } if !is_error
            && is_compactable_tool_name(tool_name)
            && !is_cleared_sentinel(output)
    )
}

fn rewrite_message_for_snip(
    message: &mut ConversationMessage,
    tool_input_index: &HashMap<&str, &str>,
) -> usize {
    if is_snippable_assistant_text(message) {
        message.blocks = vec![ContentBlock::Text {
            text: SNIP_CLEARED_ASSISTANT_TEXT_SENTINEL.to_string(),
        }];
        return estimate_message_tokens(message);
    }

    for block in &mut message.blocks {
        if !is_snippable_tool_result_block(block) {
            continue;
        }
        let ContentBlock::ToolResult {
            tool_use_id,
            tool_name,
            output,
            ..
        } = block
        else {
            continue;
        };
        let tool_input = tool_input_index.get(tool_use_id.as_str()).copied();
        *output = recoverable_sentinel(tool_name, tool_input, output);
    }
    estimate_message_tokens(message)
}

fn insert_snip_marker(
    messages: &mut Vec<ConversationMessage>,
    snipped_message_ids: &[String],
    estimated_tokens_freed: usize,
) {
    let marker = ConversationMessage::system_text(format!(
        "Snip compact cleared {} messages (~{} estimated tokens freed). Message IDs: {}",
        snipped_message_ids.len(),
        estimated_tokens_freed,
        snipped_message_ids.join(", ")
    ))
    .with_compaction_marker(CompactionMarkerKind::SnipBoundary);
    let insert_at = messages
        .iter()
        .take_while(|message| message.role == MessageRole::System)
        .count();
    messages.insert(insert_at, marker);
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
