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

use std::collections::BTreeSet;

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

/// 调用方参数 → 模板真正要用的参数。
///
/// 两件事：
/// 1. 校验参数面（只放行 [`crate::domain::workflow::ALLOWED_PARAM_SLOTS`]）。
/// 2. 把 `size` 展开成 `width` / `height`。
///
/// **只注入模板真的引用到的那几个**。`zit-img2img` 里压根没有宽高占位符，
/// 无脑注入会让调用方只传了 `size` 却收到一句
/// 「工作流没有消费这些参数：width, height」——他压根不认识这两个名字。
///
/// `size` 本身不进入结果：它是被网关消费掉的，留在里面会被
/// 「传了没消费的参数」规则判死。
pub fn prepare_slots(tpl: &Value, params: &Map<String, Value>) -> Result<Slots, RenderError> {
    crate::domain::workflow::validate_params(params).map_err(RenderError)?;

    let mut out: Slots = Map::new();
    for (k, v) in params {
        if k == "size" {
            continue;
        }
        out.insert(k.clone(), v.clone());
    }

    // `size` 不传就用缺省值，不让它落到模板的 {{width|768}} 上——
    // 出图工作流已去重成一条，模板里那个默认值不该再是一个隐式尺寸。
    let size = params
        .get("size")
        .and_then(Value::as_str)
        .unwrap_or(crate::domain::workflow::DEFAULT_SIZE);
    // validate_params 已经保证能查到（显式传错值会先被拒掉）
    if let Some((w, h)) = crate::domain::workflow::resolve_size(size) {
        let refs = placeholders(tpl);
        for (slot, px) in [("width", w), ("height", h)] {
            if refs.contains(slot) {
                out.insert(slot.to_string(), Value::from(px));
            }
        }
    }
    Ok(out)
}

/// 模板里用到的全部占位符名。
pub fn placeholders(tpl: &Value) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    collect(tpl, &mut found);
    found
}

fn collect(node: &Value, out: &mut BTreeSet<String>) {
    match node {
        Value::Object(map) => {
            for v in map.values() {
                collect(v, out);
            }
        }
        Value::Array(items) => {
            for v in items {
                collect(v, out);
            }
        }
        Value::String(s) => {
            let mut rest = s.as_str();
            while let Some(start) = rest.find("{{") {
                let Some(end) = rest[start..].find("}}") else {
                    return;
                };
                let body = rest[start + 2..start + end].trim();
                let name = body.split('|').next().unwrap_or("").trim();
                if !name.is_empty() {
                    out.insert(name.to_string());
                }
                rest = &rest[start + end + 2..];
            }
        }
        _ => {}
    }
}

/// 填充模板。返回填充后的 workflow、**实际被消费掉的参数名**、
/// 以及**没能解析出来的占位符名**。
///
/// 第三个返回值是给「试跑」用的。调用方只被允许传固定那几个参数，
/// 所以模板里任何**不在允许集里**的占位符都永远拿不到值——而
/// `TextEncode` 这类输入节点不会报错，会把 `{{negative_prompt}}`
/// 这串字面量当成提示词照单全收，图就废了还不报错。
/// 把它显式报出来，配置错误才能在试跑阶段暴露。
pub fn render(
    template: &Value,
    params: &Slots,
) -> Result<(Value, Vec<String>, Vec<String>), RenderError> {
    let mut used: Vec<String> = Vec::new();
    let mut unresolved: Vec<String> = Vec::new();
    let out = fill(template, params, &mut used, &mut unresolved)?;
    used.sort();
    used.dedup();
    unresolved.sort();
    unresolved.dedup();

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
    Ok((out, used, unresolved))
}

fn fill(
    node: &Value,
    params: &Slots,
    used: &mut Vec<String>,
    unresolved: &mut Vec<String>,
) -> Result<Value, RenderError> {
    Ok(match node {
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), fill(v, params, used, unresolved)?);
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for v in items {
                out.push(fill(v, params, used, unresolved)?);
            }
            Value::Array(out)
        }
        Value::String(s) => fill_string(s, params, used, unresolved)?,
        other => other.clone(),
    })
}

