-- A promoted payload belongs to its canonical references after its upload
-- ledger is retired. Release it only when the last reference disappears and
-- no staged writer still owns that physical key. Native SQLite only: D1 has
-- its own bounded transfer_gc queue.
CREATE TRIGGER transfer_native_release_blob_delete
AFTER DELETE ON transfer_object_refs
WHEN OLD.backend='d1' AND OLD.blob_key GLOB '.narou-transfer/data/*'
BEGIN
    DELETE FROM objects
    WHERE object_key=OLD.blob_key
      AND NOT EXISTS(SELECT 1 FROM transfer_object_refs WHERE blob_key=OLD.blob_key)
      AND NOT EXISTS(SELECT 1 FROM transfer_stage_ledger
                     WHERE stage_id=substr(OLD.blob_key,length('.narou-transfer/data/')+1));
END;

CREATE TRIGGER transfer_native_release_blob_update
AFTER UPDATE OF blob_key,backend ON transfer_object_refs
WHEN OLD.backend='d1' AND OLD.blob_key GLOB '.narou-transfer/data/*'
 AND (OLD.blob_key<>NEW.blob_key OR OLD.backend<>NEW.backend)
BEGIN
    DELETE FROM objects
    WHERE object_key=OLD.blob_key
      AND NOT EXISTS(SELECT 1 FROM transfer_object_refs WHERE blob_key=OLD.blob_key)
      AND NOT EXISTS(SELECT 1 FROM transfer_stage_ledger
                     WHERE stage_id=substr(OLD.blob_key,length('.narou-transfer/data/')+1));
END;
