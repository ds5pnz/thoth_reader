-- 0001_initial.sql: core entities (spec §9.1). Never edit after release.
CREATE TABLE documents (
  id TEXT PRIMARY KEY,
  sha256 TEXT NOT NULL UNIQUE,
  format TEXT NOT NULL CHECK(format IN ('pdf','epub')),
  title TEXT NOT NULL,
  managed_path TEXT NOT NULL UNIQUE,
  byte_size INTEGER NOT NULL CHECK(byte_size >= 0),
  state TEXT NOT NULL CHECK(state IN
    ('importing','indexing','ready','partial','failed')),
  active_extraction_id TEXT REFERENCES extractions(id) ON DELETE SET NULL,
  metadata_json TEXT NOT NULL CHECK(json_valid(metadata_json)),
  created_at INTEGER NOT NULL,
  last_opened_at INTEGER
);
CREATE TABLE extractions (
  id TEXT PRIMARY KEY,
  document_id TEXT NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  config_hash TEXT NOT NULL,
  versions_json TEXT NOT NULL CHECK(json_valid(versions_json)),
  state TEXT NOT NULL CHECK(state IN ('staging','ready','partial','failed')),
  created_at INTEGER NOT NULL,
  UNIQUE(document_id, config_hash),
  UNIQUE(id, document_id)
);
CREATE TABLE sections (
  id TEXT PRIMARY KEY,
  extraction_id TEXT NOT NULL REFERENCES extractions(id) ON DELETE CASCADE,
  ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
  locator_json TEXT NOT NULL CHECK(json_valid(locator_json)),
  state TEXT NOT NULL CHECK(state IN ('pending','staging','ready','failed')),
  checksum TEXT,
  block_count INTEGER NOT NULL DEFAULT 0 CHECK(block_count >= 0),
  UNIQUE(extraction_id, ordinal),
  UNIQUE(id, extraction_id)
);
CREATE TABLE blocks (
  id TEXT PRIMARY KEY,
  extraction_id TEXT NOT NULL,
  section_id TEXT NOT NULL,
  parent_id TEXT,
  ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
  kind TEXT NOT NULL CHECK(kind IN ('heading','paragraph','list_item','quote',
    'code','table','figure','caption','formula','unknown')),
  raw_text TEXT NOT NULL,
  source_text TEXT NOT NULL,
  source_hash TEXT NOT NULL,
  content_json TEXT NOT NULL CHECK(json_valid(content_json)),
  location_json TEXT NOT NULL CHECK(json_valid(location_json)),
  mapping_json TEXT NOT NULL CHECK(json_valid(mapping_json)),
  quality_json TEXT NOT NULL CHECK(json_valid(quality_json)),
  translation_policy TEXT NOT NULL CHECK(translation_policy IN
    ('auto','manual_only','preserve')),
  FOREIGN KEY(extraction_id) REFERENCES extractions(id) ON DELETE CASCADE,
  FOREIGN KEY(section_id, extraction_id)
    REFERENCES sections(id, extraction_id) ON DELETE CASCADE,
  FOREIGN KEY(parent_id, extraction_id)
    REFERENCES blocks(id, extraction_id),
  UNIQUE(id, extraction_id),
  UNIQUE(section_id, ordinal)
);
CREATE TABLE extraction_batches (
  extraction_id TEXT NOT NULL REFERENCES extractions(id) ON DELETE CASCADE,
  section_id TEXT NOT NULL REFERENCES sections(id) ON DELETE CASCADE,
  batch_index INTEGER NOT NULL CHECK(batch_index >= 0),
  checksum TEXT NOT NULL,
  payload_json TEXT NOT NULL CHECK(json_valid(payload_json)),
  PRIMARY KEY(section_id, batch_index)
);
CREATE TABLE block_preferences (
  block_id TEXT PRIMARY KEY REFERENCES blocks(id) ON DELETE CASCADE,
  translation_override TEXT CHECK(translation_override IN
    ('auto','manual_only','preserve')),
  version INTEGER NOT NULL DEFAULT 0,
  updated_at INTEGER NOT NULL
);
-- Provider 설정과 비밀정보 참조만 저장하며 API 키 원문은 저장하지 않는다.
CREATE TABLE providers (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL CHECK(kind IN ('ollama','openai_compatible')),
  label TEXT NOT NULL,
  endpoint TEXT NOT NULL,
  account_namespace TEXT NOT NULL,
  keychain_ref TEXT,
  capabilities_json TEXT NOT NULL CHECK(json_valid(capabilities_json)),
  enabled INTEGER NOT NULL DEFAULT 0 CHECK(enabled IN (0,1)),
  cache_epoch INTEGER NOT NULL DEFAULT 0 CHECK(cache_epoch >= 0),
  created_at INTEGER NOT NULL
);
CREATE TABLE translation_profiles (
  id TEXT PRIMARY KEY,
  provider_id TEXT NOT NULL REFERENCES providers(id),
  profile_hash TEXT NOT NULL UNIQUE,
  source_language TEXT NOT NULL,
  target_language TEXT NOT NULL,
  model_id TEXT NOT NULL,
  snapshot_json TEXT NOT NULL CHECK(json_valid(snapshot_json)),
  created_at INTEGER NOT NULL
);
CREATE TABLE translation_revisions (
  id TEXT PRIMARY KEY,
  block_id TEXT NOT NULL REFERENCES blocks(id) ON DELETE CASCADE,
  target_language TEXT NOT NULL,
  source_hash TEXT NOT NULL,
  origin TEXT NOT NULL CHECK(origin IN ('generated','manual','restore')),
  profile_id TEXT REFERENCES translation_profiles(id),
  parent_revision_id TEXT,
  cache_key TEXT,
  text TEXT NOT NULL,
  metadata_json TEXT NOT NULL CHECK(json_valid(metadata_json)),
  created_at INTEGER NOT NULL,
  CHECK(origin <> 'generated' OR (profile_id IS NOT NULL AND cache_key IS NOT NULL)),
  UNIQUE(id, block_id, target_language),
  FOREIGN KEY(parent_revision_id, block_id, target_language)
    REFERENCES translation_revisions(id, block_id, target_language)
);
CREATE TABLE translation_heads (
  block_id TEXT NOT NULL REFERENCES blocks(id) ON DELETE CASCADE,
  target_language TEXT NOT NULL,
  manual_revision_id TEXT,
  generated_revision_id TEXT,
  version INTEGER NOT NULL DEFAULT 0 CHECK(version >= 0),
  updated_at INTEGER NOT NULL,
  PRIMARY KEY(block_id, target_language),
  FOREIGN KEY(manual_revision_id, block_id, target_language)
    REFERENCES translation_revisions(id, block_id, target_language),
  FOREIGN KEY(generated_revision_id, block_id, target_language)
    REFERENCES translation_revisions(id, block_id, target_language)
);
CREATE TABLE translation_cache (
  cache_key TEXT PRIMARY KEY,
  block_id TEXT NOT NULL REFERENCES blocks(id) ON DELETE CASCADE,
  target_language TEXT NOT NULL,
  revision_id TEXT NOT NULL,
  descriptor_json TEXT NOT NULL CHECK(json_valid(descriptor_json)),
  created_at INTEGER NOT NULL,
  last_accessed_at INTEGER NOT NULL,
  FOREIGN KEY(revision_id, block_id, target_language)
    REFERENCES translation_revisions(id, block_id, target_language)
);
CREATE TABLE translation_jobs (
  id TEXT PRIMARY KEY,
  block_id TEXT NOT NULL REFERENCES blocks(id) ON DELETE CASCADE,
  profile_id TEXT NOT NULL REFERENCES translation_profiles(id),
  cache_key TEXT NOT NULL,
  request_id TEXT NOT NULL UNIQUE,
  session_id TEXT NOT NULL,
  generation INTEGER NOT NULL CHECK(generation >= 0),
  priority INTEGER NOT NULL CHECK(priority BETWEEN 0 AND 2),
  state TEXT NOT NULL CHECK(state IN ('QUEUED','RUNNING','STREAMING',
    'RETRY_WAIT','COMPLETED','FAILED','CANCELLED')),
  attempt_count INTEGER NOT NULL DEFAULT 0 CHECK(attempt_count >= 0),
  last_part_index INTEGER NOT NULL DEFAULT 0 CHECK(last_part_index >= 0),
  next_attempt_at INTEGER,
  request_snapshot_json TEXT NOT NULL CHECK(json_valid(request_snapshot_json)),
  result_revision_id TEXT REFERENCES translation_revisions(id) ON DELETE SET NULL,
  error_code TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX jobs_one_active_key ON translation_jobs(cache_key)
  WHERE state IN ('QUEUED','RUNNING','STREAMING','RETRY_WAIT');
CREATE INDEX jobs_schedule ON translation_jobs(state, priority, next_attempt_at);
CREATE INDEX revisions_history ON translation_revisions
  (block_id, target_language, created_at DESC, id);
CREATE INDEX blocks_reading_order ON blocks(extraction_id, section_id, ordinal);
CREATE TABLE reading_positions (
  document_id TEXT PRIMARY KEY REFERENCES documents(id) ON DELETE CASCADE,
  anchor_json TEXT NOT NULL CHECK(json_valid(anchor_json)),
  view_json TEXT NOT NULL CHECK(json_valid(view_json)),
  version INTEGER NOT NULL CHECK(version >= 0),
  updated_at INTEGER NOT NULL
);
CREATE TABLE bookmarks (
  id TEXT PRIMARY KEY,
  document_id TEXT NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  anchor_json TEXT NOT NULL CHECK(json_valid(anchor_json)),
  label TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE TABLE file_operations (
  id TEXT PRIMARY KEY,
  document_id TEXT NOT NULL,
  kind TEXT NOT NULL CHECK(kind IN ('import','delete')),
  state TEXT NOT NULL CHECK(state IN
    ('planned','db_committed','files_applied','complete','failed')),
  manifest_json TEXT NOT NULL CHECK(json_valid(manifest_json)),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE TABLE settings (
  key TEXT PRIMARY KEY,
  value_json TEXT NOT NULL CHECK(json_valid(value_json)),
  version INTEGER NOT NULL DEFAULT 0,
  updated_at INTEGER NOT NULL
);
CREATE TABLE ipc_receipts (
  request_id TEXT PRIMARY KEY,
  command TEXT NOT NULL,
  payload_hash TEXT NOT NULL,
  result_json TEXT NOT NULL CHECK(json_valid(result_json)),
  created_at INTEGER NOT NULL
);
CREATE TABLE search_entries (
  id INTEGER PRIMARY KEY,
  document_id TEXT NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  block_id TEXT NOT NULL REFERENCES blocks(id) ON DELETE CASCADE,
  scope TEXT NOT NULL CHECK(scope IN ('source','translation')),
  language TEXT NOT NULL,
  revision_id TEXT REFERENCES translation_revisions(id),
  text TEXT NOT NULL,
  UNIQUE(block_id, scope, language)
);
