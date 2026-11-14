-- Child-side indexes for foreign keys on translation_revisions: without them every deleted
-- revision scans all of search_entries and translation_jobs (document delete, cache purge).
CREATE INDEX IF NOT EXISTS search_entries_revision ON search_entries(revision_id) WHERE revision_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS jobs_result_revision ON translation_jobs(result_revision_id) WHERE result_revision_id IS NOT NULL;
