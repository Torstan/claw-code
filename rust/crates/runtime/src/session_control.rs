#![allow(dead_code)]
use std::env;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::session::{Session, SessionError};

/// Per-worktree session store that namespaces on-disk session files by
/// workspace fingerprint so that parallel `opencode serve` instances never
/// collide.
///
/// Create via [`SessionStore::from_cwd`] (derives the store path from the
/// server's working directory) or [`SessionStore::from_data_dir`] (honours an
/// explicit `--data-dir` flag).  Both constructors produce a directory layout
/// of `<data_dir>/sessions/<workspace_hash>/` where `<workspace_hash>` is a
/// stable hex digest of the canonical workspace root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStore {
    /// Resolved root of the session namespace, e.g.
    /// `/home/user/project/.claw/sessions/a1b2c3d4e5f60718/`.
    sessions_root: PathBuf,
    /// The canonical workspace path that was fingerprinted.
    workspace_root: PathBuf,
}

impl SessionStore {
    /// Build a store from the server's current working directory.
    ///
    /// The on-disk layout becomes `<cwd>/.claw/sessions/<workspace_hash>/`.
    pub fn from_cwd(cwd: impl AsRef<Path>) -> Result<Self, SessionControlError> {
        let cwd = cwd.as_ref();
        let sessions_root = cwd
            .join(".claw")
            .join("sessions")
            .join(workspace_fingerprint(cwd));
        fs::create_dir_all(&sessions_root)?;
        Ok(Self {
            sessions_root,
            workspace_root: cwd.to_path_buf(),
        })
    }

    /// Build a store from an explicit `--data-dir` flag.
    ///
    /// The on-disk layout becomes `<data_dir>/sessions/<workspace_hash>/`
    /// where `<workspace_hash>` is derived from `workspace_root`.
    pub fn from_data_dir(
        data_dir: impl AsRef<Path>,
        workspace_root: impl AsRef<Path>,
    ) -> Result<Self, SessionControlError> {
        let workspace_root = workspace_root.as_ref();
        let sessions_root = data_dir
            .as_ref()
            .join("sessions")
            .join(workspace_fingerprint(workspace_root));
        fs::create_dir_all(&sessions_root)?;
        Ok(Self {
            sessions_root,
            workspace_root: workspace_root.to_path_buf(),
        })
    }

    /// The fully resolved sessions directory for this namespace.
    #[must_use]
    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_root
    }

    /// The workspace root this store is bound to.
    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    #[must_use]
    pub fn create_handle(&self, session_id: &str) -> SessionHandle {
        let id = session_id.to_string();
        let path = self
            .sessions_root
            .join(format!("{id}.{PRIMARY_SESSION_EXTENSION}"));
        SessionHandle { id, path }
    }

    pub fn resolve_reference(&self, reference: &str) -> Result<SessionHandle, SessionControlError> {
        if is_session_reference_alias(reference) {
            let latest = self.latest_session()?;
            return Ok(SessionHandle {
                id: latest.id,
                path: latest.path,
            });
        }

        let direct = PathBuf::from(reference);
        let candidate = if direct.is_absolute() {
            direct.clone()
        } else {
            self.workspace_root.join(&direct)
        };
        let looks_like_path = direct.extension().is_some() || direct.components().count() > 1;
        let path = if candidate.exists() {
            candidate
        } else if looks_like_path {
            return Err(SessionControlError::Format(
                format_missing_session_reference(reference),
            ));
        } else {
            self.resolve_managed_path(reference)?
        };

        Ok(SessionHandle {
            id: session_id_from_path(&path).unwrap_or_else(|| reference.to_string()),
            path,
        })
    }

    pub fn resolve_managed_path(&self, session_id: &str) -> Result<PathBuf, SessionControlError> {
        for extension in [PRIMARY_SESSION_EXTENSION, LEGACY_SESSION_EXTENSION] {
            let path = self.sessions_root.join(format!("{session_id}.{extension}"));
            if path.exists() {
                return Ok(path);
            }
        }
        Err(SessionControlError::Format(
            format_missing_session_reference(session_id),
        ))
    }

    pub fn list_sessions(&self) -> Result<Vec<ManagedSessionSummary>, SessionControlError> {
        let mut sessions = Vec::new();
        let read_result = fs::read_dir(&self.sessions_root);
        let entries = match read_result {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(sessions),
            Err(err) => return Err(err.into()),
        };
        for entry in entries {
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
        sort_managed_session_summaries(&mut sessions);
        Ok(sessions)
    }

    pub fn latest_session(&self) -> Result<ManagedSessionSummary, SessionControlError> {
        self.list_sessions()?
            .into_iter()
            .next()
            .ok_or_else(|| SessionControlError::Format(format_no_managed_sessions()))
    }

    pub fn load_session(
        &self,
        reference: &str,
    ) -> Result<LoadedManagedSession, SessionControlError> {
        let handle = self.resolve_reference(reference)?;
        let session = Session::load_from_path(&handle.path)?;
        Ok(LoadedManagedSession {
            handle: SessionHandle {
                id: session.session_id.clone(),
                path: handle.path,
            },
            session,
        })
    }

    pub fn fork_session(
        &self,
        session: &Session,
        branch_name: Option<String>,
    ) -> Result<ForkedManagedSession, SessionControlError> {
        let parent_session_id = session.session_id.clone();
        let forked = session.fork(branch_name);
        let handle = self.create_handle(&forked.session_id);
        let branch_name = forked
            .fork
            .as_ref()
            .and_then(|fork| fork.branch_name.clone());
        let forked = forked.with_persistence_path(handle.path.clone());
        forked.save_to_path(&handle.path)?;
        Ok(ForkedManagedSession {
            parent_session_id,
            handle,
            session: forked,
            branch_name,
        })
    }
}

