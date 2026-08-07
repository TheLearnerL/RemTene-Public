//! Diagnostics sink implementation for collecting content-free runtime events.

use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use remtene_application::ports::{DiagnosticEvent, DiagnosticsControl, DiagnosticsSink};
use serde::Serialize;
use time::{Duration, OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};

const LOCAL_LOG_RETENTION_DAYS: i64 = 3;
const MAX_DIAGNOSTIC_FIELD_CHARS: usize = 512;

/// Console-based diagnostics sink.
///
/// Writes diagnostic events to stderr. `DiagnosticEvent` carries only identifiers,
/// a phase label, a duration and an error code — never transcript text, audio paths,
/// selection contents or API data — so it stays inside the minimal-data invariant.
/// Release builds must report too: a silent sink turns a failing chain into
/// "nothing happened at all", which is indistinguishable from a hang.
pub struct ConsoleDiagnosticsSink {
    enabled: AtomicBool,
}

impl ConsoleDiagnosticsSink {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            enabled: AtomicBool::new(true),
        }
    }
}

impl Default for ConsoleDiagnosticsSink {
    fn default() -> Self {
        Self::new()
    }
}

impl DiagnosticsSink for ConsoleDiagnosticsSink {
    fn record(&self, event: DiagnosticEvent) {
        if !self.enabled() {
            return;
        }
        let session = event
            .session_id
            .map(|id| id.as_uuid().to_string())
            .unwrap_or_else(|| "-".to_owned());
        let phase = event.phase.as_deref().unwrap_or("-");
        let state = event.state.as_deref().unwrap_or("-");
        let error = event.error_code.as_deref().unwrap_or("-");
        let detail = event.detail.as_deref().unwrap_or("-");
        match event.duration_ms {
            Some(duration_ms) => eprintln!(
                "[diagnostic] session={session} phase={phase} state={state} error={error} duration_ms={duration_ms} detail={detail}"
            ),
            None => eprintln!(
                "[diagnostic] session={session} phase={phase} state={state} error={error} detail={detail}"
            ),
        }
    }
}

impl DiagnosticsControl for ConsoleDiagnosticsSink {
    fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }
}

/// Application-cache diagnostics sink.
///
/// One JSON object is appended per line. The event contract deliberately has
/// no transcript/audio/selection/secret fields; callers may provide only
/// stable phase, state, error and structural detail codes. File failures are
/// best-effort and never alter the input workflow's result.
pub struct FileDiagnosticsSink {
    directory: PathBuf,
    enabled: AtomicBool,
    state: Mutex<FileDiagnosticsState>,
}

#[derive(Default)]
struct FileDiagnosticsState {
    last_pruned_date: Option<String>,
}

#[derive(Serialize)]
struct DiagnosticLogLine {
    timestamp: String,
    session_id: Option<String>,
    phase: Option<String>,
    state: Option<String>,
    duration_ms: Option<u64>,
    error_code: Option<String>,
    detail: Option<String>,
}

impl FileDiagnosticsSink {
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>, enabled: bool) -> Self {
        Self {
            directory: directory.into(),
            enabled: AtomicBool::new(enabled),
            state: Mutex::new(FileDiagnosticsState::default()),
        }
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    fn record_at(&self, event: DiagnosticEvent, now: OffsetDateTime) {
        if !self.enabled() {
            return;
        }

        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if fs::create_dir_all(&self.directory).is_err() {
            return;
        }

        let date = now.date().to_string();
        if state.last_pruned_date.as_deref() != Some(date.as_str()) {
            prune_owned_logs(&self.directory, now);
            state.last_pruned_date = Some(date.clone());
        }

        let line = DiagnosticLogLine {
            timestamp: now
                .format(&Rfc3339)
                .unwrap_or_else(|_| format!("unix-nanos:{}", now.unix_timestamp_nanos())),
            session_id: event.session_id.map(|id| id.as_uuid().to_string()),
            phase: sanitize_optional(event.phase),
            state: sanitize_optional(event.state),
            duration_ms: event.duration_ms,
            error_code: sanitize_optional(event.error_code),
            detail: sanitize_optional(event.detail),
        };
        let Ok(mut bytes) = serde_json::to_vec(&line) else {
            return;
        };
        bytes.push(b'\n');

        let path = self.directory.join(format!("remtene-{date}.log"));
        let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
            return;
        };
        let _ = file.write_all(&bytes);
    }
}

impl DiagnosticsSink for FileDiagnosticsSink {
    fn record(&self, event: DiagnosticEvent) {
        self.record_at(event, current_log_time());
    }
}

impl DiagnosticsControl for FileDiagnosticsSink {
    fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }
}

fn sanitize_optional(value: Option<String>) -> Option<String> {
    value.map(|value| {
        value
            .chars()
            .take(MAX_DIAGNOSTIC_FIELD_CHARS)
            .map(|character| {
                if character.is_control() {
                    '�'
                } else {
                    character
                }
            })
            .collect()
    })
}

fn current_log_time() -> OffsetDateTime {
    let now_utc = OffsetDateTime::now_utc();
    let local_offset = UtcOffset::local_offset_at(now_utc).ok();
    // Keep an honest UTC timestamp if the operating system cannot resolve its
    // local offset; never invent or hard-code a regional timezone.
    log_time_at_offset(now_utc, local_offset)
}

fn log_time_at_offset(now_utc: OffsetDateTime, local_offset: Option<UtcOffset>) -> OffsetDateTime {
    local_offset.map_or(now_utc, |offset| now_utc.to_offset(offset))
}

