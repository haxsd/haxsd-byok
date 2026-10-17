-- `chain_message` 的递归 CTE 每一跳都要按 `message_id` 定位消息身份列（05/P2-4 之后不再
-- 解码 payload，但仍然是每跳一次查找）。`checkpoint_messages` 上已有的三个索引没有一个以
-- `message_id` 打头：
--   * `PRIMARY KEY (checkpoint_id, ordinal)`；
--   * `UNIQUE (checkpoint_id, message_id)`——前导列是 checkpoint_id；
--   * `checkpoint_messages_conversation_message (conversation_id, message_id)`——0015 为复合外键
--     建的，前导列是 conversation_id。
-- 所以"只按 message_id 查"用不上任何一个，只能全表扫；而这条表只增不减，扫描量随对话历史
-- 单调增长。补一条以 message_id 打头的索引（链上每一跳都能一次定位）。
CREATE INDEX IF NOT EXISTS checkpoint_messages_message
    ON checkpoint_messages(message_id);