/// Stable hex fingerprint of a workspace path.
///
/// Uses FNV-1a (64-bit) to produce a 16-char hex string that partitions the
/// on-disk session directory per workspace root.
#[must_use]
pub fn workspace_fingerprint(workspace_root: &Path) -> String {
    let input = workspace_root.to_string_lossy();
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

pub const PRIMARY_SESSION_EXTENSION: &str = "jsonl";
pub const LEGACY_SESSION_EXTENSION: &str = "json";
pub const LATEST_SESSION_REFERENCE: &str = "latest";

const SESSION_REFERENCE_ALIASES: &[&str] = &[LATEST_SESSION_REFERENCE, "last", "recent"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHandle {
    pub id: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedSessionSummary {
    pub id: String,
    pub path: PathBuf,
    pub modified_epoch_millis: u128,
    pub message_count: usize,
    pub parent_session_id: Option<String>,
    pub branch_name: Option<String>,
    created_at_ms: u64,
    session_counter: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedManagedSession {
    pub handle: SessionHandle,
    pub session: Session,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkedManagedSession {
    pub parent_session_id: String,
    pub handle: SessionHandle,
    pub session: Session,
    pub branch_name: Option<String>,
}

#[derive(Debug)]
pub enum SessionControlError {
    Io(std::io::Error),
    Session(SessionError),
    Format(String),
}

impl Display for SessionControlError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Session(error) => write!(f, "{error}"),
            Self::Format(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for SessionControlError {}

impl From<std::io::Error> for SessionControlError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<SessionError> for SessionControlError {
    fn from(value: SessionError) -> Self {
        Self::Session(value)
    }
}

pub fn sessions_dir() -> Result<PathBuf, SessionControlError> {
    managed_sessions_dir_for(env::current_dir()?)
}

pub fn managed_sessions_dir_for(
    base_dir: impl AsRef<Path>,
) -> Result<PathBuf, SessionControlError> {
    let path = base_dir.as_ref().join(".claw").join("sessions");
    fs::create_dir_all(&path)?;
    Ok(path)
}

pub fn create_managed_session_handle(
    session_id: &str,
) -> Result<SessionHandle, SessionControlError> {
    create_managed_session_handle_for(env::current_dir()?, session_id)
}

pub fn create_managed_session_handle_for(
    base_dir: impl AsRef<Path>,
    session_id: &str,
) -> Result<SessionHandle, SessionControlError> {
    let id = session_id.to_string();
    let path =
        managed_sessions_dir_for(base_dir)?.join(format!("{id}.{PRIMARY_SESSION_EXTENSION}"));
    Ok(SessionHandle { id, path })
}

pub fn resolve_session_reference(reference: &str) -> Result<SessionHandle, SessionControlError> {
    resolve_session_reference_for(env::current_dir()?, reference)
}

pub fn resolve_session_reference_for(
    base_dir: impl AsRef<Path>,
    reference: &str,
) -> Result<SessionHandle, SessionControlError> {
    let base_dir = base_dir.as_ref();
    if is_session_reference_alias(reference) {
        let latest = latest_managed_session_for(base_dir)?;
        return Ok(SessionHandle {
            id: latest.id,
            path: latest.path,
        });
    }

    let direct = PathBuf::from(reference);
    let candidate = if direct.is_absolute() {
        direct.clone()
    } else {
        base_dir.join(&direct)
    };
    let looks_like_path = direct.extension().is_some() || direct.components().count() > 1;
    let path = if candidate.exists() {
        candidate
    } else if looks_like_path {
        return Err(SessionControlError::Format(
            format_missing_session_reference(reference),
        ));
    } else {
        resolve_managed_session_path_for(base_dir, reference)?
    };

    Ok(SessionHandle {
        id: session_id_from_path(&path).unwrap_or_else(|| reference.to_string()),
        path,
    })
}

pub fn resolve_managed_session_path(session_id: &str) -> Result<PathBuf, SessionControlError> {
    resolve_managed_session_path_for(env::current_dir()?, session_id)
}

pub fn resolve_managed_session_path_for(
    base_dir: impl AsRef<Path>,
    session_id: &str,
) -> Result<PathBuf, SessionControlError> {
    let directory = managed_sessions_dir_for(base_dir)?;
    for extension in [PRIMARY_SESSION_EXTENSION, LEGACY_SESSION_EXTENSION] {
        let path = directory.join(format!("{session_id}.{extension}"));
        if path.exists() {
            return Ok(path);
        }
    }
    Err(SessionControlError::Format(
        format_missing_session_reference(session_id),
    ))
}

#[must_use]
pub fn is_managed_session_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|extension| {
            extension == PRIMARY_SESSION_EXTENSION || extension == LEGACY_SESSION_EXTENSION
        })
}

pub fn list_managed_sessions() -> Result<Vec<ManagedSessionSummary>, SessionControlError> {
    list_managed_sessions_for(env::current_dir()?)
}

pub fn list_managed_sessions_for(
    base_dir: impl AsRef<Path>,
) -> Result<Vec<ManagedSessionSummary>, SessionControlError> {
    let mut sessions = Vec::new();
    for entry in fs::read_dir(managed_sessions_dir_for(base_dir)?)? {
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
    sort_managed_session_summaries(&mut sessions);
    Ok(sessions)
}

pub fn latest_managed_session() -> Result<ManagedSessionSummary, SessionControlError> {
    latest_managed_session_for(env::current_dir()?)
}

pub fn latest_managed_session_for(
    base_dir: impl AsRef<Path>,
) -> Result<ManagedSessionSummary, SessionControlError> {
    list_managed_sessions_for(base_dir)?
        .into_iter()
        .next()
        .ok_or_else(|| SessionControlError::Format(format_no_managed_sessions()))
}

pub fn load_managed_session(reference: &str) -> Result<LoadedManagedSession, SessionControlError> {
    load_managed_session_for(env::current_dir()?, reference)
}

pub fn load_managed_session_for(
    base_dir: impl AsRef<Path>,
    reference: &str,
) -> Result<LoadedManagedSession, SessionControlError> {
    let handle = resolve_session_reference_for(base_dir, reference)?;
    let session = Session::load_from_path(&handle.path)?;
    Ok(LoadedManagedSession {
        handle: SessionHandle {
            id: session.session_id.clone(),
            path: handle.path,
        },
        session,
    })
}

pub fn fork_managed_session(
    session: &Session,
    branch_name: Option<String>,
) -> Result<ForkedManagedSession, SessionControlError> {
    fork_managed_session_for(env::current_dir()?, session, branch_name)
}

pub fn fork_managed_session_for(
    base_dir: impl AsRef<Path>,
    session: &Session,
    branch_name: Option<String>,
) -> Result<ForkedManagedSession, SessionControlError> {
    let parent_session_id = session.session_id.clone();
    let forked = session.fork(branch_name);
    let handle = create_managed_session_handle_for(base_dir, &forked.session_id)?;
    let branch_name = forked
        .fork
        .as_ref()
        .and_then(|fork| fork.branch_name.clone());
    let forked = forked.with_persistence_path(handle.path.clone());
    forked.save_to_path(&handle.path)?;
    Ok(ForkedManagedSession {
        parent_session_id,
        handle,
        session: forked,
        branch_name,
    })
}

#[must_use]
pub fn is_session_reference_alias(reference: &str) -> bool {
    SESSION_REFERENCE_ALIASES
        .iter()
        .any(|alias| reference.eq_ignore_ascii_case(alias))
}

fn session_id_from_path(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|value| value.to_str())
        .and_then(|name| {
            name.strip_suffix(&format!(".{PRIMARY_SESSION_EXTENSION}"))
                .or_else(|| name.strip_suffix(&format!(".{LEGACY_SESSION_EXTENSION}")))
        })
        .map(ToOwned::to_owned)
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

#[cfg(test)]
mod tests {
    use super::{
        create_managed_session_handle_for, fork_managed_session_for, is_session_reference_alias,
        latest_managed_session_for, list_managed_sessions_for, load_managed_session_for,
        resolve_session_reference_for, workspace_fingerprint, ManagedSessionSummary, SessionStore,
        LATEST_SESSION_REFERENCE,
    };
    use crate::session::Session;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("runtime-session-control-{nanos}"))
    }

    fn persist_session(root: &Path, text: &str) -> Session {
        let mut session = Session::new();
        session
            .push_user_text(text)
            .expect("session message should save");
        let handle = create_managed_session_handle_for(root, &session.session_id)
            .expect("managed session handle should build");
        let session = session.with_persistence_path(handle.path.clone());
        session
            .save_to_path(&handle.path)
            .expect("session should persist");
        session
    }

    fn persist_session_with_metadata(
        root: &Path,
        session_id: &str,
        created_at_ms: u64,
        updated_at_ms: u64,
    ) -> Session {
        let mut session = Session::new();
        session.session_id = session_id.to_string();
        session
            .push_user_text(session_id)
            .expect("session message should save");
        session.created_at_ms = created_at_ms;
        session.updated_at_ms = updated_at_ms;
        let handle = create_managed_session_handle_for(root, &session.session_id)
            .expect("managed session handle should build");
        let session = session.with_persistence_path(handle.path.clone());
        session
            .save_to_path(&handle.path)
            .expect("session should persist");
        session
    }

    fn wait_for_next_millisecond() {
        let start = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_millis();
        while SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_millis()
            <= start
        {}
    }

    fn set_modified_time(path: &Path, timestamp_ms: u64) {
        let timestamp = UNIX_EPOCH + std::time::Duration::from_millis(timestamp_ms);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("session file should open");
        let times = std::fs::FileTimes::new()
            .set_accessed(timestamp)
            .set_modified(timestamp);
        file.set_times(times)
            .expect("session file timestamps should update");
    }

    fn summary_by_id<'a>(
        summaries: &'a [ManagedSessionSummary],
        id: &str,
    ) -> &'a ManagedSessionSummary {
        summaries
            .iter()
            .find(|summary| summary.id == id)
            .expect("session summary should exist")
    }

    #[test]
    fn creates_and_lists_managed_sessions() {
        // given
        let root = temp_dir();
        fs::create_dir_all(&root).expect("root dir should exist");
        let older = persist_session(&root, "older session");
        wait_for_next_millisecond();
        let newer = persist_session(&root, "newer session");

        // when
        let sessions = list_managed_sessions_for(&root).expect("managed sessions should list");

        // then
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, newer.session_id);
        assert_eq!(summary_by_id(&sessions, &older.session_id).message_count, 1);
        assert_eq!(summary_by_id(&sessions, &newer.session_id).message_count, 1);
        fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[test]
    fn resolves_latest_alias_and_loads_session_from_workspace_root() {
        // given
        let root = temp_dir();
        fs::create_dir_all(&root).expect("root dir should exist");
        let older = persist_session(&root, "older session");
        wait_for_next_millisecond();
        let newer = persist_session(&root, "newer session");

        // when
        let handle = resolve_session_reference_for(&root, LATEST_SESSION_REFERENCE)
            .expect("latest alias should resolve");
        let loaded = load_managed_session_for(&root, "recent")
            .expect("recent alias should load the latest session");

        // then
        assert_eq!(handle.id, newer.session_id);
        assert_eq!(loaded.handle.id, newer.session_id);
        assert_eq!(loaded.session.messages.len(), 1);
        assert_ne!(loaded.handle.id, older.session_id);
        assert!(is_session_reference_alias("last"));
        fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[test]
    fn forks_session_into_managed_storage_with_lineage() {
        // given
        let root = temp_dir();
        fs::create_dir_all(&root).expect("root dir should exist");
        let source = persist_session(&root, "parent session");

        // when
        let forked = fork_managed_session_for(&root, &source, Some("incident-review".to_string()))
            .expect("session should fork");
        let sessions = list_managed_sessions_for(&root).expect("managed sessions should list");
        let summary = summary_by_id(&sessions, &forked.handle.id);

        // then
        assert_eq!(forked.parent_session_id, source.session_id);
        assert_eq!(forked.branch_name.as_deref(), Some("incident-review"));
        assert_eq!(
            summary.parent_session_id.as_deref(),
            Some(source.session_id.as_str())
        );
        assert_eq!(summary.branch_name.as_deref(), Some("incident-review"));
        assert_eq!(
            forked.session.persistence_path(),
            Some(forked.handle.path.as_path())
        );
        fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    // ------------------------------------------------------------------
    // Per-worktree session isolation (SessionStore) tests
    // ------------------------------------------------------------------

    fn persist_session_via_store(store: &SessionStore, text: &str) -> Session {
        let mut session = Session::new();
        session
            .push_user_text(text)
            .expect("session message should save");
        let handle = store.create_handle(&session.session_id);
        let session = session.with_persistence_path(handle.path.clone());
        session
            .save_to_path(&handle.path)
            .expect("session should persist");
        session
    }

    fn persist_session_via_store_with_metadata(
        store: &SessionStore,
        session_id: &str,
        created_at_ms: u64,
        updated_at_ms: u64,
    ) -> Session {
        let mut session = Session::new();
        session.session_id = session_id.to_string();
        session
            .push_user_text(session_id)
            .expect("session message should save");
        session.created_at_ms = created_at_ms;
        session.updated_at_ms = updated_at_ms;
        let handle = store.create_handle(&session.session_id);
        let session = session.with_persistence_path(handle.path.clone());
        session
            .save_to_path(&handle.path)
            .expect("session should persist");
        session
    }

    #[test]
    fn workspace_fingerprint_is_deterministic_and_differs_per_path() {
        // given
        let path_a = Path::new("/tmp/worktree-alpha");
        let path_b = Path::new("/tmp/worktree-beta");

        // when
        let fp_a1 = workspace_fingerprint(path_a);
        let fp_a2 = workspace_fingerprint(path_a);
        let fp_b = workspace_fingerprint(path_b);

        // then
        assert_eq!(fp_a1, fp_a2, "same path must produce the same fingerprint");
        assert_ne!(
            fp_a1, fp_b,
            "different paths must produce different fingerprints"
        );
        assert_eq!(fp_a1.len(), 16, "fingerprint must be a 16-char hex string");
    }

    #[test]
    fn session_store_from_cwd_isolates_sessions_by_workspace() {
        // given
        let base = temp_dir();
        let workspace_a = base.join("repo-alpha");
        let workspace_b = base.join("repo-beta");
        fs::create_dir_all(&workspace_a).expect("workspace a should exist");
        fs::create_dir_all(&workspace_b).expect("workspace b should exist");

        let store_a = SessionStore::from_cwd(&workspace_a).expect("store a should build");
        let store_b = SessionStore::from_cwd(&workspace_b).expect("store b should build");

        // when
        let session_a = persist_session_via_store(&store_a, "alpha work");
        let _session_b = persist_session_via_store(&store_b, "beta work");

        // then — each store only sees its own sessions
        let list_a = store_a.list_sessions().expect("list a");
        let list_b = store_b.list_sessions().expect("list b");
        assert_eq!(list_a.len(), 1, "store a should see exactly one session");
        assert_eq!(list_b.len(), 1, "store b should see exactly one session");
        assert_eq!(list_a[0].id, session_a.session_id);
        assert_ne!(
            store_a.sessions_dir(),
            store_b.sessions_dir(),
            "session directories must differ across workspaces"
        );
        fs::remove_dir_all(base).expect("temp dir should clean up");
    }

    #[test]
    fn session_store_from_data_dir_namespaces_by_workspace() {
        // given
        let base = temp_dir();
        let data_dir = base.join("global-data");
        let workspace_a = PathBuf::from("/tmp/project-one");
        let workspace_b = PathBuf::from("/tmp/project-two");
        fs::create_dir_all(&data_dir).expect("data dir should exist");

        let store_a =
            SessionStore::from_data_dir(&data_dir, &workspace_a).expect("store a should build");
        let store_b =
            SessionStore::from_data_dir(&data_dir, &workspace_b).expect("store b should build");

        // when
        persist_session_via_store(&store_a, "work in project-one");
        persist_session_via_store(&store_b, "work in project-two");

        // then
        assert_ne!(
            store_a.sessions_dir(),
            store_b.sessions_dir(),
            "data-dir stores must namespace by workspace"
        );
        assert_eq!(store_a.list_sessions().expect("list a").len(), 1);
        assert_eq!(store_b.list_sessions().expect("list b").len(), 1);
        assert_eq!(store_a.workspace_root(), workspace_a.as_path());
        assert_eq!(store_b.workspace_root(), workspace_b.as_path());
        fs::remove_dir_all(base).expect("temp dir should clean up");
    }

    #[test]
    fn session_store_create_and_load_round_trip() {
        // given
        let base = temp_dir();
        fs::create_dir_all(&base).expect("base dir should exist");
        let store = SessionStore::from_cwd(&base).expect("store should build");
        let session = persist_session_via_store(&store, "round-trip message");

        // when
        let loaded = store
            .load_session(&session.session_id)
            .expect("session should load via store");

        // then
        assert_eq!(loaded.handle.id, session.session_id);
        assert_eq!(loaded.session.messages.len(), 1);
        fs::remove_dir_all(base).expect("temp dir should clean up");
    }

    #[test]
    fn session_store_latest_and_resolve_reference() {
        // given
        let base = temp_dir();
        fs::create_dir_all(&base).expect("base dir should exist");
        let store = SessionStore::from_cwd(&base).expect("store should build");
        let _older = persist_session_via_store(&store, "older");
        wait_for_next_millisecond();
        let newer = persist_session_via_store(&store, "newer");

        // when
        let latest = store.latest_session().expect("latest should resolve");
        let handle = store
            .resolve_reference("latest")
            .expect("latest alias should resolve");

        // then
        assert_eq!(latest.id, newer.session_id);
        assert_eq!(handle.id, newer.session_id);
        fs::remove_dir_all(base).expect("temp dir should clean up");
    }

    #[test]
    fn session_store_latest_prefers_session_updated_at_when_file_mtime_ties() {
        // given
        let base = temp_dir();
        fs::create_dir_all(&base).expect("base dir should exist");
        let store = SessionStore::from_cwd(&base).expect("store should build");
        let mut older = Session::new();
        older.session_id = "session-z-older".to_string();
        older
            .push_user_text("older")
            .expect("older session message should save");
        older.created_at_ms = 10;
        older.updated_at_ms = 10;
        let older_handle = store.create_handle(&older.session_id);
        let older = older.with_persistence_path(older_handle.path.clone());
        older
            .save_to_path(&older_handle.path)
            .expect("older session should persist");

        let mut newer = Session::new();
        newer.session_id = "session-a-newer".to_string();
        newer
            .push_user_text("newer")
            .expect("newer session message should save");
        newer.created_at_ms = 20;
        newer.updated_at_ms = 20;
        let newer_handle = store.create_handle(&newer.session_id);
        let newer = newer.with_persistence_path(newer_handle.path.clone());
        newer
            .save_to_path(&newer_handle.path)
            .expect("newer session should persist");

        let tied_timestamp_ms = 1_000;
        set_modified_time(
            older.persistence_path().expect("older path should exist"),
            tied_timestamp_ms,
        );
        set_modified_time(
            newer.persistence_path().expect("newer path should exist"),
            tied_timestamp_ms,
        );

        // when
        let latest = store.latest_session().expect("latest should resolve");
        let handle = store
            .resolve_reference("latest")
            .expect("latest alias should resolve");

        // then
        assert_eq!(latest.id, newer.session_id);
        assert_eq!(handle.id, newer.session_id);
        fs::remove_dir_all(base).expect("temp dir should clean up");
    }

    #[test]
    fn latest_managed_session_uses_numeric_session_counter_when_updated_at_ties() {
        // given
        let root = temp_dir();
        fs::create_dir_all(&root).expect("root dir should exist");
        let older = persist_session_with_metadata(&root, "session-1000-9", 1_000, 5_000);
        let newer = persist_session_with_metadata(&root, "session-1000-10", 1_000, 5_000);

        // when
        let sessions = list_managed_sessions_for(&root).expect("managed sessions should list");
        let latest = latest_managed_session_for(&root).expect("latest managed session should load");

        // then
        assert_eq!(sessions[0].id, newer.session_id);
        assert_eq!(sessions[1].id, older.session_id);
        assert_eq!(latest.id, newer.session_id);
        fs::remove_dir_all(root).expect("temp dir should clean up");
    }

    #[test]
    fn session_store_latest_uses_numeric_session_counter_when_updated_at_ties() {
        // given
        let base = temp_dir();
        fs::create_dir_all(&base).expect("base dir should exist");
        let store = SessionStore::from_cwd(&base).expect("store should build");
        let older = persist_session_via_store_with_metadata(&store, "session-1000-9", 1_000, 5_000);
        let newer =
            persist_session_via_store_with_metadata(&store, "session-1000-10", 1_000, 5_000);

        // when
        let sessions = store.list_sessions().expect("list sessions");
        let latest = store.latest_session().expect("latest should resolve");
        let handle = store
            .resolve_reference("latest")
            .expect("latest alias should resolve");

        // then
        assert_eq!(sessions[0].id, newer.session_id);
        assert_eq!(sessions[1].id, older.session_id);
        assert_eq!(latest.id, newer.session_id);
        assert_eq!(handle.id, newer.session_id);
        fs::remove_dir_all(base).expect("temp dir should clean up");
    }

    #[test]
    fn session_store_fork_stays_in_same_namespace() {
        // given
        let base = temp_dir();
        fs::create_dir_all(&base).expect("base dir should exist");
        let store = SessionStore::from_cwd(&base).expect("store should build");
        let source = persist_session_via_store(&store, "parent work");

        // when
        let forked = store
            .fork_session(&source, Some("bugfix".to_string()))
            .expect("fork should succeed");
        let sessions = store.list_sessions().expect("list sessions");

        // then
        assert_eq!(
            sessions.len(),
            2,
            "forked session must land in the same namespace"
        );
        assert_eq!(forked.parent_session_id, source.session_id);
        assert_eq!(forked.branch_name.as_deref(), Some("bugfix"));
        assert!(
            forked.handle.path.starts_with(store.sessions_dir()),
            "forked session path must be inside the store namespace"
        );
        fs::remove_dir_all(base).expect("temp dir should clean up");
    }
}
