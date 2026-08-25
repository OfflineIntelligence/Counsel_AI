// Centralized backend URL management
// Resolves the dynamic port from the Tauri backend on startup

import { invoke } from '@tauri-apps/api/core';

/**
 * Loopback host, and why it is the literal IPv4 address rather than "localhost".
 *
 * The backend binds to `API_HOST`, which is `127.0.0.1` in the project's single
 * .env (config.rs: `env::var("API_HOST").unwrap_or("127.0.0.1")`) — an
 * IPv4-only socket.
 *
 * On Windows, "localhost" resolves to BOTH ::1 and 127.0.0.1, with the IPv6
 * address ordered first (RFC 6724). A socket bound only to 127.0.0.1 does not
 * accept a connection to [::1], so every request to http://localhost:PORT
 * spends an attempt on ::1 before the browser retries on IPv4. WebView2 does
 * perform that fallback, so it "works" — but it costs a failed connect per new
 * connection, and it stops working entirely on machines where IPv6 loopback is
 * routed to a black hole rather than actively refusing (some VPN and endpoint
 * security products do exactly that).
 *
 * Addressing the bind address directly removes the round trip and the failure
 * mode. It also makes the frontend agree with the rest of the app: the Tauri
 * shell already uses 127.0.0.1 for its health poll and for /admin/shutdown.
 *
 * index.html's CSP must permit `http://127.0.0.1:*` in `connect-src` for this
 * to be reachable — the two are a pair, do not change one without the other.
 */
const LOOPBACK_HOST = '127.0.0.1';

/**
 * Port the backend binds when IPC is unavailable (browser/dev context).
 *
 * Safe as a constant because the backend binds `API_PORT` exclusively and fails
 * fast if it is taken — `bind_and_notify_port` has no random-port fallback — so
 * a running backend is always on this port unless API_PORT was changed, in
 * which case the Tauri IPC path below reports the real value.
 */
const DEFAULT_BACKEND_PORT = 8888;

let _backendPort: number | null = null;
let _initPromise: Promise<number> | null = null;

/** Resolve the backend port from Tauri managed state (async, cached) */
export async function getBackendPort(): Promise<number> {
    if (_backendPort !== null) {
        console.log('[backendUrl] Returning cached port:', _backendPort);
        return _backendPort;
    }
    if (_initPromise) {
        console.log('[backendUrl] Waiting for existing promise...');
        return _initPromise;
    }

    _initPromise = (async () => {
        console.log('[backendUrl] Starting port resolution...');

        // Always resolve via Tauri IPC so we get the actual bound port.
        // The backend binds exclusively to API_PORT — there is no random
        // fallback. window.BACKEND_PORT_OVERRIDE is a static build-time
        // value and must NOT take precedence over the IPC result.
        try {
            console.log('[backendUrl] Trying Tauri IPC...');
            const port = await invoke<number>('get_backend_port');
            if (port && port > 0) {
                console.log(`[backendUrl] Got port from Tauri: ${port}`);
                _backendPort = port;
                return port;
            }
        } catch (error) {
            console.warn('[backendUrl] Tauri IPC failed:', error);
        }

        // Fallback to default port (dev / non-Tauri context only)
        console.log(`[backendUrl] Using fallback port: ${DEFAULT_BACKEND_PORT}`);
        _backendPort = DEFAULT_BACKEND_PORT;
        return DEFAULT_BACKEND_PORT;
    })();

    return _initPromise;
}

/** Get the full API base URL (async, for initial setup) */
export async function getApiBase(): Promise<string> {
    const port = await getBackendPort();
    const url = `http://${LOOPBACK_HOST}:${port}`;
    console.log('[backendUrl] API base:', url);
    return url;
}

/**
 * Synchronous API base getter - valid after LoadingScreen resolves the port.
 *
 * LoadingScreen awaits getApiBase() and gates the whole app behind it, so by
 * the time any component calls this, the port is cached. The fallback below
 * only applies if that ordering is ever broken.
 */
export function getApiBaseSync(): string {
    const port = _backendPort ?? DEFAULT_BACKEND_PORT;
    return `http://${LOOPBACK_HOST}:${port}`;
}
