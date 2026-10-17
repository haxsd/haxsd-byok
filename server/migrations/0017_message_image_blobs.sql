-- 消息里的工具图片（Read 工具读进来的图片）只被 payload_json 的
-- `$.content.image.blob_id` 引用：既不在 conversation_blob_roots 里，也没有 blob_edges，
-- 回收趟只能靠每 60 秒扫一遍整张 messages、逐行做两次 json_extract 才知道它们还在用。
-- 库一大这段就长时间占着写锁（0015 记下的同一个失效模式），而引用在写消息时就是确定的，
-- 于是写消息时把引用登记下来，回收趟与"清理详细记录"改读这张小表。
--
-- 值仍然是 payload 里那份 base64 文本：SQLite 没有 base64 解码函数，需要与
-- blobs.blob_id 比较的地方（回收趟、清理明细）由 Rust 解一次。
CREATE TABLE IF NOT EXISTS message_image_blobs (
    conversation_id TEXT NOT NULL,
    blob_id TEXT NOT NULL,
    PRIMARY KEY (conversation_id, blob_id),
    FOREIGN KEY (conversation_id) REFERENCES conversations(conversation_id)
);

-- 已有消息补登记一次：不补的话，历史会话里的图片会在无根窗口之后被回收趟当成垃圾删掉，
-- 而它们仍然被 messages 引用。
INSERT OR IGNORE INTO message_image_blobs(conversation_id, blob_id)
SELECT conversation_id, json_extract(payload_json, '$.content.image.blob_id')
FROM messages
WHERE json_extract(payload_json, '$.content.image.blob_id') IS NOT NULL;
