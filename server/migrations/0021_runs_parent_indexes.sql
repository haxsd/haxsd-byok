-- `runs` 上按父反查的两条路径一直没有索引，而它们都在热路径上：
--   * `has_running_background_subagents`（`run/engine.rs` 的父轮收尾判定，子代理在跑时
--     每 5 秒问一次）按 `parent_run_id` 找自己的子运行；
--   * `finished_child_runs`（每次 checkpoint 构建）按 `parent_tool_call_id` 批量找子代理
--     的终态。
-- 两条都是"一个父、少量子"的形状，缺索引就是全表扫——runs 只随会话回收连带删除，
-- 所以扫描量随对话历史单调增长。
CREATE INDEX IF NOT EXISTS runs_parent_run
    ON runs(parent_run_id);

CREATE INDEX IF NOT EXISTS runs_parent_tool_call
    ON runs(parent_tool_call_id);
