-- Undo chunk sharing: give every data row its own copy of its bytes, the layout
-- builds before dedup read (they fail with EIO on shared chunks). Needs
-- unchunk() for compressed chunks, so run it through the wrapper, unmounted:
--   python3 scripts/undo-dedup.py fs.sqlite
-- The chunk_gc_update trigger deletes each chunk as its last reference goes, so
-- the chunk table ends up empty. The database grows by every duplicate copy and
-- by the compression it loses.
UPDATE data
SET data = (SELECT unchunk(chunk.data, chunk.compressed) FROM chunk
            WHERE chunk.hash = data.chunk_hash),
    chunk_hash = NULL
WHERE chunk_hash IS NOT NULL;
