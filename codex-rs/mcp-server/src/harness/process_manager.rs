use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;
use std::time::Instant;

use codex_utils_pty::ProcessHandle;
use codex_utils_pty::ProcessSignal;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use crate::harness::types::ProcessSignalParam;

pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024; // 1 MB

#[derive(Default)]
pub struct OutputBuffers {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub total_stdout_bytes: usize,
    pub total_stderr_bytes: usize,
}

pub struct ActiveSession {
    pub session_id: i32,
    pub process: ProcessHandle,
    pub buffers: Arc<StdMutex<OutputBuffers>>,
    pub stdout_offset: usize,
    pub stderr_offset: usize,
    pub exit_rx: Option<oneshot::Receiver<i32>>,
    pub exit_code: Option<i32>,
    pub created_at: Instant,
    pub last_activity_at: Instant,
}

impl ActiveSession {
    pub fn pid(&self) -> Option<u32> {
        self.process.pid()
    }

    pub fn is_alive(&self) -> bool {
        self.exit_code.is_none() && !self.process.has_exited()
    }

    pub fn get_exit_code(&mut self) -> Option<i32> {
        if let Some(code) = self.exit_code {
            return Some(code);
        }
        if let Some(rx) = &mut self.exit_rx
            && let Ok(code) = rx.try_recv()
        {
            self.exit_code = Some(code);
            return Some(code);
        }
        if self.process.has_exited() {
            let code = self.process.exit_code().unwrap_or(-1);
            self.exit_code = Some(code);
            return Some(code);
        }
        None
    }

    pub fn read_delta(&mut self) -> (String, String, String, bool, usize) {
        let buffers = self.buffers.lock().unwrap();

        let new_stdout = if self.stdout_offset < buffers.stdout.len() {
            &buffers.stdout[self.stdout_offset..]
        } else {
            &[]
        };
        let new_stderr = if self.stderr_offset < buffers.stderr.len() {
            &buffers.stderr[self.stderr_offset..]
        } else {
            &[]
        };

        let stdout_str = String::from_utf8_lossy(new_stdout).to_string();
        let stderr_str = String::from_utf8_lossy(new_stderr).to_string();

        let mut combined = String::with_capacity(stdout_str.len() + stderr_str.len());
        combined.push_str(&stdout_str);
        if !stdout_str.is_empty() && !stderr_str.is_empty() && !stdout_str.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&stderr_str);

        self.stdout_offset = buffers.stdout.len();
        self.stderr_offset = buffers.stderr.len();

        let truncated = buffers.stdout_truncated || buffers.stderr_truncated;
        let total = buffers.total_stdout_bytes + buffers.total_stderr_bytes;

        (stdout_str, stderr_str, combined, truncated, total)
    }

    pub async fn write_stdin(&mut self, chars: &[u8], close_stdin: bool) -> std::io::Result<()> {
        let sender = self.process.writer_sender();
        if !chars.is_empty() {
            sender.send(chars.to_vec()).await.map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stdin is closed")
            })?;
        }
        if close_stdin {
            self.process.close_stdin();
        }
        self.last_activity_at = Instant::now();
        Ok(())
    }

    pub fn send_signal(&mut self, signal: ProcessSignalParam) -> std::io::Result<()> {
        self.last_activity_at = Instant::now();
        match signal {
            ProcessSignalParam::Sigint => {
                if let Some(pid) = self.pid() {
                    let _ = codex_utils_pty::process_group::interrupt_process_group_by_pid(pid);
                }
                self.process.signal(ProcessSignal::Interrupt)
            }
            ProcessSignalParam::Sigterm => {
                if let Some(pid) = self.pid() {
                    let _ = codex_utils_pty::process_group::terminate_process_group_by_pid(pid);
                }
                self.process.request_terminate();
                Ok(())
            }
            ProcessSignalParam::Sigkill => {
                if let Some(pid) = self.pid() {
                    let _ = codex_utils_pty::process_group::kill_process_group_by_pid(pid);
                }
                self.process.terminate();
                Ok(())
            }
        }
    }

    pub fn terminate(&mut self) {
        if let Some(pid) = self.pid() {
            let _ = codex_utils_pty::process_group::kill_process_group_by_pid(pid);
        }
        self.process.terminate();
    }
}

