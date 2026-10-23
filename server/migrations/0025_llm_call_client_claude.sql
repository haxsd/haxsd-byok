-- `client_source` 的取值集合是建表时的 CHECK 约束（迁移 0013），SQLite 不能原地修改
-- CHECK，所以按整表重建放开 `claude`：Claude Code 经入站 Anthropic 网关发起的每次
-- 推理都要在调用记录里归属到它自己的来源，不能混进 internal 或 cursor。
--
-- 重建顺序与 0004 / 0007 一致：先建三张新表（子表指向新父表），拷数据，再删旧表、
-- 换名、补索引。`defer_foreign_keys` 让删除旧父表时不必逐行校验子表引用。
PRAGMA defer_foreign_keys = ON;

CREATE TABLE llm_calls_new (
    call_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    provider_call_index INTEGER NOT NULL,
    model_hash TEXT,
    provider_type TEXT NOT NULL,
    provider_url TEXT NOT NULL,
    request_type TEXT NOT NULL,
    request_url TEXT NOT NULL,
    model_id TEXT NOT NULL,
    display_name TEXT NOT NULL,
    status TEXT NOT NULL,
    finish_reason TEXT,
    created_at_ms INTEGER NOT NULL,
    request_started_at_ms INTEGER,
    response_headers_at_ms INTEGER,
    first_event_at_ms INTEGER,
    first_text_at_ms INTEGER,
    finished_at_ms INTEGER,
    queue_ms INTEGER,
    ttfb_ms INTEGER,
    ttft_ms INTEGER,
    duration_ms INTEGER,
    input_tokens INTEGER,
    output_tokens INTEGER,
    total_tokens INTEGER,
    cache_read_tokens INTEGER,
    cache_write_tokens INTEGER,
    reasoning_tokens INTEGER,
    usage_json TEXT,
    message_count INTEGER NOT NULL,
    tool_count INTEGER NOT NULL,
    request_bytes INTEGER,
    response_bytes INTEGER NOT NULL DEFAULT 0,
    stream_event_count INTEGER NOT NULL DEFAULT 0,
    http_status INTEGER,
    error_kind TEXT,
    error_message TEXT,
    detailed INTEGER NOT NULL,
    reasoning_effort TEXT,
    fast INTEGER NOT NULL DEFAULT 0 CHECK (fast IN (0, 1)),
    first_valid_response_at_ms INTEGER,
    ttfr_ms INTEGER,
    client_source TEXT NOT NULL DEFAULT 'internal'
        CHECK (client_source IN ('cursor', 'devin', 'codex', 'claude', 'internal')),
    purpose TEXT NOT NULL DEFAULT 'chat'
);

INSERT INTO llm_calls_new (
    call_id, run_id, conversation_id, provider_call_index, model_hash, provider_type,
    provider_url, request_type, request_url, model_id, display_name, status, finish_reason,
    created_at_ms, request_started_at_ms, response_headers_at_ms, first_event_at_ms,
    first_text_at_ms, finished_at_ms, queue_ms, ttfb_ms, ttft_ms, duration_ms,
    input_tokens, output_tokens, total_tokens, cache_read_tokens, cache_write_tokens,
    reasoning_tokens, usage_json, message_count, tool_count, request_bytes, response_bytes,
    stream_event_count, http_status, error_kind, error_message, detailed, reasoning_effort,
    fast, first_valid_response_at_ms, ttfr_ms, client_source, purpose
)
SELECT
    call_id, run_id, conversation_id, provider_call_index, model_hash, provider_type,
    provider_url, request_type, request_url, model_id, display_name, status, finish_reason,
    created_at_ms, request_started_at_ms, response_headers_at_ms, first_event_at_ms,
    first_text_at_ms, finished_at_ms, queue_ms, ttfb_ms, ttft_ms, duration_ms,
    input_tokens, output_tokens, total_tokens, cache_read_tokens, cache_write_tokens,
    reasoning_tokens, usage_json, message_count, tool_count, request_bytes, response_bytes,
    stream_event_count, http_status, error_kind, error_message, detailed, reasoning_effort,
    fast, first_valid_response_at_ms, ttfr_ms, client_source, purpose
FROM llm_calls;

CREATE TABLE llm_call_requests_new (
    call_id TEXT PRIMARY KEY,
    headers_json TEXT NOT NULL,
    body_json TEXT NOT NULL,
    byte_count INTEGER NOT NULL,
    FOREIGN KEY(call_id) REFERENCES llm_calls_new(call_id) ON DELETE CASCADE
);

INSERT INTO llm_call_requests_new(call_id, headers_json, body_json, byte_count)
SELECT call_id, headers_json, body_json, byte_count FROM llm_call_requests;

CREATE TABLE llm_call_response_chunks_new (
    call_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    received_offset_ms INTEGER NOT NULL,
    data BLOB NOT NULL,
    byte_count INTEGER NOT NULL,
    PRIMARY KEY(call_id, seq),
    FOREIGN KEY(call_id) REFERENCES llm_calls_new(call_id) ON DELETE CASCADE
);

INSERT INTO llm_call_response_chunks_new(call_id, seq, received_offset_ms, data, byte_count)
SELECT call_id, seq, received_offset_ms, data, byte_count FROM llm_call_response_chunks;

DROP TABLE llm_call_requests;
DROP TABLE llm_call_response_chunks;
DROP TABLE llm_calls;

ALTER TABLE llm_calls_new RENAME TO llm_calls;
ALTER TABLE llm_call_requests_new RENAME TO llm_call_requests;
ALTER TABLE llm_call_response_chunks_new RENAME TO llm_call_response_chunks;

CREATE INDEX llm_calls_created ON llm_calls(created_at_ms DESC);
CREATE INDEX llm_calls_run ON llm_calls(run_id, provider_call_index);
CREATE INDEX llm_calls_model ON llm_calls(model_hash, created_at_ms DESC);
CREATE INDEX llm_calls_client_source ON llm_calls(client_source);
CREATE INDEX llm_calls_conversation_created
ON llm_calls(conversation_id, created_at_ms DESC);
