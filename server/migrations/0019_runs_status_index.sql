-- 60 秒一拍的回收趟在写锁内先数"还有没有活跃 run"
-- （`retention::prune_inactive_storage`：`WHERE status = 'running' AND updated_at_ms >= ?`）。
-- runs 上现有的索引前导列是 conversation_id（runs_conversation_status），status 谓词用不上，
-- 于是这条 COUNT 每趟全表扫一遍——0015 记录的维护事务占写锁、管理请求排队超时的同一段代价。
-- 索引按 (status, updated_at_ms) 排列后，这条语句变成一次范围查找，只碰真正在跑的 run。
CREATE INDEX IF NOT EXISTS runs_status_updated ON runs(status, updated_at_ms);
