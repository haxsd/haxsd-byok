//! Codex 工具声明 → 上游工具定义；保留原名、命名空间与 custom/freeform 语义的可逆映射。
use crate::{Error, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// 上游工具名的长度上限：OpenAI 兼容层的实际限制。
const UPSTREAM_NAME_MAX: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ToolKind {
    Function,
    Custom,
}

#[derive(Clone, Debug)]
pub(super) struct ToolSpec {
    /// Codex 声明的原始工具名。
    pub name: String,
    /// `namespace` 工具展开出的命名空间。
    pub namespace: Option<String>,
    pub kind: ToolKind,
    pub description: Option<String>,
    pub schema: Value,
}

/// 上游工具名 ↔ Codex 工具声明的双向映射；一个请求一份，随响应侧的 `BridgeStream` 复用。
#[derive(Clone, Debug, Default)]
pub struct ToolMap {
    specs: Vec<ToolSpec>,
    /// `names[i]` 是 `specs[i]` 对应的上游工具名。
    names: Vec<String>,
}

impl ToolMap {
    pub(super) fn parse(tools: Option<&Value>) -> Result<Self> {
        let mut map = Self::default();
        if let Some(tools) = tools {
            let tools = tools
                .as_array()
                .ok_or_else(|| Error::Protocol("Codex tools must be an array".into()))?;
            for tool in tools {
                map.add(tool, None)?;
            }
        }
        Ok(map)
    }

    fn add(&mut self, tool: &Value, namespace: Option<&str>) -> Result<()> {
        let kind = tool
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("function");
        if kind == "namespace" {
            if namespace.is_some() {
                return Err(Error::Protocol(
                    "nested Codex tool namespaces are not supported".into(),
                ));
            }
            let namespace = required_name(tool, "namespace")?;
            let children = tool
                .get("tools")
                .or_else(|| tool.get("children"))
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    Error::Protocol("Codex namespace tool must declare its tools".into())
                })?;
            for child in children {
                self.add(child, Some(&namespace))?;
            }
            return Ok(());
        }
        let spec = match kind {
            "function" | "custom" => ToolSpec {
                name: required_name(tool, kind)?,
                namespace: namespace
                    .map(str::to_string)
                    .or_else(|| tool.get("namespace").and_then(Value::as_str).map(str::to_string)),
                kind: if kind == "custom" { ToolKind::Custom } else { ToolKind::Function },
                description: match (tool.get("description").and_then(Value::as_str), tool.get("format").filter(|_| kind == "custom")) {
                    (description, Some(format)) => Some(format!("{}\nOriginal custom tool input format: {format}", description.unwrap_or(""))),
                    (description, None) => description.filter(|value| !value.is_empty()).map(String::from),
                },
                schema: parameters(tool.get("parameters")),
            },
            other => {
                return Err(Error::Protocol(format!(
                    "unsupported Codex hosted tool: {other}; the bridge only implements function, custom and namespace tools"
                )))
            }
        };
        if self
            .specs
            .iter()
            .any(|existing| existing.name == spec.name && existing.namespace == spec.namespace)
        {
            return Err(Error::Protocol(
                "duplicate Codex tool name in namespace".into(),
            ));
        }
        let name = self.assign_name(&spec);
        self.specs.push(spec);
        self.names.push(name);
        Ok(())
    }

    /// 上游名：`namespace__name`；超过 64 字符或与已分配的名字冲突时补稳定哈希后缀。
    fn assign_name(&self, spec: &ToolSpec) -> String {
        let flat = flatten_name(spec.namespace.as_deref(), &spec.name);
        if !self.names.iter().any(|name| name == &flat) {
            return flat;
        }
        let identity = format!("{:?}", (spec.kind, &spec.namespace, &spec.name));
        let suffix = format!("__{}", short_hash(&identity));
        format!(
            "{}{suffix}",
            truncate(&flat, UPSTREAM_NAME_MAX - suffix.len())
        )
    }

    /// 上游工具声明；Chat 用 `function` 形状，Anthropic 用 `input_schema`。
    pub(super) fn definitions(&self, anthropic: bool) -> Vec<Value> {
        self.specs
            .iter()
            .zip(&self.names)
            .map(|(spec, name)| {
                let schema = match spec.kind {
                    ToolKind::Custom => custom_schema(),
                    ToolKind::Function => spec.schema.clone(),
                };
                if anthropic {
                    let mut tool = json!({"name": name, "input_schema": schema});
                    if let Some(description) = &spec.description {
                        tool["description"] = json!(description);
                    }
                    tool
                } else {
                    let mut function = json!({"name": name, "parameters": schema});
                    if let Some(description) = &spec.description {
                        function["description"] = json!(description);
                    }
                    json!({"type": "function", "function": function})
                }
            })
            .collect()
    }

    pub(super) fn spec_for(&self, upstream_name: &str) -> Option<&ToolSpec> {
        if let Some(index) = self.names.iter().position(|name| name == upstream_name) {
            return Some(&self.specs[index]);
        }
        // Codex 的命名空间工具在上游被扁平化成 `namespace__name`，但模型的 instructions
        // （base.md）仍然按裸名 `update_plan` 指代它。模型照 instructions 调用裸名时，
        // 只要这个名字在本次请求里唯一，就应当认回同一个 spec；否则会把模型合法生成的
        // 工具调用误判成 “upstream returned undeclared tool”。
        let mut matches = self.specs.iter().filter(|spec| spec.name == upstream_name);
        let first = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        Some(first)
    }

    /// 历史条目与 tool_choice 使用同一套上游名：已声明就取分配名，未声明就按同一规则扁平化。
    pub(super) fn upstream_name(&self, name: &str, namespace: Option<&str>) -> String {
        if let Some(index) = self.find_index(name, namespace) {
            return self.names[index].clone();
        }
        flatten_name(namespace, name)
    }

    /// 按 Codex 声明的 `(name, namespace)` 找 spec；裸名在本次请求里唯一时也接受。
    fn find_index(&self, name: &str, namespace: Option<&str>) -> Option<usize> {
        if let Some(index) = self
            .specs
            .iter()
            .position(|spec| spec.name == name && spec.namespace.as_deref() == namespace)
        {
            return Some(index);
        }
        if namespace.is_some() {
            return None;
        }
        let mut matches = self
            .specs
            .iter()
            .enumerate()
            .filter(|(_, spec)| spec.name == name);
        let (index, _) = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        Some(index)
    }

    /// 强制选择必须落在已声明的工具上，否则上游会直接拒绝整个请求。
    pub(super) fn upstream_name_for_tool_choice(&self, tool_choice: &Value) -> Result<String> {
        let name = tool_choice
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                Error::Protocol(format!(
                    "unsupported tool_choice without a name: {tool_choice}"
                ))
            })?;
        let namespace = tool_choice.get("namespace").and_then(Value::as_str);
        let index = self.find_index(name, namespace).ok_or_else(|| {
            Error::Protocol(format!(
                "tool_choice names a tool that this request does not declare: {name}"
            ))
        })?;
        Ok(self.names[index].clone())
    }

    /// 上游只能返回本请求声明的工具；参数与 call_id 必须完整。
    pub(super) fn restore_call(
        &self,
        upstream_name: &str,
        call_id: &str,
        arguments: &str,
        item_id: &str,
    ) -> Result<Value> {
        let spec = self.spec_for(upstream_name).ok_or_else(|| {
            Error::Protocol(format!(
                "upstream returned undeclared tool: {upstream_name}"
            ))
        })?;
        if call_id.trim().is_empty() {
            return Err(Error::Protocol(
                "upstream tool call is missing call_id".into(),
            ));
        }
        let parsed: Value = serde_json::from_str(arguments)?;
        if !parsed.is_object() {
            return Err(Error::Protocol(
                "upstream tool arguments must be a JSON object".into(),
            ));
        }
        let mut item = if spec.kind == ToolKind::Custom {
            let input = parsed.get("input").and_then(Value::as_str).ok_or_else(|| {
                Error::Protocol("custom tool arguments must contain a string input".into())
            })?;
            json!({"id":item_id,"type":"custom_tool_call","status":"completed","call_id":call_id,"name":spec.name,"input":input})
        } else {
            json!({"id":item_id,"type":"function_call","status":"completed","call_id":call_id,"name":spec.name,"arguments":arguments})
        };
        if let Some(namespace) = &spec.namespace {
            item["namespace"] = json!(namespace);
        }
        Ok(item)
    }
}

