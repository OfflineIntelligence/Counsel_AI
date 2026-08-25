/**
 * System metrics.
 *
 * Rebuilt around ROLLING HISTORY. The previous version polled every three
 * seconds and discarded each sample, so a bar could jump from 10% to 62% with
 * nothing on screen explaining the change — and an instantaneous reading is the
 * least informative view of a metric that moves. What matters while a model
 * loads or a scanned PDF is being OCR'd is the shape of the last minute or two.
 *
 * Each headline metric therefore keeps a bounded window of samples and draws it
 * as a sparkline. That is where "dynamic" comes from: real history, not extra
 * animation.
 *
 * The data fetching below is unchanged in behaviour — it still prefers
 * /metrics/system and degrades to /hardware/info when the live endpoint is
 * unavailable — because that logic was correct.
 */

import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { getApiBaseSync } from '../api/backendUrl';
import { ArrowLeft, Cpu, HardDrive, Activity, RefreshCw, MemoryStick, Zap, Thermometer } from 'lucide-react';
import './MetricsPanel.css';

interface SystemMetrics {
  cpu: {
    usage_percent: number;
    cores: number;
    model_name: string;
    frequency_mhz: number;
    per_core_usage: number[];
  };
  gpu: {
    available: boolean;
    name: string;
    usage_percent: number;
    vram_total_gb: number;
    vram_used_gb: number;
    temperature_c: number;
  };
  memory: {
    total_gb: number;
    used_gb: number;
    available_gb: number;
    usage_percent: number;
  };
  storage: {
    total_gb: number;
    used_gb: number;
    available_gb: number;
    models_size_gb: number;
  };
  inference: {
    active_model: string | null;
    tokens_per_second: number;
    total_requests: number;
    avg_latency_ms: number;
    device: string; // "GPU" | "CPU" | "CPU+GPU"
    gpu_layers: number;
  };
}

interface LiveMetricsResponse {
  cpu_usage_percent: number;
  per_core_usage?: number[];
  cpu_model_name?: string;
  cpu_frequency_mhz?: number;
  cpu_cores?: number;
  gpu_available: boolean;
  gpu_name?: string;
  gpu_usage_percent: number;
  gpu_vram_total_gb: number;
  gpu_vram_used_gb: number;
  gpu_temperature_c: number;
  memory_total_gb: number;
  memory_used_gb: number;
  memory_available_gb: number;
  inference_device?: string;
  gpu_layers_offloaded?: number;
}

interface HardwareInfoResponse {
  cpu_cores: number;
  gpu_available: boolean;
  gpu_vram_gb?: number;
  total_ram_gb: number;
  available_ram_gb: number;
  storage_used_bytes: number;
  storage_available_bytes: number;
}

const POLL_MS = 3000;
/** Samples retained per series. At a 3s poll this is a two-minute window —
 *  long enough to show a model load or an OCR burst, short enough that the
 *  line does not compress into noise. */
const HISTORY_LEN = 40;

type Series = { cpu: number[]; memory: number[]; gpu: number[]; vram: number[] };
const EMPTY_SERIES: Series = { cpu: [], memory: [], gpu: [], vram: [] };

const push = (series: number[], value: number): number[] => {
  const next = series.length >= HISTORY_LEN ? series.slice(1) : series.slice();
  next.push(Number.isFinite(value) ? Math.min(Math.max(value, 0), 100) : 0);
  return next;
};

/** Severity for a percentage, driving the tile's accent token. */
const severity = (percent: number, warn = 75, crit = 90): 'ok' | 'warn' | 'crit' =>
  percent >= crit ? 'crit' : percent >= warn ? 'warn' : 'ok';

