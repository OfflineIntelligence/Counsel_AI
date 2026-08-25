/**
 * API Keys Management Service
 *
 * Handles all API key operations with the backend:
 * - Save/retrieve HuggingFace keys
 * - Persist keys in encrypted database
 */

import { getApiBaseSync } from './backendUrl';
import { getHfToken } from './hfToken';

export type ApiKeyType = 'huggingface';

export interface ApiKeyResponse {
  key_type: string;
  value: string | null;
  created_at?: string;
  last_used_at?: string;
  last_mode?: string;
  usage_count?: number;
}

export interface SaveApiKeyRequest {
  key_type: ApiKeyType;
  value: string;
}

/**
 * Save or update an API key in the database
 */
export async function saveApiKey(keyType: ApiKeyType, value: string): Promise<boolean> {
  try {
    const response = await fetch(`${getApiBaseSync()}/api-keys`, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
      },
      body: JSON.stringify({
        key_type: keyType,
        value: value,
      }),
    });

    if (!response.ok) {
      console.error(`Failed to save ${keyType} key:`, response.statusText);
      return false;
    }

    const data = await response.json();
    return data.success === true;
  } catch (error) {
    console.error(`Error saving ${keyType} key:`, error);
    return false;
  }
}

/**
 * Retrieve an API key from the database (decrypted)
 */
export async function getApiKey(keyType: ApiKeyType): Promise<string | null> {
  try {
    const response = await fetch(
      `${getApiBaseSync()}/api-keys?key_type=${keyType}`
    );

    if (!response.ok) {
      console.error(`Failed to get ${keyType} key:`, response.statusText);
      return null;
    }

    const data: ApiKeyResponse = await response.json();
    return data.value || null;
  } catch (error) {
    console.error(`Error getting ${keyType} key:`, error);
    return null;
  }
}

/**
 * Delete an API key from the database
 */
export async function deleteApiKey(keyType: ApiKeyType): Promise<boolean> {
  try {
    const response = await fetch(
      `${getApiBaseSync()}/api-keys?key_type=${keyType}`,
      {
        method: 'DELETE',
      }
    );

    if (!response.ok) {
      console.error(`Failed to delete ${keyType} key:`, response.statusText);
      return false;
    }

    return true;
  } catch (error) {
    console.error(`Error deleting ${keyType} key:`, error);
    return false;
  }
}

/**
 * Mark an API key as used in a specific mode
 */
export async function markApiKeyUsed(
  keyType: ApiKeyType,
  mode: 'online' | 'offline'
): Promise<void> {
  try {
    await fetch(`${getApiBaseSync()}/api-keys/mark-used`, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
      },
      body: JSON.stringify({
        key_type: keyType,
        mode: mode,
      }),
    });
  } catch (error) {
    console.error(`Error marking ${keyType} key as used:`, error);
  }
}

/**
 * Get all API keys (useful for settings page)
 */
export async function getAllApiKeys(): Promise<ApiKeyResponse[]> {
  try {
    const response = await fetch(`${getApiBaseSync()}/api-keys/all`);

    if (!response.ok) {
      console.error('Failed to get all API keys:', response.statusText);
      return [];
    }

    const data = await response.json();
    return data.keys || [];
  } catch (error) {
    console.error('Error getting all API keys:', error);
    return [];
  }
}

/**
 * Migrate keys from localStorage to backend (one-time migration)
 */
export async function migrateKeysFromLocalStorage(): Promise<void> {
  // Check for HuggingFace token (getHfToken also folds any legacy-key value
  // into the canonical browser-storage key before reading)
  const hfToken = getHfToken();
  if (hfToken) {
    const saved = await saveApiKey('huggingface', hfToken);
    if (saved) {
      console.log('✅ Migrated HuggingFace token to backend');
    }
  }
}
