//! Interactive Tokio PTY session engine (`exec_command`, `write_stdin`, resize, terminate,
//! foreground-to-background promotion) with sanitized shell environments and retained raw output.
//! Payloads run through the platform shell: `/bin/bash`/`sh` on Unix, PowerShell on Windows.

use std::collections::BTreeMap;
use std::io::{Read, Write};
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use tekvra_config::AgentSettings;
use tekvra_protocol::ToolOutput;
use tekvra_storage::StateIndex;
use tokio::sync::{Notify, mpsc};
use uuid::Uuid;

use crate::ToolsError;
use crate::output_filter::filter_command_output;

const SAFE_HOST_ENV_VARS: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "TEMP",
    "TMP",
    // Windows: children spawned through ConPTY fail without these (SystemRoot is
    // required by Winsock/crypto init; the rest keep shells and toolchains happy).
    "SystemRoot",
    "SystemDrive",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
];

/// Notification emitted when a backgrounded PTY command finishes so the session actor can wake at a safe turn boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyCompletionNotice {
    /// Completed PTY `session_id`.
    pub session_id: String,
    /// Original command line.
    pub command: String,
    /// Process exit code.
    pub exit_code: i32,
}

/// Parameters for launching a shell command inside a PTY (`exec_command`).
#[derive(Debug, Clone)]
pub struct PtyCommandRequest {
    /// Owning conversation/session id.
    pub conversation_id: String,
    /// Shell command string.
    pub command: String,
    /// Working directory.
    pub cwd: PathBuf,
    /// Initial wait window in milliseconds before returning a running `session_id` receipt.
    pub yield_time_ms: u64,
    /// Optional explicit background mode (`true` promotes immediately after spawn).
    pub background: bool,
    /// Terminal rows.
    pub rows: u16,
    /// Terminal columns.
    pub cols: u16,
}

/// Builds a sanitized shell environment map (`PATH`, `HOME`, `USER`, `LANG`, `TERM`, `TMPDIR`
/// plus explicit `AgentSettings` passthroughs and overrides, never auto-loading `.env` secrets).
#[must_use]
pub fn build_sanitized_shell_env(settings: &AgentSettings) -> BTreeMap<String, String> {
    let mut env_map = BTreeMap::new();
    for &key in SAFE_HOST_ENV_VARS {
        if let Ok(val) = std::env::var(key) {
            env_map.insert(key.to_string(), val);
        }
    }
    env_map.insert("TERM".to_string(), "xterm-256color".to_string());
    for pass_key in &settings.shell_env_passthrough {
        if let Ok(val) = std::env::var(pass_key) {
            env_map.insert(pass_key.clone(), val);
        }
    }
    for (k, v) in &settings.shell_env_set {
        env_map.insert(k.clone(), v.clone());
    }
    env_map
}

/// Resolves the shell used to run `exec_command` payloads: `/bin/bash` (or `sh`) on Unix,
/// PowerShell on Windows — `pwsh.exe` (PowerShell 7) when found on `PATH`, else the
/// always-present `powershell.exe`. Returns the program plus the arguments that make it
/// run `command` as a single script string.
fn shell_invocation(command: &str) -> (String, Vec<String>) {
    #[cfg(unix)]
    {
        let shell = if Path::new("/bin/bash").is_file() {
            "/bin/bash"
        } else {
            "sh"
        };
        (
            shell.to_string(),
            vec!["-c".to_string(), command.to_string()],
        )
    }
    #[cfg(windows)]
    {
        let program = if find_on_path("pwsh.exe").is_some() {
            "pwsh.exe"
        } else {
            "powershell.exe"
        };
        (
            program.to_string(),
            vec![
                "-NoLogo".to_string(),
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                command.to_string(),
            ],
        )
    }
}

#[cfg(windows)]
fn find_on_path(file_name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path_var| {
        std::env::split_paths(&path_var)
            .map(|dir| dir.join(file_name))
            .find(|candidate| candidate.is_file())
    })
}

struct PtySharedBuffer {
    full_raw: String,
    unread_cursor: usize,
    exit_code: Option<i32>,
    reader_eof: bool,
    promoted_to_background: bool,
}

struct ActivePtySession {
    command: String,
    conversation_id: String,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>,
    shared: Arc<Mutex<PtySharedBuffer>>,
    notify: Arc<Notify>,
}

/// Manages live interactive and background PTY command sessions (`exec_command` / `write_stdin`).
#[derive(Clone, Default)]
pub struct PtySessionManager {
    sessions: Arc<Mutex<BTreeMap<String, Arc<ActivePtySession>>>>,
    completion_tx: Option<mpsc::UnboundedSender<PtyCompletionNotice>>,
}

impl std::fmt::Debug for PtySessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        f.debug_struct("PtySessionManager")
            .field("active_sessions", &count)
            .finish()
    }
}