fn fill_string(
    s: &str,
    params: &Slots,
    used: &mut Vec<String>,
    unresolved: &mut Vec<String>,
) -> Result<Value, RenderError> {
    // 整串就是一个占位符 → 保留原始类型
    if let Some((name, default)) = whole_placeholder(s) {
        return Ok(resolve(&name, default, params, used, unresolved));
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
        let v = resolve(&name, default, params, used, unresolved);
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
/// 同时把这个名字记进 `unresolved`，让试跑能报出来。
fn resolve(
    name: &str,
    default: Option<String>,
    params: &Slots,
    used: &mut Vec<String>,
    unresolved: &mut Vec<String>,
) -> Value {
    if let Some(v) = params.get(name) {
        used.push(name.to_string());
        return v.clone();
    }
    match default {
        Some(d) => parse_scalar(&d),
        None => {
            unresolved.push(name.to_string());
            Value::String(format!("{{{{{name}}}}}"))
        }
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
        let (out, used, _) = render(&tpl, &slots(json!({"seed": 42}))).unwrap();
        // 必须是数字 42，不能是字符串 "42"
        assert_eq!(out["3"]["inputs"]["seed"], 42);
        assert!(serde_json::to_string(&out).unwrap().contains("\"seed\":42"));
        assert_eq!(used, vec!["seed".to_string()]);
    }

    #[test]
    fn default_is_used_when_param_absent() {
        let tpl = json!({"a": "{{width|1024}}", "b": "{{height}}", "c": "{{flag|true}}", "d": "{{style|anime}}", "e": "{{neg|}}"});
        let (out, _, _) = render(&tpl, &Slots::new()).unwrap();
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
        let (out, _, _) = render(&tpl, &slots(json!({"width": 512}))).unwrap();
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
        let (out, _, _) = render(&tpl, &slots(json!({"subject": "cat"}))).unwrap();
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
        let (out, used, _) = render(&tpl, &slots(json!({"prompt": "cat"}))).unwrap();
        assert_eq!(out["6"]["inputs"]["text"], "cat");
        assert_eq!(out["7"]["inputs"]["text"], "negative: cat");
        assert_eq!(used.len(), 1);
    }

    #[test]
    fn nested_arrays_are_walked() {
        let tpl = json!({"batch": [1, {"inputs": ["{{a}}", "x{{a}}"]}]});
        let (out, _, _) = render(&tpl, &slots(json!({"a": "Z"}))).unwrap();
        assert_eq!(out["batch"][1]["inputs"][0], "Z");
        assert_eq!(out["batch"][1]["inputs"][1], "xZ");
    }

    /// 参数面收紧后，模板里**不在允许集里**的占位符永远拿不到值。
    /// 它必须被报出来：`TextEncode` 这类输入节点不会报错，会把
    /// `{{negative_prompt}}` 当成提示词原文用，图废了而且无人察觉。
    #[test]
    fn unresolved_placeholders_are_reported() {
        let tpl = json!({
            "6": {"inputs": {"text": "{{prompt}}", "text_neg": "{{negative_prompt}}"}},
            "7": {"inputs": {"width": "{{width|1024}}", "seed": "{{seed}}"}}
        });
        let (out, used, unresolved) =
            render(&tpl, &slots(json!({"prompt": "cat", "seed": 7}))).unwrap();

        // 填得出的照填
        assert_eq!(out["6"]["inputs"]["text"], "cat");
        assert_eq!(out["7"]["inputs"]["seed"], 7);
        assert_eq!(out["7"]["inputs"]["width"], 1024, "带默认值的算解析成功");
        assert_eq!(used, vec!["prompt".to_string(), "seed".to_string()]);

        // 填不出的要报出来，且字面量保留在输出里（让 ComfyUI 自己看到）
        assert_eq!(unresolved, vec!["negative_prompt".to_string()]);
        assert_eq!(out["6"]["inputs"]["text_neg"], "{{negative_prompt}}");
    }

    /// 带默认值（哪怕是空串）就不算 unresolved——那是作者明确写的固定值。
    #[test]
    fn empty_default_counts_as_resolved() {
        let tpl = json!({"a": "{{negative_prompt|}}"});
        let (out, _, unresolved) = render(&tpl, &Slots::new()).unwrap();
        assert_eq!(out["a"], "");
        assert!(
            unresolved.is_empty(),
            "写了空默认值就不该再报出来：{unresolved:?}"
        );
    }

    /// 同一个占位符出现多次只报一次名，别把错误信息刷成一片。
    #[test]
    fn unresolved_is_deduped() {
        let tpl = json!({
            "1": {"inputs": {"a": "{{negative_prompt}}"}},
            "2": {"inputs": {"b": "{{negative_prompt}}"}}
        });
        let (_, _, unresolved) = render(&tpl, &Slots::new()).unwrap();
        assert_eq!(unresolved, vec!["negative_prompt".to_string()]);
    }

    // ── prepare_slots：参数面校验 + size 展开 ──

    fn params(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    /// 尺寸由网关展开，模板照旧写 `{{width}}` / `{{height}}`。
    #[test]
    fn size_expands_into_width_and_height() {
        let tpl = json!({"6": {"inputs": {"width": "{{width}}", "height": "{{height}}"}}});

        let s = prepare_slots(&tpl, &params(json!({"size": "3x4"}))).unwrap();
        assert_eq!(s["width"], 768);
        assert_eq!(s["height"], 1024);
        assert!(
            !s.contains_key("size"),
            "size 被网关消费掉了，不该进渲染参数"
        );

        let s = prepare_slots(&tpl, &params(json!({"size": "16x9"}))).unwrap();
        assert_eq!(s["width"], 1024);
        assert_eq!(s["height"], 576);
    }

    /// 承重的坑：模板里没有宽高占位符时（img2img 就是），无脑注入会让调用方
    /// 只传了 `size` 却收到「没有消费这些参数：width, height」。
    #[test]
    fn size_is_ignored_when_template_has_no_size_placeholders() {
        let tpl = json!({"1": {"inputs": {"text": "{{prompt}}", "denoise": "{{denoise|0.5}}"}}});
        let s = prepare_slots(&tpl, &params(json!({"prompt": "cat", "size": "3x4"}))).unwrap();
        assert!(!s.contains_key("width") && !s.contains_key("height"));

        // 关键：整条渲染链路不该因此报错
        let (out, used, unresolved) = render(&tpl, &s).unwrap();
        assert_eq!(out["1"]["inputs"]["text"], "cat");
        assert_eq!(out["1"]["inputs"]["denoise"], 0.5);
        assert_eq!(used, vec!["prompt".to_string()]);
        assert!(unresolved.is_empty());
    }

    /// 只引用宽、不引用高时（某些 latent 节点会自己推另一边），
    /// 也只注入宽，别凭空多塞一个。
    #[test]
    fn only_referenced_size_slots_are_injected() {
        let tpl = json!({"6": {"inputs": {"width": "{{width}}"}}});
        let s = prepare_slots(&tpl, &params(json!({"size": "16x9"}))).unwrap();
        assert!(s.contains_key("width"));
        assert!(!s.contains_key("height"));
    }

    /// 不传 size 就用网关的缺省值，而不是模板里写的那个。
    /// 工作流已去重成一条，模板的 `{{width|768}}` 不该再是一个隐式尺寸。
    #[test]
    fn omitting_size_uses_the_gateway_default() {
        let tpl = json!({
            "1": {"inputs": {"text": "{{prompt}}"}},
            "6": {"inputs": {"width": "{{width|9999}}", "height": "{{height|9999}}"}}
        });
        let s = prepare_slots(&tpl, &params(json!({"prompt": "cat"}))).unwrap();
        // 缺省 3x4 覆盖掉模板里那个 9999
        assert_eq!(s["width"], 768);
        assert_eq!(s["height"], 1024);
        let (out, _, _) = render(&tpl, &s).unwrap();
        assert_eq!(out["6"]["inputs"]["width"], 768);
    }

    #[test]
    fn prepare_slots_rejects_unsupported_keys() {
        let tpl = json!({"1": {"inputs": {"text": "{{prompt}}"}}});
        let e = prepare_slots(&tpl, &params(json!({"prompt": "cat", "steps": 20}))).unwrap_err();
        assert!(e.0.contains("steps"), "{e}");
    }

    #[test]
    fn prepare_slots_rejects_raw_pixels() {
        let tpl = json!({"6": {"inputs": {"width": "{{width}}"}}});
        let e = prepare_slots(&tpl, &params(json!({"width": 1024}))).unwrap_err();
        assert!(e.0.contains("width"), "{e}");
    }

    #[test]
    fn prepare_slots_rejects_unknown_size() {
        let tpl = json!({"6": {"inputs": {"width": "{{width}}"}}});
        let e = prepare_slots(&tpl, &params(json!({"size": "1x1"}))).unwrap_err();
        assert!(e.0.contains("16x9"), "错误里要列出可选尺寸：{e}");
    }

    /// 占位符收集：决定「要不要注入宽高」的那份依据，不能漏也不能多。
    #[test]
    fn placeholders_collects_from_nested_structures() {
        let tpl = json!({
            "1": {"inputs": {"text": "{{prompt}}", "neg": "{{negative_prompt|}}"}},
            "2": {"inputs": {"w": "{{width|1024}}", "interp": "a {{seed}} b"}},
            "3": {"inputs": {"list": ["{{height}}", {"deep": "{{prompt}}"}]}},
            "4": {"inputs": {"plain": "no placeholders here"}}
        });
        let got: Vec<String> = placeholders(&tpl).into_iter().collect();
        assert_eq!(
            got,
            vec!["height", "negative_prompt", "prompt", "seed", "width"]
        );
    }

    /// 收集器不能被没闭合的 `{{` 带偏——它一遇到就停，而不是把后面的
    /// 字符串全吃进来当成占位符名。
    #[test]
    fn placeholders_stop_at_unclosed_brace() {
        let tpl = json!({"1": {"inputs": {"a": "{{oops", "b": "{{width}}"}}});
        // 该报错由 render 负责；收集器只保证自己不产出垃圾名字
        let got = placeholders(&tpl);
        assert!(!got.contains("oops"), "不该把残缺片段当占位符：{got:?}");
    }
}
