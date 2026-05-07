use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use runtime::Session;

use crate::{
    LATEST_SESSION_REFERENCE, LEGACY_SESSION_EXTENSION, PRIMARY_SESSION_EXTENSION,
    SESSION_REFERENCE_ALIASES,
};

#[derive(Debug, Clone)]
pub(crate) struct SessionHandle {
    pub(crate) id: String,
    pub(crate) path: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct ManagedSessionSummary {
    pub(crate) id: String,
    pub(crate) path: PathBuf,
    pub(crate) modified_epoch_millis: u128,
    pub(crate) message_count: usize,
    pub(crate) parent_session_id: Option<String>,
    pub(crate) branch_name: Option<String>,
    pub(crate) created_at_ms: u64,
    pub(crate) session_counter: Option<u64>,
}

fn sessions_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let store = runtime::SessionStore::from_cwd(&cwd)
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
    Ok(store.sessions_dir().to_path_buf())
}

pub(crate) fn create_managed_session_handle(
    session_id: &str,
) -> Result<SessionHandle, Box<dyn std::error::Error>> {
    let id = session_id.to_string();
    let path = sessions_dir()?.join(format!("{id}.{PRIMARY_SESSION_EXTENSION}"));
    Ok(SessionHandle { id, path })
}

pub(crate) fn resolve_session_reference(
    reference: &str,
) -> Result<SessionHandle, Box<dyn std::error::Error>> {
    if SESSION_REFERENCE_ALIASES
        .iter()
        .any(|alias| reference.eq_ignore_ascii_case(alias))
    {
        let latest = latest_managed_session()?;
        return Ok(SessionHandle {
            id: latest.id,
            path: latest.path,
        });
    }

    let direct = PathBuf::from(reference);
    let looks_like_path = direct.extension().is_some() || direct.components().count() > 1;
    let path = if direct.exists() {
        direct
    } else if looks_like_path {
        return Err(format_missing_session_reference(reference).into());
    } else {
        resolve_managed_session_path(reference)?
    };
    let id = path
        .file_name()
        .and_then(|value| value.to_str())
        .and_then(|name| {
            name.strip_suffix(&format!(".{PRIMARY_SESSION_EXTENSION}"))
                .or_else(|| name.strip_suffix(&format!(".{LEGACY_SESSION_EXTENSION}")))
        })
        .unwrap_or(reference)
        .to_string();
    Ok(SessionHandle { id, path })
}

fn resolve_managed_session_path(session_id: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let directory = sessions_dir()?;
    for extension in [PRIMARY_SESSION_EXTENSION, LEGACY_SESSION_EXTENSION] {
        let path = directory.join(format!("{session_id}.{extension}"));
        if path.exists() {
            return Ok(path);
        }
    }
    // Backward compatibility: pre-isolation sessions were stored at
    // `.claw/sessions/<id>.{jsonl,json}` without the per-workspace hash
    // subdirectory. Walk up from `directory` to the `.claw/sessions/` root
    // and try the flat layout as a fallback so users do not lose access
    // to their pre-upgrade managed sessions.
    if let Some(legacy_root) = directory
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == "sessions"))
    {
        for extension in [PRIMARY_SESSION_EXTENSION, LEGACY_SESSION_EXTENSION] {
            let path = legacy_root.join(format!("{session_id}.{extension}"));
            if path.exists() {
                return Ok(path);
            }
        }
    }
    Err(format_missing_session_reference(session_id).into())
}

fn is_managed_session_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|extension| {
            extension == PRIMARY_SESSION_EXTENSION || extension == LEGACY_SESSION_EXTENSION
        })
}

fn collect_sessions_from_dir(
    directory: &Path,
    sessions: &mut Vec<ManagedSessionSummary>,
) -> Result<(), Box<dyn std::error::Error>> {
    if !directory.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if !is_managed_session_file(&path) {
            continue;
        }
        let metadata = entry.metadata()?;
        let modified_epoch_millis = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis())
            .unwrap_or_default();
        let (
            id,
            message_count,
            parent_session_id,
            branch_name,
            logical_modified_epoch_millis,
            created_at_ms,
            session_counter,
        ) = if let Ok(session) = Session::load_from_path(&path) {
            let parent_session_id = session
                .fork
                .as_ref()
                .map(|fork| fork.parent_session_id.clone());
            let branch_name = session
                .fork
                .as_ref()
                .and_then(|fork| fork.branch_name.clone());
            let session_counter = session_counter_from_id(&session.session_id);
            (
                session.session_id,
                session.messages.len(),
                parent_session_id,
                branch_name,
                u128::from(session.updated_at_ms),
                session.created_at_ms,
                session_counter,
            )
        } else {
            let id = path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("unknown")
                .to_string();
            (
                id.clone(),
                0,
                None,
                None,
                modified_epoch_millis,
                session_created_at_from_id(&id)
                    .unwrap_or(u64::try_from(modified_epoch_millis).unwrap_or(u64::MAX)),
                session_counter_from_id(&id),
            )
        };
        sessions.push(ManagedSessionSummary {
            id,
            path,
            modified_epoch_millis: logical_modified_epoch_millis,
            message_count,
            parent_session_id,
            branch_name,
            created_at_ms,
            session_counter,
        });
    }
    Ok(())
}

