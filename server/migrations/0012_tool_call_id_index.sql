-- 子代理唤醒链路按 call_id 反查父会话里那次 Task 调用：
--   * 父轮等待后台子代理期间，每几秒要问一次"还有后台子代理在跑吗"（runs ⋈ tool_round_calls）；
--   * 子代理跑完时要知道自己属于哪次调用、该唤醒哪个会话。
-- tool_round_calls 现有的键是 (round_id, call_index) 主键与 (round_id, call_id) 唯一键，
-- 前缀都是 round_id，按 call_id 查用不上，只能扫全表——表会一直长，这条查询又会反复跑。
CREATE INDEX IF NOT EXISTS tool_round_calls_call_id
ON tool_round_calls(call_id);