/* ── Sparkline ───────────────────────────────────────────────────────────────
   Plain inline SVG, no charting library. The whole series is one polyline plus
   one filled polygon: two DOM nodes per tile, which matters because this page
   re-renders every 3 seconds on hardware that is simultaneously running
   inference.

   The Y axis is pinned to 0–100 rather than auto-scaled to the data. An
   auto-scaled sparkline makes 2%–4% jitter look like a crisis, which is exactly
   the wrong signal on a utilisation chart. */
const Sparkline: React.FC<{ values: number[] }> = ({ values }) => {
  const W = 100;
  const H = 30;

  if (values.length < 2) {
    return (
      <svg className="spark" viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" role="presentation">
        <text className="spark__empty" x="0" y={H / 2 + 3} style={{ fontSize: 7 }}>
          collecting…
        </text>
      </svg>
    );
  }

  const stepX = W / (values.length - 1);
  const points = values.map((v, i) => `${(i * stepX).toFixed(2)},${(H - (v / 100) * H).toFixed(2)}`);
  const last = values[values.length - 1];

  return (
    <svg className="spark" viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" role="presentation">
      <polygon className="spark__fill" points={`0,${H} ${points.join(' ')} ${W},${H}`} />
      <polyline className="spark__line" points={points.join(' ')} />
      {/* r is in viewBox units and the box is stretched by preserveAspectRatio:
          none, so the dot would render as an ellipse. Drawn as a tiny rect
          instead, which distorts predictably. */}
      <rect
        className="spark__head"
        x={W - 1.6}
        y={H - (last / 100) * H - 0.8}
        width={1.6}
        height={1.6}
      />
    </svg>
  );
};

const StatTile: React.FC<{
  label: string;
  icon: React.ReactNode;
  value: string;
  unit?: string;
  detail?: string;
  history: number[];
  meter?: number;
  tone?: 'ok' | 'warn' | 'crit';
}> = ({ label, icon, value, unit, detail, history, meter, tone = 'ok' }) => (
  <div className={`metric-tile metric-tile--${tone}`}>
    <div className="metric-tile__head">
      {icon}
      <span className="metric-tile__label">{label}</span>
    </div>
    <div className="metric-tile__value">
      {value}
      {unit && <span className="metric-tile__unit">{unit}</span>}
    </div>
    <Sparkline values={history} />
    {meter != null && (
      <div
        className="ui-meter"
        style={{ '--progress': meter, '--progress-color': 'var(--tile-accent)' } as React.CSSProperties}
      >
        <div className="ui-meter__fill" />
      </div>
    )}
    {detail && <span className="metric-tile__detail">{detail}</span>}
  </div>
);

/**
 * `embedded` renders the metrics WITHOUT the full-height page chrome — no
 * back button, no 100vh wrapper — so the same live component can sit inside
 * the Settings page as its first section. Metrics are no longer a separate
 * destination; this prop is what makes one implementation serve both without
 * forking the polling/history logic.
 */
