//! ComfyUI 出图工作流：服务端模板（默认）与原始 JSON 两条通道。

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowMode {
    /// 服务端维护命名工作流，客户端只传参数；占位符 `{{prompt}}` / `{{width|1024}}`。
    Template,
    /// 客户端直接提交完整 workflow JSON。
    Raw,
}

/// 模板模式下**唯一允许调用方传的参数**。
///
/// 这是一个刻意收紧的口子：参数面越小，能被调用方注入的东西越少，
/// 「这张图长这样」就越由服务端模板决定。`seed` 留在这儿是因为要能
/// 重跑出同一张图；`negative_prompt`、采样步数、denoise 之类一律
/// 固定在模板里（用 `{{negative_prompt|}}` 这种带默认值的写法）。
///
/// 注意**没有** `width` / `height`：尺寸不自由传，只能在
/// [`SIZE_PRESETS`] 这两组里选一个，然后由网关展开成像素。
///
/// **唯一的权威位置**：真实提交路径（`api::images`）与试跑
/// （`api::admin::dry_run_workflow`）都从这里取，两边不一致会让
/// 「试跑通过、实际被拒」这种排障噩梦重新出现。
pub const ALLOWED_PARAM_SLOTS: [&str; 3] = ["prompt", "size", "seed"];

/// 固定尺寸组合：名字 → (宽, 高)。
///
/// 只有两组，且是网关拥有映射关系的唯一地方。调用方传 `size`，
/// 网关展开成 `width` / `height` 填进模板的占位符——所以模板照旧写
/// `{{width}}` / `{{height}}`，不用跟着改。
///
/// `3x4` 沿用原有模板的 768×1024；`16x9` 取 1024×576，与 3:4
/// 同属 1024 系，两组都是精确比例。
pub const SIZE_PRESETS: [(&str, i64, i64); 2] = [("3x4", 768, 1024), ("16x9", 1024, 576)];

/// `size` 不传时用这一组。
///
/// 尺寸只有两组、模板本身不再自带「默认尺寸」的语义（工作流已去重成一条），
/// 所以缺省值必须由网关明确给，不能落到模板的 `{{width|768}}` 上——
/// 那样会多出一个没登记过的隐式尺寸。
pub const DEFAULT_SIZE: &str = "3x4";

/// 解析调用方要用的出图工作流：**按模型选，不按工作流选**。
///
/// 工作流是 ComfyUI 图，是实现细节；调用方能理解的、也该由他决定的是
/// 「用哪个模型出图」。模型名由 [`derive_model`] 从模板里推出来，
/// 所以这里不需要、也没有存过一个独立的 model 字段。
///
/// 缺省值来自**唯一性**而不是某条设置：只有一个启用的模型时不传 `model`
/// 也能用，多于一个时明确报错并列出可选值。**没有** `default_image_model`
/// 这种设置——那种「配了不生效」正是本仓库最该根除的一类问题。
///
/// 三种情况一律报错而不是随便挑一条：
/// - 同一个模型有多条启用的工作流 —— 选哪条取决于库里残留了什么；
/// - 有启用的模板工作流推不出模型名 —— 静默跳过会让「只有一个模型」
///   的结论变成假的；
/// - 调用方报了个不存在的模型。
// 返回值借 workflows，所以显式标 'a：requested 也是入参，生命周期省略规则不适用。
pub fn resolve_by_model<'a>(
    workflows: &'a [Workflow],
    requested: Option<&str>,
) -> Result<&'a Workflow, String> {
    let enabled: Vec<&Workflow> = workflows
        .iter()
        .filter(|w| w.enabled && w.mode == WorkflowMode::Template)
        .collect();

    if enabled.is_empty() {
        return Err("网关没有启用的出图工作流，先在管理台「出图」里配置并启用".into());
    }

    // 推不出模型名的工作流不能静默跳过：那会让「只有一个模型」的结论
    // 建立在「有一条我假装看不见」之上，调用方选了别的模型就再也回不来。
    let mut broken: Vec<String> = Vec::new();
    let mut by_model: Vec<(String, &Workflow)> = Vec::new();
    for w in &enabled {
        match derive_model(&w.comfy_workflow) {
            Ok(m) => {
                if let Some((_, dup)) = by_model.iter().find(|(seen, _)| *seen == m) {
                    return Err(format!(
                        "模型 {m} 有两条启用的出图工作流（{}、{}），\
                         网关不知道该选哪条；停用一条，或把它们的 checkpoint 改成不同模型",
                        dup.name, w.name
                    ));
                }
                by_model.push((m, w));
            }
            Err(why) => broken.push(format!("{}（{why}）", w.name)),
        }
    }
    if !broken.is_empty() {
        return Err(format!(
            "这些启用的出图工作流推不出模型名，没法按 model 选：{}",
            broken.join("；")
        ));
    }

    match requested.filter(|m| !m.is_empty()) {
        Some(want) => by_model
            .iter()
            .find(|(m, _)| m == want)
            .map(|(_, w)| *w)
            .ok_or_else(|| format!("没有叫 {want} 的出图模型。可用：{}", model_list(&by_model))),
        None => match by_model.as_slice() {
            [(_, only)] => Ok(only),
            [] => Err("网关没有可用的出图模型".into()),
            many => Err(format!(
                "有 {} 个出图模型（{}），请在请求体顶层传 model 指定",
                many.len(),
                many.iter()
                    .map(|(m, _)| m.as_str())
                    .collect::<Vec<_>>()
                    .join("、")
            )),
        },
    }
}

