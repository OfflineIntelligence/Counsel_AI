// Storage budget: the app's total app-data disk ceiling.
//
// The backend owns the value and the enforcement; this module is only the
// transport. In particular the CHOICES below are presentation, not policy —
// the backend accepts any value at or above its own minimum, so adding an
// option here needs no backend change.

import { getApiBaseSync } from './backendUrl'

/** Where the effective limit came from. Mirrors `LimitSource` in Rust. */
export type LimitSource = 'user_setting' | 'environment'

/** Per-component usage, in bytes. Mirrors `DiskUsage` in Rust. */
export interface DiskUsage {
  models_bytes: number
  engines_bytes: number
  database_bytes: number
  vault_bytes: number
  /**
   * Document workspace drafts and their version history.
   *
   * Protected, like the Vault: never evicted automatically. The only draft
   * data the governor will reclaim is superseded versions — never the current
   * one, and never version 1, which is the file exactly as it was opened.
   */
  drafts_bytes: number
  downloads_bytes: number
  kv_cache_bytes: number
  other_bytes: number
  total_bytes: number
}

export interface StorageSettings {
  limit_mb: number
  source: LimitSource
  default_limit_mb: number
  usage: DiskUsage
  usage_human: string
  limit_human: string
  used_percent: number | null
  over_budget: boolean
}

export interface EvictedItem {
  kind: string
  id: string
  freed_bytes: number
}

export interface EvictionOutcome {
  ran: boolean
  usage_before_bytes: number
  usage_after_bytes: number
  limit_bytes: number
  freed_bytes: number
  evicted: EvictedItem[]
  shortfall_bytes: number
  reason: string
}

export interface UpdateStorageSettingsResult extends StorageSettings {
  eviction: EvictionOutcome
}

/**
 * Selectable budgets.
 *
 * The smallest option is 5 GB rather than something tighter because a usable
 * install is an engine plus at least one model, and offering a value that
 * could never be satisfied would just produce a permanent over-budget warning.
 * The backend independently refuses anything under 1 GB.
 */
export const LIMIT_CHOICES_MB: readonly { label: string; value: number }[] = [
  { label: '5 GB', value: 5 * 1024 },
  { label: '10 GB', value: 10 * 1024 },
  { label: '20 GB', value: 20 * 1024 },
  { label: '50 GB', value: 50 * 1024 },
  { label: '100 GB', value: 100 * 1024 },
  { label: 'Unlimited', value: 0 },
]

export async function fetchStorageSettings(): Promise<StorageSettings> {
  const response = await fetch(`${getApiBaseSync()}/settings/storage`)
  if (!response.ok) {
    throw new Error(`Could not load storage settings (HTTP ${response.status})`)
  }
  return response.json()
}

/**
 * Set the budget. `null` clears the user's choice and restores the shipped
 * default. Eviction runs inside this request, so the returned usage already
 * reflects anything that was removed.
 */
export async function updateStorageLimit(
  limitMb: number | null,
): Promise<UpdateStorageSettingsResult> {
  const response = await fetch(`${getApiBaseSync()}/settings/storage`, {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ limit_mb: limitMb }),
  })
  if (!response.ok) {
    // The backend names its refusals (e.g. limit_too_small); surface that
    // rather than a bare status code, which would tell the user nothing about
    // what to pick instead.
    let detail = `HTTP ${response.status}`
    try {
      const body = await response.json()
      if (body?.detail) detail = body.detail
    } catch {
      /* non-JSON body: keep the status code */
    }
    throw new Error(detail)
  }
  return response.json()
}

/** Byte count for display. Matches the backend's format_bytes output style. */
export function formatBytes(bytes: number): string {
  if (bytes === 0) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  const i = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1)
  return `${(bytes / Math.pow(1024, i)).toFixed(1)} ${units[i]}`
}
