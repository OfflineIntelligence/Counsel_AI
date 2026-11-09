/**
 * Document retrieval (RAG): the embedding model, index progress and which
 * matter a chat retrieves from. Mirrors `api/rag_api.rs`.
 */

import { fetchWithNetworkRetry } from './fetchWithTimeout'
import { base, json } from './http'

export interface IndexTotals {
  documents: number
  documents_chunked: number
  documents_embedded: number
  documents_failed: number
  chunks: number
  chunks_embedded: number
}

export interface EmbeddingDownload {
  state: 'idle' | 'downloading' | 'verifying' | 'paused' | 'failed' | 'installed'
  downloaded_bytes: number
  total_bytes: number
  error: string | null
}

export interface EmbeddingStatus {
  model: { id: string; display_name: string; file_name: string; size_bytes: number }
  installed: boolean
  download: EmbeddingDownload
  server_running: boolean
  index: IndexTotals
}

export async function getEmbeddingStatus(): Promise<EmbeddingStatus> {
  return json<EmbeddingStatus>(await fetchWithNetworkRetry(`${base()}/embeddings/status`))
}

export async function installEmbeddingModel(): Promise<void> {
  await json(await fetch(`${base()}/embeddings/install`, { method: 'POST' }))
}

export async function rebuildIndex(): Promise<void> {
  await json(await fetch(`${base()}/rag/reindex`, { method: 'POST' }))
}

export interface DocumentIndexState {
  document_id: number
  status: 'pending' | 'chunked' | 'embedding' | 'embedded' | 'failed'
  chunk_count?: number
  embedded_count?: number
  error?: string | null
}

export async function getDocumentIndexState(documentId: number): Promise<DocumentIndexState> {
  return json<DocumentIndexState>(await fetchWithNetworkRetry(`${base()}/documents/${documentId}/index`))
}

export interface MatterDocument {
  id: number
  original_filename: string
  char_count: number
  extraction_status: string
  last_referenced_at: string
  index: DocumentIndexState | null
}

export async function listMatterDocuments(matterId: number): Promise<MatterDocument[]> {
  return json<MatterDocument[]>(await fetchWithNetworkRetry(`${base()}/matters/${matterId}/documents`))
}

export async function addMatterDocument(matterId: number, documentId: number): Promise<void> {
  await json(
    await fetch(`${base()}/matters/${matterId}/documents`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ document_id: documentId }),
    }),
  )
}

export async function removeMatterDocument(matterId: number, documentId: number): Promise<void> {
  await json(await fetch(`${base()}/matters/${matterId}/documents/${documentId}`, { method: 'DELETE' }))
}

export interface StoredDocument {
  id: number
  original_filename: string
  char_count: number
  extraction_status: string
  last_referenced_at: string
}

export async function searchDocuments(query: string): Promise<StoredDocument[]> {
  const q = query.trim() ? `?q=${encodeURIComponent(query.trim())}&limit=50&summary=true` : '?limit=50&summary=true'
  return json<StoredDocument[]>(await fetchWithNetworkRetry(`${base()}/documents${q}`))
}

export interface EmbeddingModelEntry {
  id: string
  display_name: string
  description: string
  size_bytes: number
  license: string
  active: boolean
  installed: boolean
  download: EmbeddingDownload
}

export async function listEmbeddingModels(): Promise<{ active: string; models: EmbeddingModelEntry[] }> {
  return json(await fetchWithNetworkRetry(`${base()}/embeddings/models`))
}

async function postJson(path: string, body?: unknown): Promise<void> {
  await json(
    await fetch(`${base()}${path}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body ?? {}),
    }),
  )
}

export const installEmbeddingModelById = (modelId: string) => postJson('/embeddings/install', { model_id: modelId })
export const pauseEmbeddingDownload = () => postJson('/embeddings/pause')
export const cancelEmbeddingDownload = (modelId: string) => postJson('/embeddings/cancel', { model_id: modelId })
export const selectEmbeddingModel = (modelId: string) => postJson('/embeddings/select', { model_id: modelId })

export async function removeEmbeddingModel(modelId: string): Promise<void> {
  const r = await fetch(`${base()}/embeddings/models/${encodeURIComponent(modelId)}`, { method: 'DELETE' })
  if (!r.ok) throw new Error((await r.text()) || `Request failed (${r.status})`)
}
