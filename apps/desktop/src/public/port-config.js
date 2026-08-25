// Port is resolved at runtime via Tauri IPC (get_backend_port command).
// The backend binds exclusively to port 8888 (API_PORT). Do NOT set
// BACKEND_PORT_OVERRIDE here — the IPC call returns the authoritative value.
window.BACKEND_PORT_OVERRIDE = null;
