# 数据模型

> 权威来源是 `crates/gateway/migrations/*.sql`。本文描述的是**六个迁移全部应用之后**的结构，
> 不是 `0001_init.sql` 的原始样子。两者有实质差异（见文末「迁移史里删掉的东西」）。

## 通用约定

这几条贯穿全部表，读表结构前先记住：

| 约定 | 说明 |
| --- | --- |
| **金额是整数微元** | `1 元 = 1,000,000 微元`，所有金额列都是 `INTEGER`，全程没有浮点。`*_micro` 后缀即此意。 |
| **时间戳是 unix 秒** | `INTEGER`，**存 UTC**。按天分桶和「禁用时段」用哪个时区，由 `HOME_AI_TIMEZONE` 决定，不在存储层。 |
| **布尔用 INTEGER** | `1` / `0`，`enabled` 列。 |
| **JSON 存 TEXT** | `extra_headers` / `extra_body` / `comfy_workflow` / `value`（`setting`）都是 TEXT 里放 JSON，靠应用层解析。 |
| **级联删除** | 子表的外键大多带 `ON DELETE CASCADE`。删账号会带走它的 key、请求明细、用量汇总、流水、出图提示词。 |

---

## 一、身份与配额

三层结构：**账号管钱，key 管频率，余额只挂在账号上**。

```
account (1) ──< api_key (N) ──< request_log
    │                  │
    │                  └── 并发额度：llm_max_concurrency / image_max_concurrency 等
    │
    └── balance_micro / held_micro   ← 钱只在这里
```

### `account` — 账号

余额、账目归属的主体。**不含任何 key 字段**（`0006` 拆走了）。

| 列 | 类型 | 说明 |
| --- | --- | --- |
| `id` | INTEGER PK | |
| `name` | TEXT UNIQUE | 账号名 |
| `enabled` | INTEGER | 是否启用 |
| `balance_micro` | INTEGER | 可用余额（微元） |
| `held_micro` | INTEGER | **预扣冻结中**的金额。准入时按最大可能费用加上去，实扣退差后减掉 |
| `created_at` / `updated_at` | INTEGER | |

> `held_micro` 是「预扣 → 实扣 → 退差」三段式里中间那一段的落点。只在结束时扣款的话，
> 并发请求会同时看到余额充足然后一起透支。

### `api_key` — 调用方 API Key

`0006` 引入。**一个账号多把 key，各自限流、各自统计**——「一台设备一把 key」是家庭网关的常态。

| 列 | 类型 | 说明 |
| --- | --- | --- |
| `id` | INTEGER PK | |
| `account_id` | INTEGER FK → `account` | CASCADE |
| `name` | TEXT | 给人看的名字，**同账号内唯一** |
| `key_hash` | TEXT UNIQUE | **只存 SHA-256**。库里被读走也无法直接冒用 |
| `key_prefix` | TEXT | 明文前缀（`sk-hac-` 之类），供管理台识别是哪把 |
| `enabled` | INTEGER | |
| `llm_max_concurrency` / `llm_max_queue` | INTEGER | LLM 侧并发与队列上限，**每把 key 独立一份** |
| `image_max_concurrency` / `image_max_queue` | INTEGER | 出图侧同上 |

- 明文 key **只在创建时显示一次**，之后无法取回。
- 额度从 `account` 下沉到 key：建 3 把 key 就是 3 份并发，账号层面不再单独设限。
  两层闸门会配出「key 限 100、账号总 20」这种没人看得懂的拒绝。

### `admin_user` / `session` — 管理台

| 表 | 列 | 说明 |
| --- | --- | --- |
| `admin_user` | `id` PK **CHECK (id = 1)**、`username`、`password_hash` | 单行表，天然只有一位管理员。用户名固定 `admin` |
| `session` | `token` PK、`expires_at`、`created_at` | 管理台会话，与调用方 API Key 是**完全独立**的两套凭证 |

`password_hash` 的具体算法见 `crates/gateway/src/api/admin.rs`。

---

## 二、LLM 节点与上游 Key

### `llm_node` — 上游节点

| 列 | 说明 |
| --- | --- |
| `name` UNIQUE | 节点名 |
| `kind` | 默认 `openai` |
| `base_url` | 公网/主地址，**要填到端点前缀**（如 `http://<host>:7001/v1`），网关只在其后拼 `/chat/completions` |
| `lan_base_url` | 内网地址。**内网优先 + 自动回退**：只有连接级失败（连不上/超时）才改走公网 |
| `max_concurrency` | 1..64，CHECK 约束 |
| `default_max_output_tokens` | 缺省输出上限 |
| `disabled_start` / `disabled_end` / `disabled_timezone` | 每日禁用时段，跨午夜直接填 `23 → 7`。起止相同会被拒绝 |
| `extra_headers` / `extra_body` | 节点级额外请求体，用来关思考（各家参数名不统一） |

