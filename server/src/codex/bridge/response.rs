//! 上游 SSE 分片 → Responses 事件与终态 response；异常结束永远不会变成成功。
//!
//! 事件形状与顺序对齐 Codex 客户端认识的 Responses 生命周期：created/in_progress → 条目
//! added/delta/done → completed/incomplete。没有上游终态（finish_reason / stop_reason）时
//! `finish()` 返回错误，绝不伪造 completed。
use super::{replay, tools::ToolKind, ToolMap, UpstreamFormat};
use crate::{Error, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Clone)]
enum Terminal {
    Completed,
    Incomplete(&'static str),
}

enum Slot {
    Text {
        id: String,
        text: String,
        added: bool,
    },
    Reasoning {
        id: String,
        text: String,
        /// Anthropic 的原始 thinking 块；Chat 没有结构化状态。
        blocks: Option<Vec<Value>>,
        added: bool,
    },
    Tool {
        id: String,
        call_id: String,
        name: String,
        arguments: String,
        /// Anthropic 在 content_block_start 里一次性给出的 input。
        initial: Option<Value>,
        added: bool,
    },
}

/// 把一条上游 SSE 流翻译成 Responses 事件；终态与最终 response 由 `output()`/`usage()` 暴露。
pub struct BridgeStream {
    format: UpstreamFormat,
    model: String,
    replay_owner: String,
    tools: ToolMap,
    id: String,
    created: u64,
    slots: Vec<Slot>,
    /// Anthropic content_block index → slot。
    blocks: BTreeMap<u64, usize>,
    /// Chat tool_calls index → slot。
    tool_slots: BTreeMap<u64, usize>,
    text_slot: Option<usize>,
    reasoning_slot: Option<usize>,
    usage: Value,
    fresh_anthropic_input: u64,
    terminal: Option<Terminal>,
    started: bool,
    message_stopped: bool,
    sequence: u64,
    finished: bool,
    output: Vec<Value>,
}

impl BridgeStream {
    pub fn new(format: UpstreamFormat, model: String, tools: ToolMap) -> Self {
        Self {
            format,
            replay_owner: model.clone(),
            model,
            tools,
            id: format!("resp_{}", uuid::Uuid::new_v4().simple()),
            created: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or_default(),
            slots: Vec::new(),
            blocks: BTreeMap::new(),
            tool_slots: BTreeMap::new(),
            text_slot: None,
            reasoning_slot: None,
            usage: zero_usage(),
            fresh_anthropic_input: 0,
            terminal: None,
            started: false,
            message_stopped: false,
            sequence: 0,
            finished: false,
            output: Vec::new(),
        }
    }

    pub(crate) fn with_replay_owner(mut self, owner: String) -> Self {
        self.replay_owner = owner;
        self
    }

    pub fn response_id(&self) -> &str {
        &self.id
    }

    /// 终态 response 的 output；终态出现前为空。
    pub fn output(&self) -> Vec<Value> {
        self.output.clone()
    }

    /// 最后一次已知的上游用量（缺项为 0）。
    pub fn usage(&self) -> Value {
        self.usage.clone()
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    pub fn push(&mut self, chunk: &Value) -> Result<Vec<Value>> {
        if self.finished {
            return Err(Error::Protocol(
                "received upstream data after the terminal Codex response".into(),
            ));
        }
        if chunk.get("type").and_then(Value::as_str) == Some("error")
            || chunk.get("error").is_some_and(|error| !error.is_null())
        {
            return Err(Error::Provider(format!(
                "Codex upstream returned an error: {chunk}"
            )));
        }
        // 上游给出终态标记后再收一个分片，用来接住紧随其后的 usage，然后立即收尾。
        let deferred = self.terminal.is_some();
        let mut events = self.start_events();
        match self.format {
            UpstreamFormat::Chat => self.chat(chunk, &mut events)?,
            UpstreamFormat::Anthropic => self.anthropic(chunk, &mut events)?,
        }
        if (deferred && self.format == UpstreamFormat::Chat) || self.message_stopped {
            events.extend(self.close()?);
        }
        Ok(self.numbered(events))
    }

    /// 上游流结束时调用；幂等。没有合法终态时返回错误而不是伪造成功。
    pub fn finish(&mut self) -> Result<Vec<Value>> {
        let events = self.close()?;
        Ok(self.numbered(events))
    }

    fn numbered(&mut self, mut events: Vec<Value>) -> Vec<Value> {
        for event in &mut events {
            event["sequence_number"] = json!(self.sequence);
            self.sequence += 1;
        }
        events
    }

    fn start_events(&mut self) -> Vec<Value> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        let response = self.snapshot("in_progress", Vec::new());
        vec![
            json!({"type": "response.created", "response": response.clone()}),
            json!({"type": "response.in_progress", "response": response}),
        ]
    }

