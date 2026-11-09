/**
 * Every call to Counsel's own backend carries the signed-in session.
 *
 * Routes that hold secrets (API keys, cloud models) need it once this
 * computer has an account (backend: api/authz.rs). Rather than thread the
 * token through every fetch in the app, the one window.fetch is wrapped:
 * requests to the backend's address gain `Authorization: Bearer <token>`
 * when a session exists and the caller set none. Nothing else is touched.
 */
import { getApiBaseSync } from './backendUrl';
import { getStoredToken } from './auth';

let installed = false;

export function installAuthFetch() {
    if (installed) return;
    installed = true;
    const original = window.fetch.bind(window);
    window.fetch = (input: RequestInfo | URL, init?: RequestInit) => {
        const token = getStoredToken();
        const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;
        let base = '';
        try {
            base = getApiBaseSync();
        } catch {
            /* backend address not known yet */
        }
        if (!token || !base || !url.startsWith(base)) return original(input, init);
        const headers = new Headers(init?.headers ?? (input instanceof Request ? input.headers : undefined));
        if (!headers.has('Authorization')) headers.set('Authorization', `Bearer ${token}`);
        return original(input, { ...init, headers });
    };
}
