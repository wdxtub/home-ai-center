# AGENTS.md

家庭内部 AI 网关：把 LM Studio / Ollama / vLLM / ComfyUI 收进统一的 OpenAI 兼容网关。
Rust（axum + tokio）+ SQLite 单容器，前端 React + Vite。

**业务与架构细节看 [README.md](README.md)，本文件只写动手前必须知道的流程与坑。**
计费口径、协议层设计、并发闸门、key 轮换规则都只在 README 维护，改那些地方前先读
README 对应小节，不要从代码反推意图。

## 硬约束：前端是编译期内嵌的

`include_dir!` 在 `cargo build` 时把 `web/dist` 打进二进制
（`crates/gateway/src/webui.rs`），`web/dist` 不存在会直接 panic。

所以**任何涉及前端的改动，顺序固定为「先 `npm run build`，再 `cargo build`」**，
反了会编不过或内嵌旧产物。`web/dist` 是 gitignore 的产物、不是源码。
Dockerfile 与 CI 都是两段构建，不要为了「省一步」合并成一段。

## 本地开发

```bash
# 后端 → :8080
set -a && . ./.env && set +a    # 必需，见下
cargo run -p home-ai-center

# 前端 → :5173，已把 /api 与 /v1 代理到 127.0.0.1:8080
cd web && npm run dev
```

**代码不读 `.env` 文件，只认进程环境变量** —— `config.rs` 用 `std::env::var`，
没有 dotenv 依赖。忘了 `set -a && . ./.env` 就起服务，管理员不会被初始化，
只在日志里留一句 warn。`docker compose up -d` 是唯一会自动读 `.env` 的路径。

`.env` 含管理员口令，已在 `.gitignore`；可提交的那份是 `.env.example`。
口令只在**首次启动**时用于初始化，改这个变量不会改已存的口令（要改走「设置 → 管理员」）。

## 验证

按改动范围跑，CI 跑的是全量：

```bash
cd web && npm run build          # 改了前端
cargo test --all                 # 258 项 lib 测试（无集成测试、无 doctest）
python3 scripts/audit_sql.py     # 改了 SQL 才需要
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
```

**完成判据是「原始复现消失」**：改动前能复现的失败，在同一条命令下不再出现。
只跑新加的测试、或只确认编译通过，都不算完成。

`audit_sql.py` 查的两类错误编译期查不出、运行时才炸，症状都是
「某个 NOT NULL 列莫名其妙是 NULL」：SQL 里 `?` 数量与 `.bind()` 次数不符、
INSERT 列数与 VALUES 值数不符。**改完 SQL 要跑它**，`cargo test` 覆盖不到。

### 迁移只能追加，不能改已应用的

`sqlx::migrate!` 会把每个迁移文件的 checksum 记进 `_sqlx_migrations`，
**改动已跑过的迁移文件会让下次启动直接失败**（checksum mismatch），
已有的 `./data` 库和所有部署都会起不来。

要改已上线的结构，只能新加 `000N_xxx.sql`；想纠正旧迁移的**注释**也一样，
新写一条迁移或写进代码注释里，别回去动 `0001`。

### fmt / clippy 基线是红的

`cargo fmt --all --check`（约 194 处 diff，横跨 35 个文件）和
`cargo clippy --all-targets -- -D warnings`（约 20 处）在 main 上就已失败，
**与你的改动无关**。

不要为了让检查变绿去跑 `cargo fmt --all` 或批量修 lint —— 那会混进几十个文件的
无关 diff，其中「函数参数过多」一类往往是有意的设计取舍。正确做法：只保证
**自己碰过的行**干净（`rustfmt --check <file>` 可单文件核对），交付时说明
「基线本就红，失败项与本次改动无关」。修基线是独立的、值得单独提的改动。

## 出图按 model 选，不按 workflow 选

调用方传的是 `model`（请求体**顶层**，和 `/v1/chat/completions` 一致），
工作流是 ComfyUI 图、是实现细节。**请求体里没有 `workflow` 字段**。

模型名**不是存出来的列**，而是 `domain::workflow::derive_model` 从模板的
checkpoint 文件名推出来的，所以没有 schema 变更，别为此加列。加了就会出现
「改了 checkpoint 忘了改名字」的两边不一致——那正是本仓库要根除的那类问题。

`MODEL_LOADERS` 只收主模型加载器，**VAE / CLIP / LoRA 加载器必须排除**。
判据是「谁会填上它」：换 `flux_ae.safetensors` 是无害的组件升级，
让它改掉调用方看到的 model 名就是破坏性变更。改这张表时照着真实的
ComfyUI 节点名和 input 字段名写，别凭印象。

**改 checkpoint 文件名就等于改 model**，现有调用方会立刻 404。所以管理台和
导入脚本都要把推导结果显式展示出来（`scripts/import_erotic_sci.py` 里有一份
**必须与 Rust 侧一致的** `MODEL_LOADERS` 副本，两边对不上时用户只会看到
「网关说没这个模型」）。