/// raw 通道的选法：调用方整份提交 workflow，没有模型可选，
/// 所以只在「恰好有一条启用的 raw 工作流」时才成立。
pub fn resolve_raw(workflows: &[Workflow]) -> Result<&Workflow, String> {
    let raws: Vec<&Workflow> = workflows
        .iter()
        .filter(|w| w.enabled && w.mode == WorkflowMode::Raw)
        .collect();
    match raws.as_slice() {
        [only] => Ok(only),
        [] => Err("raw 通道需要一条 mode=raw 的启用工作流，管理台「出图」里没有".into()),
        many => Err(format!(
            "有 {} 条启用的 raw 工作流（{}），raw 通道没法猜用哪条",
            many.len(),
            many.iter()
                .map(|w| w.name.as_str())
                .collect::<Vec<_>>()
                .join("、")
        )),
    }
}

fn model_list(items: &[(String, &Workflow)]) -> String {
    items
        .iter()
        .map(|(m, _)| m.as_str())
        .collect::<Vec<_>>()
        .join("、")
}

/// ComfyUI 里**主模型** checkpoint 的加载器：`(class_type, inputs 里的字段名)`。
///
/// 刻意不收 VAE / CLIP / LoRA 加载器：`flux_ae.safetensors`、
/// `Qwen3-4B-Q8_0.gguf` 这些是组件不是模型，换个 VAE 文件不该改掉
/// 调用方看到的 model 名——那会让一次无害的组件升级变成一次破坏性变更。
///
/// 收进来的都是 ComfyUI 里真正加载主模型权重、且权重名放在标量输入里的节点。
const MODEL_LOADERS: [(&str, &str); 6] = [
    // SD1.5 / SDXL / 老的单文件 checkpoint
    ("CheckpointLoaderSimple", "ckpt_name"),
    ("ImageOnlyCheckpointLoader", "ckpt_name"),
    // SD3 / Flux：主模型与 VAE 分开加载
    ("UNETLoader", "unet_name"),
    ("UnetLoader", "unet_name"),
    // GGUF 量化版（llama.cpp 的 ComfyUI 节点）
    ("UNETLoaderGGUF", "unet_name"),
    ("UnetLoaderGGUF", "unet_name"),
];

/// checkpoint 文件名的扩展名，剥掉后才是模型 id。
const MODEL_EXTENSIONS: [&str; 6] = [".safetensors", ".gguf", ".ckpt", ".pt", ".pth", ".bin"];