    fn snapshot(&self, status: &str, output: Vec<Value>) -> Value {
        json!({
            "id": self.id,
            "object": "response",
            "created_at": self.created,
            "status": status,
            "model": self.model,
            "output": output,
            "usage": self.usage,
        })
    }

    fn new_id(&self, prefix: &str) -> String {
        format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
    }

    fn push_slot(&mut self, slot: Slot) -> usize {
        self.slots.push(slot);
        self.slots.len() - 1
    }

    fn slot_id(&self, index: usize) -> String {
        match &self.slots[index] {
            Slot::Text { id, .. } | Slot::Reasoning { id, .. } | Slot::Tool { id, .. } => {
                id.clone()
            }
        }
    }

    /// 当前槽已经认下的上游 call_id；还没有就是 `None`。
    fn tool_call_id(&self, slot: usize) -> Option<&str> {
        match self.slots.get(slot) {
            Some(Slot::Tool { call_id, .. }) if !call_id.is_empty() => Some(call_id),
            _ => None,
        }
    }

    // ===================== Chat =====================

    fn chat(&mut self, chunk: &Value, out: &mut Vec<Value>) -> Result<()> {
        if let Some(usage) = chunk.get("usage").filter(|usage| !usage.is_null()) {
            self.merge_usage(usage, false);
        }
        let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
            return Ok(());
        };
        if choices.len() > 1 {
            return Err(Error::Protocol(
                "Codex bridge does not support multiple completion choices".into(),
            ));
        }
        let Some(choice) = choices.first() else {
            return Ok(());
        };
        if let Some(delta) = choice.get("delta") {
            let reasoning = delta
                .get("reasoning_content")
                .and_then(Value::as_str)
                .or_else(|| delta.get("reasoning").and_then(Value::as_str))
                .filter(|text| !text.is_empty());
            if let Some(text) = reasoning {
                let index = self.chat_reasoning_slot();
                self.reasoning_delta(index, text, out);
            }
            if let Some(text) = delta
                .get("content")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                let index = self.chat_text_slot();
                self.text_delta(index, text, out);
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    self.chat_tool_delta(call, out)?;
                }
            }
        }
        if let Some(reason) = choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
        {
            self.terminal = Some(chat_terminal(reason)?);
        }
        Ok(())
    }

    fn chat_text_slot(&mut self) -> usize {
        if let Some(index) = self.text_slot {
            return index;
        }
        let index = self.push_slot(Slot::Text {
            id: self.new_id("msg"),
            text: String::new(),
            added: false,
        });
        self.text_slot = Some(index);
        index
    }

    fn chat_reasoning_slot(&mut self) -> usize {
        if let Some(index) = self.reasoning_slot {
            return index;
        }
        let index = self.push_slot(Slot::Reasoning {
            id: self.new_id("rs"),
            text: String::new(),
            blocks: None,
            added: false,
        });
        self.reasoning_slot = Some(index);
        index
    }

    /// 归并一条 Chat 工具分片。
    ///
    /// 判据不能只看上游的 `index`：有的上游每次调用都从 0 重新编号，只按 index 归并
    /// 会把两次调用并成一个、参数被拼在一起；也有的分片不带 index，按原样整轮失败。
    /// 所以"新调用"的判据是"带非空 id，且与当前槽已有的 call_id 不同"；没有 id 的
    /// 分片退回按 index 追加，index 缺失按 0 处理。
    fn chat_tool_delta(&mut self, call: &Value, out: &mut Vec<Value>) -> Result<()> {
        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
        let incoming_id = call
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty());
        let current = self.tool_slots.get(&index).copied();
        let starts_new_call = match (incoming_id, current) {
            (Some(id), Some(slot)) => self
                .tool_call_id(slot)
                .is_some_and(|existing| existing != id),
            _ => false,
        };
        let slot = match current.filter(|_| !starts_new_call) {
            Some(slot) => slot,
            None => {
                let slot = self.push_slot(Slot::Tool {
                    id: self.new_id("fc"),
                    call_id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                    initial: None,
                    added: false,
                });
                self.tool_slots.insert(index, slot);
                slot
            }
        };
        let mut added_now = false;
        let mut fragment = String::new();
        let (call_id, name) = {
            let Some(Slot::Tool {
                call_id,
                name,
                arguments,
                added,
                ..
            }) = self.slots.get_mut(slot)
            else {
                return Ok(());
            };
            if let Some(id) = incoming_id {
                if call_id.is_empty() {
                    call_id.push_str(id);
                }
            }
            if let Some(piece) = call.pointer("/function/name").and_then(Value::as_str) {
                name.push_str(piece);
            }
            fragment.push_str(
                call.pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            );
            arguments.push_str(&fragment);
            if !*added && !call_id.is_empty() && self.tools.spec_for(name).is_some() {
                *added = true;
                added_now = true;
            }
            (call_id.clone(), name.clone())
        };
        let custom = self
            .tools
            .spec_for(&name)
            .is_some_and(|spec| spec.kind == ToolKind::Custom);
        if added_now {
            let item_id = self.slot_id(slot);
            let mut item = self.tools.restore_call(
                &name,
                &call_id,
                if custom { r#"{"input":""}"# } else { "{}" },
                &item_id,
            )?;
            item["status"] = json!("in_progress");
            item[if custom { "input" } else { "arguments" }] = json!("");
            out.push(
                json!({"type": "response.output_item.added", "output_index": slot, "item": item}),
            );
            if !custom {
                if let Slot::Tool { arguments, .. } = &self.slots[slot] {
                    fragment = arguments.clone();
                }
            }
        }
        let added = matches!(&self.slots[slot], Slot::Tool { added: true, .. });
        if added && !custom && !fragment.is_empty() {
            out.push(json!({"type": "response.function_call_arguments.delta",
                "item_id": self.slot_id(slot), "output_index": slot, "delta": fragment}));
        }
        Ok(())
    }

    // ===================== Anthropic =====================

    fn anthropic(&mut self, chunk: &Value, out: &mut Vec<Value>) -> Result<()> {
        match chunk.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => {
                if let Some(usage) = chunk
                    .pointer("/message/usage")
                    .filter(|usage| !usage.is_null())
                {
                    self.merge_usage(usage, true);
                }
            }
            "content_block_start" => {
                let index = chunk.get("index").and_then(Value::as_u64).ok_or_else(|| {
                    Error::Protocol("Anthropic content block start is missing its index".into())
                })?;
                let block = chunk.get("content_block").cloned().unwrap_or(Value::Null);
                self.start_block(index, &block, out)?;
            }
            "content_block_delta" => self.block_delta(chunk, out)?,
            "content_block_stop" | "ping" => {}
            "message_stop" => {
                self.message_stopped = true;
            }
            "message_delta" => {
                if let Some(reason) = chunk
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|reason| !reason.is_empty())
                {
                    self.terminal = Some(anthropic_terminal(reason)?);
                }
                if let Some(usage) = chunk.get("usage").filter(|usage| !usage.is_null()) {
                    self.merge_usage(usage, true);
                }
            }
            other => {
                return Err(Error::Protocol(format!(
                    "unsupported Anthropic SSE event: {other}"
                )))
            }
        }
        Ok(())
    }

    fn start_block(&mut self, index: u64, block: &Value, out: &mut Vec<Value>) -> Result<()> {
        let slot = match block.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                let slot = self.push_slot(Slot::Text {
                    id: self.new_id("msg"),
                    text: String::new(),
                    added: false,
                });
                if let Some(text) = block
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    self.text_delta(slot, text, out);
                }
                slot
            }
            "thinking" | "redacted_thinking" => {
                let mut state = block.clone();
                if state["type"] == "thinking" {
                    state["thinking"] = json!("");
                }
                let slot = self.push_slot(Slot::Reasoning {
                    id: self.new_id("rs"),
                    text: String::new(),
                    blocks: Some(vec![state]),
                    added: false,
                });
                let initial = if block.get("type").and_then(Value::as_str) == Some("thinking") {
                    block.get("thinking").and_then(Value::as_str).unwrap_or("")
                } else {
                    ""
                };
                if !initial.is_empty() {
                    self.reasoning_delta(slot, initial, out);
                }
                slot
            }
            "tool_use" => self.push_slot(Slot::Tool {
                id: self.new_id("fc"),
                call_id: block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                name: block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                arguments: String::new(),
                initial: block
                    .get("input")
                    .cloned()
                    .filter(|input| input.as_object().is_some_and(|object| !object.is_empty())),
                added: false,
            }),
            other => {
                return Err(Error::Protocol(format!(
                    "unsupported Anthropic response block: {other}"
                )))
            }
        };
        self.blocks.insert(index, slot);
        Ok(())
    }

    fn block_delta(&mut self, chunk: &Value, out: &mut Vec<Value>) -> Result<()> {
        let index = chunk.get("index").and_then(Value::as_u64).ok_or_else(|| {
            Error::Protocol("Anthropic content block delta is missing its index".into())
        })?;
        let slot = *self.blocks.get(&index).ok_or_else(|| {
            Error::Protocol("Anthropic delta arrived before its content block start".into())
        })?;
        let delta = &chunk["delta"];
        match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => {
                let text = delta.get("text").and_then(Value::as_str).unwrap_or("");
                if !text.is_empty() {
                    self.text_delta(slot, text, out);
                }
            }
            Some("input_json_delta") => {
                let partial = delta
                    .get("partial_json")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                match self.slots.get_mut(slot) {
                    Some(Slot::Tool { arguments, .. }) => arguments.push_str(partial),
                    _ => {
                        return Err(Error::Protocol(
                            "Anthropic tool arguments target a non-tool block".into(),
                        ))
                    }
                }
            }
            Some("thinking_delta") => {
                let text = delta.get("thinking").and_then(Value::as_str).unwrap_or("");
                if !text.is_empty() {
                    self.reasoning_delta(slot, text, out);
                }
            }
            Some("signature_delta") => {
                let signature = delta.get("signature").and_then(Value::as_str).unwrap_or("");
                match self.slots.get_mut(slot) {
                    Some(Slot::Reasoning {
                        blocks: Some(blocks),
                        ..
                    }) => {
                        if let (Some(state), true) = (blocks.first_mut(), !signature.is_empty()) {
                            let existing = state
                                .get("signature")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            state["signature"] = json!(format!("{existing}{signature}"));
                        }
                    }
                    _ => {
                        return Err(Error::Protocol(
                            "Anthropic signature delta targets a non-thinking block".into(),
                        ))
                    }
                }
            }
            other => {
                return Err(Error::Protocol(format!(
                    "unsupported Anthropic content delta: {other:?}"
                )))
            }
        }
        Ok(())
    }

    // ===================== 条目增量 =====================

    fn text_delta(&mut self, index: usize, text: &str, out: &mut Vec<Value>) {
        let id = match self.slots.get_mut(index) {
            Some(Slot::Text {
                id,
                text: buffer,
                added,
            }) => {
                buffer.push_str(text);
                if !*added {
                    *added = true;
                    let item_id = id.clone();
                    out.push(
                        json!({"type": "response.output_item.added", "output_index": index,
                        "item": {"id": item_id.clone(), "type": "message", "status": "in_progress",
                            "role": "assistant", "content": []}}),
                    );
                    out.push(
                        json!({"type": "response.content_part.added", "item_id": item_id,
                        "output_index": index, "content_index": 0,
                        "part": {"type": "output_text", "text": "", "annotations": []}}),
                    );
                }
                id.clone()
            }
            _ => return,
        };
        out.push(
            json!({"type": "response.output_text.delta", "item_id": id, "output_index": index,
            "content_index": 0, "delta": text}),
        );
    }

    fn reasoning_delta(&mut self, index: usize, text: &str, out: &mut Vec<Value>) {
        let id = match self.slots.get_mut(index) {
            Some(Slot::Reasoning {
                id,
                text: buffer,
                blocks,
                added,
            }) => {
                buffer.push_str(text);
                if let Some(state) = blocks.as_mut().and_then(|blocks| blocks.first_mut()) {
                    let existing = state
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    state["thinking"] = json!(format!("{existing}{text}"));
                }
                if !*added {
                    *added = true;
                    let item_id = id.clone();
                    out.push(
                        json!({"type": "response.output_item.added", "output_index": index,
                        "item": {"id": item_id.clone(), "type": "reasoning", "summary": []}}),
                    );
                    out.push(
                        json!({"type": "response.reasoning_summary_part.added", "item_id": item_id,
                        "output_index": index, "summary_index": 0,
                        "part": {"type": "summary_text", "text": ""}}),
                    );
                }
                id.clone()
            }
            _ => return,
        };
        out.push(
            json!({"type": "response.reasoning_summary_text.delta", "item_id": id,
            "output_index": index, "summary_index": 0, "delta": text}),
        );
    }

    // ===================== 用量与收尾 =====================

    fn merge_usage(&mut self, value: &Value, anthropic: bool) {
        let (input_key, output_key) = if anthropic {
            ("input_tokens", "output_tokens")
        } else {
            ("prompt_tokens", "completion_tokens")
        };
        if let Some(input) = value.get(input_key).and_then(Value::as_u64) {
            if anthropic {
                self.fresh_anthropic_input = input;
            }
            self.usage["input_tokens"] = json!(input);
        }
        if let Some(output) = value.get(output_key).and_then(Value::as_u64) {
            self.usage["output_tokens"] = json!(output);
        }
        if anthropic {
            for (key, target) in [
                ("cache_read_input_tokens", "cached_tokens"),
                ("cache_creation_input_tokens", "cache_creation_tokens"),
            ] {
                if let Some(count) = value.get(key).and_then(Value::as_u64) {
                    self.usage["input_tokens_details"][target] = json!(count);
                }
            }
        } else {
            if let Some(count) = value
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(Value::as_u64)
            {
                self.usage["input_tokens_details"]["cached_tokens"] = json!(count);
            }
            if let Some(count) = value
                .pointer("/completion_tokens_details/reasoning_tokens")
                .and_then(Value::as_u64)
            {
                self.usage["output_tokens_details"]["reasoning_tokens"] = json!(count);
            }
        }
        if anthropic {
            self.usage["input_tokens"] = json!(self
                .fresh_anthropic_input
                .saturating_add(
                    self.usage["input_tokens_details"]["cached_tokens"]
                        .as_u64()
                        .unwrap_or(0)
                )
                .saturating_add(
                    self.usage["input_tokens_details"]["cache_creation_tokens"]
                        .as_u64()
                        .unwrap_or(0)
                ));
        }
        let upstream_total = if anthropic {
            None
        } else {
            value.get("total_tokens").and_then(Value::as_u64)
        };
        self.usage["total_tokens"] = json!(upstream_total.unwrap_or_else(|| {
            self.usage["input_tokens"]
                .as_u64()
                .unwrap_or_default()
                .saturating_add(self.usage["output_tokens"].as_u64().unwrap_or_default())
        }));
    }

    fn reasoning_state(&self, text: &str, blocks: Option<&[Value]>) -> Result<Option<String>> {
        match self.format {
            UpstreamFormat::Anthropic => {
                let Some(blocks) = blocks else {
                    return Ok(None);
                };
                if !blocks.iter().all(replayable_thinking_block) {
                    // 没有签名就没有回放价值，也不能伪装成可回放状态。
                    return Ok(None);
                }
                Ok(Some(replay::encode(
                    UpstreamFormat::Anthropic,
                    &self.replay_owner,
                    &Value::Array(blocks.to_vec()),
                )?))
            }
            UpstreamFormat::Chat => {
                if text.trim().is_empty() {
                    return Ok(None);
                }
                Ok(Some(replay::encode(
                    UpstreamFormat::Chat,
                    &self.replay_owner,
                    &json!(text),
                )?))
            }
        }
    }

    fn close(&mut self) -> Result<Vec<Value>> {
        if self.finished {
            return Ok(Vec::new());
        }
        if self.format == UpstreamFormat::Anthropic && !self.message_stopped {
            return Err(Error::Provider(
                "Anthropic stream ended without message_stop".into(),
            ));
        }
        let terminal = self.terminal.clone().ok_or_else(|| {
            Error::Provider("Codex upstream stream ended without a terminal marker".into())
        })?;
        let mut events = Vec::new();
        let mut output = Vec::new();
        let slots = std::mem::take(&mut self.slots);
        for (index, slot) in slots.into_iter().enumerate() {
            match slot {
                Slot::Text { id, text, added } => {
                    if !added {
                        events.push(
                            json!({"type": "response.output_item.added", "output_index": index,
                            "item": {"id": id, "type": "message", "status": "in_progress",
                                "role": "assistant", "content": []}}),
                        );
                        events.push(json!({"type": "response.content_part.added", "item_id": id,
                            "output_index": index, "content_index": 0,
                            "part": {"type": "output_text", "text": "", "annotations": []}}));
                    }
                    events.push(json!({"type": "response.output_text.done", "item_id": id,
                        "output_index": index, "content_index": 0, "text": text}));
                    events.push(json!({"type": "response.content_part.done", "item_id": id,
                        "output_index": index, "content_index": 0,
                        "part": {"type": "output_text", "text": text, "annotations": []}}));
                    let item = json!({"id": id, "type": "message", "role": "assistant", "status": "completed",
                        "content": [{"type": "output_text", "text": text, "annotations": []}]});
                    events.push(json!({"type": "response.output_item.done", "output_index": index, "item": item}));
                    output.push(item);
                }
                Slot::Reasoning {
                    id,
                    text,
                    blocks,
                    added,
                } => {
                    let encrypted = self.reasoning_state(&text, blocks.as_deref())?;
                    if text.trim().is_empty() && encrypted.is_none() {
                        // 既没有可见摘要也没有回放状态：这个条目没有信息量。
                        continue;
                    }
                    if !added {
                        events.push(
                            json!({"type": "response.output_item.added", "output_index": index,
                            "item": {"id": id, "type": "reasoning", "summary": []}}),
                        );
                        events.push(
                            json!({"type": "response.reasoning_summary_part.added", "item_id": id,
                            "output_index": index, "summary_index": 0,
                            "part": {"type": "summary_text", "text": ""}}),
                        );
                    }
                    events.push(
                        json!({"type": "response.reasoning_summary_text.done", "item_id": id,
                        "output_index": index, "summary_index": 0, "text": text}),
                    );
                    events.push(
                        json!({"type": "response.reasoning_summary_part.done", "item_id": id,
                        "output_index": index, "summary_index": 0,
                        "part": {"type": "summary_text", "text": text}}),
                    );
                    let mut item = json!({"id": id, "type": "reasoning",
                        "summary": [{"type": "summary_text", "text": text}]});
                    if let Some(encrypted) = encrypted {
                        item["encrypted_content"] = json!(encrypted);
                    }
                    events.push(json!({"type": "response.output_item.done", "output_index": index, "item": item}));
                    output.push(item);
                }
                Slot::Tool {
                    id,
                    call_id,
                    name,
                    arguments,
                    initial,
                    added,
                } => {
                    if name.trim().is_empty() || call_id.trim().is_empty() {
                        return Err(Error::Protocol(
                            "upstream tool call is missing its name or call_id".into(),
                        ));
                    }
                    let arguments = if arguments.is_empty() {
                        initial
                            .as_ref()
                            .map(|value| value.to_string())
                            .unwrap_or_else(|| "{}".to_string())
                    } else {
                        arguments
                    };
                    let item = self.tools.restore_call(&name, &call_id, &arguments, &id)?;
                    let custom = item["type"] == "custom_tool_call";
                    if !added {
                        let mut provisional = item.clone();
                        provisional["status"] = json!("in_progress");
                        provisional[if custom { "input" } else { "arguments" }] = json!("");
                        events.push(json!({"type": "response.output_item.added", "output_index": index, "item": provisional}));
                    }
                    if custom {
                        let input = item["input"].as_str().unwrap_or_default();
                        if !input.is_empty() {
                            events.push(json!({"type": "response.custom_tool_call_input.delta", "item_id": id,
                                "output_index": index, "delta": input}));
                        }
                        events.push(
                            json!({"type": "response.custom_tool_call_input.done", "item_id": id,
                            "output_index": index, "input": input}),
                        );
                    } else {
                        events.push(
                            json!({"type": "response.function_call_arguments.done", "item_id": id,
                            "output_index": index, "arguments": item["arguments"].clone()}),
                        );
                    }
                    events.push(json!({"type": "response.output_item.done", "output_index": index, "item": item}));
                    output.push(item);
                }
            }
        }
        if output.is_empty() {
            return Err(Error::Provider(
                "Codex upstream finished the turn without any usable output".into(),
            ));
        }
        let (status, event_type) = match terminal {
            Terminal::Completed => ("completed", "response.completed"),
            Terminal::Incomplete(_) => ("incomplete", "response.incomplete"),
        };
        let mut response = self.snapshot(status, output.clone());
        if let Terminal::Incomplete(reason) = terminal {
            response["incomplete_details"] = json!({"reason": reason});
        }
        events.push(json!({"type": event_type, "response": response}));
        self.output = output;
        self.finished = true;
        Ok(events)
    }
}

