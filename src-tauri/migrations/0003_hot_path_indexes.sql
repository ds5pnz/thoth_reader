-- 0003_hot_path_indexes.sql: indexes for hot lookups found in the real-app self-test.
-- Forward-only; 0001/0002 are never edited.
CREATE INDEX IF NOT EXISTS blocks_parent ON blocks(parent_id) WHERE parent_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS jobs_block ON translation_jobs(block_id, state);
CREATE INDEX IF NOT EXISTS cache_block ON translation_cache(block_id);
CREATE INDEX IF NOT EXISTS search_entries_doc ON search_entries(document_id, scope);
CREATE INDEX IF NOT EXISTS sections_extraction_state ON sections(extraction_id, state);