/// 从 ComfyUI 图里认出**主模型**的 id。
///
/// 模型名不单独存，而是从模板里的 checkpoint 文件名推出来——单一事实来源，
/// 不存在「改了模型忘了改名字」这种两边对不上的状态。代价是**改 checkpoint
/// 文件名就等于改 model id**，调用方会因此 404，所以管理台和导入报告都要把
/// 推导出来的名字显式展示出来。
pub fn derive_model(comfy: &Value) -> Result<String, String> {
    let Some(nodes) = comfy.as_object() else {
        return Err("workflow JSON 不是对象".into());
    };

    let mut found: Vec<(String, String)> = Vec::new();
    for (node_id, node) in nodes {
        let Some(class) = node.get("class_type").and_then(Value::as_str) else {
            continue;
        };
        let Some((_, field)) = MODEL_LOADERS.iter().find(|(c, _)| *c == class) else {
            continue;
        };
        let Some(raw) = node
            .get("inputs")
            .and_then(|i| i.get(*field))
            .and_then(Value::as_str)
        else {
            continue;
        };
        found.push((node_id.clone(), model_id_of(raw)));
    }

    match found.as_slice() {
        [] => Err(format!(
            "模板里没有模型加载器节点（认识这些：{}）。\
             出图按 model 选工作流，没有模型就没法选",
            MODEL_LOADERS
                .iter()
                .map(|(c, _)| *c)
                .collect::<Vec<_>>()
                .join("、")
        )),
        [(_, only)] => Ok(only.clone()),
        // 高低噪各一个 UNETLoader 的图是存在的，但那是双模型出图，
        // 与「一个 model 对应一条工作流」的前提冲突，必须让人来改模板，
        // 不能让网关自己挑一个然后让调用方按那个名字选。
        many => {
            let mut ids: Vec<&str> = many.iter().map(|(_, m)| m.as_str()).collect();
            ids.sort_unstable();
            ids.dedup();
            if ids.len() > 1 {
                return Err(format!(
                    "模板里有多个不同的模型（{}），出图一次只能用一个",
                    ids.join("、")
                ));
            }
            Ok(ids[0].to_string())
        }
    }
}

/// `z_image_turbo-Q8_0.gguf` → `z_image_turbo-Q8_0`。
fn model_id_of(filename: &str) -> String {
    let trimmed = filename.trim();
    MODEL_EXTENSIONS
        .iter()
        .find_map(|ext| trimmed.strip_suffix(ext))
        .unwrap_or(trimmed)
        .to_string()
}

/// 给调用方看的可传参数说明（`GET /v1/workflows` 用）。
pub fn allowed_slots_json() -> Vec<&'static str> {
    ALLOWED_PARAM_SLOTS.to_vec()
}

/// 给调用方看的尺寸选项：名字 + 实际像素。
pub fn size_presets_json() -> Vec<Value> {
    SIZE_PRESETS
        .iter()
        .map(|(name, w, h)| serde_json::json!({ "size": name, "width": w, "height": h }))
        .collect()
}

/// 校验调用方传进来的参数，只放行 [`ALLOWED_PARAM_SLOTS`]。
///
/// 返回人类可读的错误原因——调用方要能一眼看出「我传了不支持的东西」
/// 和「能传什么」，而不是只拿到一个 400。
pub fn validate_params(params: &serde_json::Map<String, serde_json::Value>) -> Result<(), String> {
    let unknown: Vec<&str> = params
        .keys()
        .map(String::as_str)
        .filter(|k| !ALLOWED_PARAM_SLOTS.contains(k))
        .collect();
    if !unknown.is_empty() {
        // model 放请求体顶层（跟 /v1/chat/completions 和 OpenAI 一致），
        // 放进 params 的人多半以为它也是模板占位符——直接告诉他放哪。
        let hint = if unknown.contains(&"model") {
            format!(
                "model 放请求体顶层，不在 params 里。出图请求体：\"model\": \"…\", {}",
                "params"
            )
        } else {
            "params".to_string()
        };
        return Err(format!(
            "不支持的参数：{}。出图只接受 {}（{hint}）",
            unknown.join(", "),
            ALLOWED_PARAM_SLOTS.join(", ")
        ));
    }
    // 传了 size 就必须是那两组之一，不能是任意字符串
    if let Some(size) = params.get("size") {
        let name = size
            .as_str()
            .ok_or_else(|| format!("size 必须是字符串，可选：{}", size_names().join(", ")))?;
        if !SIZE_PRESETS.iter().any(|(n, _, _)| *n == name) {
            return Err(format!(
                "不支持的尺寸：{name}。出图尺寸是固定的，只有 {}",
                size_names().join(", ")
            ));
        }
    }
    Ok(())
}

