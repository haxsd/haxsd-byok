-- Blob deletion checks this foreign key once per blob. Without the index it
-- scans every captured artifact, making background collection block writes.
CREATE INDEX cursor_run_trace_artifacts_blob
ON cursor_run_trace_artifacts(blob_id);