fn sort_managed_session_summaries(sessions: &mut [ManagedSessionSummary]) {
    sessions.sort_by(|left, right| {
        right
            .modified_epoch_millis
            .cmp(&left.modified_epoch_millis)
            .then_with(|| right.created_at_ms.cmp(&left.created_at_ms))
            .then_with(|| right.session_counter.cmp(&left.session_counter))
            .then_with(|| right.id.cmp(&left.id))
    });
}

fn session_created_at_from_id(session_id: &str) -> Option<u64> {
    parse_session_id_components(session_id).map(|(created_at_ms, _)| created_at_ms)
}

fn session_counter_from_id(session_id: &str) -> Option<u64> {
    parse_session_id_components(session_id).map(|(_, counter)| counter)
}

fn parse_session_id_components(session_id: &str) -> Option<(u64, u64)> {
    let suffix = session_id.strip_prefix("session-")?;
    let (created_at_ms, counter) = suffix.rsplit_once('-')?;
    Some((created_at_ms.parse().ok()?, counter.parse().ok()?))
}

pub(crate) fn list_managed_sessions(
) -> Result<Vec<ManagedSessionSummary>, Box<dyn std::error::Error>> {
    let mut sessions = Vec::new();
    let primary_dir = sessions_dir()?;
    collect_sessions_from_dir(&primary_dir, &mut sessions)?;

    // Backward compatibility: include sessions stored in the pre-isolation
    // flat `.claw/sessions/` root so users do not lose access to existing
    // managed sessions after the workspace-hashed subdirectory rollout.
    if let Some(legacy_root) = primary_dir
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == "sessions"))
    {
        collect_sessions_from_dir(legacy_root, &mut sessions)?;
    }

    sort_managed_session_summaries(&mut sessions);
    Ok(sessions)
}

pub(crate) fn latest_managed_session() -> Result<ManagedSessionSummary, Box<dyn std::error::Error>>
{
    list_managed_sessions()?
        .into_iter()
        .next()
        .ok_or_else(|| format_no_managed_sessions().into())
}

pub(crate) fn delete_managed_session(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if !path.exists() {
        return Err(format!("session file does not exist: {}", path.display()).into());
    }
    fs::remove_file(path)?;
    Ok(())
}

pub(crate) fn confirm_session_deletion(session_id: &str) -> bool {
    print!("Delete session '{session_id}'? This cannot be undone. [y/N]: ");
    io::stdout().flush().unwrap_or(());
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES")
}

fn format_missing_session_reference(reference: &str) -> String {
    format!(
        "session not found: {reference}\nHint: managed sessions live in .claw/sessions/. Try `{LATEST_SESSION_REFERENCE}` for the most recent session or `/session list` in the REPL."
    )
}

fn format_no_managed_sessions() -> String {
    format!(
        "no managed sessions found in .claw/sessions/\nStart `claw` to create a session, then rerun with `--resume {LATEST_SESSION_REFERENCE}`."
    )
}

pub(crate) fn render_session_list(
    active_session_id: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let sessions = list_managed_sessions()?;
    let mut lines = vec![
        "Sessions".to_string(),
        format!("  Directory         {}", sessions_dir()?.display()),
    ];
    if sessions.is_empty() {
        lines.push("  No managed sessions saved yet.".to_string());
        return Ok(lines.join("\n"));
    }
    for session in sessions {
        let marker = if session.id == active_session_id {
            "● current"
        } else {
            "○ saved"
        };
        let lineage = match (
            session.branch_name.as_deref(),
            session.parent_session_id.as_deref(),
        ) {
            (Some(branch_name), Some(parent_session_id)) => {
                format!(" branch={branch_name} from={parent_session_id}")
            }
            (None, Some(parent_session_id)) => format!(" from={parent_session_id}"),
            (Some(branch_name), None) => format!(" branch={branch_name}"),
            (None, None) => String::new(),
        };
        lines.push(format!(
            "  {id:<20} {marker:<10} msgs={msgs:<4} modified={modified}{lineage} path={path}",
            id = session.id,
            msgs = session.message_count,
            modified = format_session_modified_age(session.modified_epoch_millis),
            lineage = lineage,
            path = session.path.display(),
        ));
    }
    Ok(lines.join("\n"))
}

fn format_session_modified_age(modified_epoch_millis: u128) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map_or(modified_epoch_millis, |duration| duration.as_millis());
    let delta_seconds = now
        .saturating_sub(modified_epoch_millis)
        .checked_div(1_000)
        .unwrap_or_default();
    match delta_seconds {
        0..=4 => "just-now".to_string(),
        5..=59 => format!("{delta_seconds}s-ago"),
        60..=3_599 => format!("{}m-ago", delta_seconds / 60),
        3_600..=86_399 => format!("{}h-ago", delta_seconds / 3_600),
        _ => format!("{}d-ago", delta_seconds / 86_400),
    }
}

pub(crate) fn write_session_clear_backup(
    session: &Session,
    session_path: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let backup_path = session_clear_backup_path(session_path);
    session.save_to_path(&backup_path)?;
    Ok(backup_path)
}

fn session_clear_backup_path(session_path: &Path) -> PathBuf {
    let timestamp = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map_or(0, |duration| duration.as_millis());
    let file_name = session_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("session.jsonl");
    session_path.with_file_name(format!("{file_name}.before-clear-{timestamp}.bak"))
}