fn size_names() -> Vec<&'static str> {
    SIZE_PRESETS.iter().map(|(n, _, _)| *n).collect()
}

/// 尺寸名 → (宽, 高)。只接受 [`SIZE_PRESETS`] 里的名字。
pub fn resolve_size(name: &str) -> Option<(i64, i64)> {
    SIZE_PRESETS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, w, h)| (*w, *h))
}

/// 出图前的占位符体检。
///
/// `unresolved` 分两类，处理方式不同：
///
/// - **属于 [`ALLOWED_PARAM_SLOTS`]** —— 调用方本来就能传，是调用方漏传了。
///   必须在这里拦下来：字面量 `{{seed}}` 送到 ComfyUI 会得到一串
///   「Prompt outputs failed validation / Failed to convert an input value
///   to a INT value」——调用方完全看不出是自己少传了 seed。
/// - **不属于允许集** —— 调用方永远传不进来（`negative_prompt` 之类），
///   那不是调用方的错，是模板该写 `{{negative_prompt|}}`。放行，
///   由「试跑」的 unresolved 列表去提醒模板作者。
pub fn check_unresolved(workflow: &str, unresolved: &[String]) -> Result<(), String> {
    let missing: Vec<&str> = unresolved
        .iter()
        .map(String::as_str)
        .filter(|n| ALLOWED_PARAM_SLOTS.contains(n))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let other: Vec<&str> = unresolved
        .iter()
        .map(String::as_str)
        .filter(|n| !ALLOWED_PARAM_SLOTS.contains(n))
        .collect();
    Err(format!(
        "工作流 {workflow} 需要这些参数，本次没传：{}。{}",
        missing.join(", "),
        if other.is_empty() {
            "尺寸不传时网关会补 3x4".to_string()
        } else {
            format!(
                "另有 {} 是模板自带的占位符，调用方传不了，\
                 请在模板里给它们加默认值（如 {{{{ {} |}}}}）",
                other.join(", "),
                other[0]
            )
        }
    ))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    pub id: i64,
    pub name: String,
    pub mode: WorkflowMode,
    /// 该工作流可用的 ComfyUI 端点（空 = 全部启用端点）。
    pub node_ids: Vec<i64>,
    /// ComfyUI API 格式的 workflow JSON（模板模式下含占位符）。
    pub comfy_workflow: Value,
    /// 从 `comfy_workflow` 推导出的模型 id，调用方用它选工作流。
    ///
    /// 加载时算好放进快照，请求路径上就不用反复解析整张图。
    /// 推不出来时是空串 —— 由 [`resolve_by_model`] 报错，不在这里静默兜底。
    pub model: String,
    pub price_micro: i64,
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn params(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn the_three_allowed_slots_all_pass() {
        let p = params(json!({"prompt": "cat", "size": "3x4", "seed": 7}));
        assert!(validate_params(&p).is_ok());
    }

    /// 收紧的整个意义就在这条：以前 seed 要看工作流声明，现在无条件放行；
    /// 而 negative_prompt 之类**任何时候**都进不来。
    #[test]
    fn seed_and_size_are_always_allowed() {
        for k in ALLOWED_PARAM_SLOTS {
            let v = if k == "size" {
                json!("3x4")
            } else {
                json!("v")
            };
            let p = params(json!({ k: v }));
            assert!(validate_params(&p).is_ok(), "{k} 应当被放行");
        }
    }

    #[test]
    fn negative_prompt_is_rejected() {
        let e = validate_params(&params(json!({"negative_prompt": "blurry"}))).unwrap_err();
        assert!(e.contains("negative_prompt"), "{e}");
    }

    /// 尺寸不自由传：`width` / `height` 不在允许集里，
    /// 想传具体像素必须经由 `size` 预设。
    #[test]
    fn raw_pixels_are_rejected() {
        for bad in [
            json!({"width": 1024}),
            json!({"height": 1024}),
            json!({"width": 1024, "height": 768}),
        ] {
            let p = params(bad.clone());
            let e = validate_params(&p).unwrap_err();
            assert!(e.contains("width") || e.contains("height"), "{bad} → {e}");
        }
    }

    #[test]
    fn only_the_two_presets_are_valid_sizes() {
        for good in ["3x4", "16x9"] {
            let p = params(json!({ "size": good }));
            assert!(validate_params(&p).is_ok(), "{good} 应当是合法尺寸");
            assert!(resolve_size(good).is_some());
        }
        for bad in ["1x1", "768x1024", "3:4", "", "3X4", "landscape"] {
            let p = params(json!({ "size": bad }));
            let e = validate_params(&p).unwrap_err();
            assert!(
                e.contains("3x4") && e.contains("16x9"),
                "错误里要列出可选值：{e}"
            );
            assert!(resolve_size(bad).is_none());
        }
    }

    /// size 传成非字符串要报错，且同样要告诉调用方能传什么。
    #[test]
    fn non_string_size_is_rejected() {
        for bad in [json!(42), json!(null), json!(["3x4"]), json!({"w": 768})] {
            let p = params(json!({ "size": bad }));
            let e = validate_params(&p).unwrap_err();
            assert!(e.contains("3x4"), "{bad} → {e}");
        }
    }

    #[test]
    fn presets_resolve_to_the_agreed_pixels() {
        assert_eq!(resolve_size("3x4"), Some((768, 1024)));
        assert_eq!(resolve_size("16x9"), Some((1024, 576)));
    }

    /// 比例得是精确的，不能是「看起来差不多」——latent 尺寸不是 16 的倍数时
    /// 部分采样器会自己裁剪裁回来，反而不如固定值可控。
    #[test]
    fn presets_are_exact_ratios_and_16_aligned() {
        // (名字, 目标比例 宽:高)
        let targets = [("3x4", 3i64, 4i64), ("16x9", 16, 9)];
        assert_eq!(SIZE_PRESETS.len(), targets.len());

        for ((name, w, h), (_, ar_w, ar_h)) in SIZE_PRESETS.iter().zip(targets.iter()) {
            assert_eq!(w % 16, 0, "{name} 宽 {w} 不是 16 的倍数");
            assert_eq!(h % 16, 0, "{name} 高 {h} 不是 16 的倍数");
            // 交叉相乘判精确比例，避免整数除法把 7:4 当成 16:9
            assert_eq!(
                w * ar_h,
                h * ar_w,
                "{name} 实际 {w}x{h} 不是精确的 {}:{}",
                ar_w,
                ar_h
            );
        }
    }

    /// 报错要同时说出「我传错了什么」和「能传什么」，否则调用方只能猜。
    #[test]
    fn error_lists_both_the_bad_key_and_the_allowed_set() {
        let e = validate_params(&params(json!({"steps": 20, "cfg": 7}))).unwrap_err();
        assert!(e.contains("steps"), "{e}");
        assert!(e.contains("cfg"), "{e}");
        for k in ALLOWED_PARAM_SLOTS {
            assert!(e.contains(k), "错误信息里应列出可传的 {k}：{e}");
        }
    }

    /// 曾经「扫描模板得出槽位」的设计下，negative_prompt 是合法槽位。
    /// 这条守住它现在进不来——不然有人会想把校验退回旧逻辑。
    #[test]
    fn arbitrary_keys_never_pass() {
        for k in [
            "steps",
            "cfg",
            "denoise",
            "sampler",
            "image",
            "batch_size",
            "",
        ] {
            let p = params(json!({ k: "v" }));
            assert!(validate_params(&p).is_err(), "{k} 不该被放行");
        }
    }

    #[test]
    fn empty_params_are_fine() {
        assert!(validate_params(&serde_json::Map::new()).is_ok());
    }

    // ── 工作流解析：请求体里已经没有 workflow 字段了 ──

    /// 造一条带主模型 checkpoint 的模板工作流。
    ///
    /// 走真的 ComfyUI 节点名（UnetLoaderGGUF）而不是随便一个 key ——
    /// 测试要是用假结构，推导逻辑和真实模板就对不上了。
    fn wf_with_model(name: &str, checkpoint: &str, enabled: bool) -> Workflow {
        let mut w = wf(name, enabled);
        w.comfy_workflow = serde_json::json!({
            "3": {"class_type": "KSampler", "inputs": {"model": ["34", 0]}},
            "34": {"class_type": "UnetLoaderGGUF", "inputs": {"unet_name": checkpoint}},
        });
        w.model = derive_model(&w.comfy_workflow).unwrap_or_default();
        w
    }

    fn wf(name: &str, enabled: bool) -> Workflow {
        Workflow {
            id: 1,
            name: name.into(),
            mode: WorkflowMode::Template,
            node_ids: vec![],
            comfy_workflow: serde_json::Value::Object(Default::default()),
            model: String::new(),
            price_micro: 0,
            enabled,
        }
    }

    // ── 模型名推导 ──

    #[test]
    fn model_id_strips_the_checkpoint_extension() {
        let w = wf_with_model("z", "z_image_turbo-Q8_0.gguf", true);
        assert_eq!(w.model, "z_image_turbo-Q8_0");

        for (file, want) in [
            ("flux1-dev.safetensors", "flux1-dev"),
            ("anything.safetensors", "anything"),
            ("noext", "noext"),
            ("  spaced.safetensors  ", "spaced"),
        ] {
            assert_eq!(
                derive_model(&wf_with_model("x", file, true).comfy_workflow).unwrap(),
                want
            );
        }
    }

    /// VAE / CLIP 换了不该改 model 名。判据是「谁会填上它」——
    /// 组件升级是无害的，改掉调用方的 model id 是破坏性的。
    #[test]
    fn vae_and_clip_loaders_are_not_mistaken_for_the_model() {
        let tpl = json!({
            "17": {"class_type": "VAELoader", "inputs": {"vae_name": "flux_ae.safetensors"}},
            "32": {"class_type": "CLIPLoaderGGUF", "inputs": {"clip_name": "Qwen3-4B-Q8_0.gguf"}},
            "34": {"class_type": "UnetLoaderGGUF", "inputs": {"unet_name": "z_image_turbo-Q8_0.gguf"}},
            "5":  {"class_type": "LoraLoader", "inputs": {"lora_name": "style.safetensors"}},
        });
        assert_eq!(derive_model(&tpl).unwrap(), "z_image_turbo-Q8_0");
    }

    #[test]
    fn sd_checkpoint_loader_is_recognised() {
        let tpl = json!({
            "4": {"class_type": "CheckpointLoaderSimple", "inputs": {"ckpt_name": "dreamshaper.safetensors"}},
        });
        assert_eq!(derive_model(&tpl).unwrap(), "dreamshaper");
    }

    /// 图里只有 VAE / CLIP 没有主模型时必须报错，不能随便拿一个当模型。
    #[test]
    fn a_graph_without_a_model_loader_is_an_error() {
        let tpl =
            json!({"17": {"class_type": "VAELoader", "inputs": {"vae_name": "ae.safetensors"}}});
        let e = derive_model(&tpl).unwrap_err();
        assert!(e.contains("没有模型加载器"), "{e}");
    }

    /// 同一张图里两个不同的主模型（高低噪那种）不能替调用方挑一个。
    #[test]
    fn two_different_models_in_one_graph_is_an_error() {
        let tpl = json!({
            "34": {"class_type": "UnetLoaderGGUF", "inputs": {"unet_name": "high.safetensors"}},
            "35": {"class_type": "UnetLoaderGGUF", "inputs": {"unet_name": "low.safetensors"}},
        });
        let e = derive_model(&tpl).unwrap_err();
        assert!(e.contains("high") && e.contains("low"), "{e}");
    }

    /// 两个节点指向**同一个** checkpoint 不算冲突，照常出模型名。
    #[test]
    fn two_loaders_of_the_same_checkpoint_are_one_model() {
        let tpl = json!({
            "34": {"class_type": "UnetLoaderGGUF", "inputs": {"unet_name": "same.safetensors"}},
            "35": {"class_type": "UNETLoaderGGUF", "inputs": {"unet_name": "same.safetensors"}},
        });
        assert_eq!(derive_model(&tpl).unwrap(), "same");
    }

    // ── 按模型选工作流 ──

    /// 只有一个启用模型时，调用方不传 model 也能用。
    #[test]
    fn sole_model_is_picked_implicitly() {
        let list = vec![wf_with_model("zit-scene", "z_image_turbo-Q8_0.gguf", true)];
        assert_eq!(
            resolve_by_model(&list, None).unwrap().name,
            "zit-scene",
            "只有一个模型时不该逼调用方传 model"
        );
    }

    /// 多个模型时必须显式指定，并列出可选值。
    #[test]
    fn several_models_require_an_explicit_choice() {
        let list = vec![
            wf_with_model("a", "z_image_turbo-Q8_0.gguf", true),
            wf_with_model("b", "flux1-dev.safetensors", true),
        ];
        let e = resolve_by_model(&list, None).unwrap_err();
        assert!(e.contains("2"), "{e}");
        assert!(
            e.contains("z_image_turbo-Q8_0") && e.contains("flux1-dev"),
            "{e}"
        );

        assert_eq!(
            resolve_by_model(&list, Some("flux1-dev")).unwrap().name,
            "b"
        );
    }

    /// 传了个不存在的模型 → 报错并列出能用的，不静默退回唯一那条。
    #[test]
    fn unknown_model_is_an_error_listing_what_exists() {
        let list = vec![wf_with_model("a", "z_image_turbo-Q8_0.gguf", true)];
        let e = resolve_by_model(&list, Some("sdxl")).unwrap_err();
        assert!(
            e.contains("sdxl") && e.contains("z_image_turbo-Q8_0"),
            "{e}"
        );
    }

    /// 同一个模型挂两条启用工作流 = 又导入了重复模板，必须报错。
    /// 这是去重护栏在 model 维度上的平移。
    #[test]
    fn two_enabled_workflows_for_one_model_is_refused() {
        let list = vec![
            wf_with_model("zit-scene", "z_image_turbo-Q8_0.gguf", true),
            wf_with_model("zit-1024", "z_image_turbo-Q8_0.gguf", true),
        ];
        let e = resolve_by_model(&list, None).unwrap_err();
        assert!(e.contains("z_image_turbo-Q8_0"), "{e}");
        assert!(e.contains("zit-scene") && e.contains("zit-1024"), "{e}");
    }

    /// 推不出模型名的启用工作流不能被静默跳过：跳过了，
    /// 「只有一个模型」的结论就是假的。
    #[test]
    fn an_enabled_workflow_without_a_model_blocks_resolution() {
        let list = vec![
            wf_with_model("a", "z_image_turbo-Q8_0.gguf", true),
            wf("broken", true),
        ];
        let e = resolve_by_model(&list, None).unwrap_err();
        assert!(e.contains("broken"), "{e}");
    }

    /// 停用的坏工作流不参与——没启用的东西不该拦住出图。
    #[test]
    fn disabled_workflows_are_ignored_entirely() {
        let list = vec![
            wf_with_model("a", "z_image_turbo-Q8_0.gguf", true),
            wf("broken", false),
            wf_with_model("b", "flux1-dev.safetensors", false),
        ];
        assert_eq!(resolve_by_model(&list, None).unwrap().name, "a");
    }

    #[test]
    fn no_enabled_workflow_is_refused() {
        let list = vec![wf_with_model("a", "z_image_turbo-Q8_0.gguf", false)];
        let e = resolve_by_model(&list, None).unwrap_err();
        assert!(e.contains("没有启用的出图工作流"), "{e}");
    }

    /// 空串等同于没传。调用方拼 URL 或读环境变量很容易带出空值，
    /// 那不该被当成「找名叫空串的模型」。
    #[test]
    fn empty_model_string_falls_through_to_the_sole_one() {
        let list = vec![wf_with_model("a", "z_image_turbo-Q8_0.gguf", true)];
        assert_eq!(resolve_by_model(&list, Some("")).unwrap().name, "a");
    }

    /// raw 通道不参与按模型选：调用方整份提交 workflow，没有模型可选。
    #[test]
    fn raw_workflows_are_not_selectable_by_model() {
        let mut raw = wf("raw-one", true);
        raw.mode = WorkflowMode::Raw;
        let list = vec![raw, wf_with_model("a", "z_image_turbo-Q8_0.gguf", true)];
        assert_eq!(resolve_by_model(&list, None).unwrap().name, "a");
        assert_eq!(resolve_raw(&list).unwrap().name, "raw-one");
    }

    #[test]
    fn raw_channel_needs_exactly_one_raw_workflow() {
        let list = vec![wf_with_model("a", "z_image_turbo-Q8_0.gguf", true)];
        assert!(resolve_raw(&list).unwrap_err().contains("raw 通道需要一条"));

        let mut r1 = wf("r1", true);
        r1.mode = WorkflowMode::Raw;
        let mut r2 = wf("r2", true);
        r2.mode = WorkflowMode::Raw;
        let e = resolve_raw(&[r1, r2]).unwrap_err();
        assert!(
            e.contains("2") && e.contains("r1") && e.contains("r2"),
            "{e}"
        );
    }

    // ── 占位符体检 ──

    /// 承重规则：调用方漏传了它本来能传的参数，必须在网关侧拦下来。
    /// 放到 ComfyUI 的结果是「Prompt outputs failed validation」，
    /// 调用方根本看不出是自己少传了 seed。
    #[test]
    fn missing_allowed_slot_is_rejected_before_submit() {
        let e = check_unresolved("zit", &["seed".to_string()]).unwrap_err();
        assert!(e.contains("seed"), "{e}");
        assert!(e.contains("zit"), "{e}");
        assert!(!e.contains("ComfyUI"), "{e}");
    }

    /// 调用方永远传不进来的占位符不是它的错，放行。
    /// 那是模板该写 `{{negative_prompt|}}` 的事。
    #[test]
    fn non_allowed_placeholder_does_not_block_generation() {
        assert!(check_unresolved("zit", &["negative_prompt".to_string()]).is_ok());
        // 真实模板就是这个组合：negative_prompt 放行，seed 拦住
        let e = check_unresolved("zit", &["negative_prompt".to_string(), "seed".to_string()])
            .unwrap_err();
        assert!(e.contains("seed"), "{e}");
        assert!(e.contains("negative_prompt"), "应顺带提示模板作者：{e}");
    }

    #[test]
    fn nothing_unresolved_is_fine() {
        assert!(check_unresolved("zit", &[]).is_ok());
    }

    /// size 缺省后由网关补上，所以模板里的 width/height 不该再落到 unresolved。
    #[test]
    fn size_default_prevents_unresolved_size_slots() {
        let tpl = serde_json::json!({
            "3": {"inputs": {"seed": "{{seed}}"}},
            "13": {"inputs": {"width": "{{width}}", "height": "{{height}}"}}
        });
        let params = serde_json::json!({ "seed": 1 })
            .as_object()
            .cloned()
            .unwrap();
        let slots = crate::upstream::workflow::prepare_slots(&tpl, &params).unwrap();
        let (_, _, unresolved) = crate::upstream::workflow::render(&tpl, &slots).unwrap();
        assert!(
            !unresolved.contains(&"width".to_string())
                && !unresolved.contains(&"height".to_string()),
            "缺省 size 应补齐宽高：{unresolved:?}"
        );
    }
}
