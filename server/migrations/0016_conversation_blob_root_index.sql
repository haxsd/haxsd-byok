-- 0010 建的 conversation_blob_roots 只有 (conversation_id, blob_id) 主键。
-- "这条内容块是不是某个会话的根"是只按 blob_id 的查询：管理面的"清理详细记录"
-- 与 60 秒一拍的回收趟都要用它判活。没有以 blob_id 开头的索引时，每个候选块都要
-- 扫一遍主键索引，而这些语句跑在持有写锁的事务里——正是 0015 处理过的同一类代价。
CREATE INDEX IF NOT EXISTS conversation_blob_roots_blob
ON conversation_blob_roots(blob_id);
