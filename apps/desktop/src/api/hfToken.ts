/**
 * Single source of truth for the HuggingFace token in browser storage.
 *
 * Exactly ONE localStorage key exists for the token:
 *   'offline-intelligence-hf-token'
 *
 * Historically a second key ('aud-io-hf-token') was written by the Models and
 * Settings panels while the rest of the app used the key above, so a token
 * entered in one place was invisible to the other. Every read/write now goes
 * through this module, and any value still stored under the legacy key is
 * migrated (then deleted) the first time the token is accessed.
 *
 * The encrypted backend store (POST /api-keys) remains the durable home for
 * the token; this module only owns the browser-side copy.
 */

export const HF_TOKEN_KEY = 'offline-intelligence-hf-token';
const LEGACY_HF_TOKEN_KEY = 'aud-io-hf-token';

let migrated = false;

/** Move a legacy-key token to the canonical key, once per page load. */
function migrateLegacyKey(): void {
  if (migrated) return;
  migrated = true;
  try {
    const legacy = localStorage.getItem(LEGACY_HF_TOKEN_KEY);
    if (legacy) {
      if (!localStorage.getItem(HF_TOKEN_KEY)) {
        localStorage.setItem(HF_TOKEN_KEY, legacy);
        console.log('[hfToken] Migrated HuggingFace token from legacy storage key');
      }
      localStorage.removeItem(LEGACY_HF_TOKEN_KEY);
    }
  } catch {
    // localStorage unavailable — nothing to migrate
  }
}

/** Read the HuggingFace token ('' when not set). */
export function getHfToken(): string {
  migrateLegacyKey();
  try {
    return localStorage.getItem(HF_TOKEN_KEY) ?? '';
  } catch {
    return '';
  }
}

/** Store the HuggingFace token (empty/whitespace-only values clear it). */
export function setHfToken(token: string): void {
  migrateLegacyKey();
  try {
    const trimmed = token.trim();
    if (trimmed) {
      localStorage.setItem(HF_TOKEN_KEY, trimmed);
    } else {
      localStorage.removeItem(HF_TOKEN_KEY);
    }
  } catch {
    // localStorage unavailable — token lives only in component state
  }
}

/** Remove the HuggingFace token from browser storage (both keys). */
export function clearHfToken(): void {
  migrated = true; // nothing left to migrate after an explicit clear
  try {
    localStorage.removeItem(HF_TOKEN_KEY);
    localStorage.removeItem(LEGACY_HF_TOKEN_KEY);
  } catch {
    // localStorage unavailable
  }
}