### `llm_node_key` — 上游 API Key 池

一个节点可挂任意多把 key，跑不同组织 / 订阅账号。

> ⚠️ **`secret` 是明文存储的**，这是**刻意的选择**，迁移注释里写了理由：
> 假脱敏只会制造「key 对不上」的排障成本。API 列表只回 `label` + 末 4 位。
> 这是本项目与调用方 key（只存哈希）**最大的安全策略差异**，详见 [security.md](security.md)。

| 列 | 说明 |
| --- | --- |
| `node_id` FK | CASCADE，`UNIQUE (node_id, label)` |
| `label` / `secret` | 标签与**明文**上游密钥 |
| `state` | `active` / `cooling` / `quarantined` / `disabled`（CHECK） |
| `cooldown_until` / `reset_source` / `quota_class` / `matched_rule` / `matched_signal` | 冷却状态与命中规则。**冷却状态落库是「运行时态只在内存」的唯一例外**：5 小时冷却若因容器重启丢失，重启后会立刻再撞一次限额 |
| `rate_limit_scope` | `perKey`（默认）/ `account` |
| `soft_cap_window_ms` / `soft_cap_tokens` / `window_started_at` / `window_tokens_used` | 软上限窗口，提前切换、主动避开 429 |
| `count_429` / `count_rotations` / `last_used_at` | 统计。`last_used_at` 一个请求才写一次，**轮换排序键取自内存**，从 DB 读永远是旧值 |

### `key_rotation_event` — 轮换审计

`id, key_id, node_id, request_id, quota_class, matched_rule, matched_signal, cooldown_ms, reset_source, created_at`

排障时能看到「哪次请求、命中哪条规则、为什么冷却」。

### `llm_route` — 模型路由

| 列 | 说明 |
| --- | --- |
| `model_name` | **对外**模型名，调用方传的就是它 |
| `node_id` FK | CASCADE |
| `upstream_model` | 真正发给上游的 `model` 字段。**必须在请求路径上生效**，否则别名 / 灰度全是摆设 |
| `priority` | 多个节点时配合「并发占用率最低」选 |
| — | `UNIQUE (model_name, node_id)` |

### `model_price` — 定价

| 列 | 说明 |
| --- | --- |
| `model_name` PK | |
| `input_micro_per_1k` / `output_micro_per_1k` | 微元 / 1k token |
| `cached_input_micro_per_1k` | 缓存输入价，**可空**。不配就按普通输入价 |

> 1.2 元 / 百万 token = **1200 微元 / 1k**。
> **没配单价的模型会被直接拒绝（402），这是刻意的。**

---

## 三、出图

### `comfy_node` — ComfyUI 端点

结构与 `llm_node` 类似，但注意 **`lan_base_url` 是无条件优先、没有回退**。给 ComfyUI 配内网地址前
先想清楚那台机器关机时会发生什么。

`username` / `password` 是 ComfyUI 自身的凭证，同样明文。

### `workflow` — 出图工作流

| 列 | 说明 |
| --- | --- |
| `name` UNIQUE | |
| `mode` | `template`（默认，服务端存模板、客户端只传参数）/ `raw`（客户端提交完整 workflow） |
| `node_ids` | 关联的 ComfyUI 节点，JSON 数组 |
| `comfy_workflow` | **ComfyUI workflow JSON 原文** |
| `price_micro` | 单价。**不能是 0**，网关对未配价的工作流直接拒绝出图 |

> **模型名不是这张表的列。** 它由 `domain::workflow::derive_model` 从 `comfy_workflow` 里的
> checkpoint 文件名推导，所以改 checkpoint 文件名**就等于改 model**，现有调用方会立刻 404。
> 单一事实来源，不会出现「换了模型忘了改名字」。

`param_slots`（按工作流声明参数面）已在 `0003` 删除，改为全局常量
`domain::workflow::ALLOWED_PARAM_SLOTS` = `prompt` / `size` / `seed`。

---

## 四、计费与用量

### `request_log` — 调用明细

**只存元数据，不存 prompt / completion 正文**（建表注释里明写）。

| 分组 | 列 |
| --- | --- |
| 身份 | `request_id` UNIQUE、`account_id` CASCADE、**`api_key_id`** → `api_key` ON DELETE **SET NULL** |
| 目标 | `kind`（`llm`/`image`）、`protocol`（`chat`/`responses`/`messages`）、`target`、`node_id`、`node_name` |
| 上游 key | **`node_key_id`** → `llm_node_key`（`0006` 从 `key_id` 改名，避免与调用方的 `api_key` 混淆） |
| 结果 | `status`（`ok`/`error`/`rejected`）、`error_kind`、`error_message`、`stream`、`rotated_count` |
| 耗时 | `queue_wait_ms`、`latency_ms` |
| Token | `prompt_tokens`、`completion_tokens`、`cached_tokens`、`reasoning_tokens`、`tokens_source` |
| 计费 | `cost_micro`、`image_count` |

