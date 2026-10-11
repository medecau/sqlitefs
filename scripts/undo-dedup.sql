-- Undo chunk sharing: give every data row its own copy of its bytes, the layout
-- builds before dedup read (they fail with EIO on shared chunks). Unmount first:
--   sqlite3 fs.sqlite < scripts/undo-dedup.sql
-- The chunk_gc_update trigger deletes each chunk as its last reference goes, so
-- the chunk table ends up empty. The database grows by every duplicate copy.
UPDATE data
SET data = (SELECT chunk.data FROM chunk WHERE chunk.hash = data.chunk_hash),
    chunk_hash = NULL
WHERE chunk_hash IS NOT NULL;
