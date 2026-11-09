// apps/desktop/src-tauri/src/main.rs

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use offline_intelligence::{Config, run_thread_server};
use std::sync::Mutex;
use std::time::Duration;

mod instance;

/// Holds the dynamically selected backend port so the frontend can query it
struct BackendPort(Mutex<u16>);

#[tauri::command]
fn get_backend_port(state: tauri::State<'_, BackendPort>) -> u16 {
    *state.0.lock().unwrap()
}

/// Get the user data directory for the application.
/// All platforms land in the local (non-roaming) data dir under "Offline Counsel AI".
/// Windows : AppData\Local\Offline Counsel AI
/// macOS   : ~/Library/Application Support/Offline Counsel AI
/// Linux   : ~/.local/share/Offline Counsel AI
fn get_data_dir() -> Option<std::path::PathBuf> {
    dirs::data_local_dir().map(|d| d.join("Offline Counsel AI"))
}

/// Initialize user data directories for models and settings
/// These persist across app updates
fn initialize_user_directories() {
    if let Some(base_dir) = get_data_dir() {
        let dirs_to_create = [
            base_dir.join("data"),        // Database and persistent storage
            base_dir.join("models"),      // Downloaded GGUF models
            base_dir.join("registry"),    // Model metadata
            base_dir.join("downloads"),   // Temporary download cache
            base_dir.join("logs"),        // Startup and crash logs
        ];

        for dir in &dirs_to_create {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("Failed to create directory {}: {}", dir.display(), e);
            }
        }

        eprintln!("User data directory: {}", base_dir.display());
    } else {
        eprintln!("Could not determine user data directory");
    }
}

/// Write a startup log entry for crash diagnostics
fn log_startup(message: &str) {
    eprintln!("{}", message);
    if let Some(base_dir) = get_data_dir() {
        let log_path = base_dir.join("logs").join("startup.log");
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let entry = format!("[{}] {}\n", timestamp, message);
        // Append to log file, ignore errors (best-effort logging)
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, entry.as_bytes()));
    }
}

