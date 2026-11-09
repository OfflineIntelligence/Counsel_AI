// E-mail verified accounts: a code by e-mail to join (or to use a new
// computer), then user name and password. Talks to /auth/* in the backend.

import { getApiBaseSync } from './backendUrl';

export interface AccountUser {
    id: number;
    name: string;
    email: string;
    email_verified: boolean;
}

export interface AccountSession {
    user: AccountUser;
    token: string;
}

/** A sign-in that needs a confirmed date of birth before it gets a session
 *  (Google / Microsoft sign-ups, and accounts made before the age check). */
export interface NeedsDob {
    needs_dob: true;
    ticket: string;
}

export type SignInResult = AccountSession | NeedsDob;

export function needsDob(r: SignInResult): r is NeedsDob {
    return (r as NeedsDob).needs_dob === true;
}

/** Thrown when the age check fails; the app then shows the neutral notice. */
export class AgeBlockedError extends Error {
    readonly ageBlocked = true;
}

export interface Registration {
    first_name: string;
    last_name: string;
    email: string;
    username: string;
    password: string;
    age: number;
    dob: string;
}

async function post<T>(path: string, body: unknown): Promise<T> {
    let response: Response;
    try {
        response = await fetch(`${getApiBaseSync()}${path}`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(body),
        });
    } catch {
        throw new Error('Could not reach the app. Try again in a moment.');
    }
    const data = await response.json().catch(() => ({}));
    if (data.age_blocked) throw new AgeBlockedError(data.message);
    if (!response.ok || data.success === false) {
        throw new Error(data.message || `Something went wrong (HTTP ${response.status}).`);
    }
    return data as T;
}

/** Sends the six-digit code to the address. */
export async function startRegistration(details: Registration): Promise<void> {
    await post('/auth/register/start', details);
}

/** Checks the code; on success the account exists and the session is returned. */
export async function verifyRegistration(email: string, code: string): Promise<AccountSession> {
    return post<AccountSession>('/auth/register/verify', { email, code });
}

export async function signIn(identifier: string, password: string): Promise<SignInResult> {
    return post<SignInResult>('/auth/signin', { identifier, password });
}

/** Confirm a date of birth for a sign-in waiting on one. */
export async function confirmAge(ticket: string, dob: string, attest: boolean): Promise<AccountSession> {
    return post<AccountSession>('/auth/age', { ticket, dob, attest });
}

// ── Passwordless and single sign-on ─────────────────────────────────────────

export interface SignInMethods {
    google: boolean;
    microsoft: boolean;
    email_code: boolean;
}

/** Which ways to sign in this computer is set up for. */
export async function getSignInMethods(): Promise<SignInMethods> {
    try {
        const r = await fetch(`${getApiBaseSync()}/auth/providers`);
        if (r.ok) return await r.json();
    } catch {
        /* backend still starting */
    }
    return { google: false, microsoft: false, email_code: false };
}

/** E-mail a sign-in code to an existing account. Throws with
 *  `needsAccount` set when the address has no account here. */
export async function startCodeSignIn(email: string): Promise<void> {
    const r = await fetch(`${getApiBaseSync()}/auth/otp/start`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ email }),
    }).catch(() => {
        throw new Error('Could not reach the app. Try again in a moment.');
    });
    const data = await r.json().catch(() => ({}));
    if (!r.ok || data.success === false) {
        const err = new Error(data.message || `Something went wrong (HTTP ${r.status}).`) as Error & { needsAccount?: boolean };
        err.needsAccount = !!data.needs_account;
        throw err;
    }
}

export async function verifyCodeSignIn(email: string, code: string): Promise<SignInResult> {
    return post<SignInResult>('/auth/otp/verify', { email, code });
}

/**
 * Sign in with Google or Microsoft: the provider opens in the user's browser,
 * returns to the backend, and this resolves with the session when it is done
 * (polled once a second, up to ten minutes). `open` opens a URL externally.
 */
export async function signInWithProvider(
    provider: 'google' | 'microsoft',
    open: (url: string) => Promise<void> | void,
    signal?: AbortSignal,
): Promise<SignInResult> {
    const { url, state } = await post<{ url: string; state: string }>('/auth/oauth/start', { provider });
    await open(url);
    const deadline = Date.now() + 10 * 60 * 1000;
    while (Date.now() < deadline) {
        if (signal?.aborted) throw new Error('Sign-in was cancelled.');
        await new Promise(r => setTimeout(r, 1000));
        const r = await fetch(`${getApiBaseSync()}/auth/oauth/result?state=${encodeURIComponent(state)}`).catch(() => null);
        const data = r ? await r.json().catch(() => null) : null;
        if (!data || data.status === 'pending') continue;
        if (data.status === 'done') return data.session as SignInResult;
        if (data.message && /not eligible/i.test(data.message)) throw new AgeBlockedError(data.message);
        throw new Error(data.message || 'The sign-in expired. Try again.');
    }
    throw new Error('The sign-in took too long. Try again.');
}
