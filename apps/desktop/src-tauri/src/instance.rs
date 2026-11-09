//! Finding, focusing and waiting for a previous instance of this app, so a
//! second launch never starts a second backend next to the first.

use std::time::Duration;

#[cfg(target_os = "windows")]
mod imp {
    use std::ffi::c_void;
    use std::time::Duration;

    const SYNCHRONIZE: u32 = 0x0010_0000;
    const PROCESS_TERMINATE: u32 = 0x0001;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const WAIT_TIMEOUT: u32 = 0x102;
    const SW_RESTORE: i32 = 9;

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn WaitForSingleObject(handle: *mut c_void, ms: u32) -> u32;
        fn TerminateProcess(handle: *mut c_void, code: u32) -> i32;
        fn QueryFullProcessImageNameW(handle: *mut c_void, flags: u32, name: *mut u16, size: *mut u32) -> i32;
    }
    #[link(name = "user32")]
    extern "system" {
        fn FindWindowExW(parent: *mut c_void, after: *mut c_void, class: *const u16, title: *const u16) -> *mut c_void;
        fn GetWindowThreadProcessId(hwnd: *mut c_void, pid: *mut u32) -> u32;
        fn ShowWindow(hwnd: *mut c_void, cmd: i32) -> i32;
        fn SetForegroundWindow(hwnd: *mut c_void) -> i32;
    }

    /// Open `pid` only if it is a live process running this same executable:
    /// after a crash Windows can hand the old PID to any other program.
    fn open_ours(pid: u32) -> Option<*mut c_void> {
        let handle = unsafe { OpenProcess(SYNCHRONIZE | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let named = unsafe { QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len) } != 0;
        let same_exe = named
            && std::env::current_exe().map_or(false, |me| {
                String::from_utf16_lossy(&buf[..len as usize]).eq_ignore_ascii_case(&me.to_string_lossy())
            });
        let alive = unsafe { WaitForSingleObject(handle, 0) } == WAIT_TIMEOUT;
        if same_exe && alive {
            Some(handle)
        } else {
            unsafe { CloseHandle(handle) };
            None
        }
    }

    pub fn is_running(pid: u32) -> bool {
        match open_ours(pid) {
            Some(handle) => {
                unsafe { CloseHandle(handle) };
                true
            }
            None => false,
        }
    }

    pub fn wait_or_end(pid: u32, wait: Duration) -> bool {
        let Some(handle) = open_ours(pid) else { return true };
        let mut gone = unsafe { WaitForSingleObject(handle, wait.as_millis() as u32) } != WAIT_TIMEOUT;
        if !gone {
            // It is shutting down anyway; its job object takes the engines with it.
            unsafe { TerminateProcess(handle, 0) };
            gone = unsafe { WaitForSingleObject(handle, 3000) } != WAIT_TIMEOUT;
        }
        unsafe { CloseHandle(handle) };
        gone
    }

    pub fn focus_window(pid: u32) -> bool {
        let title: Vec<u16> = "Offline Counsel AI".encode_utf16().chain(Some(0)).collect();
        let mut hwnd = std::ptr::null_mut();
        loop {
            hwnd = unsafe { FindWindowExW(std::ptr::null_mut(), hwnd, std::ptr::null(), title.as_ptr()) };
            if hwnd.is_null() {
                return false;
            }
            let mut owner = 0u32;
            unsafe { GetWindowThreadProcessId(hwnd, &mut owner) };
            if owner == pid {
                unsafe {
                    ShowWindow(hwnd, SW_RESTORE);
                    SetForegroundWindow(hwnd);
                }
                return true;
            }
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    use std::time::Duration;

    pub fn is_running(pid: u32) -> bool {
        std::path::Path::new("/proc").join(pid.to_string()).exists()
    }

    pub fn wait_or_end(pid: u32, wait: Duration) -> bool {
        let deadline = std::time::Instant::now() + wait;
        while is_running(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        if is_running(pid) {
            let _ = std::process::Command::new("kill").args(["-9", &pid.to_string()]).status();
            std::thread::sleep(Duration::from_millis(500));
        }
        !is_running(pid)
    }

    pub fn focus_window(_pid: u32) -> bool {
        false
    }
}

/// True if `pid` is a live instance of this app.
pub fn is_running(pid: u32) -> bool {
    imp::is_running(pid)
}

/// Wait up to `wait` for `pid` to exit, then end it. True once it is gone.
pub fn wait_or_end(pid: u32, wait: Duration) -> bool {
    imp::wait_or_end(pid, wait)
}

/// Bring `pid`'s main window to the front. False if it has none yet.
pub fn focus_window(pid: u32) -> bool {
    imp::focus_window(pid)
}