两个删除策略值得注意：

- `api_key_id` 用 **SET NULL**：调用方轮换 key 后**历史记录还要留着**；
  `ON DELETE CASCADE` 会把别人的历史账一起删掉。
- `account_id` 用 CASCADE：账号都没了，明细没有保留意义。

`tokens_source` 用来标记 token 是实测还是估算——上游不认 `stream_options.include_usage` 时按字符估算，
**不会记成 0**。

### `usage_daily` — 按天汇总

主键 `(day, account_id, api_key_id, target, node_id)`，**永久保留**（不受明细保留期影响）。
「每把 key 的 rpm / tpm / 张数」就是靠 `request_log` 上那个 `idx_request_log_api_key` 索引切出来的。

### `image_prompt` — 出图提示词

**唯一被记录的请求正文**。LLM 侧仍然一字不记。

| 列 | 说明 |
| --- | --- |
| `request_id` UNIQUE FK → `request_log(request_id)` | **ON DELETE CASCADE** |
| `account_id` | CASCADE |
| `workflow` / `prompt` | raw 模式没有具名提示词，记**空串** |
| `created_at` | |

独立成表而不是给 `request_log` 加一列，是为了保住那张表「元数据表」的语义和保留策略。
保留期跟随 `log_retention_days`，不单独设开关——两者无限增长是同一个问题。

### `balance_txn` — 余额流水

`id, account_id` CASCADE, `kind`（`topup`/`adjust`/`settle`/`refund`）, `amount_micro`, `balance_after`, `request_id`, `note`, `created_at`

`balance_after` 是快照，**可用来对账**：任意一条流水的 `balance_after` 应该等于当时该账号的 `balance_micro`。

---

## 五、运维表

| 表 | 说明 |
| --- | --- |
| `setting` | KV，`key` PK。已知的 `log_retention_days`（明细保留天数，默认 90）。`0005` 删掉了 `default_image_workflow` |
| `backup_record` | `path`、`size_bytes`、`status`（`ok`/`error`）、`error`、`started_at`、`finished_at`。备份有没有成功查这张表 |

---

## 迁移史里删掉的东西

迁移不只是建表，也在**主动删死字段**。这几条是理解当前 schema 为什么「缺东西」的关键：

| 迁移 | 删了什么 | 为什么 |
| --- | --- | --- |
| `0002` | `workflow.archive`、`archive_retention_days` | 归档写出去的 url 指向 `/api/admin/images/...`，而**这个路由从来没有存在过**；保留期配了从来没有执行者 |
| `0003` | `workflow.param_slots` | 「每个工作流自己声明参数面」等于把「调用方能改什么」交给模板作者，改为全局常量 |
| `0004` | `account.rpm_limit`、`llm_node_key.probe_failed`、`llm_node_key.count_false_positive`、`llm_node.capabilities` | 共同点是**没有任何执行路径读它们**。留着会让后来的人以为这些机制已生效 |
| `0005` | `setting.default_image_workflow` 那一行数据 | 「配了不生效」的典型：库里残留什么，出图结果就变成什么 |
| `0006` | `account` 上的 6 个 key / 额度列 | 并发额度下沉到 `api_key`；`idx_account_prefix` 必须**先删**再 DROP COLUMN，否则索引校验报错导致迁移失败 |

> `count_false_positive` 尤其能说明这套思路：它被读进内存，但三处结构体初始化又硬置 0，
> **从不写库**，所以恒为 0。要统计误报得先有「误报」的判定，那个判定至今不存在。

## 两条硬约束

**迁移只能追加，不能改已应用的文件。**

`sqlx::migrate!` 会把每个迁移文件的 checksum 记进 `_sqlx_migrations`。
**改动已经跑过的迁移文件会让下次启动直接失败**（checksum mismatch），已有的库和所有部署都起不来。
要改已上线的结构，只能新加 `000N_xxx.sql`；想纠正旧迁移的**注释**也一样。

**SQLite 的 `PRAGMA foreign_keys` 在事务内是空操作。**

`sqlx::migrate!` 把每个迁移文件包在一个事务里执行，所以**不要在迁移里写 `PRAGMA foreign_keys = OFF`**
——写了不但没用，还会掩盖重建父表触发的级联删除：外键仍开着 → 删 `account` 时 CASCADE 把子表数据
一并带走 → 迁移「成功」但**数据全没了**。`0006` 的做法是保留 `account` 原样，只用
`ALTER TABLE … DROP COLUMN` 删列（需要 SQLite 3.35+）。
