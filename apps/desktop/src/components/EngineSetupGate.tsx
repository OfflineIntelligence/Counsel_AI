import { useCallback, useEffect, useRef, useState } from 'react';
import { getApiBase } from '../api/backendUrl';
import './LoadingScreen.css';

// Engine state as serialized by the backend (EngineState in
// crates/offline-intelligence/src/engine_management/mod.rs)
interface EngineStateInfo {
  state: 'ready' | 'not_installed' | 'corrupted';
  engine_id?: string;
  acceleration?: string;
  version?: string;
  reason?: string;
}

interface HealthzResponse {
  status: string;
  runtime_ready: boolean;
  message?: string | null;
  engine?: EngineStateInfo | null;
  engine_manager_available?: boolean;
}

type GateStatus =
  | { kind: 'checking' }
  | { kind: 'ok' }
  | { kind: 'blocked'; engine: EngineStateInfo | null; managerAvailable: boolean }
  | { kind: 'installing'; percent: number | null }
  | { kind: 'install-failed'; detail: string };

interface EngineSetupGateProps {
  children: React.ReactNode;
}

/**
 * Blocks the app behind an explicit engine-problem screen when the backend
 * reports no usable inference engine (installer download cancelled/failed,
 * engine corrupted on disk, hardware changed).
 *
 * Design contract: NOTHING downloads automatically. The single recovery path
 * is the user clicking "Install engine for this computer", which calls
 * POST /engines/install with no id — the backend installs the one engine its
 * hardware decision table selects. Failures are shown verbatim with a retry
 * button; there is no silent retry and no alternative engine.
 */
export function EngineSetupGate({ children }: EngineSetupGateProps) {
  const [status, setStatus] = useState<GateStatus>({ kind: 'checking' });
  const pollTimer = useRef<ReturnType<typeof setInterval> | null>(null);

  const checkEngine = useCallback(async () => {
    try {
      const apiBase = await getApiBase();
      const response = await fetch(`${apiBase}/healthz`, {
        headers: { Accept: 'application/json' },
      });
      if (!response.ok) {
        // Backend unreachable is LoadingScreen's problem, not ours — let the
        // app through so its own error handling reports it.
        setStatus({ kind: 'ok' });
        return;
      }
      const data: HealthzResponse = await response.json();

      if (data.engine_manager_available === false) {
        setStatus({ kind: 'blocked', engine: null, managerAvailable: false });
        return;
      }
      if (data.engine && data.engine.state !== 'ready') {
        setStatus({ kind: 'blocked', engine: data.engine, managerAvailable: true });
        return;
      }
      // engine ready, or an older backend without the engine field
      setStatus({ kind: 'ok' });
    } catch (e) {
      console.error('[EngineSetupGate] healthz check failed:', e);
      setStatus({ kind: 'ok' });
    }
  }, []);

  useEffect(() => {
    checkEngine();
    const recheck = () => { checkEngine(); };
    window.addEventListener('oca-recheck-engine', recheck);
    return () => {
      window.removeEventListener('oca-recheck-engine', recheck);
      if (pollTimer.current) clearInterval(pollTimer.current);
    };
  }, [checkEngine]);

  const startInstall = async () => {
    setStatus({ kind: 'installing', percent: null });

    // Progress polling runs alongside the install request
    const apiBase = await getApiBase();
    pollTimer.current = setInterval(async () => {
      try {
        const r = await fetch(`${apiBase}/engines/progress`);
        if (!r.ok) return;
        const d = await r.json();
        const active = Array.isArray(d.downloads) && d.downloads.length > 0 ? d.downloads[0] : null;
        if (active && typeof active.progress_percentage === 'number') {
          setStatus(prev =>
            prev.kind === 'installing' ? { kind: 'installing', percent: active.progress_percentage } : prev
          );
        }
      } catch {
        // progress polling is cosmetic; the install request below is authoritative
      }
    }, 1000);

    try {
      const response = await fetch(`${apiBase}/engines/install`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({}),
      });
      if (pollTimer.current) { clearInterval(pollTimer.current); pollTimer.current = null; }

      if (response.ok) {
        // Re-check /healthz — the gate opens only when the backend itself
        // reports the engine as ready.
        await checkEngine();
      } else {
        const errorData = await response.json().catch(() => ({} as any));
        setStatus({
          kind: 'install-failed',
          detail:
            errorData.detail ||
            errorData.error ||
            `Engine install failed (HTTP ${response.status}).`,
        });
      }
    } catch (e) {
      if (pollTimer.current) { clearInterval(pollTimer.current); pollTimer.current = null; }
      setStatus({
        kind: 'install-failed',
        detail: `Engine install request failed: ${e instanceof Error ? e.message : String(e)}`,
      });
    }
  };

  if (status.kind === 'ok') {
    return <>{children}</>;
  }

  if (status.kind === 'checking') {
    return (
      <div className="loading-screen">
        <div className="loading-content">
          <div className="spinner"></div>
          <h2>Offline Counsel AI</h2>
          <p className="loading-text">Checking inference engine...</p>
        </div>
      </div>
    );
  }

  if (status.kind === 'installing') {
    return (
      <div className="loading-screen">
        <div className="loading-content">
          <div className="spinner"></div>
          <h2>Installing Inference Engine</h2>
          <p className="loading-text">
            {status.percent !== null
              ? `Downloading... ${status.percent.toFixed(0)}%`
              : 'Downloading and verifying...'}
          </p>
          <p className="loading-subtext">
            The engine matched to this computer&apos;s hardware is being downloaded,
            integrity-checked, and verified. This is a one-time setup.
          </p>
        </div>
      </div>
    );
  }

  // blocked / install-failed
  const isCorrupted = status.kind === 'blocked' && status.engine?.state === 'corrupted';
  const managerDown = status.kind === 'blocked' && !status.managerAvailable;

  return (
    <div className="loading-screen error">
      <div className="loading-content">
        <div className="error-icon">⚠</div>
        <h2>
          {status.kind === 'install-failed'
            ? 'Engine Installation Failed'
            : managerDown
              ? 'Engine Manager Unavailable'
              : isCorrupted
                ? 'Inference Engine Corrupted'
                : 'Inference Engine Not Installed'}
        </h2>
        <p className="error-message">
          {status.kind === 'install-failed'
            ? status.detail
            : managerDown
              ? 'The engine manager failed to start. Check the application logs and restart the application.'
              : isCorrupted
                ? `Engine '${status.engine?.engine_id}' failed verification: ${status.engine?.reason}`
                : 'No inference engine is installed on this computer. It is normally installed by the setup wizard; the download may have been cancelled or failed.'}
        </p>
        {!managerDown && (
          <div className="error-actions">
            <button className="retry-button primary" onClick={startInstall}>
              {isCorrupted ? 'Reinstall engine for this computer' : 'Install engine for this computer'}
            </button>
          </div>
        )}
        <p className="error-hint">
          The engine is chosen to exactly match this computer&apos;s hardware
          (graphics card and drivers). Nothing is downloaded without this explicit action.
        </p>
      </div>
    </div>
  );
}