impl Drop for ActiveSession {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[derive(Clone, Default)]
pub struct HarnessProcessManager {
    sessions: Arc<StdMutex<HashMap<i32, Arc<Mutex<ActiveSession>>>>>,
    next_session_id: Arc<AtomicI32>,
}

impl HarnessProcessManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(StdMutex::new(HashMap::new())),
            next_session_id: Arc::new(AtomicI32::new(100)),
        }
    }

    pub fn allocate_session_id(&self) -> i32 {
        self.next_session_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn register_session(
        &self,
        session_id: i32,
        process: ProcessHandle,
        buffers: Arc<StdMutex<OutputBuffers>>,
        stdout_offset: usize,
        stderr_offset: usize,
        exit_rx: Option<oneshot::Receiver<i32>>,
    ) {
        let session = ActiveSession {
            session_id,
            process,
            buffers,
            stdout_offset,
            stderr_offset,
            exit_rx,
            exit_code: None,
            created_at: Instant::now(),
            last_activity_at: Instant::now(),
        };
        self.sessions
            .lock()
            .unwrap()
            .insert(session_id, Arc::new(Mutex::new(session)));
    }

    pub fn get_session(&self, session_id: i32) -> Option<Arc<Mutex<ActiveSession>>> {
        let map = self.sessions.lock().unwrap();
        map.get(&session_id).cloned()
    }

    pub fn remove_session(&self, session_id: i32) -> Option<Arc<Mutex<ActiveSession>>> {
        self.sessions.lock().unwrap().remove(&session_id)
    }

    pub async fn terminate_all(&self) {
        let to_terminate: Vec<Arc<Mutex<ActiveSession>>> = {
            let mut sessions = self.sessions.lock().unwrap();
            sessions.drain().map(|(_, arc)| arc).collect()
        };
        for session_arc in to_terminate {
            let mut session = session_arc.lock().await;
            session.terminate();
        }
    }
}

/// Spawns output collector tasks that drain stdout and stderr into OutputBuffers up to max_bytes.
pub fn start_output_collectors(
    stdout_rx: mpsc::Receiver<Vec<u8>>,
    stderr_rx: mpsc::Receiver<Vec<u8>>,
    max_bytes: usize,
) -> Arc<StdMutex<OutputBuffers>> {
    let buffers = Arc::new(StdMutex::new(OutputBuffers::default()));

    let buf_out = Arc::clone(&buffers);
    tokio::spawn(async move {
        collect_stream(stdout_rx, buf_out, max_bytes, /*is_stdout*/ true).await;
    });

    let buf_err = Arc::clone(&buffers);
    tokio::spawn(async move {
        collect_stream(stderr_rx, buf_err, max_bytes, /*is_stdout*/ false).await;
    });

    buffers
}

async fn collect_stream(
    mut rx: mpsc::Receiver<Vec<u8>>,
    buffers: Arc<StdMutex<OutputBuffers>>,
    max_bytes: usize,
    is_stdout: bool,
) {
    while let Some(chunk) = rx.recv().await {
        let mut b = buffers.lock().unwrap();
        if is_stdout {
            b.total_stdout_bytes += chunk.len();
            if b.stdout.len() < max_bytes {
                let remaining = max_bytes - b.stdout.len();
                if chunk.len() <= remaining {
                    b.stdout.extend_from_slice(&chunk);
                } else {
                    b.stdout.extend_from_slice(&chunk[..remaining]);
                    b.stdout_truncated = true;
                }
            } else {
                b.stdout_truncated = true;
            }
        } else {
            b.total_stderr_bytes += chunk.len();
            if b.stderr.len() < max_bytes {
                let remaining = max_bytes - b.stderr.len();
                if chunk.len() <= remaining {
                    b.stderr.extend_from_slice(&chunk);
                } else {
                    b.stderr.extend_from_slice(&chunk[..remaining]);
                    b.stderr_truncated = true;
                }
            } else {
                b.stderr_truncated = true;
            }
        }
    }
}
