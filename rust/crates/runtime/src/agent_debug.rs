use std::fs::{create_dir_all, File, OpenOptions};
use std::io::Write as _;
use std::panic::Location;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const AGENT_DEBUG_ENV_VAR: &str = "CLAWD_AGENT_DEBUG";
const AGENT_DEBUG_FILE_NAME: &str = "clawd-agent-debug.log";
const DEBUG_LOG_LOCAL_OFFSET_SECONDS: i64 = 8 * 3_600;

#[derive(Debug, Default)]
struct AgentDebugWriter {
    current_dir: Option<PathBuf>,
    file: Option<File>,
}

impl AgentDebugWriter {
    fn file_for_dir(&mut self, dir: &Path) -> Option<&mut File> {
        let needs_reopen = self.current_dir.as_deref() != Some(dir) || self.file.is_none();
        if needs_reopen {
            create_dir_all(dir).ok()?;
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join(AGENT_DEBUG_FILE_NAME))
                .ok()?;
            self.current_dir = Some(dir.to_path_buf());
            self.file = Some(file);
        }
        self.file.as_mut()
    }
}

fn agent_debug_writer() -> &'static Mutex<AgentDebugWriter> {
    static WRITER: OnceLock<Mutex<AgentDebugWriter>> = OnceLock::new();
    WRITER.get_or_init(|| Mutex::new(AgentDebugWriter::default()))
}

fn agent_debug_dir() -> Option<PathBuf> {
    let raw = std::env::var(AGENT_DEBUG_ENV_VAR).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = trimmed.to_ascii_lowercase();
    if normalized == "0" || normalized == "false" || normalized == "off" {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

#[track_caller]
pub fn agent_debug_log(event: &str, detail: impl AsRef<str>) {
    let Some(dir) = agent_debug_dir() else {
        return;
    };

    let mut writer = agent_debug_writer()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(file) = writer.file_for_dir(&dir) else {
        return;
    };

    let now_us = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_micros())
        .unwrap_or(0);
    let formatted_timestamp = format_timestamp_micros(now_us);
    let thread = std::thread::current();
    let thread_name = thread.name().unwrap_or("unnamed");
    let thread_id = format!("{:?}", thread.id());
    let caller = Location::caller();
    let caller_file = Path::new(caller.file())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(caller.file());
    let caller_site = format!("{caller_file}:{}", caller.line());
    let prefix = format!(
        "[clawd-agent-debug ts={formatted_timestamp} pid={} thread={thread_name} tid={thread_id} caller={caller_site}] {event}",
        std::process::id()
    );
    let detail = detail.as_ref();
    if detail.is_empty() {
        let _ = writeln!(file, "{prefix}");
    } else {
        for line in detail.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let _ = writeln!(file, "{prefix} {line}");
        }
    }
    let _ = file.flush();
}

fn format_timestamp_micros(timestamp_us: u128) -> String {
    format_timestamp_micros_with_offset(timestamp_us, DEBUG_LOG_LOCAL_OFFSET_SECONDS)
}

fn format_timestamp_micros_with_offset(timestamp_us: u128, offset_seconds: i64) -> String {
    let secs = i128::try_from(timestamp_us / 1_000_000).unwrap_or(0) + i128::from(offset_seconds);
    let micros_of_second = timestamp_us % 1_000_000;
    let millis = micros_of_second / 1_000;
    let micros = micros_of_second % 1_000;
    let days_since_epoch = secs.div_euclid(86_400);
    let seconds_of_day = secs.rem_euclid(86_400);
    let hours = seconds_of_day / 3_600;
    let minutes = (seconds_of_day % 3_600) / 60;
    let seconds = seconds_of_day % 60;
    let (year, month, day) = civil_from_days(i64::try_from(days_since_epoch).unwrap_or(0));
    format!(
        "{year:04}-{month:02}-{day:02} {hours:02}:{minutes:02}:{seconds:02} {millis:03} {micros:03}"
    )
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 {
        z / 146_097
    } else {
        (z - 146_096) / 146_097
    };
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = y + i64::from(m <= 2);
    (y as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::{
        agent_debug_log, format_timestamp_micros_with_offset, AGENT_DEBUG_ENV_VAR,
        AGENT_DEBUG_FILE_NAME,
    };

    #[test]
    fn agent_debug_log_writes_to_configured_directory() {
        let _guard = crate::test_env_lock();
        let dir = std::env::temp_dir().join(format!(
            "clawd-agent-debug-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));

        std::env::set_var(AGENT_DEBUG_ENV_VAR, &dir);
        agent_debug_log("test.event", "detail=ok");
        agent_debug_log("test.multiline", "line-1\nline-2");

        let contents =
            std::fs::read_to_string(dir.join(AGENT_DEBUG_FILE_NAME)).expect("debug log file");
        for line in contents.lines() {
            assert!(line.contains("ts="));
        }
        assert!(contents.contains("caller=agent_debug.rs:"));
        assert!(contents.contains("test.event detail=ok"));
        assert!(contents.contains("test.multiline line-1"));
        assert!(contents.contains("test.multiline line-2"));

        std::env::remove_var(AGENT_DEBUG_ENV_VAR);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn format_timestamp_micros_renders_human_readable_timestamp() {
        let timestamp_us: u128 = 1_673_786_096_789_123;
        let formatted = format_timestamp_micros_with_offset(timestamp_us, 0);
        assert_eq!(formatted, "2023-01-15 12:34:56 789 123");
    }

    #[test]
    fn format_timestamp_micros_uses_local_timezone() {
        let timestamp_us: u128 = 1_673_786_096_789_123;
        let formatted = format_timestamp_micros_with_offset(timestamp_us, 8 * 3_600);
        assert_eq!(formatted, "2023-01-15 20:34:56 789 123");
    }
}
