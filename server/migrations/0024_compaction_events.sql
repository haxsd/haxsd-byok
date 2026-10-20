-- 每一次提交成功的上下文压缩留一条记录。
--
-- 压缩此前只写结构化日志：用户看到"模型忘了前面的事"却没有任何界面线索，排查也只能翻
-- 日志。一行记录就够回答界面需要的三个问题：这次压缩发生在哪个 run、压掉了多少条、
-- 归档在哪。按 run 查询（调用详情）与按会话倒序（概览）各一条索引。
CREATE TABLE IF NOT EXISTS compaction_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    checkpoint_id INTEGER NOT NULL,
    trigger TEXT NOT NULL,
    retention TEXT NOT NULL,
    estimated_tokens INTEGER,
    calibration REAL,
    compactable_messages INTEGER NOT NULL,
    replacement_messages INTEGER NOT NULL,
    summary_chars INTEGER NOT NULL,
    archive_path TEXT,
    created_at_ms INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS compaction_events_run ON compaction_events(run_id);
CREATE INDEX IF NOT EXISTS compaction_events_conversation
    ON compaction_events(conversation_id, created_at_ms);
