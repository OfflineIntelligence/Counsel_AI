//! Inference-server process lifecycle utilities.
//!
//! Every runtime adapter (GGUF/ONNX/Safetensors/TensorRT) spawns its inference
//! server through these helpers so the child process is managed the same way
//! Ollama and LM Studio manage theirs:
//!
//!   1. `configure_server_command` — pipes stdout/stderr and (Windows) sets
//!      CREATE_NO_WINDOW so no console window flashes in the GUI app.
//!   2. `attach_managed_child` — MUST be called right after spawn:
//!      - Drains stdout/stderr into `tracing` on dedicated reader threads.
//!        Without a reader, the OS pipe buffer fills (llama-server logs every
//!        request to stderr) and the server blocks mid-write, hanging inference.
//!        The threads exit on their own when the pipes close (child death).
//!      - (Windows) assigns the child to a process-wide Job Object created with
//!        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE. The job handle is held for the
//!        life of this process and closed by the OS at process exit — even a
//!        Task-Manager force-kill of the app therefore kills the inference
//!        server. No orphan can outlive the app.
//!   3. `kill_orphaned_llama_servers` — startup sweep that kills any
//!      llama-server still running FROM OUR OWN engines directory (left behind
//!      by a pre-Job-Object build or a failed job assignment). The exe-path
//!      check guarantees we never touch a user's own llama.cpp processes.

use std::process::{Child, Command, Stdio};
use tracing::{debug, info, warn};

/// Apply the standard spawn configuration for an inference-server child:
/// piped stdout/stderr (drained later by `attach_managed_child`) and, on
/// Windows, CREATE_NO_WINDOW so no console flashes in the release GUI app.
pub fn configure_server_command(cmd: &mut Command) {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
}

/// Post-spawn management: drain output pipes and tie the child's lifetime to
/// this process. Call immediately after `Command::spawn()`.
///
/// `label` names the child in log lines (e.g. "llama-server").
pub fn attach_managed_child(child: &mut Child, label: &'static str) {
    // ── Windows: child dies with this process, no matter how we exit ──
    #[cfg(target_os = "windows")]
    {
        if win_job::assign_to_kill_on_close_job(child) {
            debug!("{} (pid {}) assigned to kill-on-close job object", label, child.id());
        } else {
            // Loud, not silent: the app still kills the child on graceful
            // shutdown, but a force-killed app could orphan it. The startup
            // sweep (kill_orphaned_llama_servers) is the recovery for that.
            warn!(
                "{} (pid {}) could NOT be assigned to the kill-on-close job object — \
                 it may survive if this application is force-killed",
                label,
                child.id()
            );
        }
    }

    // ── Drain stdout/stderr so the child can never block on a full pipe ──
    // stderr is the inference server's log stream (model load progress, device
    // selection, per-request timings like "prompt eval time" and tokens/sec) —
    // forwarded at info level so performance is diagnosable from the app log.
    if let Some(stdout) = child.stdout.take() {
        spawn_pipe_reader(stdout, label, "stdout", false);
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_pipe_reader(stderr, label, "stderr", true);
    }
}

/// Read a child's pipe line-by-line and forward into `tracing` until EOF.
/// A dedicated OS thread (not a tokio task) so draining continues even if the
/// async runtime is saturated. Exits when the pipe closes.
fn spawn_pipe_reader<R: std::io::Read + Send + 'static>(
    pipe: R,
    label: &'static str,
    stream: &'static str,
    log_at_info: bool,
) {
    let spawned = std::thread::Builder::new()
        .name(format!("{}-{}", label, stream))
        .spawn(move || {
            use std::io::{BufRead, BufReader};
            for line in BufReader::new(pipe).lines() {
                match line {
                    Ok(line) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        if log_at_info {
                            info!(target: "inference_server", "[{}] {}", label, line);
                        } else {
                            debug!(target: "inference_server", "[{} {}] {}", label, stream, line);
                        }
                    }
                    Err(_) => break,
                }
            }
        });

    if let Err(e) = spawned {
        // Failing to log is acceptable; failing to DRAIN is not — this is the
        // exact hang the reader exists to prevent, so say so loudly.
        warn!(
            "Could not start {} {} reader thread: {} — the server may stall once \
             its pipe buffer fills",
            label, stream, e
        );
    }
}