`resolve_by_model` 的缺省值来自**唯一性**，不是某条设置：只有一个启用的模型时
不传也能用，多于一个明确报错并列出可选值。**没有 `default_image_model` 那种
设置** —— 那种「配了不生效」要根除。两种情况一律报错而不是随便挑一条：
同一个模型有两条启用工作流（= 又导入了重复模板，是去重护栏在 model 维度上的
平移）；有启用的模板工作流推不出模型名（静默跳过会让「只有一个模型」的结论
变成假的）。

顶层再传 `workflow` 会 400（`api::images::reject_legacy_workflow_field`），
`model` 放进 `params` 也有专门报错 —— **都别改回静默忽略**。请求体是裸
`Value`，serde 不拦多余字段；吞掉的话调用方以为自己在选工作流，实际拿到的是
默认那条。判据是「调用方传了它，行为真的变了吗」。

## 出图参数面是收紧的，别放宽

模板模式只接受 `prompt` / `size` / `seed`，定义在
`domain::workflow::ALLOWED_PARAM_SLOTS`。**`width` / `height` 不在允许集里**：
尺寸不自由传，调用方只能在 `SIZE_PRESETS` 的 `3x4`(768×1024) / `16x9`(1024×576)
里选一个，由网关展开成宽高填进模板的 `{{width}}` / `{{height}}` —— 模板不用改。

真实提交路径（`api::images`）与「试跑」（`api::admin::dry_run_workflow`）
**必须走同一个 `workflow::prepare_slots`** —— 各写一份校验就会出现
「试跑通过、实际调用被拒」。

反向提示词、步数、denoise 之类要固定住，写在模板里：
`{{negative_prompt|}}`。**不要**给它们开放成调用方参数。

两个静默失败的坑：

1. `TextEncode` 这类输入节点不会因为收到 `{{negative_prompt}}` 这串字面量而报错，
   会照单全收当提示词用。所以 `workflow::render` 额外返回 `unresolved`
   （填不出来的占位符名），试跑要把它报出来。改动 `render` 的返回值时记得同步两个调用方。
2. `prepare_slots` **只注入模板真的引用到的** `width` / `height`。无脑注入的话，
   `zit-img2img`（没有宽高占位符）会让调用方只传了 `size` 却收到
   「没有消费这些参数：width, height」。
3. 出图前用 `domain::workflow::check_unresolved` 体检：**属于允许集**的占位符
   填不上就当场拒（比如调用方漏传 `seed`）。不拦的话 `{{seed}}` 字面量会送到
   ComfyUI，调用方拿到「Prompt outputs failed validation」，看不出是自己漏传。
   **不属于**允许集（`negative_prompt` 之类）只 warn 不拦 —— 那是模板该加默认值。

## 别加「只写不读」的列

`0004` 一次性清了四个字段，共同点是**没有任何执行路径读它们**：
`account.rpm_limit`（早期按 RPM 调度时代的遗留，改按并发度后就没用了）、
`llm_node_key.probe_failed`（零引用）、`count_false_positive`（从不写，恒为 0）、
`llm_node.capabilities`（只写不查，连「填了什么」都看不到）。

加新字段前先问一句：**谁读它？** 读不到就等于没加。确实需要预留，
就把理由写进迁移注释并让它进配置快照（能查），否则下一个人会当死字段删掉。


## 测试的两个反模式

**不要写死绝对日期。** key 重置时间要过 `clamp_reset`，它**拒绝已经过去的时刻**
（生产行为正确）。写死 `2026-10-01` 这种日期的测试，会在某天零点过后必然失败。
用 `keys::future_utc_date(2)` 取相对日期，它保证落在未来且在 8 天合理区间内。

**不要恒真断言。** `assert!(cond || true)` 恒通过、什么都没验，
是「把红测试变绿」最常见的写法。key 轮换、并发闸门这类承重模块的回归守卫一旦空转，
后面真出问题时没有任何东西挡着。assert 写不下去说明这条性质没想清楚，
该改测试设计而不是加 `|| true`。

## 其他容易踩的

- **Node 版本**：CI 用 22；本地 25 实测可用（vite 6）。仓库无 `rust-toolchain` 文件，
  Rust 下限由 `Cargo.toml` 的 `rust-version = 1.82` 约束。
- **`HOME_AI_PROBE_INTERVAL` 的单位是分钟**（`tasks/probe.rs` 内部 `* 60`），
  且下限 10 分钟。别按秒配置，也别把注释和文档写成秒。
- **`scripts/import_erotic_sci.py` 是数据迁移，不是环境配置**：依赖系统 python3
  的 pyyaml 和隔壁 `../erotic_sci` 仓库，**要求网关已启动**，会往库里写
  节点 / key / 路由 / 定价 / 账号。预演用 `--dry-run`。它幂等（按名字 upsert），
  但仍属于会改变数据的操作，执行前确认用户要。
- 上游地址要填到「端点前缀」（如 `http://host:7001/v1`），网关只在其后拼
  `/chat/completions`。这是配置问题不是代码问题，细节见 README。
