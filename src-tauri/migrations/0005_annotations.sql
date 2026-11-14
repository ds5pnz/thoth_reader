-- Highlights and notes. Anchored by quote rather than by offsets so they survive a
-- re-extraction: `block_id` is the block at creation time, `source_hash` finds the same
-- paragraph again when block ids change, and `quote` locates the words inside it.
CREATE TABLE annotations (
  id TEXT PRIMARY KEY,
  document_id TEXT NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  block_id TEXT NOT NULL,
  source_hash TEXT NOT NULL,
  side TEXT NOT NULL CHECK(side IN ('source','translation')),
  target_language TEXT,
  quote TEXT NOT NULL,
  occurrence INTEGER NOT NULL DEFAULT 0 CHECK(occurrence >= 0),
  color TEXT NOT NULL CHECK(color IN ('yellow','green','blue','pink')),
  note TEXT NOT NULL DEFAULT '',
  location_json TEXT NOT NULL CHECK(json_valid(location_json)),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE INDEX annotations_doc ON annotations(document_id, created_at);