/// custom/freeform 工具的统一包装：上游按对象调用，输入是对象里的 `input` 字符串。
fn custom_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "input": {
                "type": "string",
                "description": "Exact raw string input for the original Codex custom tool. Preserve the payload byte for byte."
            }
        },
        "required": ["input"]
    })
}

fn parameters(value: Option<&Value>) -> Value {
    match value {
        Some(Value::Object(object)) if !object.is_empty() => {
            let mut schema = Value::Object(object.clone());
            if let Some(schema) = schema.as_object_mut() {
                schema.entry("type").or_insert_with(|| json!("object"));
            }
            schema
        }
        _ => json!({"type": "object", "properties": {}}),
    }
}

fn required_name(tool: &Value, kind: &str) -> Result<String> {
    tool.get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .ok_or_else(|| Error::Protocol(format!("Codex {kind} tool must declare a name")))
}

fn flatten_name(namespace: Option<&str>, name: &str) -> String {
    let flat = match namespace
        .map(str::trim)
        .filter(|namespace| !namespace.is_empty())
    {
        Some(namespace) => format!("{namespace}__{name}"),
        None => name.to_string(),
    };
    let flat = flat
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if flat.len() <= UPSTREAM_NAME_MAX {
        return flat;
    }
    let suffix = format!("__{}", short_hash(&flat));
    format!(
        "{}{suffix}",
        truncate(&flat, UPSTREAM_NAME_MAX - suffix.len())
    )
}