/// Show a native error dialog on fatal startup failures
fn fatal_error(message: &str) -> ! {
    let full_message = format!(
        "Offline Counsel AI failed to start:\n\n{}\n\nCheck logs at:\n{}",
        message,
        get_data_dir()
            .map(|d| d.join("logs").join("startup.log").display().to_string())
            .unwrap_or_else(|| "(unknown)".to_string())
    );
    log_startup(&format!("FATAL: {}", message));

    // Clean up BEFORE the dialog: it stays open until the user clicks OK, and
    // the backend thread and any engine it started would keep running (and
    // keep their ports) the whole time.
    let killed = offline_intelligence::model_runtime::process_util::kill_all_managed_children();
    if killed > 0 {
        log_startup(&format!("Ended {} engine process(es) after startup failure", killed));
    }
    release_lock();

    #[cfg(target_os = "windows")]
    {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        let text: Vec<u16> = OsStr::new(&full_message).encode_wide().chain(Some(0)).collect();
        let caption: Vec<u16> = OsStr::new("Offline Counsel AI - Startup Error").encode_wide().chain(Some(0)).collect();
        unsafe {
            #[link(name = "user32")]
            extern "system" {
                fn MessageBoxW(hwnd: *mut std::ffi::c_void, text: *const u16, caption: *const u16, utype: u32) -> i32;
            }
            MessageBoxW(std::ptr::null_mut(), text.as_ptr(), caption.as_ptr(), 0x10 /* MB_ICONERROR */);
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        // On non-Windows, stderr is the best we have before Tauri starts
        eprintln!("{}", full_message);
    }

    std::process::exit(1);
}

/// Mark this instance as closing in app.lock, so a launch during shutdown
/// waits for it to finish instead of treating it as running.
fn mark_lock_closing() {
    if let Some(base_dir) = get_data_dir() {
        let _ = std::fs::write(base_dir.join("app.lock"), format!("{} closing", std::process::id()));
    }
}

fn release_lock() {
    if let Some(base_dir) = get_data_dir() {
        let _ = std::fs::remove_file(base_dir.join("app.lock"));
    }
}

fn main() {
    log_startup("=== Offline Counsel AI Starting ===");
    log_startup(&format!("Current directory: {:?}", std::env::current_dir().unwrap_or_default()));

    #[cfg(debug_assertions)]
    log_startup("Build mode: DEBUG");
    #[cfg(not(debug_assertions))]
    log_startup("Build mode: RELEASE");

    // Single-instance lock. app.lock holds "<pid>" while an instance runs and
    // "<pid> closing" once it has started shutting down (see ExitRequested).
    // A previous instance must never cost the user an error dialog:
    //  - still running -> bring its window to the front and quietly exit;
    //  - still closing -> wait for it to finish (it holds the ports), then go.
    if let Some(base_dir) = get_data_dir() {
        let lock_file = base_dir.join("app.lock");

        if let Ok(contents) = std::fs::read_to_string(&lock_file) {
            let mut parts = contents.split_whitespace();
            let existing_pid = parts.next().and_then(|p| p.parse::<u32>().ok());
            let closing = parts.next() == Some("closing");
            match existing_pid.filter(|&pid| pid != std::process::id() && instance::is_running(pid)) {
                Some(pid) if closing => {
                    log_startup(&format!("Previous instance (PID {}) is still closing; waiting for it", pid));
                    if instance::wait_or_end(pid, Duration::from_secs(4)) {
                        log_startup("Previous instance has exited");
                    } else {
                        log_startup("Previous instance could not be ended; continuing");
                    }
                }
                Some(pid) => {
                    // A real running instance (or one still starting, with no
                    // window yet): never start a second backend next to it.
                    if instance::focus_window(pid) {
                        log_startup(&format!("Already running (PID {}): brought it to the front", pid));
                    } else {
                        log_startup(&format!("Already running (PID {}), still starting: leaving it to open", pid));
                    }
                    std::process::exit(0);
                }
                None if existing_pid.is_some() => {
                    log_startup("Removing stale lock file (previous instance not running)");
                }
                None => {}
            }
        }

        // Create new lock file with current PID
        match std::fs::write(&lock_file, format!("{}", std::process::id())) {
            Ok(_) => {
                log_startup(&format!("Acquired instance lock (PID: {})", std::process::id()));
            }
            Err(e) => {
                log_startup(&format!("Warning: Could not create lock file: {}", e));
                log_startup("Continuing without instance lock (multiple instances may conflict)");
            }
        }
    }

    // Initialize user data directories
    initialize_user_directories();

    // Channels for backend coordination
    let (started_tx, started_rx) = std::sync::mpsc::channel::<bool>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<bool>();
    let (port_tx, port_rx) = std::sync::mpsc::channel::<u16>();
    let (actual_port_tx, actual_port_rx) = std::sync::mpsc::channel::<u16>();

    std::thread::spawn(move || {
        // Load .env first so the runtime itself can be sized from it:
        // BACKEND_WORKER_THREADS (default: one per core) and
        // BACKEND_MAX_BLOCKING_THREADS (default 512; caps the pool used for
        // extraction, hashing and file I/O).
        let config_result = Config::from_env();
        // Tokio reads TOKIO_WORKER_THREADS itself and PANICS on anything but a
        // positive integer (an older .env set it to "auto"). Drop an invalid
        // value so it can never take the backend down at startup.
        if let Ok(v) = std::env::var("TOKIO_WORKER_THREADS") {
            if v.trim().parse::<usize>().map_or(true, |n| n == 0) {
                log_startup(&format!("Ignoring invalid TOKIO_WORKER_THREADS={:?}", v));
                std::env::remove_var("TOKIO_WORKER_THREADS");
            }
        }
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.enable_all();
        if let Some(n) = offline_intelligence::tuning::parse::<usize>("BACKEND_WORKER_THREADS") {
            builder.worker_threads(n.max(1));
        }
        if let Some(n) = offline_intelligence::tuning::parse::<usize>("BACKEND_MAX_BLOCKING_THREADS") {
            builder.max_blocking_threads(n.max(1));
        }
        let rt = match builder.build() {
            Ok(rt) => rt,
            Err(e) => {
                log_startup(&format!("Failed to create the async runtime: {}", e));
                let _ = started_tx.send(false);
                let _ = ready_tx.send(false);
                return;
            }
        };
        rt.block_on(async {
            let mut config = match config_result {
                Ok(cfg) => cfg,
                Err(e) => {
                    log_startup(&format!("Failed to load config: {}", e));
                    let _ = started_tx.send(false);
                    let _ = ready_tx.send(false);
                    return;
                }
            };

            // Use the host and port from config (loaded from environment)
            // Override only the values that are specifically needed for the app
            config.max_concurrent_streams = 4;
            config.generate_timeout_seconds = 300;

            eprintln!("Starting backend service on {}:{}...", config.api_host, config.api_port);

            // Signal that backend thread is running (config loaded successfully).
            // Engine scanning / runtime loading happens inside run_thread_server and
            // must NOT block here — the main thread is waiting on port_rx with a timeout.
            let _ = started_tx.send(true);

            // Start the server
            // port_tx is moved, not cloned: if the server fails before binding,
            // the channel closes and the wait below ends at once instead of
            // hanging until the main thread's timeout.
            let server_handle = tokio::spawn(run_thread_server(config, Some(port_tx)));

            let actual_port = match tokio::task::spawn_blocking(move || port_rx.recv()).await {
                Ok(Ok(port)) => {
                    log_startup(&format!("Backend bound to port {}", port));
                    port
                }
                _ => {
                    // The server ended without binding: put its error in
                    // startup.log (stderr is invisible in a release build).
                    let reason = match server_handle.await {
                        Ok(Err(e)) => format!("{:#}", e),
                        Err(e) => format!("server task panicked: {}", e),
                        Ok(Ok(())) => "server exited before binding".to_string(),
                    };
                    log_startup(&format!("Backend failed before binding its port: {}", reason));
                    // Dropping actual_port_tx tells the main thread immediately.
                    return;
                }
            };

            // Send actual port back to main thread for Tauri state
            let _ = actual_port_tx.send(actual_port);

            // Poll the health endpoint
            let client = reqwest::Client::new();
            let mut attempts = 0;
            // Allow up to 3 minutes for the runtime to load the last-used model into
            // llama-server before the Tauri window opens.  Port binding happens before
            // any heavy init now, so the port arrives quickly; this covers only the
            // time from port-bind to the first successful HTTP response.
            let max_attempts = 180;

            loop {
                let health_url = format!("http://127.0.0.1:{}/healthz", actual_port);
                match client.get(&health_url).send().await {
                    Ok(response) if response.status().is_success() => {
                        eprintln!("Backend service is ready on port {}!", actual_port);
                        let _ = ready_tx.send(true);
                        break;
                    }
                    Ok(response) => {
                        attempts += 1;
                        if attempts % 10 == 0 {
                            eprintln!("Health check status: {} (attempt {}/{})", response.status(), attempts, max_attempts);
                        }
                        if attempts >= max_attempts {
                            eprintln!("Backend failed to become ready after {} attempts", max_attempts);
                            let _ = ready_tx.send(false);
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                    }
                    Err(e) => {
                        attempts += 1;
                        if attempts % 10 == 0 {
                            eprintln!("Health check error on port {}: {} (attempt {}/{})", actual_port, e, attempts, max_attempts);
                        }
                        if attempts >= max_attempts {
                            eprintln!("Backend failed to become ready after {} attempts", max_attempts);
                            let _ = ready_tx.send(false);
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                    }
                }
            }

            // Keep server running
            match server_handle.await {
                Ok(Err(e)) => log_startup(&format!("Backend server stopped with error: {:#}", e)),
                Err(e) => log_startup(&format!("Backend server task panicked: {}", e)),
                Ok(Ok(())) => {}
            }
        });
    });

    // Wait for backend to signal it's starting (with timeout)
    log_startup("Waiting for backend to start...");
    match started_rx.recv_timeout(Duration::from_secs(30)) {
        Ok(true) => log_startup("Backend thread started"),
        Ok(false) => fatal_error("Backend configuration failed to load. Check your .env file or environment variables."),
        Err(_) => fatal_error("Backend thread failed to start within 30 seconds. The application may have encountered a fatal initialization error."),
    }

    // Receive the actual port the backend bound to.
    // Port binding now happens before any engine/runtime initialisation, so it
    // arrives quickly.  60 seconds covers antivirus scanning and slow disk I/O.
    let actual_port = match actual_port_rx.recv_timeout(Duration::from_secs(60)) {
        Ok(port) => {
            log_startup(&format!("Backend port confirmed: {}", port));
            port
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => fatal_error("The backend stopped before it could open port 8888. The reason is in startup.log and backend.log."),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => fatal_error("Backend failed to bind to port 8888 within 60 seconds. Check if:\n- Another instance is already running\n- Firewall is blocking port 8888\n- Antivirus is interfering with local connections"),
    };

    // Wait for backend to be fully ready (with timeout)
    // Increased to 180 seconds to allow for first-run initialization:
    // - Engine manager scanning
    // - Model manager initialization
    // - Database setup
    // - Runtime manager background tasks
    log_startup("Waiting for backend to be ready...");
    match ready_rx.recv_timeout(Duration::from_secs(180)) {
        Ok(true) => log_startup("Backend is ready!"),
        Ok(false) => fatal_error("Backend started but failed health checks. Check startup.log for details."),
        Err(_) => fatal_error("Backend health check timed out after 180 seconds. The server may be stuck during initialization."),
    }

    log_startup("Starting Tauri frontend...");

    // Build Tauri app with better error handling
    let app_result = tauri::Builder::default()
        .manage(BackendPort(Mutex::new(actual_port)))
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![get_backend_port])
        .setup(|app| {
            // Redirect WebView2 user data into our own folder so that only
            // AppData\Local\Offline Counsel AI\ exists — no separate
            // com.offlinecounselai.desktop folder is created.
            let webview_data_dir = dirs::data_local_dir()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
                .join("Offline Counsel AI")
                .join("webview");

            tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::App("index.html".into()),
            )
            .title("Offline Counsel AI")
            // Opens maximized — including the first launch straight after the
            // installer finishes. 800x600, centred, is the size it restores to.
            .inner_size(800.0, 600.0)
            .center()
            .maximized(true)
            .resizable(true)
            // The app draws its own title bar (TitleBar.tsx) in the sidebar's
            // colour, with no icon or title; the system one cannot be recoloured.
            .decorations(false)
            .visible(true)
            .data_directory(webview_data_dir)
            // While minimised or hidden, timers and animation frames are
            // throttled instead of running at full rate. Not suspended: a
            // streaming answer must keep being read in the background.
            .background_throttling(tauri::utils::config::BackgroundThrottlingPolicy::Throttle)
            .build()?;

            Ok(())
        })
        .build(tauri::generate_context!());

    let app = match app_result {
        Ok(app) => {
            log_startup("Tauri app built successfully");
            app
        }
        Err(e) => {
            fatal_error(&format!("Failed to build Tauri application: {}", e));
        }
    };

    // Run with exit handler for graceful shutdown
    let shutdown_port = actual_port;
    log_startup("Running Tauri event loop...");
    
    // Run the app - it blocks until the app exits
    // Any errors during run will be logged by Tauri
    app.run(move |_app_handle, event| match event {
        tauri::RunEvent::ExitRequested { .. } => {
            // Can arrive more than once; the shutdown below runs exactly once.
            static SHUTTING_DOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if SHUTTING_DOWN.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            // Closing the app closes EVERYTHING it started. Every step is
            // written to startup.log so a shutdown can be diagnosed after the
            // fact.
            log_startup("Exit requested: shutting down");
            mark_lock_closing();
            let started = std::time::Instant::now();

            // Watchdog: whatever happens below (a hung engine, a slow disk),
            // the process ends. The Windows job object then takes every
            // child with it.
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_secs(9));
                log_startup("Shutdown did not finish within 9s: forcing exit");
                release_lock();
                std::process::exit(0);
            });

            // 1. Backend: pause downloads, save the warm-start cache, stop the
            //    chat and embedding engines.
            let url = format!("http://127.0.0.1:{}/admin/shutdown", shutdown_port);
            let result = reqwest::blocking::Client::new()
                .post(&url)
                .timeout(Duration::from_secs(7))
                .send();
            log_startup(&match result {
                Ok(r) => format!("Backend shutdown: HTTP {} after {} ms", r.status(), started.elapsed().as_millis()),
                Err(e) => format!("Backend shutdown request failed after {} ms: {}", started.elapsed().as_millis(), e),
            });

            // 2. Any engine process still running is ended explicitly, not
            //    left to the job object alone.
            let killed = offline_intelligence::model_runtime::process_util::kill_all_managed_children();
            log_startup(&format!("Ended {} remaining engine process(es)", killed));

            // The lock is released at Exit, once the process really ends and
            // its ports are free; until then it says "closing", so a launch in
            // the meantime waits for us instead of colliding on the ports.
            log_startup(&format!("Shutdown complete in {} ms", started.elapsed().as_millis()));
        }
        tauri::RunEvent::Exit => {
            // The event loop has ended; the backend thread and anything else
            // still running go with the process.
            log_startup("Exited");
            release_lock();
            std::process::exit(0);
        }
        _ => {}
    });
}
