-- 一次调用在业务上的用途。
--
-- 自动压缩的摘要调用与普通对话共用 `llm_calls`：它确实是一次 provider 调用，用量也要
-- 计费统计。但它的输入是被裁剪过的重放历史，**不代表会话当前的上下文长度**；上下文
-- 用量锚点（`latest_context_usage`）只能读普通对话的行，否则一次手动 /summarize 之后的
-- 估算会按"摘要调用的输入"起算，系统性低估、预检不再触发。
--
-- 历史行都是普通对话，默认值对它们是正确语义（旧版本写下的摘要行无法区分，按 chat 处理，
-- 影响仅限于一轮估算，下一轮就会被新的真实用量覆盖）。
ALTER TABLE llm_calls ADD COLUMN purpose TEXT NOT NULL DEFAULT 'chat';
