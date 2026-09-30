//! 工作流模板填充。
//!
//! 两种模式：
//! - `Template`：服务端存 ComfyUI workflow JSON，客户端只传
//!   `{{prompt}}` / `{{width|1024}}` 这样的占位符参数。
//! - `Raw`：客户端直接提交完整 workflow，服务端不碰。
//!
//! ## 两条硬规则
//!
//! 1. **整串就是占位符**时按原始 JSON 类型替换——`{{seed}}` 要变成
//!    数字 `42` 而不是字符串 `"42"`，否则 ComfyUI 校验直接报错。
//! 2. **传了模板没消费的参数必须报错**。静默丢弃会让「设了 seed 但
//!    每次出图都一样」这类问题永远查不出来。

use serde_json::{Map, Value};

/// 参数名 → 出错原因。
pub type Slots = Map<String, Value>;

/// 占位符语法错误 / 传了没被消费的参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderError(pub String);

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for RenderError {}

/// 填充模板。返回填充后的 workflow，以及**实际被消费掉的参数名**。
pub fn render(template: &Value, params: &Slots) -> Result<(Value, Vec<String>), RenderError> {
    let mut used: Vec<String> = Vec::new();
    let out = fill(template, params, &mut used)?;
    used.sort();
    used.dedup();

    // 传了但没被消费：这是配置或调用方的错，不能默默吞掉
    let unused: Vec<&str> = params
        .keys()
        .filter(|k| !used.iter().any(|u| u == *k))
        .map(String::as_str)
        .collect();
    if !unused.is_empty() {
        return Err(RenderError(format!(
            "工作流没有消费这些参数：{}（已消费：{}）",
            unused.join(", "),
            if used.is_empty() {
                "无".to_string()
            } else {
                used.join(", ")
            }
        )));
    }
    Ok((out, used))
}

fn fill(node: &Value, params: &Slots, used: &mut Vec<String>) -> Result<Value, RenderError> {
    Ok(match node {
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), fill(v, params, used)?);
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for v in items {
                out.push(fill(v, params, used)?);
            }
            Value::Array(out)
        }
        Value::String(s) => fill_string(s, params, used)?,
        other => other.clone(),
    })
}

fn fill_string(s: &str, params: &Slots, used: &mut Vec<String>) -> Result<Value, RenderError> {
    // 整串就是一个占位符 → 保留原始类型
    if let Some((name, default)) = whole_placeholder(s) {
        return Ok(resolve(&name, default, params, used));
    }
    if !s.contains("{{") {
        return Ok(Value::String(s.to_string()));
    }
    // 夹杂文本 → 逐个做字符串插值
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find("}}") else {
            return Err(RenderError(format!("占位符没有闭合：{s}")));
        };
        let body = rest[start + 2..start + end].trim();
        let (name, default) = split_default(body);
        let v = resolve(&name, default, params, used);
        match v {
            Value::String(t) => out.push_str(&t),
            other => out.push_str(&scalar_text(&other)),
        }
        rest = &rest[start + end + 2..];
    }
    out.push_str(rest);
    Ok(Value::String(out))
}

/// 整串匹配 `{{ name }}` 或 `{{ name | default }}`。
fn whole_placeholder(s: &str) -> Option<(String, Option<String>)> {
    let t = s.trim();
    let inner = t.strip_prefix("{{")?.strip_suffix("}}")?;
    if inner.contains("{{") {
        return None;
    }
    let (name, default) = split_default(inner.trim());
    if name.is_empty() {
        return None;
    }
    Some((name, default))
}

fn split_default(body: &str) -> (String, Option<String>) {
    match body.split_once('|') {
        Some((n, d)) => (n.trim().to_string(), Some(d.trim().to_string())),
        None => (body.to_string(), None),
    }
}

/// 参数优先级：**调用方 > 模板默认值**。
/// 两者都没有时保留占位符原样，让 ComfyUI 自己报错——
/// 悄悄塞一个空串会让「参数名写错了」变成出图变黑图。
fn resolve(name: &str, default: Option<String>, params: &Slots, used: &mut Vec<String>) -> Value {
    if let Some(v) = params.get(name) {
        used.push(name.to_string());
        return v.clone();
    }
    match default {
        Some(d) => parse_scalar(&d),
        None => Value::String(format!("{{{{{name}}}}}"))
    }
}

