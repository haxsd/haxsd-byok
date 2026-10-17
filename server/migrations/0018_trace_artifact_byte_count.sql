-- 观测附件的正文存在 blobs 里，`statistics_storage` 过去要 join 回 blobs 逐行
-- SUM(LENGTH(data)) 才能算出"清理详细记录能腾出多少"的估算值——而设置页一挂载就会请求
-- 这个接口，等于每次进设置页都翻一遍全库的附件正文。附件写入时本来就知道正文长度，
-- 把它存成列，估算就与既有的 llm_call_requests.byte_count / llm_call_response_chunks.byte_count
-- 一样只读已存列。
ALTER TABLE cursor_run_trace_artifacts ADD COLUMN byte_count INTEGER NOT NULL DEFAULT 0;

-- 已有附件回填一次：NULL 会被当成"没有正文"，估算值凭空变小。
UPDATE cursor_run_trace_artifacts
SET byte_count = COALESCE(
    (SELECT LENGTH(b.data) FROM blobs b WHERE b.blob_id = cursor_run_trace_artifacts.blob_id),
    0
);