/// Kill llama-server processes left over from a previous session.
///
/// Only processes whose executable lives inside this app's own engines
/// directory (%LOCALAPPDATA%\Offline Counsel AI\engines on Windows) are
/// touched — a user's independently-running llama.cpp is never affected.
///
/// Call ONCE at backend startup, BEFORE any engine verification or runtime
/// spawn, so it can never race against our own healthy children.
pub fn kill_orphaned_llama_servers() {
    let engines_dir = crate::config::get_app_data_dir().join("engines");
    if !engines_dir.exists() {
        return;
    }

    let mut sys = sysinfo::System::new();
    sys.refresh_processes();

    let mut killed = 0u32;
    for (pid, process) in sys.processes() {
        let name = process.name().to_ascii_lowercase();
        if name != "llama-server" && name != "llama-server.exe" {
            continue;
        }
        let Some(exe) = process.exe() else { continue };
        if !exe.starts_with(&engines_dir) {
            continue;
        }

        if process.kill() {
            warn!(
                "Killed orphaned llama-server from a previous session (pid {}, {})",
                pid,
                exe.display()
            );
            killed += 1;
        } else {
            warn!(
                "Found orphaned llama-server (pid {}, {}) but could not kill it — \
                 port {} may be unavailable until it exits",
                pid,
                exe.display(),
                9639
            );
        }
    }

    if killed > 0 {
        info!("Orphan sweep complete: {} stale llama-server process(es) terminated", killed);
    } else {
        debug!("Orphan sweep complete: no stale llama-server processes found");
    }
}

/// Windows Job Object with JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE.
///
/// Raw kernel32 FFI (same pattern as the MessageBoxW call in the desktop
/// shell) — no extra crate. The job handle is created once, stored in a
/// OnceLock, and deliberately never closed: the OS closes it when this
/// process exits (normally OR force-killed), which terminates every process
/// assigned to the job.
#[cfg(target_os = "windows")]
mod win_job {
    use std::ffi::c_void;
    use std::process::Child;
    use std::ptr;
    use std::sync::OnceLock;
    use tracing::warn;

    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
    /// JOBOBJECTINFOCLASS::JobObjectExtendedLimitInformation
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS: u32 = 9;

    #[repr(C)]
    #[derive(Default)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct JobObjectBasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct JobObjectExtendedLimitInformation {
        basic_limit_information: JobObjectBasicLimitInformation,
        io_info: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> *mut c_void;
        fn SetInformationJobObject(
            job: *mut c_void,
            class: u32,
            info: *mut c_void,
            info_len: u32,
        ) -> i32;
        fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn GetLastError() -> u32;
    }

    /// Raw HANDLE wrapper so it can live in a OnceLock (HANDLEs are just
    /// kernel object references; using one from multiple threads is fine).
    struct JobHandle(*mut c_void);
    unsafe impl Send for JobHandle {}
    unsafe impl Sync for JobHandle {}

    static KILL_ON_CLOSE_JOB: OnceLock<Option<JobHandle>> = OnceLock::new();

    fn job_handle() -> Option<*mut c_void> {
        KILL_ON_CLOSE_JOB
            .get_or_init(|| unsafe {
                let job = CreateJobObjectW(ptr::null_mut(), ptr::null());
                if job.is_null() {
                    warn!("CreateJobObjectW failed (error {}) — child processes will not be \
                           auto-killed on force-exit", GetLastError());
                    return None;
                }

                let mut info = JobObjectExtendedLimitInformation::default();
                info.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

                let ok = SetInformationJobObject(
                    job,
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
                    &mut info as *mut _ as *mut c_void,
                    std::mem::size_of::<JobObjectExtendedLimitInformation>() as u32,
                );
                if ok == 0 {
                    warn!("SetInformationJobObject failed (error {}) — child processes will \
                           not be auto-killed on force-exit", GetLastError());
                    CloseHandle(job);
                    return None;
                }

                Some(JobHandle(job))
            })
            .as_ref()
            .map(|h| h.0)
    }

    /// Assign a freshly spawned child to the kill-on-close job.
    /// Returns false (with a logged reason) when the job could not be created
    /// or the assignment failed — never panics, never blocks.
    pub fn assign_to_kill_on_close_job(child: &Child) -> bool {
        use std::os::windows::io::AsRawHandle;

        let Some(job) = job_handle() else {
            return false;
        };

        let ok = unsafe { AssignProcessToJobObject(job, child.as_raw_handle() as *mut c_void) };
        if ok == 0 {
            warn!(
                "AssignProcessToJobObject failed for pid {} (error {})",
                child.id(),
                unsafe { GetLastError() }
            );
            return false;
        }
        true
    }
}
