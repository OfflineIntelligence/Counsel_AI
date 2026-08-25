/**
 * Fetch that retries TRANSIENT network failures (fetch throwing — e.g.
 * net::ERR_NETWORK_IO_SUSPENDED when Windows suspends the WebView's network
 * stack on sleep/lid-close, seen in production 2026-08-08). HTTP error
 * responses are real backend answers and are returned as-is, never retried.
 *
 * Use ONLY for requests that are safe to repeat: idempotent reads, or writes
 * the backend deduplicates (uploads are content-hash-deduped). Do NOT use for
 * generation requests.
 */
export async function fetchWithNetworkRetry(
    url: string,
    options: RequestInit = {},
    attempts: number = 4,
): Promise<Response> {
    const RETRY_DELAY_MS = 1500;
    let lastError: unknown;
    for (let attempt = 1; attempt <= attempts; attempt++) {
        try {
            return await fetch(url, options);
        } catch (error) {
            lastError = error;
            console.warn(
                `[net] ${options.method ?? 'GET'} ${url} attempt ${attempt}/${attempts} failed: ${String(error)}` +
                    (attempt < attempts ? ' — retrying' : ''),
            );
            if (attempt < attempts) {
                await new Promise(r => setTimeout(r, RETRY_DELAY_MS * attempt));
            }
        }
    }
    throw new Error(
        'The connection was interrupted (this can happen when the computer sleeps ' +
            `or switches networks). Please try again. [${String(lastError)}]`,
    );
}

// Fetch wrapper with timeout support
export async function fetchWithTimeout(
    url: string,
    options: RequestInit = {},
    timeoutMs: number = 30000
): Promise<Response> {
    const controller = new AbortController();
    const timeoutId = setTimeout(() => controller.abort(), timeoutMs);
    
    try {
        const response = await fetch(url, {
            ...options,
            signal: controller.signal,
        });
        clearTimeout(timeoutId);
        return response;
    } catch (error) {
        clearTimeout(timeoutId);
        if (error instanceof Error && error.name === 'AbortError') {
            throw new Error(`Request timeout after ${timeoutMs}ms`);
        }
        throw error;
    }
}
