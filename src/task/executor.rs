//! Task definition and execution.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;

/// A task to be executed in a sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// Unique identifier for this task.
    pub id: String,

    /// Command to execute (first element is the program, rest are arguments).
    pub command: Vec<String>,

    /// Environment variables.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Optional task-level working directory override inside the sandbox.
    ///
    /// When unset, execution inherits the session working directory. Relative
    /// paths are resolved against the session working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<PathBuf>,

    /// Timeout for the task.
    #[serde(with = "duration_millis_serde")]
    pub timeout: Duration,

    /// Additional directories to mount as writable.
    #[serde(default)]
    pub writable_mounts: Vec<MountSpec>,

    /// Additional directories to mount as read-only.
    #[serde(default)]
    pub readonly_mounts: Vec<MountSpec>,

    /// User ID to run the command as (inside the sandbox).
    #[serde(default)]
    pub uid: Option<u32>,

    /// Group ID to run the command as (inside the sandbox).
    #[serde(default)]
    pub gid: Option<u32>,

    /// Whether to capture stdout.
    #[serde(default = "default_true")]
    pub capture_stdout: bool,

    /// Whether to capture stderr.
    #[serde(default = "default_true")]
    pub capture_stderr: bool,

    /// Maximum size of stdout/stderr to capture (in bytes).
    #[serde(default = "default_max_output")]
    pub max_output_size: usize,

    /// Optional stdin data.
    #[serde(default)]
    pub stdin: Option<Vec<u8>>,

    /// Custom metadata.
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

fn default_true() -> bool {
    true
}

fn default_max_output() -> usize {
    10 * 1024 * 1024 // 10 MB
}

/// A mount specification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountSpec {
    /// Source path on the host.
    pub source: PathBuf,
    /// Absolute destination path inside the sandbox.
    pub dest: PathBuf,
}

impl Task {
    fn with_command(command: Vec<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            command,
            env: HashMap::new(),
            working_dir: None,
            timeout: Duration::from_secs(300),
            writable_mounts: Vec::new(),
            readonly_mounts: Vec::new(),
            uid: None,
            gid: None,
            capture_stdout: true,
            capture_stderr: true,
            max_output_size: default_max_output(),
            stdin: None,
            metadata: HashMap::new(),
        }
    }

    /// Create a new task with a command string.
    pub fn new(command: &str) -> Self {
        Self::with_command(
            shell_words::split(command).unwrap_or_else(|_| vec![command.to_string()]),
        )
    }

    /// Create a new task with a command and arguments.
    pub fn with_args<S: AsRef<str>>(program: &str, args: impl IntoIterator<Item = S>) -> Self {
        let mut command = vec![program.to_string()];
        command.extend(args.into_iter().map(|s| s.as_ref().to_string()));
        Self::with_command(command)
    }

    /// Create a builder for constructing tasks.
    pub fn builder() -> TaskBuilder {
        TaskBuilder::default()
    }

    /// Set the task ID.
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    /// Set the timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the working directory.
    pub fn working_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(path.into());
        self
    }

    /// Add an environment variable.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Add multiple environment variables.
    pub fn envs<K, V>(mut self, vars: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        for (k, v) in vars {
            self.env.insert(k.into(), v.into());
        }
        self
    }

    /// Add a writable mount.
    pub fn writable_mount(mut self, source: impl Into<PathBuf>, dest: impl Into<PathBuf>) -> Self {
        self.writable_mounts.push(MountSpec {
            source: source.into(),
            dest: dest.into(),
        });
        self
    }

    /// Add a read-only mount.
    pub fn readonly_mount(mut self, source: impl Into<PathBuf>, dest: impl Into<PathBuf>) -> Self {
        self.readonly_mounts.push(MountSpec {
            source: source.into(),
            dest: dest.into(),
        });
        self
    }

    /// Set stdin data.
    pub fn stdin(mut self, data: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(data.into());
        self
    }

    /// Add metadata.
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }
}

/// Builder for constructing tasks.
#[derive(Debug)]
pub struct TaskBuilder {
    id: Option<String>,
    command: Option<Vec<String>>,
    env: HashMap<String, String>,
    working_dir: Option<PathBuf>,
    timeout: Option<Duration>,
    writable_mounts: Vec<MountSpec>,
    readonly_mounts: Vec<MountSpec>,
    uid: Option<u32>,
    gid: Option<u32>,
    capture_stdout: bool,
    capture_stderr: bool,
    max_output_size: Option<usize>,
    stdin: Option<Vec<u8>>,
    metadata: HashMap<String, String>,
}

impl Default for TaskBuilder {
    fn default() -> Self {
        Self {
            id: None,
            command: None,
            env: HashMap::new(),
            working_dir: None,
            timeout: None,
            writable_mounts: Vec::new(),
            readonly_mounts: Vec::new(),
            uid: None,
            gid: None,
            capture_stdout: true,
            capture_stderr: true,
            max_output_size: None,
            stdin: None,
            metadata: HashMap::new(),
        }
    }
}

impl TaskBuilder {
    /// Set the task ID.
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Set the command.
    pub fn command(mut self, command: impl Into<String>) -> Self {
        let cmd = command.into();
        self.command = Some(shell_words::split(&cmd).unwrap_or_else(|_| vec![cmd]));
        self
    }

    /// Set command with arguments.
    pub fn program(mut self, program: impl Into<String>) -> Self {
        self.command = Some(vec![program.into()]);
        self
    }