impl PtySessionManager {
    /// Creates a new [`PtySessionManager`] with an optional channel for background completion notifications.
    #[must_use]
    pub fn new(completion_tx: Option<mpsc::UnboundedSender<PtyCompletionNotice>>) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            completion_tx,
        }
    }

    /// Spawns `req.command` in the platform shell (POSIX `/bin/bash -c`, Windows PowerShell
    /// `-Command`) inside a real pseudo-terminal with a sanitized environment.
    /// Returns completed output directly if it exits within the yield/foreground window, or returns
    /// an active `session_id` receipt so `write_stdin`, `resize`, or background completion can continue.
    ///
    /// # Errors
    /// Returns [`ToolsError::Pty`] if PTY allocation or shell spawn fails.
    pub async fn exec_command(
        &self,
        req: PtyCommandRequest,
        settings: &AgentSettings,
        state_index: Option<&Arc<Mutex<StateIndex>>>,
    ) -> Result<ToolOutput, ToolsError> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: req.rows.max(24),
                cols: req.cols.max(80),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| ToolsError::Pty {
                message: format!("failed to allocate PTY: {e}"),
            })?;

        let (shell_bin, shell_args) = shell_invocation(&req.command);
        let mut cmd = CommandBuilder::new(shell_bin);
        for arg in shell_args {
            cmd.arg(arg);
        }
        cmd.cwd(&req.cwd);
        cmd.env_clear();
        for (k, v) in build_sanitized_shell_env(settings) {
            cmd.env(k, v);
        }

        let mut child = pair.slave.spawn_command(cmd).map_err(|e| ToolsError::Pty {
            message: format!("failed to spawn command in PTY: {e}"),
        })?;
        // Drop slave end in parent immediately so master receives EOF once child exits!
        drop(pair.slave);

        let killer = child.clone_killer();
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| ToolsError::Pty {
                message: format!("failed to clone PTY reader: {e}"),
            })?;
        let writer = pair.master.take_writer().map_err(|e| ToolsError::Pty {
            message: format!("failed to take PTY stdin writer: {e}"),
        })?;

        let session_id = format!("pty_{}", Uuid::now_v7().simple());
        let shared = Arc::new(Mutex::new(PtySharedBuffer {
            full_raw: String::new(),
            unread_cursor: 0,
            exit_code: None,
            reader_eof: false,
            promoted_to_background: req.background,
        }));
        let notify = Arc::new(Notify::new());

        let shared_reader = Arc::clone(&shared);
        let notify_reader = Arc::clone(&notify);
        tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let mut guard = shared_reader
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        guard.reader_eof = true;
                        drop(guard);
                        notify_reader.notify_waiters();
                        break;
                    }
                    Ok(n) => {
                        let chunk = String::from_utf8_lossy(&buf[..n]);
                        let mut guard = shared_reader
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        guard.full_raw.push_str(&chunk);
                        drop(guard);
                        notify_reader.notify_waiters();
                    }
                }
            }
        });

        let shared_waiter = Arc::clone(&shared);
        let notify_waiter = Arc::clone(&notify);
        let completion_tx = self.completion_tx.clone();
        let sid_waiter = session_id.clone();
        let cmd_waiter = req.command.clone();
        tokio::task::spawn_blocking(move || {
            let code = match child.wait() {
                Ok(status) => status.exit_code() as i32,
                Err(_) => -1,
            };
            let notify_parent = {
                let mut guard = shared_waiter
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.exit_code = Some(code);
                guard.promoted_to_background
            };
            notify_waiter.notify_waiters();
            if notify_parent && let Some(tx) = completion_tx {
                let _ = tx.send(PtyCompletionNotice {
                    session_id: sid_waiter,
                    command: cmd_waiter,
                    exit_code: code,
                });
            }
        });

        let active = Arc::new(ActivePtySession {
            command: req.command.clone(),
            conversation_id: req.conversation_id.clone(),
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            killer: Mutex::new(killer),
            shared: Arc::clone(&shared),
            notify: Arc::clone(&notify),
        });
        {
            let mut map = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.insert(session_id.clone(), Arc::clone(&active));
        }

        if req.background {
            // Brief 20ms settle to capture immediate startup banner without restarting!
            tokio::time::sleep(Duration::from_millis(20)).await;
        } else {
            let fg_cap_ms = settings.shell_foreground_limit_secs.saturating_mul(1_000);
            let wait_ms = req.yield_time_ms.min(fg_cap_ms.max(50));
            wait_for_exit_or_timeout(&shared, &notify, Duration::from_millis(wait_ms)).await;
            let mut guard = shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.exit_code.is_none() {
                guard.promoted_to_background = true;
            }
        }

        let idx_guard = state_index.map(|arc| {
            arc.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        self.collect_session_output(&session_id, &active, idx_guard.as_deref())
    }

    /// Writes `chars` to an active PTY's stdin (`write_stdin`), waits up to `yield_time_ms` for
    /// newly emitted output or exit, and returns a [`ToolOutput::Terminal`].
    ///
    /// # Errors
    /// Returns [`ToolsError`] if `session_id` is unknown or writing stdin fails.
    pub async fn write_stdin(
        &self,
        session_id: &str,
        chars: &str,
        yield_time_ms: u64,
        state_index: Option<&Arc<Mutex<StateIndex>>>,
    ) -> Result<ToolOutput, ToolsError> {
        let active = {
            let map = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.get(session_id).cloned()
        }
        .ok_or_else(|| ToolsError::Pty {
            message: format!("unknown or already closed PTY session `{session_id}`"),
        })?;

        let pre_len = {
            let guard = active
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.full_raw.len()
        };

        if !chars.is_empty() {
            let mut writer = active
                .writer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            writer
                .write_all(chars.as_bytes())
                .and_then(|()| writer.flush())
                .map_err(|e| ToolsError::Pty {
                    message: format!("failed writing stdin to `{session_id}`: {e}"),
                })?;
        }

        wait_for_new_output_or_exit(
            &active.shared,
            &active.notify,
            pre_len,
            Duration::from_millis(yield_time_ms.max(25)),
        )
        .await;

        let idx_guard = state_index.map(|arc| {
            arc.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        self.collect_session_output(session_id, &active, idx_guard.as_deref())
    }

    /// Resizes an active PTY session (`rows` x `cols`).
    ///
    /// # Errors
    /// Returns [`ToolsError::Pty`] if `session_id` is unknown or resize fails.
    pub fn resize(&self, session_id: &str, rows: u16, cols: u16) -> Result<(), ToolsError> {
        let active = {
            let map = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.get(session_id).cloned()
        }
        .ok_or_else(|| ToolsError::Pty {
            message: format!("unknown PTY session `{session_id}`"),
        })?;

        let master = active
            .master
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        master
            .resize(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| ToolsError::Pty {
                message: format!("PTY resize failed: {e}"),
            })
    }

    /// Terminates an active PTY session and removes it from the active map.
    ///
    /// # Errors
    /// Returns [`ToolsError::Pty`] if `session_id` is unknown or kill fails.
    pub fn terminate(&self, session_id: &str) -> Result<(), ToolsError> {
        let removed = {
            let mut map = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.remove(session_id)
        }
        .ok_or_else(|| ToolsError::Pty {
            message: format!("unknown PTY session `{session_id}`"),
        })?;

        let mut killer = removed
            .killer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        killer.kill().map_err(|e| ToolsError::Pty {
            message: format!("failed killing PTY session `{session_id}`: {e}"),
        })
    }

    fn collect_session_output(
        &self,
        session_id: &str,
        active: &Arc<ActivePtySession>,
        state_index: Option<&StateIndex>,
    ) -> Result<ToolOutput, ToolsError> {
        let (delta_raw, full_raw, exit_code) = {
            let mut guard = active
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let slice = guard.full_raw[guard.unread_cursor..].to_string();
            guard.unread_cursor = guard.full_raw.len();
            (slice, guard.full_raw.clone(), guard.exit_code)
        };

        let is_finished = exit_code.is_some();
        if is_finished {
            let mut map = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.remove(session_id);
        }

        let mut output_id = None;
        if is_finished
            && let Some(idx) = state_index
            && (!full_raw.is_empty())
        {
            output_id = Some(idx.store_retained_output(
                &active.conversation_id,
                &active.command,
                &full_raw,
                0,
            )?);
        }

        let filtered =
            filter_command_output(&active.command, &delta_raw, exit_code, output_id.as_deref());

        Ok(ToolOutput::Terminal {
            command: active.command.clone(),
            session_id: (!is_finished).then(|| session_id.to_string()),
            exit_code,
            filtered_output: filtered.text,
            raw_output: if is_finished { full_raw } else { delta_raw },
            output_id,
            was_filtered: filtered.was_filtered,
        })
    }
}

async fn wait_for_exit_or_timeout(
    shared: &Arc<Mutex<PtySharedBuffer>>,
    notify: &Arc<Notify>,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    loop {
        let notified = notify.notified();
        {
            let guard = shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.exit_code.is_some() && guard.reader_eof {
                return;
            }
        }
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        if tokio::time::timeout(deadline - now, notified)
            .await
            .is_err()
        {
            return;
        }
    }
}

async fn wait_for_new_output_or_exit(
    shared: &Arc<Mutex<PtySharedBuffer>>,
    notify: &Arc<Notify>,
    prev_len: usize,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    let mut saw_output = false;
    loop {
        let notified = notify.notified();
        {
            let guard = shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.exit_code.is_some() && guard.reader_eof {
                return;
            }
            if guard.full_raw.len() > prev_len {
                saw_output = true;
            }
        }
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let wait_slice = if saw_output {
            // Once new output arrives, allow a brief 35ms coalescing window for the rest of the line/exit.
            Duration::from_millis(35).min(deadline - now)
        } else {
            deadline - now
        };
        if tokio::time::timeout(wait_slice, notified).await.is_err() {
            return;
        }
    }
}