fn truncate(value: &str, limit: usize) -> String {
    let mut out = String::new();
    for character in value.chars() {
        if out.len() + character.len_utf8() > limit {
            break;
        }
        out.push(character);
    }
    out
}

fn short_hash(value: &str) -> String {
    hex::encode(&Sha256::digest(value.as_bytes())[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn namespaced_map() -> ToolMap {
        let tools = json!([{
            "type": "namespace",
            "name": "functions",
            "tools": [
                {"type": "function", "name": "update_plan", "parameters": {"type": "object", "properties": {}}},
                {"type": "function", "name": "shell", "parameters": {"type": "object", "properties": {}}}
            ]
        }]);
        ToolMap::parse(Some(&tools)).unwrap()
    }

    #[test]
    fn a_bare_namespaced_name_resolves_when_unique() {
        let map = namespaced_map();
        assert!(map.spec_for("functions__update_plan").is_some());

        let spec = map
            .spec_for("update_plan")
            .expect("bare name should resolve");
        assert_eq!(spec.name, "update_plan");
        assert_eq!(spec.namespace.as_deref(), Some("functions"));

        // 上游按裸名返回时，恢复出的 item 必须带上 namespace，Codex 才认。
        let item = map
            .restore_call("update_plan", "call-1", "{\"plan\":[]}", "item-1")
            .unwrap();
        assert_eq!(item["name"], "update_plan");
        assert_eq!(item["namespace"], "functions");
        assert_eq!(item["type"], "function_call");

        // 历史与 tool_choice 也要把裸名映射回上游分配名。
        assert_eq!(
            map.upstream_name("update_plan", None),
            "functions__update_plan"
        );
        assert_eq!(
            map.upstream_name_for_tool_choice(&json!({"name": "update_plan"}))
                .unwrap(),
            "functions__update_plan"
        );
    }

    #[test]
    fn a_bare_name_is_rejected_when_it_is_ambiguous() {
        let tools = json!([
            {"type": "namespace", "name": "one", "tools": [{"type": "function", "name": "dup"}]},
            {"type": "namespace", "name": "two", "tools": [{"type": "function", "name": "dup"}]}
        ]);
        let map = ToolMap::parse(Some(&tools)).unwrap();
        assert!(map.spec_for("one__dup").is_some());
        assert!(map.spec_for("two__dup").is_some());
        assert!(map.spec_for("dup").is_none());
    }
}
