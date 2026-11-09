-- Topic memory (context_engine/topic_memory.rs): each conversation's most
-- frequent terms, clause references, file names and party names, indexed
-- with FTS5 so a question on the same matter finds the conversation.
CREATE TABLE IF NOT EXISTS conversation_topics (
  session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
  terms      TEXT NOT NULL,
  counts     TEXT NOT NULL DEFAULT '{}',
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE VIRTUAL TABLE IF NOT EXISTS conversation_topics_fts USING fts5(
  terms, content='conversation_topics', content_rowid='rowid',
  tokenize='porter unicode61 remove_diacritics 2'
);
CREATE TRIGGER IF NOT EXISTS conversation_topics_ai AFTER INSERT ON conversation_topics BEGIN
  INSERT INTO conversation_topics_fts(rowid, terms) VALUES (new.rowid, new.terms);
END;
CREATE TRIGGER IF NOT EXISTS conversation_topics_ad AFTER DELETE ON conversation_topics BEGIN
  INSERT INTO conversation_topics_fts(conversation_topics_fts, rowid, terms) VALUES ('delete', old.rowid, old.terms);
END;
CREATE TRIGGER IF NOT EXISTS conversation_topics_au AFTER UPDATE ON conversation_topics BEGIN
  INSERT INTO conversation_topics_fts(conversation_topics_fts, rowid, terms) VALUES ('delete', old.rowid, old.terms);
  INSERT INTO conversation_topics_fts(rowid, terms) VALUES (new.rowid, new.terms);
END;
