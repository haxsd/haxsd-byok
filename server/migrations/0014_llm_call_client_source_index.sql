-- 状态接口按 client_source 统计各自客户端的调用数：Devin 页与 Codex 页每次刷新都会
-- 调一次。没有索引时这是一次 llm_calls 全表扫描（30 万行、debug 构建实测中位数
-- 约 97 ms/次，见 server/tests/client_source_count_measurement.rs）。索引是覆盖的：
-- 计数只读 client_source，不需要回表。
CREATE INDEX llm_calls_client_source ON llm_calls(client_source);