fn zero_usage() -> Value {
    json!({
        "input_tokens": 0,
        "output_tokens": 0,
        "total_tokens": 0,
        "input_tokens_details": {"cached_tokens": 0, "cache_creation_tokens": 0},
        "output_tokens_details": {"reasoning_tokens": 0},
    })
}

fn replayable_thinking_block(block: &Value) -> bool {
    match block.get("type").and_then(Value::as_str) {
        Some("thinking") => block
            .get("signature")
            .and_then(Value::as_str)
            .is_some_and(|signature| !signature.trim().is_empty()),
        Some("redacted_thinking") => block
            .get("data")
            .and_then(Value::as_str)
            .is_some_and(|data| !data.trim().is_empty()),
        _ => false,
    }
}

/// Chat 的 `finish_reason`：只把"确定是失败"的值当失败，其余都按完成处理。
///
/// 以前这里是成功白名单（`stop` / `tool_calls`），不在表里的值一律 `Err`：上游换用
/// 旧取值（`function_call`）或自造成功值（`eos`）时，一整轮明明产出正常的回答被判成
/// 失败。真正的失败不会因此被放过——这一轮没有任何可用输出时，`finish()` 仍会报错。
fn chat_terminal(reason: &str) -> Result<Terminal> {
    match reason.to_ascii_lowercase().as_str() {
        "length" => Ok(Terminal::Incomplete("max_output_tokens")),
        "content_filter" => Ok(Terminal::Incomplete("content_filter")),
        failed
            if failed.contains("fail") || failed.contains("error") || failed.contains("cancel") =>
        {
            Err(Error::Protocol(format!(
                "the Chat upstream reported a failed turn: {reason}"
            )))
        }
        _ => Ok(Terminal::Completed),
    }
}