/// 默认值按 JSON 解析：`1024` → 数字、`true` → 布尔，否则当字符串。
fn parse_scalar(d: &str) -> Value {
    match serde_json::from_str::<Value>(d) {
        Ok(v @ (Value::Number(_) | Value::Bool(_) | Value::Null)) => v,
        _ => Value::String(d.to_string()),
    }
}

fn scalar_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn slots(v: Value) -> Slots {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn whole_placeholder_preserves_number_type() {
        let tpl = json!({"3": {"inputs": {"seed": "{{seed}}"}}});
        let (out, used) = render(&tpl, &slots(json!({"seed": 42}))).unwrap();
        // 必须是数字 42，不能是字符串 "42"
        assert_eq!(out["3"]["inputs"]["seed"], 42);
        assert!(serde_json::to_string(&out).unwrap().contains("\"seed\":42"));
        assert_eq!(used, vec!["seed".to_string()]);
    }

    #[test]
    fn default_is_used_when_param_absent() {
        let tpl = json!({"a": "{{width|1024}}", "b": "{{height}}", "c": "{{flag|true}}", "d": "{{style|anime}}", "e": "{{neg|}}"});
        let (out, _) = render(&tpl, &Slots::new()).unwrap();
        assert_eq!(out["a"], 1024);
        // 没有默认值时保留占位符，交给 ComfyUI 报错
        assert_eq!(out["b"], "{{height}}");
        assert_eq!(out["c"], true);
        assert_eq!(out["d"], "anime");
        assert_eq!(out["e"], "");
    }

    #[test]
    fn caller_param_beats_template_default() {
        let tpl = json!({"a": "{{width|1024}}"});
        let (out, _) = render(&tpl, &slots(json!({"width": 512}))).unwrap();
        assert_eq!(out["a"], 512);
    }

    /// 承重规则：静默丢弃会让「设了 seed 却每次一样」永远查不出来。
    #[test]
    fn unconsumed_param_is_an_error() {
        let tpl = json!({"a": "{{prompt}}"});
        let e = render(&tpl, &slots(json!({"prompt": "cat", "seed": 7}))).unwrap_err();
        assert!(e.0.contains("seed"), "错误信息要指出是哪个参数：{}", e.0);
    }

    #[test]
    fn interpolation_inside_larger_string() {
        let tpl = json!({"text": "a photo of {{subject}}, {{style|realistic}} style"});
        let (out, _) = render(&tpl, &slots(json!({"subject": "cat"}))).unwrap();
        assert_eq!(out["text"], "a photo of cat, realistic style");
    }

    #[test]
    fn unclosed_placeholder_is_rejected() {
        let tpl = json!({"text": "{{oops"});
        assert!(render(&tpl, &Slots::new()).is_err());
    }

    /// 同一个参数在多个节点出现是常态（prompt 常见），
    /// 填充要全部替换，且只算消费了一次。
    #[test]
    fn same_param_in_many_nodes_is_filled_everywhere() {
        let tpl = json!({
            "6": {"inputs": {"text": "{{prompt}}"}},
            "7": {"inputs": {"text": "negative: {{prompt}}"}}
        });
        let (out, used) = render(&tpl, &slots(json!({"prompt": "cat"}))).unwrap();
        assert_eq!(out["6"]["inputs"]["text"], "cat");
        assert_eq!(out["7"]["inputs"]["text"], "negative: cat");
        assert_eq!(used.len(), 1);
    }

    #[test]
    fn nested_arrays_are_walked() {
        let tpl = json!({"batch": [1, {"inputs": ["{{a}}", "x{{a}}"]}]});
        let (out, _) = render(&tpl, &slots(json!({"a": "Z"}))).unwrap();
        assert_eq!(out["batch"][1]["inputs"][0], "Z");
        assert_eq!(out["batch"][1]["inputs"][1], "xZ");
    }
}
