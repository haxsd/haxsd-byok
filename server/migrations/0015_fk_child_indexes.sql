-- 删除父行时，SQLite 要为每一行到子表确认"没有引用它"。子表的外键列没有索引时，这个
-- 检查退化成一次全表扫描，代价随"删掉的父行数 × 子表行数"增长：会话回收按会话批量删
-- checkpoint 与 message，于是单条 DELETE 从毫秒级涨到分钟级，60 秒一拍的维护事务一直
-- 占着写锁——桌面端的设置保存（管理请求 15 秒超时）、Cursor 运行的落库全部排队失败。
--
-- 下面七条索引补齐了"子表侧没有索引"的全部外键：conversation_checkpoints 的自引用
-- parent_checkpoint_id、指向它和 messages 的其余外键，以及 messages 的复合外键。
-- 外键子列有索引后，同一批语句的实测耗时降到千分之一量级；索引本身的创建在既有库上
-- 是亚秒级。复合外键 checkpoint_messages(conversation_id, message_id) 的索引列顺序
-- 必须与父表 messages 的主键一致，否则外键检查用不上它。
CREATE INDEX checkpoint_messages_conversation_message
ON checkpoint_messages(conversation_id, message_id);

CREATE INDEX conversation_checkpoints_parent_lookup
ON conversation_checkpoints(parent_checkpoint_id);

CREATE INDEX runs_base_checkpoint ON runs(base_checkpoint_id);
CREATE INDEX runs_head_checkpoint ON runs(head_checkpoint_id);

CREATE INDEX tool_rounds_base_checkpoint ON tool_rounds(base_checkpoint_id);
CREATE INDEX tool_round_calls_committed_checkpoint ON tool_round_calls(committed_checkpoint_id);

CREATE INDEX input_anchors_base_checkpoint ON input_anchors(base_checkpoint_id);