const MetricsPanel: React.FC<{
  isOpen: boolean;
  onClose?: () => void;
  embedded?: boolean;
}> = ({ isOpen, onClose, embedded = false }) => {
  const [metrics, setMetrics] = useState<SystemMetrics | null>(null);
  const [history, setHistory] = useState<Series>(EMPTY_SERIES);
  const [autoRefresh, setAutoRefresh] = useState(true);
  const [tick, setTick] = useState(0);

  /** Kept in a ref so the sampler is not re-created (and the interval not
   *  restarted) every time a sample lands. */
  const seriesRef = useRef<Series>(EMPTY_SERIES);

  const sample = useCallback(async (): Promise<SystemMetrics | null> => {
    const [liveRes, hwRes] = await Promise.all([
      fetch(`${getApiBaseSync()}/metrics/system`).catch(() => null),
      fetch(`${getApiBaseSync()}/hardware/info`).catch(() => null),
    ]);

    const live: LiveMetricsResponse | null = liveRes?.ok ? await liveRes.json() : null;
    const hw: HardwareInfoResponse | null = hwRes?.ok ? await hwRes.json() : null;

    if (live) {
      const storageTotal = hw ? (hw.storage_used_bytes + hw.storage_available_bytes) / 1024 ** 3 : 0;
      return {
        cpu: {
          usage_percent: live.cpu_usage_percent,
          cores: live.per_core_usage?.length || (hw?.cpu_cores ?? 0),
          model_name: live.cpu_model_name || 'System CPU',
          frequency_mhz: live.cpu_frequency_mhz || 0,
          per_core_usage: live.per_core_usage || [],
        },
        gpu: {
          available: live.gpu_available,
          name: live.gpu_name || (live.gpu_available ? 'GPU' : 'Not detected'),
          usage_percent: live.gpu_usage_percent,
          vram_total_gb: live.gpu_vram_total_gb,
          vram_used_gb: live.gpu_vram_used_gb,
          temperature_c: live.gpu_temperature_c,
        },
        memory: {
          total_gb: live.memory_total_gb,
          used_gb: live.memory_used_gb,
          available_gb: live.memory_available_gb,
          usage_percent: live.memory_total_gb > 0 ? (live.memory_used_gb / live.memory_total_gb) * 100 : 0,
        },
        storage: {
          total_gb: storageTotal,
          used_gb: hw ? hw.storage_used_bytes / 1024 ** 3 : 0,
          available_gb: hw ? hw.storage_available_bytes / 1024 ** 3 : 0,
          models_size_gb: hw ? hw.storage_used_bytes / 1024 ** 3 : 0,
        },
        inference: {
          active_model: null,
          tokens_per_second: 0,
          total_requests: 0,
          avg_latency_ms: 0,
          device: live.inference_device || 'CPU',
          gpu_layers: live.gpu_layers_offloaded || 0,
        },
      };
    }

    // Live endpoint unavailable — fall back to static hardware facts so the
    // page still describes the machine instead of showing nothing.
    if (hw) {
      const used = (hw.total_ram_gb || 0) - (hw.available_ram_gb || 0);
      return {
        cpu: { usage_percent: 0, cores: hw.cpu_cores || 0, model_name: 'System CPU', frequency_mhz: 0, per_core_usage: [] },
        gpu: {
          available: hw.gpu_available || false,
          name: hw.gpu_available ? 'GPU' : 'Not detected',
          usage_percent: 0,
          vram_total_gb: hw.gpu_vram_gb || 0,
          vram_used_gb: 0,
          temperature_c: 0,
        },
        memory: {
          total_gb: hw.total_ram_gb || 0,
          used_gb: used,
          available_gb: hw.available_ram_gb || 0,
          usage_percent: (hw.total_ram_gb || 0) > 0 ? (used / hw.total_ram_gb) * 100 : 0,
        },
        storage: {
          total_gb: (hw.storage_used_bytes + hw.storage_available_bytes) / 1024 ** 3,
          used_gb: hw.storage_used_bytes / 1024 ** 3,
          available_gb: hw.storage_available_bytes / 1024 ** 3,
          models_size_gb: hw.storage_used_bytes / 1024 ** 3,
        },
        inference: { active_model: null, tokens_per_second: 0, total_requests: 0, avg_latency_ms: 0, device: 'CPU', gpu_layers: 0 },
      };
    }
    return null;
  }, []);

  // One effect drives both the initial read and every poll. `tick` is the
  // manual-refresh trigger; the interval below advances it on a timer, so there
  // is exactly one code path that takes a sample.
  useEffect(() => {
    if (!isOpen) return;
    let cancelled = false;
    (async () => {
      const next = await sample().catch(e => {
        console.error('[Metrics] sample failed:', e);
        return null;
      });
      if (cancelled || !next) return;
      const vram = next.gpu.vram_total_gb > 0 ? (next.gpu.vram_used_gb / next.gpu.vram_total_gb) * 100 : 0;
      seriesRef.current = {
        cpu: push(seriesRef.current.cpu, next.cpu.usage_percent),
        memory: push(seriesRef.current.memory, next.memory.usage_percent),
        gpu: push(seriesRef.current.gpu, next.gpu.usage_percent),
        vram: push(seriesRef.current.vram, vram),
      };
      setMetrics(next);
      setHistory(seriesRef.current);
    })();
    return () => {
      cancelled = true;
    };
  }, [isOpen, sample, tick]);

  useEffect(() => {
    if (!isOpen || !autoRefresh) return;
    const id = setInterval(() => setTick(t => t + 1), POLL_MS);
    return () => clearInterval(id);
  }, [isOpen, autoRefresh]);

  // History is per-visit: leaving the page and returning must not stitch a gap
  // into the middle of a line as though the samples were continuous.
  //
  // Discarded in this effect's CLEANUP rather than in a body that checks
  // `!isOpen`. Two reasons: the cleanup is the precise moment the page closes,
  // and clearing from an effect body is a synchronous setState during render
  // commit — the cascading-render pattern the hooks lint rule exists to catch.
  useEffect(() => {
    if (!isOpen) return;
    return () => {
      seriesRef.current = EMPTY_SERIES;
      setHistory(EMPTY_SERIES);
    };
  }, [isOpen]);

  const vramPercent = useMemo(
    () => (metrics?.gpu.vram_total_gb ? (metrics.gpu.vram_used_gb / metrics.gpu.vram_total_gb) * 100 : 0),
    [metrics],
  );

  if (!isOpen) return null;

  const device = metrics?.inference.device ?? 'CPU';
  const deviceTone = device === 'GPU' ? 'success' : device === 'CPU+GPU' ? 'accent' : undefined;

  return (
    <div className={embedded ? 'metrics is-embedded' : 'metrics'}>
      <div className="metrics-header">
        {!embedded && (
          <button type="button" className="ui-iconbtn" onClick={onClose} title="Back" aria-label="Back">
            <ArrowLeft size={17} />
          </button>
        )}
        <div className="metrics-header__titles">
          {embedded ? (
            <h2 className="metrics-title">
              <Activity size={17} strokeWidth={1.5} />
              System
            </h2>
          ) : (
            <h1 className="metrics-title">
              <Activity size={19} strokeWidth={1.5} />
              System
            </h1>
          )}
          <p className="metrics-subtitle">
            {autoRefresh
              ? `Sampling every ${POLL_MS / 1000}s · ${history.cpu.length}/${HISTORY_LEN} samples held`
              : 'Live sampling paused'}
          </p>
        </div>
        <div className="metrics-header__actions">
          <button
            type="button"
            className={`metrics-live${autoRefresh ? ' is-live' : ''}`}
            onClick={() => setAutoRefresh(v => !v)}
            aria-pressed={autoRefresh}
            title={autoRefresh ? 'Pause live sampling' : 'Resume live sampling'}
          >
            <span className="metrics-live__dot" />
            {autoRefresh ? 'Live' : 'Paused'}
          </button>
          <button
            type="button"
            className="ui-iconbtn"
            onClick={() => setTick(t => t + 1)}
            title="Take a sample now"
            aria-label="Refresh"
          >
            <RefreshCw size={15} />
          </button>
        </div>
      </div>

      <div className="metrics-body">
        {!metrics ? (
          <div className="metrics-grid">
            {Array.from({ length: 4 }).map((_, i) => (
              <div key={i} className="ui-skeleton metrics-skeleton" />
            ))}
          </div>
        ) : (
          <div className="metrics-grid">
            {/* ── Inference configuration: the most consequential thing on this
                page, because it explains the speed the user is experiencing. */}
            <div className="metric-card metrics-grid__full">
              <div className="metric-card__head">
                <Zap size={16} strokeWidth={1.75} />
                <span className="metric-card__title">Inference</span>
                <span className={`ui-badge${deviceTone ? ` ui-badge--${deviceTone}` : ''}`}>{device}</span>
              </div>
              <p className="metric-card__note">
                {device === 'GPU' &&
                  `Every layer is on the GPU (${metrics.inference.gpu_layers} offloaded). This is the fastest configuration available on this machine.`}
                {device === 'CPU+GPU' &&
                  `${metrics.inference.gpu_layers} layers are on the GPU and the rest run on the CPU. Generation speed is bounded by the CPU portion — a smaller quantisation would let more layers fit in VRAM.`}
                {device === 'CPU' && !metrics.gpu.available &&
                  'No GPU was detected, so all layers run on the CPU. A smaller model or a lower quantisation will respond noticeably faster.'}
                {device === 'CPU' && metrics.gpu.available &&
                  'A GPU is present but no layers are offloaded to it. Either the model did not fit in available VRAM, or GPU_LAYERS is pinned to 0 in configuration.'}
              </p>
              <div className="metric-facts">
                <div className="metric-fact">
                  <span className="metric-fact__key">Layers on GPU</span>
                  <span className="metric-fact__val">{metrics.inference.gpu_layers}</span>
                </div>
                <div className="metric-fact">
                  <span className="metric-fact__key">VRAM headroom</span>
                  <span className="metric-fact__val">
                    {metrics.gpu.available
                      ? `${Math.max(metrics.gpu.vram_total_gb - metrics.gpu.vram_used_gb, 0).toFixed(1)} GB free`
                      : '—'}
                  </span>
                </div>
                <div className="metric-fact">
                  <span className="metric-fact__key">RAM headroom</span>
                  <span className="metric-fact__val">{metrics.memory.available_gb.toFixed(1)} GB free</span>
                </div>
                <div className="metric-fact">
                  <span className="metric-fact__key">Threads available</span>
                  <span className="metric-fact__val">{metrics.cpu.cores}</span>
                </div>
              </div>
            </div>

            {/* ── Headline tiles ─────────────────────────────────────────── */}
            <StatTile
              label="CPU"
              icon={<Cpu size={14} strokeWidth={1.75} />}
              value={metrics.cpu.usage_percent.toFixed(0)}
              unit="%"
              detail={`${metrics.cpu.cores} cores${metrics.cpu.frequency_mhz > 0 ? ` · ${(metrics.cpu.frequency_mhz / 1000).toFixed(1)} GHz` : ''}`}
              history={history.cpu}
              meter={metrics.cpu.usage_percent}
              tone={severity(metrics.cpu.usage_percent, 80, 92)}
            />

            <StatTile
              label="Memory"
              icon={<MemoryStick size={14} strokeWidth={1.75} />}
              value={metrics.memory.used_gb.toFixed(1)}
              unit={`/ ${metrics.memory.total_gb.toFixed(0)} GB`}
              detail={`${metrics.memory.available_gb.toFixed(1)} GB available`}
              history={history.memory}
              meter={metrics.memory.usage_percent}
              tone={severity(metrics.memory.usage_percent, 80, 92)}
            />

            {metrics.gpu.available ? (
              <>
                <StatTile
                  label="GPU"
                  icon={<Activity size={14} strokeWidth={1.75} />}
                  value={metrics.gpu.usage_percent.toFixed(0)}
                  unit="%"
                  detail={metrics.gpu.name}
                  history={history.gpu}
                  meter={metrics.gpu.usage_percent}
                  tone={severity(metrics.gpu.usage_percent, 85, 96)}
                />
                <StatTile
                  label="VRAM"
                  icon={<HardDrive size={14} strokeWidth={1.75} />}
                  value={metrics.gpu.vram_used_gb.toFixed(1)}
                  unit={`/ ${metrics.gpu.vram_total_gb.toFixed(1)} GB`}
                  detail={
                    metrics.gpu.temperature_c > 0
                      ? `${metrics.gpu.temperature_c.toFixed(0)}°C`
                      : `${vramPercent.toFixed(0)}% in use`
                  }
                  history={history.vram}
                  meter={vramPercent}
                  // VRAM crosses into trouble earlier than other resources: an
                  // exhausted VRAM pool does not degrade, it fails the load.
                  tone={severity(vramPercent, 82, 93)}
                />
              </>
            ) : (
              <div className="metric-card metrics-grid__full">
                <div className="metric-card__head">
                  <Activity size={16} strokeWidth={1.75} />
                  <span className="metric-card__title">GPU</span>
                  <span className="ui-badge">Not detected</span>
                </div>
                <p className="metric-card__note">
                  No compatible GPU was found, so inference runs entirely on the CPU. That is fully
                  supported — expect slower generation, and prefer a smaller model or a lower
                  quantisation for a responsive experience.
                </p>
              </div>
            )}

            {/* ── Per-core load ──────────────────────────────────────────── */}
            {metrics.cpu.per_core_usage.length > 0 && (
              <div className="metric-card metrics-grid__full">
                <div className="metric-card__head">
                  <Cpu size={16} strokeWidth={1.75} />
                  <span className="metric-card__title">Per-core load</span>
                  <span className="ui-badge">{metrics.cpu.model_name}</span>
                </div>
                <div className="core-grid">
                  {metrics.cpu.per_core_usage.map((load, i) => (
                    <div
                      key={i}
                      className={`core-cell${load >= 90 ? ' is-max' : load >= 70 ? ' is-hot' : ''}`}
                      style={{ '--load': Math.round(load) } as React.CSSProperties}
                      title={`Core ${i}: ${load.toFixed(0)}%`}
                    >
                      <span className="core-cell__fill" />
                    </div>
                  ))}
                </div>
                <p className="metric-card__note">
                  Cores filling evenly is what a healthy CPU inference run looks like. A single
                  saturated core usually means the thread count is set below the core count.
                </p>
              </div>
            )}

            {/* ── Storage ────────────────────────────────────────────────── */}
            <div className="metric-card metrics-grid__full">
              <div className="metric-card__head">
                <HardDrive size={16} strokeWidth={1.75} />
                <span className="metric-card__title">Storage</span>
              </div>
              <div
                className="ui-meter"
                style={
                  {
                    '--progress': metrics.storage.total_gb > 0 ? (metrics.storage.used_gb / metrics.storage.total_gb) * 100 : 0,
                  } as React.CSSProperties
                }
              >
                <div className="ui-meter__fill" />
              </div>
              <div className="metric-facts">
                <div className="metric-fact">
                  <span className="metric-fact__key">Used</span>
                  <span className="metric-fact__val">{metrics.storage.used_gb.toFixed(1)} GB</span>
                </div>
                <div className="metric-fact">
                  <span className="metric-fact__key">Free</span>
                  <span className="metric-fact__val">{metrics.storage.available_gb.toFixed(1)} GB</span>
                </div>
                <div className="metric-fact">
                  <span className="metric-fact__key">Total</span>
                  <span className="metric-fact__val">{metrics.storage.total_gb.toFixed(1)} GB</span>
                </div>
              </div>
            </div>

            {metrics.gpu.available && metrics.gpu.temperature_c > 85 && (
              <div className="metric-card metrics-grid__full">
                <div className="metric-card__head">
                  <Thermometer size={16} strokeWidth={1.75} style={{ color: 'var(--danger-fg)' }} />
                  <span className="metric-card__title">GPU running hot</span>
                  <span className="ui-badge ui-badge--danger">{metrics.gpu.temperature_c.toFixed(0)}°C</span>
                </div>
                <p className="metric-card__note">
                  Sustained temperatures at this level usually mean the GPU is thermally throttling,
                  which shows up as generation speed dropping partway through a long answer.
                </p>
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
};

export default MetricsPanel;