fn anthropic_terminal(reason: &str) -> Result<Terminal> {
    match reason {
        "max_tokens" => Ok(Terminal::Incomplete("max_output_tokens")),
        "refusal" => Ok(Terminal::Incomplete("content_filter")),
        // `pause_turn` 是上游"这一轮先停在这里"的正常取值，不是失败。
        "end_turn" | "tool_use" | "stop_sequence" | "pause_turn" => Ok(Terminal::Completed),
        other => Err(Error::Protocol(format!(
            "unsupported Anthropic stop_reason: {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 上游用旧取值（`function_call`）或自造成功值（`eos`）命名成功的轮次时，不能被判成
    /// 失败；失败值仍然报错（`codex_bridge_safety` 里那条 `vendor_failed` 断言就是这条边界）。
    #[test]
    fn legacy_and_unknown_chat_finish_reasons_complete_the_turn() {
        for reason in [
            "stop",
            "tool_calls",
            "function_call",
            "eos",
            "vendor_specific",
        ] {
            assert!(
                matches!(chat_terminal(reason).unwrap(), Terminal::Completed),
                "{reason}"
            );
        }
        assert!(matches!(
            chat_terminal("length").unwrap(),
            Terminal::Incomplete("max_output_tokens")
        ));
        assert!(matches!(
            chat_terminal("content_filter").unwrap(),
            Terminal::Incomplete("content_filter")
        ));
        for reason in ["vendor_failed", "error", "cancelled", "CANCELED"] {
            assert!(chat_terminal(reason).is_err(), "{reason}");
        }
        // Anthropic 的 `pause_turn` 是"这一轮先停在这里"，不是失败。
        assert!(matches!(
            anthropic_terminal("pause_turn").unwrap(),
            Terminal::Completed
        ));
    }

    fn shell_tools() -> ToolMap {
        ToolMap::parse(Some(&json!([{
            "type": "function", "name": "shell",
            "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}
        }])))
        .unwrap()
    }

    fn chat_chunk(delta: Value, finish: Value) -> Value {
        json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    }

    fn added_calls(events: &[Value]) -> Vec<(String, String)> {
        events
            .iter()
            .filter(|event| event["type"] == "response.output_item.added")
            .filter(|event| event["item"]["type"] == "function_call")
            .map(|event| {
                (
                    event["item"]["call_id"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                    event["item"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                )
            })
            .collect()
    }

    fn arguments_of(events: &[Value]) -> String {
        events
            .iter()
            .filter(|event| event["type"] == "response.function_call_arguments.delta")
            .map(|event| event["delta"].as_str().unwrap_or_default())
            .collect()
    }

    /// 上游复用同一个 index 连发两次调用：它们是两次调用，不能被并成一个。
    #[test]
    fn chat_tool_deltas_that_reuse_an_index_stay_separate_calls() {
        let mut bridge = BridgeStream::new(UpstreamFormat::Chat, "vendor".into(), shell_tools());
        let mut events = bridge
            .push(&chat_chunk(
                json!({"tool_calls": [{"index": 0, "id": "call_a", "type": "function",
                    "function": {"name": "shell", "arguments": "{\"cmd\":\"ls\"}"}}]}),
                Value::Null,
            ))
            .unwrap();
        events.extend(
            bridge
                .push(&chat_chunk(
                    json!({"tool_calls": [{"index": 0, "id": "call_b", "type": "function",
                        "function": {"name": "shell", "arguments": "{\"cmd\":\"pwd\"}"}}]}),
                    json!("tool_calls"),
                ))
                .unwrap(),
        );
        events.extend(bridge.finish().unwrap());

        assert_eq!(
            added_calls(&events),
            vec![
                ("call_a".to_string(), "shell".to_string()),
                ("call_b".to_string(), "shell".to_string()),
            ],
            "复用 index 的两次调用必须各自成为一个 function_call 条目：{events:?}"
        );
        let output = bridge.output();
        let calls = output
            .iter()
            .filter(|item| item["type"] == "function_call")
            .collect::<Vec<_>>();
        assert_eq!(
            calls.len(),
            2,
            "两个条目都要出现在最终 output 里: {output:?}"
        );
        assert_eq!(calls[0]["arguments"], "{\"cmd\":\"ls\"}");
        assert_eq!(
            calls[1]["arguments"], "{\"cmd\":\"pwd\"}",
            "参数不能被拼到前一次调用上"
        );
    }

    /// 缺 index 的分片以前整轮失败：按 0 处理，参数照常累积。
    #[test]
    fn chat_tool_deltas_without_an_index_are_appended() {
        let mut bridge = BridgeStream::new(UpstreamFormat::Chat, "vendor".into(), shell_tools());
        let mut events = bridge
            .push(&chat_chunk(
                json!({"tool_calls": [{"id": "call_a", "type": "function",
                    "function": {"name": "shell", "arguments": "{\"cmd\":"}}]}),
                Value::Null,
            ))
            .unwrap();
        events.extend(
            bridge
                .push(&chat_chunk(
                    json!({"tool_calls": [{"function": {"arguments": "\"ls\"}"}}]}),
                    json!("tool_calls"),
                ))
                .unwrap(),
        );
        events.extend(bridge.finish().unwrap());

        assert_eq!(
            added_calls(&events),
            vec![("call_a".to_string(), "shell".to_string())]
        );
        assert_eq!(
            arguments_of(&events),
            "{\"cmd\":\"ls\"}",
            "同一个 index（缺省 0）的分片必须继续累积到同一个调用上"
        );
    }

    /// 同一调用的后续分片会重复带上 id：重复的 id 不能又开一个新调用。
    #[test]
    fn repeated_ids_stay_in_one_call() {
        let mut bridge = BridgeStream::new(UpstreamFormat::Chat, "vendor".into(), shell_tools());
        let mut events = bridge
            .push(&chat_chunk(
                json!({"tool_calls": [{"index": 0, "id": "call_a", "type": "function",
                    "function": {"name": "shell", "arguments": "{\"cmd\":"}}]}),
                Value::Null,
            ))
            .unwrap();
        events.extend(
            bridge
                .push(&chat_chunk(
                    json!({"tool_calls": [{"index": 0, "id": "call_a",
                        "function": {"arguments": "\"ls\"}"}}]}),
                    json!("tool_calls"),
                ))
                .unwrap(),
        );
        events.extend(bridge.finish().unwrap());

        assert_eq!(
            added_calls(&events),
            vec![("call_a".to_string(), "shell".to_string())]
        );
        assert_eq!(bridge.output().len(), 1);
    }
}