fn prune_owned_logs(directory: &Path, now: OffsetDateTime) {
    let keep = (0..LOCAL_LOG_RETENTION_DAYS)
        .map(|days_ago| {
            let date = (now - Duration::days(days_ago)).date();
            format!("remtene-{date}.log")
        })
        .collect::<HashSet<_>>();
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };
        if is_owned_log_name(&name) && !keep.contains(&name) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn is_owned_log_name(name: &str) -> bool {
    let Some(date) = name
        .strip_prefix("remtene-")
        .and_then(|value| value.strip_suffix(".log"))
    else {
        return false;
    };
    let bytes = date.as_bytes();
    bytes.len() == 10
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7) && *byte == b'-'
                || !matches!(index, 4 | 7) && byte.is_ascii_digit()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use remtene_domain::SessionId;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_directory(name: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("remtene-diagnostics-{name}-{suffix}"))
    }

    #[test]
    fn sink_can_be_created() {
        let _sink = ConsoleDiagnosticsSink::new();
    }

    #[test]
    fn sink_accepts_diagnostic_events() {
        let sink = ConsoleDiagnosticsSink::new();
        let event = DiagnosticEvent {
            session_id: Some(SessionId::new()),
            phase: Some("test".to_owned()),
            state: Some("ready".to_owned()),
            duration_ms: Some(100),
            error_code: None,
            detail: None,
        };
        sink.record(event);
        // Should not panic
    }

    #[test]
    fn file_sink_writes_timestamped_json_lines_without_creating_desktop_logs() {
        let directory = temp_directory("write");
        let sink = FileDiagnosticsSink::new(&directory, true);
        let local_offset = UtcOffset::from_hms(8, 0, 0).unwrap();
        let now = log_time_at_offset(
            OffsetDateTime::from_unix_timestamp(1_735_711_200).unwrap(),
            Some(local_offset),
        );
        sink.record_at(
            DiagnosticEvent {
                session_id: Some(SessionId::new()),
                phase: Some("asr.transcribe".to_owned()),
                state: Some("completed".to_owned()),
                duration_ms: Some(420),
                error_code: None,
                detail: Some("engine=qwen".to_owned()),
            },
            now,
        );

        let path = directory.join(format!("remtene-{}.log", now.date()));
        let contents = fs::read_to_string(path).unwrap();
        let value: serde_json::Value = serde_json::from_str(contents.trim()).unwrap();
        assert_eq!(value["timestamp"], "2025-01-01T14:00:00+08:00");
        assert_eq!(value["phase"], "asr.transcribe");
        assert_eq!(value["duration_ms"], 420);
        assert!(!contents.contains("final_text"));
    }

    #[test]
    fn local_log_time_preserves_the_instant_and_uses_the_explicit_offset() {
        let now_utc = OffsetDateTime::from_unix_timestamp(1_735_711_200).unwrap();
        let local_offset = UtcOffset::from_hms(8, 0, 0).unwrap();

        let local = log_time_at_offset(now_utc, Some(local_offset));

        assert_eq!(local.unix_timestamp(), now_utc.unix_timestamp());
        assert_eq!(local.offset(), local_offset);
        assert_eq!(local.hour(), 14);
    }

    #[test]
    fn log_time_falls_back_to_utc_when_the_system_offset_is_unavailable() {
        let now_utc = OffsetDateTime::from_unix_timestamp(1_735_711_200).unwrap();

        assert_eq!(log_time_at_offset(now_utc, None), now_utc);
    }

    #[test]
    fn current_log_time_uses_the_operating_system_offset_when_available() {
        let current = current_log_time();

        if let Ok(expected_offset) = UtcOffset::local_offset_at(current) {
            assert_eq!(current.offset(), expected_offset);
        }
    }

    #[test]
    fn file_sink_uses_the_local_calendar_date_when_utc_has_not_crossed_midnight() {
        let directory = temp_directory("local-date");
        let sink = FileDiagnosticsSink::new(&directory, true);
        let local_offset = UtcOffset::from_hms(8, 0, 0).unwrap();
        let now = log_time_at_offset(
            OffsetDateTime::from_unix_timestamp(1_735_754_400).unwrap(),
            Some(local_offset),
        );
        sink.record_at(
            DiagnosticEvent {
                session_id: None,
                phase: Some("startup".to_owned()),
                state: Some("ready".to_owned()),
                duration_ms: None,
                error_code: None,
                detail: None,
            },
            now,
        );

        assert_eq!(now.date().to_string(), "2025-01-02");
        assert!(directory.join("remtene-2025-01-02.log").exists());
        assert!(!directory.join("remtene-2025-01-01.log").exists());
    }

    #[test]
    fn disabled_file_sink_does_not_create_a_log_directory() {
        let directory = temp_directory("disabled");
        let sink = FileDiagnosticsSink::new(&directory, false);
        sink.record(DiagnosticEvent {
            session_id: None,
            phase: Some("startup".to_owned()),
            state: Some("ready".to_owned()),
            duration_ms: None,
            error_code: None,
            detail: None,
        });
        assert!(!directory.exists());
    }

    #[test]
    fn pruning_keeps_three_calendar_days_and_unrelated_files() {
        let directory = temp_directory("prune");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("remtene-2024-12-28.log"), b"old").unwrap();
        fs::write(directory.join("notes.txt"), b"keep").unwrap();
        let sink = FileDiagnosticsSink::new(&directory, true);
        let now = OffsetDateTime::from_unix_timestamp(1_735_689_600).unwrap();
        sink.record_at(
            DiagnosticEvent {
                session_id: None,
                phase: Some("startup".to_owned()),
                state: Some("ready".to_owned()),
                duration_ms: None,
                error_code: None,
                detail: None,
            },
            now,
        );

        assert!(!directory.join("remtene-2024-12-28.log").exists());
        assert!(directory.join("notes.txt").exists());
        assert!(directory.join("remtene-2025-01-01.log").exists());
    }
}
