use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::compact::{
    compact_session, format_compact_summary, get_compact_continuation_message, CompactionConfig,
    CompactionResult,
};
use crate::session::{
    CompactionMarkerKind, ContentBlock, ConversationMessage, MessageRole, Session,
};

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn session_memory_path(session: &Session) -> Option<PathBuf> {
    let persistence_path = session.persistence_path()?;
    let parent = persistence_path.parent().unwrap_or_else(|| Path::new("."));
    Some(parent.join(format!("{}.memory.md", session.session_id)))
}

pub(crate) fn refresh_session_memory(session: &Session) -> Option<PathBuf> {
    let memory_path = session_memory_path(session)?;
    fs::write(&memory_path, render_session_memory(session)).ok()?;
    Some(memory_path)
}

pub(crate) fn try_session_memory_compact(
    session: &Session,
    config: CompactionConfig,
) -> Option<CompactionResult> {
    let memory_path = session_memory_path(session)?;
    let summary = fs::read_to_string(memory_path).ok()?;
    let summary = summary.trim();
    if summary.is_empty() {
        return None;
    }

    let mut result = compact_session(session, config);
    if result.removed_message_count == 0 {
        return None;
    }

    let summary = summary.to_string();
    result.summary.clone_from(&summary);
    result.formatted_summary = format_compact_summary(&summary);
    let recent_messages_preserved = result.compacted_session.messages.len() > 1;
    if let Some(system_message) = result.compacted_session.messages.first_mut() {
        *system_message = ConversationMessage::system_text(get_compact_continuation_message(
            &summary,
            true,
            recent_messages_preserved,
        ))
        .with_compaction_marker(CompactionMarkerKind::CompactBoundary);
    }
    if let Some(compaction) = result.compacted_session.compaction.as_mut() {
        compaction.summary = summary;
    }

    Some(result)
}

#[must_use]
pub fn compact_session_with_memory(
    session: &Session,
    config: CompactionConfig,
) -> CompactionResult {
    try_session_memory_compact(session, config).unwrap_or_else(|| compact_session(session, config))
}

fn render_session_memory(session: &Session) -> String {
    let recent_prompts = collect_role_texts(session, MessageRole::User, 3);
    let goals = collect_role_texts(session, MessageRole::User, 2);
    let decisions = collect_role_texts(session, MessageRole::Assistant, 3);
    let current_work = collect_current_work(session)
        .into_iter()
        .collect::<Vec<_>>();
    let pending_work = collect_pending_work(session);
    let files = collect_files(session);

    [
        render_section("Goals", &goals),
        render_section("Decisions", &decisions),
        render_section("Files", &files),
        render_section("Current work", &current_work),
        render_section("Pending work", &pending_work),
        render_section("Recent prompts", &recent_prompts),
    ]
    .join("\n\n")
}

fn render_section(title: &str, items: &[String]) -> String {
    let mut lines = vec![format!("# {title}")];
    if items.is_empty() {
        lines.push("- None recorded".to_string());
    } else {
        lines.extend(items.iter().map(|item| format!("- {item}")));
    }
    lines.join("\n")
}

fn collect_role_texts(session: &Session, role: MessageRole, limit: usize) -> Vec<String> {
    let mut texts = session
        .messages
        .iter()
        .filter(|message| message.role == role)
        .filter_map(first_text_block)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let start = texts.len().saturating_sub(limit);
    texts.drain(..start);
    texts
}

fn collect_current_work(session: &Session) -> Option<String> {
    session
        .messages
        .iter()
        .rev()
        .filter_map(first_text_block)
        .find(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

fn collect_pending_work(session: &Session) -> Vec<String> {
    collect_role_texts(session, MessageRole::User, 2)
}

fn collect_files(session: &Session) -> Vec<String> {
    let mut files = BTreeSet::new();
    for text in session.messages.iter().filter_map(first_text_block) {
        for token in text.split_whitespace() {
            let candidate = token.trim_matches(|ch: char| {
                matches!(
                    ch,
                    ',' | '.' | ':' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\''
                )
            });
            let extension = std::path::Path::new(candidate).extension();
            if candidate.contains('/')
                || extension.is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("rs")
                        || ext.eq_ignore_ascii_case("md")
                        || ext.eq_ignore_ascii_case("toml")
                })
            {
                files.insert(candidate.to_string());
            }
        }
    }
    files.into_iter().collect()
}

fn first_text_block(message: &ConversationMessage) -> Option<&str> {
    message.blocks.iter().find_map(|block| match block {
        ContentBlock::Text { text } => Some(text.trim()),
        ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. } => None,
    })
}

#[cfg(test)]
mod tests {
    use super::{refresh_session_memory, session_memory_path};
    use crate::session::{ContentBlock, ConversationMessage, Session};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn refresh_session_memory_writes_expected_sections() {
        let path = temp_session_path("refresh");
        let mut session = Session::new().with_persistence_path(path.clone());
        session.messages = vec![
            ConversationMessage::user_text("Ship the compaction pipeline"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "I will update rust/crates/runtime/src/conversation.rs".to_string(),
            }]),
            ConversationMessage::user_text("Focus on the runtime crate first"),
        ];

        let memory_path =
            refresh_session_memory(&session).expect("session memory sidecar should be written");
        let contents =
            fs::read_to_string(&memory_path).expect("session memory sidecar should be readable");

        assert_eq!(
            Some(memory_path.clone()),
            session_memory_path(&session),
            "sidecar path should be derived from the persisted session"
        );
        assert!(contents.contains("# Goals"));
        assert!(contents.contains("# Decisions"));
        assert!(contents.contains("# Files"));
        assert!(contents.contains("# Current work"));
        assert!(contents.contains("# Pending work"));
        assert!(contents.contains("# Recent prompts"));
        assert!(contents.contains("Ship the compaction pipeline"));
        assert!(contents.contains("Focus on the runtime crate first"));

        fs::remove_file(&path).ok();
        fs::remove_file(&memory_path).ok();
    }

    fn temp_session_path(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("runtime-session-memory-{label}-{nanos}.json"))
    }
}