    /// Add an argument.
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.command.get_or_insert_with(Vec::new).push(arg.into());
        self
    }

    /// Add multiple arguments.
    pub fn args<S: Into<String>>(mut self, args: impl IntoIterator<Item = S>) -> Self {
        let cmd = self.command.get_or_insert_with(Vec::new);
        cmd.extend(args.into_iter().map(Into::into));
        self
    }

    /// Set an environment variable.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Set the working directory.
    pub fn working_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(path.into());
        self
    }

    /// Set the timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Set the timeout in seconds.
    pub fn timeout_secs(self, secs: u64) -> Self {
        self.timeout(Duration::from_secs(secs))
    }

    /// Add a writable mount.
    pub fn writable_mount(mut self, source: impl Into<PathBuf>, dest: impl Into<PathBuf>) -> Self {
        self.writable_mounts.push(MountSpec {
            source: source.into(),
            dest: dest.into(),
        });
        self
    }

    /// Add a read-only mount.
    pub fn readonly_mount(mut self, source: impl Into<PathBuf>, dest: impl Into<PathBuf>) -> Self {
        self.readonly_mounts.push(MountSpec {
            source: source.into(),
            dest: dest.into(),
        });
        self
    }

    /// Set the user ID.
    pub fn uid(mut self, uid: u32) -> Self {
        self.uid = Some(uid);
        self
    }

    /// Set the group ID.
    pub fn gid(mut self, gid: u32) -> Self {
        self.gid = Some(gid);
        self
    }

    /// Enable or disable stdout capture.
    pub fn capture_stdout(mut self, enabled: bool) -> Self {
        self.capture_stdout = enabled;
        self
    }

    /// Enable or disable stderr capture.
    pub fn capture_stderr(mut self, enabled: bool) -> Self {
        self.capture_stderr = enabled;
        self
    }

    /// Set maximum captured output size in bytes per stream.
    pub fn max_output_size(mut self, size: usize) -> Self {
        self.max_output_size = Some(size);
        self
    }

    /// Set stdin data.
    pub fn stdin(mut self, data: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(data.into());
        self
    }

    /// Add metadata.
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Build the task.
    pub fn build(self) -> anyhow::Result<Task> {
        let command = self
            .command
            .ok_or_else(|| anyhow::anyhow!("command is required"))?;

        Ok(Task {
            id: self.id.unwrap_or_else(|| Uuid::new_v4().to_string()),
            command,
            env: self.env,
            working_dir: self.working_dir,
            timeout: self.timeout.unwrap_or(Duration::from_secs(300)),
            writable_mounts: self.writable_mounts,
            readonly_mounts: self.readonly_mounts,
            uid: self.uid,
            gid: self.gid,
            capture_stdout: self.capture_stdout,
            capture_stderr: self.capture_stderr,
            max_output_size: self.max_output_size.unwrap_or(default_max_output()),
            stdin: self.stdin,
            metadata: self.metadata,
        })
    }
}

/// Result of a task execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    /// ID of the executed task.
    pub task_id: String,

    /// Exit code of the command.
    pub exit_code: i32,

    /// Captured stdout.
    pub stdout: Vec<u8>,

    /// Captured stderr.
    pub stderr: Vec<u8>,

    /// Duration of the execution.
    #[serde(with = "duration_millis_serde")]
    pub duration: Duration,

    /// Whether the task timed out.
    pub timed_out: bool,
}

impl TaskResult {
    /// Check if the task succeeded (exit code 0).
    pub fn success(&self) -> bool {
        self.exit_code == 0 && !self.timed_out
    }

    /// Get stdout as a string.
    pub fn stdout_str(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.stdout)
    }

    /// Get stderr as a string.
    pub fn stderr_str(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.stderr)
    }

    /// Get stdout as a lossy string.
    pub fn stdout_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Get stderr as a lossy string.
    pub fn stderr_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

mod duration_millis_serde {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(duration.as_millis() as u64)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let ms = u64::deserialize(deserializer)?;
        Ok(Duration::from_millis(ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_new() {
        let task = Task::new("echo hello world");
        assert_eq!(task.command, vec!["echo", "hello", "world"]);
        assert_eq!(task.working_dir, None);
    }

    #[test]
    fn test_task_with_args() {
        let task = Task::with_args("ls", ["-la", "/tmp"]);
        assert_eq!(task.command, vec!["ls", "-la", "/tmp"]);
        assert_eq!(task.working_dir, None);
    }

    #[test]
    fn test_task_builder() {
        let task = Task::builder()
            .command("python -c 'print(1)'")
            .timeout_secs(60)
            .working_dir("/home/user")
            .env("PYTHONPATH", "/app")
            .build()
            .unwrap();

        assert_eq!(task.command[0], "python");
        assert_eq!(task.timeout, Duration::from_secs(60));
        assert_eq!(task.working_dir, Some(PathBuf::from("/home/user")));
        assert_eq!(task.env.get("PYTHONPATH"), Some(&"/app".to_string()));
    }

    #[test]
    fn test_task_result() {
        let result = TaskResult {
            task_id: "test".to_string(),
            exit_code: 0,
            stdout: b"hello\n".to_vec(),
            stderr: Vec::new(),
            duration: Duration::from_millis(100),
            timed_out: false,
        };

        assert!(result.success());
        assert_eq!(result.stdout_str().unwrap(), "hello\n");
    }

    #[test]
    fn test_task_result_failure() {
        let result = TaskResult {
            task_id: "test".to_string(),
            exit_code: 1,
            stdout: Vec::new(),
            stderr: b"error\n".to_vec(),
            duration: Duration::from_millis(50),
            timed_out: false,
        };

        assert!(!result.success());
        assert_eq!(result.stderr_str().unwrap(), "error\n");
    }

    #[test]
    fn test_task_result_timeout() {
        let result = TaskResult {
            task_id: "test".to_string(),
            exit_code: -1,
            stdout: Vec::new(),
            stderr: Vec::new(),
            duration: Duration::from_secs(60),
            timed_out: true,
        };

        assert!(!result.success());
        assert!(result.timed_out);
    }
}
