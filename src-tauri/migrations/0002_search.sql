-- 0002_search.sql: FTS5 trigram search index (spec §9.1). Never edit after release.
CREATE VIRTUAL TABLE search_fts USING fts5(
  text, content='search_entries', content_rowid='id', tokenize='trigram'
);
CREATE TRIGGER search_ai AFTER INSERT ON search_entries BEGIN
  INSERT INTO search_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER search_ad AFTER DELETE ON search_entries BEGIN
  INSERT INTO search_fts(search_fts, rowid, text)
    VALUES ('delete', old.id, old.text);
END;
CREATE TRIGGER search_au AFTER UPDATE ON search_entries BEGIN
  INSERT INTO search_fts(search_fts, rowid, text)
    VALUES ('delete', old.id, old.text);
  INSERT INTO search_fts(rowid, text) VALUES (new.id, new.text);
END;
INSERT INTO search_fts(search_fts) VALUES('rebuild');
