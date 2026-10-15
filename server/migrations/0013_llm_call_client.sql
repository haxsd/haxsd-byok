-- 调用归属显式持久化。无法从历史事实确认的旧记录保留为 internal。
ALTER TABLE llm_calls ADD COLUMN client_source TEXT NOT NULL DEFAULT 'internal'
    CHECK (client_source IN ('cursor', 'devin', 'codex', 'internal'));
UPDATE llm_calls SET client_source = 'devin'
    WHERE call_id LIKE 'devin-call:%' OR call_id LIKE 'devin:%';
UPDATE llm_calls SET client_source = 'cursor'
    WHERE client_source = 'internal' AND (
        EXISTS (SELECT 1 FROM runs WHERE runs.run_id = llm_calls.run_id)
        OR call_id LIKE 'commit-message-%'
    );
