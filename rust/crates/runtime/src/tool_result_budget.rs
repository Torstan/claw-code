use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::agent_debug::agent_debug_log;
use crate::micro_compact::truncate_to_char_boundary;
use crate::session::Session;

const PERSISTED_OUTPUT_TAG: &str = "<persisted-output>";
const PERSISTED_OUTPUT_CLOSING_TAG: &str = "</persisted-output>";
const PREVIEW_SIZE_BYTES: usize = 2_000;
const DEFAULT_MAX_RESULT_SIZE_CHARS: usize = 50_000;
const EDIT_FILE_MAX_RESULT_SIZE_CHARS: usize = 20_000;

#[must_use]
pub(crate) fn stabilize_tool_result_output(
    session: &Session,
    tool_use_id: &str,
    tool_name: &str,
    output: String,
    is_error: bool,
) -> String {
    if is_error || output.chars().count() <= max_result_size_chars(tool_name) {
        return output;
    }

    let Some(path) = persisted_tool_result_path(session, tool_use_id) else {
        return output;
    };

    if let Err(error) = persist_tool_result(&path, &output) {
        agent_debug_log(
            "tool_result.persisted_output.error",
            format!(
                "session_id={} tool_use_id={} tool_name={} path={} error={}",
                session.session_id,
                tool_use_id,
                tool_name,
                path.display(),
                error
            ),
        );
        return output;
    }

    let replacement = build_persisted_output_message(&path, &output);
    agent_debug_log(
        "tool_result.persisted_output",
        format!(
            "session_id={} tool_use_id={} tool_name={} original_chars={} original_bytes={} replacement_chars={} replacement_bytes={} path={}",
            session.session_id,
            tool_use_id,
            tool_name,
            output.chars().count(),
            output.len(),
            replacement.chars().count(),
            replacement.len(),
            path.display()
        ),
    );
    replacement
}

#[must_use]
pub(crate) fn is_persisted_tool_result_output(output: &str) -> bool {
    output.starts_with(PERSISTED_OUTPUT_TAG)
}

fn max_result_size_chars(tool_name: &str) -> usize {
    if tool_name == "edit_file" {
        EDIT_FILE_MAX_RESULT_SIZE_CHARS
    } else {
        DEFAULT_MAX_RESULT_SIZE_CHARS
    }
}

fn persisted_tool_result_path(session: &Session, tool_use_id: &str) -> Option<PathBuf> {
    let session_path = session.persistence_path()?;
    let parent = session_path.parent().unwrap_or_else(|| Path::new("."));
    Some(
        parent
            .join("tool-results")
            .join(&session.session_id)
            .join(format!("{}.txt", sanitize_path_component(tool_use_id))),
    )
}

fn persist_tool_result(path: &Path, output: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => file.write_all(output.as_bytes()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

fn build_persisted_output_message(path: &Path, output: &str) -> String {
    let preview = truncate_to_char_boundary(output, PREVIEW_SIZE_BYTES);
    let mut message = String::new();
    message.push_str(PERSISTED_OUTPUT_TAG);
    message.push('\n');
    message.push_str(&format!(
        "Output too large ({}). Full output saved to: {}\n\n",
        format_file_size(output.len()),
        path.display()
    ));
    message.push_str(&format!(
        "Preview (first {}):\n",
        format_file_size(PREVIEW_SIZE_BYTES)
    ));
    message.push_str(preview);
    if preview.len() < output.len() {
        message.push_str("\n...\n");
    } else {
        message.push('\n');
    }
    message.push_str(PERSISTED_OUTPUT_CLOSING_TAG);
    message
}

fn format_file_size(bytes: usize) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    format!("{:.1} KB", bytes as f64 / 1024.0)
}

fn sanitize_path_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "tool-result".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Session;

    #[test]
    fn leaves_small_tool_result_inline() {
        let session = Session::new().with_persistence_path(temp_path("small-session.jsonl"));
        let output = "short output".to_string();

        let stabilized = stabilize_tool_result_output(
            &session,
            "tooluse-small",
            "edit_file",
            output.clone(),
            false,
        );

        assert_eq!(stabilized, output);
    }

    #[test]
    fn persists_large_edit_file_result_with_stable_preview() {
        let session_path = temp_path("large-session.jsonl");
        let session = Session::new().with_persistence_path(session_path.clone());
        let output = "abcdef".repeat(4_000);

        let stabilized = stabilize_tool_result_output(
            &session,
            "tooluse-large",
            "edit_file",
            output.clone(),
            false,
        );
        let second = stabilize_tool_result_output(
            &session,
            "tooluse-large",
            "edit_file",
            output.clone(),
            false,
        );

        assert_eq!(stabilized, second);
        assert!(stabilized.starts_with(PERSISTED_OUTPUT_TAG));
        assert!(stabilized.contains("Full output saved to:"));
        assert!(stabilized.contains("Preview (first 2.0 KB):"));
        assert!(stabilized.ends_with(PERSISTED_OUTPUT_CLOSING_TAG));

        let persisted = session_path
            .parent()
            .expect("session path should have parent")
            .join("tool-results")
            .join(&session.session_id)
            .join("tooluse-large.txt");
        assert_eq!(
            fs::read_to_string(persisted).expect("persisted output should read"),
            output
        );
    }

    #[test]
    fn does_not_persist_tool_errors() {
        let session = Session::new().with_persistence_path(temp_path("error-session.jsonl"));
        let output = "abcdef".repeat(4_000);

        let stabilized = stabilize_tool_result_output(
            &session,
            "tooluse-error",
            "edit_file",
            output.clone(),
            true,
        );

        assert_eq!(stabilized, output);
    }

    fn temp_path(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("claw-tool-result-budget-{label}-{nanos}"))
    }
}
