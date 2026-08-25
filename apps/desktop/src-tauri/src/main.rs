// apps/desktop/src-tauri/src/main.rs

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use offline_intelligence::{Config, run_thread_server};
use std::sync::Mutex;
use std::time::Duration;

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

fn main() {
    log_startup("=== Offline Counsel AI Starting ===");
    log_startup(&format!("Current directory: {:?}", std::env::current_dir().unwrap_or_default()));

    #[cfg(debug_assertions)]
    log_startup("Build mode: DEBUG");
    #[cfg(not(debug_assertions))]
    log_startup("Build mode: RELEASE");

    // Single-instance lock: prevent multiple instances from running
    // This uses a lock file in the user data directory
    // Note: Lock file is automatically released when process exits or is killed
    if let Some(base_dir) = get_data_dir() {
        let lock_file = base_dir.join("app.lock");

        // Try to read existing lock file to check if process is still running
        if lock_file.exists() {
            if let Ok(pid_str) = std::fs::read_to_string(&lock_file) {
                if let Ok(existing_pid) = pid_str.trim().parse::<u32>() {
                    // Check if the process is actually running
                    #[cfg(target_os = "windows")]
                    {
                        use std::os::windows::process::CommandExt;
                        use std::process::Command;
                        let output = Command::new("tasklist")
                            .args(&["/FI", &format!("PID eq {}", existing_pid), "/NH"])
                            .creation_flags(0x08000000) // CREATE_NO_WINDOW — no console flash
                            .output();

                        if let Ok(output) = output {
                            let output_str = String::from_utf8_lossy(&output.stdout);
                            if output_str.contains(&existing_pid.to_string()) {
                                // Process is still running
                                fatal_error(&format!(
                                    "Another instance of Offline Counsel AI is already running (PID: {}).\n\n\
                                    Only one instance can run at a time to avoid port conflicts.\n\n\
                                    To close the existing instance:\n\
                                    1. Check Task Manager for 'offline-intelligence-desktop.exe'\n\
                                    2. Or run: taskkill /F /PID {}",
                                    existing_pid, existing_pid
                                ));
                            } else {
                                // Stale lock file from crashed process
                                log_startup(&format!("Removing stale lock file (PID {} not running)", existing_pid));
                                let _ = std::fs::remove_file(&lock_file);
                            }
                        }
                    }
                    #[cfg(not(target_os = "windows"))]
                    {
                        // On Unix, check /proc/{pid}
                        let proc_path = std::path::Path::new("/proc").join(existing_pid.to_string());
                        if proc_path.exists() {
                            fatal_error(&format!(
                                "Another instance of Offline Counsel AI is already running (PID: {}).\n\n\
                                Only one instance can run at a time.\n\n\
                                To close: kill {}",
                                existing_pid, existing_pid
                            ));
                        } else {
                            // Stale lock file
                            log_startup(&format!("Removing stale lock file (PID {} not running)", existing_pid));
                            let _ = std::fs::remove_file(&lock_file);
                        }
                    }
                }
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
        let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");
        rt.block_on(async {
            let mut config = match Config::from_env() {
                Ok(cfg) => cfg,
                Err(e) => {
                    eprintln!("Failed to load config: {}", e);
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
            let server_handle = tokio::spawn(run_thread_server(config, Some(port_tx.clone())));

            tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

            // Receive the actual port from backend (may differ from configured if primary port was in use)
            let actual_port = match port_rx.recv() {
                Ok(port) => {
                    eprintln!("Backend bound to port {}", port);
                    port
                }
                Err(e) => {
                    eprintln!("Failed to receive port from backend: {}", e);
                    let _ = ready_tx.send(false);
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
            if let Err(e) = server_handle.await {
                eprintln!("Server task failed: {}", e);
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
        Err(_) => fatal_error("Backend failed to bind to port 8888 within 60 seconds. Check if:\n- Another instance is already running\n- Firewall is blocking port 8888\n- Antivirus is interfering with local connections"),
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
        Err(_) => fatal_error("Backend health check timed out after 60 seconds. The server may be stuck during initialization."),
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
            .inner_size(800.0, 600.0)
            .center()
            .resizable(true)
            .decorations(true)
            .visible(true)
            .data_directory(webview_data_dir)
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
    app.run(move |_app_handle, event| {
        if let tauri::RunEvent::ExitRequested { .. } = event {
            eprintln!("App exit requested, flushing KV cache...");
            let url = format!("http://127.0.0.1:{}/admin/shutdown", shutdown_port);
            let _ = reqwest::blocking::Client::new()
                .post(&url)
                .timeout(Duration::from_secs(5))
                .send();
            eprintln!("Shutdown signal sent");

            // Clean up lock file
            if let Some(base_dir) = get_data_dir() {
                let lock_file = base_dir.join("app.lock");
                if let Err(e) = std::fs::remove_file(&lock_file) {
                    eprintln!("Warning: Could not remove lock file: {}", e);
                } else {
                    eprintln!("Instance lock released");
                }
            }
        }
    });
}
