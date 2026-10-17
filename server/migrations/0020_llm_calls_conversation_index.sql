-- 每次 run 开头都要问一次「这个会话最近一次带上下文用量的调用」：
--   SELECT usage_json, message_count FROM llm_calls
--    WHERE conversation_id = ? AND json_extract(usage_json, '$.context_input_tokens') IS NOT NULL
--    ORDER BY created_at_ms DESC, rowid DESC LIMIT 1
-- 而 llm_calls 上只有 created_at_ms / model_hash / run / client_source 四组索引，没有
-- conversation_id：查询只能沿 created_at_ms 索引往回走，逐行比对会话号，直到撞上属于本
-- 会话的第一行。这张表按设计永不回收（只被用户手动「清理全部统计」删），所以会话越老、
-- 库里其他会话越多，这一次定位走得越远。
--
-- 组合索引把查找限制在本会话自己的行里，并且 `created_at_ms DESC` 与查询的 ORDER BY 同向，
-- 取最新一行只需要一次定位（不用再排临时 B 树）。json_extract 那个条件无法进索引，但它
-- 现在只在「本会话最近的行」上求值。
CREATE INDEX IF NOT EXISTS llm_calls_conversation_created
    ON llm_calls(conversation_id, created_at_ms DESC);
