-- Migration 020: Which message each document was attached to.
--
-- session_documents records that a document belongs to a conversation, but
-- not WHERE in it. That was enough for retrieval, which only ever asks "what
-- can this chat see", and not enough for the chat transcript, which has to
-- show the attachment chips on the turn they were sent with. Without this the
-- chips could only live in frontend memory and vanished on reload.
--
-- Nullable, and null for every row written before this migration: those
-- attachments are known to the conversation but not to any one turn, and the
-- UI shows them on the first user message rather than inventing a position.
ALTER TABLE session_documents ADD COLUMN msg_index INTEGER;

CREATE INDEX IF NOT EXISTS idx_session_documents_msg
  ON session_documents(session_id, msg_index);
