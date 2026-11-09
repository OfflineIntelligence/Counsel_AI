-- Prompt replay and prefix-indexed KV states.
--
-- message_prompts: for each turn, the exact text the engine saw (the user's
-- question with every block folded onto it) and the exact text it generated
-- (before the citation note and echo stripping). Replaying these as history
-- keeps each prompt an exact extension of the last one, so the engine's saved
-- KV state for the previous turn stays valid and only the new turn is computed.
CREATE TABLE IF NOT EXISTS message_prompts (
  session_id     TEXT    NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  message_index  INTEGER NOT NULL,
  role           TEXT    NOT NULL,
  prompt_content TEXT    NOT NULL,
  PRIMARY KEY (session_id, message_index)
);

-- kv_store_prefixes: every message prefix a saved KV state contains, so a
-- prompt that shares only its first messages with a saved state (the system
-- prompt of a new conversation, an edited conversation) can still restore it;
-- the engine discards whatever follows the shared tokens. chars is the
-- prompt text length up to that prefix, used to estimate its token count.
CREATE TABLE IF NOT EXISTS kv_store_prefixes (
  prefix_key TEXT    NOT NULL,
  entry_key  TEXT    NOT NULL REFERENCES kv_store_entries(key) ON DELETE CASCADE,
  depth      INTEGER NOT NULL,
  chars      INTEGER NOT NULL,
  PRIMARY KEY (prefix_key, entry_key)
);
CREATE INDEX IF NOT EXISTS idx_kv_store_prefixes_entry ON kv_store_prefixes(entry_key);
