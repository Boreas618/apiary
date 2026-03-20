//! Per-session execution history recording and dump.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::task::{Task, TaskResult};

/// Maximum bytes of stdout/stderr stored per execution record (each stream).
const MAX_OUTPUT_PER_RECORD: usize = 64 * 1024;

/// Snapshot of a single task execution within a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionRecord {
    pub seq: usize,
    pub task_id: String,
    pub command: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    pub exit_code: i32,
    pub timed_out: bool,
    pub duration_ms: u64,
    pub stdout: String,
    pub stderr: String,
    pub timestamp: String,
}

/// Top-level structure written to the dump file on session close.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHistoryDump {
    pub session_id: String,
    pub sandbox_id: String,
    pub working_dir: PathBuf,
    pub created_at: String,
    pub closed_at: String,
    pub executions: Vec<ExecutionRecord>,
}

/// Build an `ExecutionRecord` from a completed task.  `seq` is the
/// caller-assigned sequence number within the session.
pub fn record_execution(seq: usize, task: &Task, result: &TaskResult) -> ExecutionRecord {
    ExecutionRecord {
        seq,
        task_id: result.task_id.clone(),
        command: task.command.clone(),
        working_dir: task.working_dir.clone(),
        env: task.env.clone(),
        exit_code: result.exit_code,
        timed_out: result.timed_out,
        duration_ms: result.duration.as_millis() as u64,
        stdout: truncate_lossy(&result.stdout, MAX_OUTPUT_PER_RECORD),
        stderr: truncate_lossy(&result.stderr, MAX_OUTPUT_PER_RECORD),
        timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    }
}

/// Write the full session history to `{dump_dir}/{session_id}.json`.
///
/// Creates `dump_dir` if it doesn't exist.  Returns the path of the
/// written file on success.
pub fn dump_session_history(
    dump_dir: &Path,
    session_id: &str,
    sandbox_id: &str,
    working_dir: &Path,
    created_at: &str,
    records: Vec<ExecutionRecord>,
) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dump_dir)?;

    let dump = SessionHistoryDump {
        session_id: session_id.to_string(),
        sandbox_id: sandbox_id.to_string(),
        working_dir: working_dir.to_path_buf(),
        created_at: created_at.to_string(),
        closed_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        executions: records,
    };

    let path = dump_dir.join(format!("{session_id}.json"));
    let json = serde_json::to_string_pretty(&dump)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    std::fs::write(&path, json)?;
    Ok(path)
}

fn truncate_lossy(bytes: &[u8], max_bytes: usize) -> String {
    if bytes.len() <= max_bytes {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let truncated = &bytes[..max_bytes];
    let mut s = String::from_utf8_lossy(truncated).into_owned();
    s.push_str("... (truncated)");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn sample_task() -> Task {
        Task::new("echo hello")
            .env("FOO", "bar")
            .working_dir("/workspace")
    }

    fn sample_result() -> TaskResult {
        TaskResult {
            task_id: "task-1".to_string(),
            exit_code: 0,
            stdout: b"hello\n".to_vec(),
            stderr: Vec::new(),
            duration: Duration::from_millis(42),
            timed_out: false,
        }
    }

    #[test]
    fn record_execution_captures_fields() {
        let rec = record_execution(0, &sample_task(), &sample_result());

        assert_eq!(rec.seq, 0);
        assert_eq!(rec.task_id, "task-1");
        assert_eq!(rec.command, vec!["echo", "hello"]);
        assert_eq!(rec.exit_code, 0);
        assert!(!rec.timed_out);
        assert_eq!(rec.duration_ms, 42);
        assert_eq!(rec.stdout, "hello\n");
        assert_eq!(rec.stderr, "");
    }

    #[test]
    fn record_execution_truncates_large_output() {
        let mut result = sample_result();
        result.stdout = vec![b'x'; MAX_OUTPUT_PER_RECORD + 100];
        let rec = record_execution(0, &sample_task(), &result);
        assert!(rec.stdout.len() < result.stdout.len());
        assert!(rec.stdout.contains("truncated"));
    }

    #[test]
    fn dump_session_history_writes_valid_json() {
        let dir = tempfile::tempdir().unwrap();
        let rec = record_execution(0, &sample_task(), &sample_result());

        let path = dump_session_history(
            dir.path(),
            "sess-1",
            "sandbox-0",
            Path::new("/workspace"),
            "2026-03-19T10:00:00.000Z",
            vec![rec],
        )
        .unwrap();

        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        let dump: SessionHistoryDump = serde_json::from_str(&content).unwrap();
        assert_eq!(dump.session_id, "sess-1");
        assert_eq!(dump.sandbox_id, "sandbox-0");
        assert_eq!(dump.executions.len(), 1);
        assert_eq!(dump.executions[0].task_id, "task-1");
    }

    #[test]
    fn dump_creates_directory_if_missing() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b");
        let path = dump_session_history(
            &nested,
            "sess-2",
            "sandbox-1",
            Path::new("/workspace"),
            "2026-03-19T10:00:00.000Z",
            vec![],
        )
        .unwrap();
        assert!(path.exists());
    }

    #[test]
    fn truncate_lossy_no_op_under_limit() {
        assert_eq!(truncate_lossy(b"hi", 10), "hi");
    }

    #[test]
    fn truncate_lossy_truncates_over_limit() {
        let out = truncate_lossy(b"hello world", 5);
        assert!(out.starts_with("hello"));
        assert!(out.contains("truncated"));
    }
}
