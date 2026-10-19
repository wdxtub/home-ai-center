# 架构：端到端请求生命周期

本文讲的是**一个请求从进程入口到响应返回，中间依次经过谁、判了什么、失败时往哪走**。

计费口径、协议语义、key 轮换规则这些**业务口径不在这里重复**，它们由
[README.md](../README.md) 维护。本文只补 README 讲不了的部分：模块之间的调用顺序、
分支的判定依据，以及每个设计决定背后的理由。

## 1. 启动装配

`crates/gateway/src/main.rs:29` 的顺序是承重的，不是随手排的：

1. 读环境变量配置（`config.rs:39`）→ 建数据目录
2. 连库 → 跑迁移（`main.rs:42-43`，迁移必须先于任何查询）
3. **崩溃对账**：`ledger::reconcile_holds` 把残留的 `held_micro` 一律清零（`main.rs:47`）
4. 初始化管理员（`auth::ensure_admin`，只在库里没有 `admin_user` 时执行）
5. 建 `AppState` → `state.reload()` 装载配置快照（`main.rs:66`）
6. 挂路由 → 起后台任务

第 3 步的位置有讲究：必须在「装快照」之前。预扣冻结是**唯一会写进 DB 的运行时态**，
进程被 kill 时它就悬在库里了。不清零的话，那个账号的可用额度会永久少一块，而且没有任何
迹象指向原因。取舍写在 `ledger.rs:207`：崩溃窗口内最多有一笔预留被释放，
**宁可少收，不可超收**。

第 5 步也必须在挂路由之前——否则第一个请求会读到空快照，表现为「刚重启的前几秒
所有模型 404」。

启动流程**没有降级分支**。任何一步失败都直接退出（`main.rs:12`）：配置不全的网关会以
很难排查的方式错账，不如起不来。

### 路由挂载的一个坑

网关面的路径**不带 `/v1` 前缀**，因为整个 Router 会被 `nest("/v1", ...)` 挂上去
（`main.rs:110-118`、`main.rs:201`）。这里再写一遍前缀，所有调用都会变成
`/v1/v1/chat/completions`，然后掉进 SPA 的 `fallback` 里拿到一个 200 的 HTML——
看起来像服务挂了，其实是路由写重了。

管理面拆成 `admin_public` 和 `admin` 两个 Router，原因是 `route_layer` 作用于它之前的
**全部**路由（`main.rs:121-122`）；混在一个 Router 里会把 `/auth/login` 自己挡在门外。

## 2. 网关面主路径：`/v1/chat/completions`

三个协议端点（Chat / Responses / Messages）共用**同一条准入链路**，
差别只在入站解析和出站渲染（`api/v1.rs:3-5`）。以非流式 Chat 为例：

```text
POST /v1/chat/completions
  │
  ├─① 鉴权        api/auth.rs:58      SHA-256 比对 → (Account, ApiKey)
  ├─② 入站解析     protocol/chat.rs   请求体 → UnifiedRequest (IR)
  ├─③ 定价校验     api/v1.rs:230      没配单价 → 直接 400
  ├─④ 预扣        billing/ledger.rs:27  条件更新冻结，不够 → 402
  │
  ├─⑤ key 闸门    gate/slot_gate.rs   调用方那把 key 的并发名额
  │      ↓ for _attempt in 0..=max_rot
  ├─⑥ 选点        gate/node_gate.rs   占用率最低 ∧ 有可用 key ∧ 未冷却
  │              keys/mod.rs:165     最久未用优先挑一把上游 key
  ├─⑦ 渲染        protocol/upstream_chat.rs:16
  │              + 节点 extra_body 覆盖（v1.rs:359）
  │              + 换成上游模型名     （v1.rs:369）
  ├─⑧ 上游调用     upstream/openai.rs 内网优先，连接级失败才回退公网
  │
  ├─⑨ 实扣退差     billing/ledger.rs:124  释放冻结 + 按真实 usage 扣余额
  └─⑩ 记账        billing/usage.rs:48   request_log + usage_daily 一次写入
```

### ① 鉴权：一次 JOIN

`auth.rs:69-80` 一次 JOIN 同时取 key 和 account，注释写明了原因：分两次查的话，
中间可能有人轮换 key，两个对象对不上号。

key 只存 SHA-256 哈希，**不是 Argon2**（`auth.rs:5-7`）。网关每请求都要验一次，
而 Argon2 的参数是刻意选来慢的——那是给低频场景（管理员口令，`auth.rs:189`）准备的。
这里是个明确的口令哈希强度取舍：可预付的家庭网络里，数据库被读走无法直接冒用，
比抗离线爆破更重要。

出站凭证和管理台口令是两套完全独立的凭证，互不通用。

### ③④ 定价与预扣的顺序

先查价再预扣，顺序不能反：没有单价就没法算预扣额。**没配价直接 400 是刻意的**，
不允许免费白嫖。

预扣走条件更新 `WHERE balance_micro - held_micro >= reserved`（`ledger.rs:41-42`），
受影响 0 行即 402。判据是「余额 − 冻结」而不是「余额」：出图动辄几分钟，
并发请求会同时看到「余额充足」然后一起透支。

`Reservation`（`v1.rs:133`）是预扣的持有者，用 `Drop` 兜底：最后一个持有者析构时若仍未
结算就把冻结全额还回去（`v1.rs:189-215`）。**宁可少收一次钱，也不能让一笔冻结永久挂死。**
它活不到 `Drop` 的极端情况（进程直接没了）由启动对账回收——两层保险，对应的还是那句话。

### ⑤⑥ 两个闸门的顺序

**先 key 闸门，后节点闸门**（`v1.rs:323` 的注释）：先拿到公平额度再去抢节点。反过来会让
「一把 key 只能排 4 个」的约束失效——先占满节点再发现 key 额度用尽。

key 名额要**占满整个请求**，包括中途换上游 key、换节点、以及整个流式过程
（`HeldSlots`，`v1.rs:664-668`）。流式尤其关键：流没结束就还名额，等于并发闸门对
流式请求完全不生效——而流式恰恰是最耗资源的那种。额度挂在 **key** 上而不是 account 上
（`state.rs:119-124` 记了这个演进：早期闸门在账号层，那时 key 只是 account 的一列，
「同账号多把 key 各自限流」在结构上做不到）。

### ⑥ 选点：占用率，不是绝对并发

`node_gate.rs:186-194` 写清楚了：按 `active/limit` 最低者优先，并列再比 `active`。
按绝对并发排的话，「1 台 1 并发 + 1 台 4 并发」时那台快的会被一直塞满。

**key 可用性是候选条件的一部分**（`node_gate.rs:240`）：全部 key 冷却/隔离时该节点
自动退出选点，而不是选完点再发现没 key 再打补丁。

全部节点满载时不直接失败，而是找**最空闲的**那个排队等（`node_gate.rs:300-311`），
整体超时由 `HOME_AI_QUEUE_TIMEOUT` 兜底。

### ⑦ 渲染：单一方言

`protocol/upstream_chat.rs` 是**唯一一份上游渲染**。三个客户端协议在入站归一到
`UnifiedRequest`，出站再由各自渲染器还原（`v1.rs:928-935`）。

`model` 字段在最后覆写（`v1.rs:369`），**放在节点 `extra_body` 之后**：
路由是最终裁决者。不做这一步的话，同一模型在不同节点上用不同别名（灰度、区分量化版本）
时会一直把对外名发过去，上游要么报「没这个模型」，要么——像 LM Studio 那样——回一个空补全。

### ⑧ 上游调用与三类失败

`openai.rs:19-38` 的 `UpstreamError` 把失败分成**处置完全不同**的几类，这是整个
请求路径上最需要小心的一张表：

| 变体 | 处置 | 动节点冷却？ | 换 key？ |
| --- | --- | --- | --- |
| `Quota` | 换一把重试 | 否 | 是 |
| `Transient` | 原地退避（1–30 秒） | 否 | 否 |
| `ModelLoading` | 原地等 5 秒 | 否 | 否 |
| `Node` | 换点 | 是（按 hard/soft） | 否 |
| `Transport` | 换点（所有地址都试过了） | 是 | 否 |
| `Client` | 直接抛给客户端 | 否 | 否 |

`Quota` / `Transient` 之所以**不判节点离线**：限额是 key 的问题、瞬时错误本来就是重试信号，
把它们算进节点退避会让一次上游限流把健康节点拖进冷却。分类逻辑在
`keys/classify.rs:85` 起的**有序决策表**里，注释明确警告「顺序即优先级，不要重排」。

内网优先 + 自动回退（`v1.rs:377-411`）：候选地址是「内网 → 公网」，**只有连接级失败**
才换下一个——连上了但限额、模型没加载、内容报错，说明这个地址是通的，换地址只会把
同一个错误再犯一遍，还白等一个超时。

内网不可达只标记该节点的**内网地址** 2 分钟，**不进节点冷却**（`node_gate.rs:64-74`）：
同一台机器的公网地址可能好好的，一次换网络不该把所有节点拖下线。而且**只有内网自己打通
才清标记**（`v1.rs:409`）——用公网成功不能清，否则每个请求都重新去撞一次不通的内网。

回退那一次尝试必须自带**连接超时**（`openai.rs:54`，2 秒）。内网那台关机时内核往往不是
refuse，而是把 SYN 丢进黑洞，TCP connect 要挂到系统级超时（macOS 75 秒）。
回退写得再对，每次先白等 75 秒也等于没做。这里用的是一个**只有 connect_timeout 更短的
同款 client**（`openai.rs:45-50`），不是整体超时——整体超时会连长生成一起砍掉。

### ⑨⑩ 结算与记账

`ledger::settle` 在单事务里释放全部冻结、按真实扣余额，差额自动还回（`ledger.rs:141-151`）。

`usage::write` 一次写入同时落 `request_log` 和 `usage_daily`（`usage.rs:60-126`），
后者用 `ON CONFLICT DO UPDATE` 累加。两次写分开做的话，明细和汇总迟早对不上。

**上游没给 usage 时按字符估算，并在明细里标 `tokens_source = "estimated"`**，
绝不记 0（`v1.rs:438-441`、`v1.rs:727-733`）。记 0 等于白嫖，而且客户端看到的 usage
会和服务端账上扣的对不上。

LLM 侧**不记 prompt / completion 正文**（`usage.rs:1-6`），这是产品决策不是未完成项。
唯一的例外是出图提示词（见第 3 节）。

## 3. 流式：为什么结算挂在流的析构上

流式是整个生命周期里最容易出错的一段，因为**费用在响应体里还没产生完的时候就已经把
响应头发出去了**。

```text
上游 SSE ──► parse_sse_stream ──► UnifiedEvent ──► FramePump ──► 客户端帧
                (ChunkState)        (归一事件)      (协议渲染)      (SSE 字节)
                                                              │
                            StreamTail 挂在流上 ◄──────────────┘
                            └─ 持有 Reservation + HeldSlots
                            └─ Drop 时 spawn 一次结算
```

三个要点：

1. **结算必须挂到流上。** token 数**只有 `Done` 事件里才有**，所以非流式那种
   「返回前算钱、返回响应」的写法在流式上会把「还没生成的 token」记成 0。
   `StreamTail`（`v1.rs:674`）的 `Drop` 覆盖了三条路径：流读完、客户端半路掐断、错误返回。
   客户端掐断时上游的 token 已经花了，账必须照记。

2. **额度事件的顺序是反的。** 开了 `stream_options.include_usage` 之后上游的收尾是：

   ```text
   data: {... "choices":[{"delta":{},"finish_reason":"stop"}]}   ← 先给 finish_reason
   data: {... "choices":[], "usage":{...}}                       ← 后给 usage
   data: [DONE]
   ```

   在 `finish_reason` 块上就发 `Done`，后面那个真正带 token 数的块会被丢弃——
   **每一次流式请求都退化成估算计费**，而上游其实给了准确数字。
   `ChunkState`（`upstream_chat.rs:287`）把 `finish_reason` 记下来，等 usage 块或
   `[DONE]` 哨兵才真正发 `Done`。

3. **EOF 时必须补终止事件。** `FramePump::finish`（`v1.rs:832`）在没见到 `Done` 时
   补一个，并带上兜底 usage。漏掉 `message_stop` 会让 Anthropic SDK 永久挂起；
   兜底数字也必须和账上扣的一致。

Anthropic 是唯一需要**前置帧**的：`message_start` 必须第一帧就带上输入 token 数
（`v1.rs:645-658`），而上游要到流结束才给 `prompt_tokens`——只能用本地估算，
和预扣用的是同一套 `tokenize::estimate_request`。

## 4. 出图面：`/v1/images/generations`

出图**不走**上面的准入链路，它有自己的一套：没有 key 轮换、没有协议渲染，
但多了一段「提交前 / 提交后」的分界。

```text
POST /v1/images/generations
  │
  ├─① 定位工作流    domain/workflow.rs:63   resolve_by_model（按 model 选，不按 workflow 选）
  ├─② 拒绝遗留字段  api/images.rs:331       顶层传 workflow → 400
  ├─③ 定价          api/images.rs:88        单价 ≤ 0 → 400
  ├─④ 预扣          api/images.rs:101       单价 × n（n 缺省 1，钳到 64）
  ├─⑤ 渲染模板      upstream/workflow.rs:46 prepare_slots → render → check_unresolved
  ├─⑥ key 闸门      gate/slot_gate.rs      排队上限 600 秒（出图天然长任务）
  │
  ├─⑦ 选端点 + 提交  gate/comfy_gate.rs     ┐ 最多 3 次换端点
  │      ComfyClient::render                 ┘ **只有这一段允许 failover**
  ├─⑧ 轮询 + 取图    upstream/comfyui.rs:121  /history 轮询，/view 取字节
  │
  ├─⑨ 实扣          billing/ledger.rs:124  按**实际张数**计价
  ├─⑩ 记账          billing/usage.rs       kind="image"
  └─⑪ 记提示词      api/images.rs:360      image_prompt 表
```

**第 ⑦⑧ 步的分界是这一面最关键的结构。** ComfyUI 一次出图几十秒，`POST /prompt`
成功之后任务就绑死在那个端点上了；此时再换端点重投会产生**双倍计费 + 重复图**
（`comfy_gate.rs:1-7`）。所以代码刻意分成「选点 → 渲染 → 提交」和「轮询 → 取图」两段，
只有前一段允许 failover。`ComfyError::Job`（提交后失败）不重试，
`ComfyError::Workflow`（工作流本身错了）也不重试——换端点只是把同一个坏工作流再发一遍。

第 ⑪ 步的顺序不能反：`image_prompt.request_id` 外键指向 `request_log.request_id`
（`images.rs:309`、`:356-359`），明细没落库就插会撞外键。

第 ⑤ 步的 `prepare_slots` 被**真实提交路径和「试跑」共用**（`api/admin.rs:1495`）。
各写一份校验就会出现「试跑通过、实际调用被拒」。

第 ④ 步的 `n` 只是给预扣用的**提示**：批量大小藏在 workflow 的 `batch_size` 节点里，
网关看不见，所以让调用方给个提示，避免多图批次结算时余额被扣成负数。

`ComfyHealth` 是每次请求新建的（`images.rs:202`），不是进程级共享——这和 LLM 侧的
`NodeHealth` 相反。原因是出图的端点可用性在单次请求内就被判定完了，没有跨请求的退避
积累需求；冷却（300 秒）落在调用方重试的间隔上。

## 5. 为什么这样分层

README 的「几条容易被改坏的地方」列了条目，这里展开**为什么**。

### 单上游方言 + 薄适配器，而不是 N×N 互转矩阵

家里的后端（LM Studio / Ollama / OMLX / vLLM）只讲 OpenAI Chat Completions，
部分根本不支持 `/v1/responses`（`protocol/mod.rs:3-5`）。所以上游方言只有一种，
客户端协议只影响**入站解析**和**出站渲染**两件事。

3 个协议 × 2 个方向 = 6 个薄适配器，替代 3×3 的九组双向转换。九组互转意味着每组都要
单独测一遍「转过去再转回来语义不变」，而薄适配器只需要保证「IR → 目标协议」这一跳。
IR 保留是为了将来某个节点原生支持 Responses 时加一条分支即可。

**协议由请求路径决定，不嗅探请求体**（`protocol/mod.rs:23`）。Chat 和 Responses 的
请求体形状高度相似，嗅探等于猜——猜错的表现是「参数莫名其妙不生效」，
比明确报 404 难查得多。

### 自研 SlotGate，而不是 Semaphore

`Semaphore` 的许可数只能在构造时给定，运行期**只能增不能减**，也表达不了优先级。
而这里三条需求是硬的：

- **限额热改**：页面保存即生效。`limit` 和 `max_queue` 由调用方每次重算传入
  （`slot_gate.rs:151-152`），所以 `state.reload()` 之后调大限额能立刻放行排队者
  （`nudge`，`slot_gate.rs:225`），不必等下一次槽位释放。
- **双优先级队列**：`grant_locked` 遍历 `[Interactive, Batch]`，交互请求会插到
  已排队的批量请求前面。
- **取消安全**：等待者被 `timeout` 或外层 `select!` 取消时，若名额已授予但调用方
  还没接手，必须归还——否则该层**永久少一个槽位**。这是最容易写错的地方，所以
  `acquire` 的整个等待段包在一个栈上 RAII 守卫里（`WaitGuard`，`slot_gate.rs:257`），
  由 Drop 保证摘队/归还。`timeout_does_not_leak_the_slot` 和
  `cancelled_waiter_returns_its_slot` 是这条不变量的回归守卫。

### NodeHealth 进程级共享

`NodeHealth` 挂在 `AppState` 上（`state.rs:130`），**每个请求新建一份的话，
「标记离线 + 冷却」在下一个请求眼里根本不存在**，退避机制等于没写
（`node_gate.rs:196-199`）。

退避是指数的：连接拒绝走 60 秒到 30 分钟，超时（可能只是模型冷启动）走更轻的
30 秒到 5 分钟（`node_gate.rs:23-27`）。冷却到期自动回候选池，但**失败计数保留**
（`node_gate.rs:160-163`），连续失败会继续拉长下一次退避。

被动退避的缺口是：节点挂了要等下一次真实请求撞上才知道，而家里的机器常是
「睡一觉起来就好了」。所以 `tasks/probe.rs` 做复检，用**一次最小真实推理**
（`max_completion_tokens: 1`）而不是 `GET /models`——后者不触发模型加载，
「恢复」可能只是服务在跑而模型没加载（`probe.rs:6-9`）。复检只探**冷却中**的节点
（`probe.rs:29`），且走和真实请求同一套地址优先级。

### 额度事件先查后置

见 [§3](#3-流式为什么结算挂在流的析构上) 第 2 点。`ChunkState` 里 `pending_finish`
字段的存在本身就是这条设计的证据。

### key 排序键取自内存

`KeyRing::pick` 按 `last_used` 升序排，**取的是内存里的 `KeyRuntime.last_used` 而不是
DB 列**（`keys/mod.rs:161-184`）。DB 里的 `last_used_at` 一个请求才写一次，
在内存里选点时读到的永远是旧值，排序退化成「永远选第一把」——多 key 就等于单 key
（`keys/mod.rs:59-62`）。`pick_rotates_across_keys` 是这条的回归守卫。

配套的是 `sync` 里的**故意不覆盖**（`keys/mod.rs:132-134`）：从 DB 回灌 `last_used`
会让「刚用过」的 key 重新排到最前。

冷却状态则是**运行时态只在内存的唯一例外**（README 已列）：5 小时冷却若因容器重启丢失，
重启后会立刻再撞一次限额。`KeyRing::sync` 里 `cooldown_until` 以 DB 为准重新计算
（`keys/mod.rs:136-142`），`last_used` 保留内存值——同一个函数里两种处理方式，
因为这两种状态的跨重启语义不同。

### 2xx 不代表成功

OpenAI 兼容服务端有个坑习惯：路径写错也回 200，body 却是
`{"error":"Unexpected endpoint..."}`（LM Studio 原样如此）。`parse_response` 遇到没有
`choices` 的 body 会安静地解析出「一个空补全」，客户端收到空回答、网关照常按估算
token 计费（`upstream_chat.rs:190-241`）。配置错了却什么都看不出来。

三处静默失败各自有拦截，缺一不可：

| 场景 | 拦截点 |
| --- | --- |
| 2xx 里裹着错误信封 | `openai.rs:249` `error_envelope` |
| 非流式请求收到非 JSON | `openai.rs:122-134` |
| 流式请求收到不是 SSE 的内容 | `openai.rs:209-216` `looks_like_sse` |

第三处要留一条口子：有些服务端流式内容对但忘了写 `Content-Type`，所以先看内容像不像
SSE 帧再判（`openai.rs:260-267`）。

### 出图：拒绝而不是静默忽略

请求体是裸 `Value`，serde 不拦多余字段（`images.rs:326-330`）。顶层传 `workflow`
会拿到默认那条工作流的图，调用方以为自己在选工作流——「配了不生效」正是这个项目
要根除的一类问题。宁可 400 也要说清楚**实际会用哪条**。

模型名同理不落库，而是从模板的 checkpoint 文件名推导（`derive_model`，
`domain/workflow.rs:180`）。单一事实来源，不存在「换了模型忘了改名字」。代价是
**改 checkpoint 文件名就等于改 model**，调用方会 404，所以管理台和导入脚本都要把
推导结果显式展示出来。`MODEL_LOADERS` 只收主模型加载器，判据是「谁会填上它」——
换 `flux_ae.safetensors` 是无害的组件升级，让它改掉调用方看到的 model 名就是破坏性变更。

`resolve_by_model` 的缺省值来自**唯一性**而不是某条设置（`domain/workflow.rs:63`）：
只有一个启用的模型时不传也能用，多于一个明确报错并列出可选值。推不出模型名的启用
工作流**不能静默跳过**——那会让「只有一个模型」的结论变成假的。

## 6. 模块职责

| 模块 | 一句话职责 |
| --- | --- |
| `main.rs` | 启动装配、路由挂载、中间件、后台任务拉起 |
| `config.rs` | 全部配置来自环境变量（无 dotenv），数据目录管理 |
| `state.rs` | `AppState`：配置快照（`ArcSwap`）、三层闸门表、`NodeHealth` 共享 |
| `api/auth.rs` | 两套凭证：调用方 API Key（SHA-256）、管理台会话（Argon2 + Cookie） |
| `api/v1.rs` | 网关面准入链路、流式帧泵、`Reservation` 持有权 |
| `api/images.rs` | 出图面：定位工作流、模板渲染、提交/轮询分界、提示词记录 |
| `api/admin.rs` | 管理面 CRUD、试跑、运行态视图 |
| `api/error.rs` | 错误类型 + 按客户端协议渲染错误体 |
| `protocol/ir.rs` | 内部规范表示，`UnifiedUsage` 是计费唯一口径 |
| `protocol/upstream_chat.rs` | 唯一的上游渲染 + `ChunkState` 流式解析（先查后置） |
| `protocol/chat·responses·messages.rs` | 三个协议的入站解析与出站渲染（薄适配器） |
| `protocol/sse.rs` | SSE 帧编码（具名/匿名事件、多行负载拆分） |
| `protocol/tokenize.rs` | token 估算，预扣与 Anthropic 前置帧共用 |
| `gate/slot_gate.rs` | 并发闸门：双优先级队列、限额热改、取消安全 |
| `gate/node_gate.rs` | 节点选点（占用率最低）+ `NodeHealth` 冷却退避 |
| `gate/comfy_gate.rs` | ComfyUI 端点选点，端点级冷却 |
| `keys/mod.rs` | `KeyRing`：key 池运行态、最久未用优先选 key、冷却落库 |
| `keys/classify.rs` | 限额错误的有序决策表（先匹配先返回） |
| `keys/reset.rs` | 重置时间解析（多格式 + 合理性钳制） |
| `keys/window.rs` | 软上限分桶窗口计数（主动预测，不等 429） |
| `billing/ledger.rs` | 预扣 → 实扣 → 退差，单事务；崩溃对账 |
| `billing/usage.rs` | 调用明细与每日汇总，只存元数据不存正文 |
| `upstream/openai.rs` | OpenAI Chat 客户端：内网回退、错误分类、SSE 转发 |
| `upstream/comfyui.rs` | ComfyUI 客户端：提交 → 轮询 history → 取图 |
| `upstream/workflow.rs` | 模板占位符填充，真实提交与试跑共用 |
| `domain/workflow.rs` | 出图模型推导、参数白名单、尺寸预设、按 model 解析工作流 |
| `domain/node.rs` | 节点承载硬件属性（并发、时段、地址），模型映射是另一层 |
| `domain/pricing.rs` | 定价表与费用计算 |
| `domain/window.rs` | 每日禁用时段（跨午夜） |
| `db/load.rs` | 从 DB 重建配置快照，读路径零查库的基础 |
| `tasks/probe.rs` | 节点健康复检（最小真实推理） |
| `tasks/backup.rs` | 每日 `VACUUM INTO` 备份 + 滚动清理 |
| `tasks/retention.rs` | 调用明细保留期清理，`usage_daily` 永久保留 |
| `webui.rs` | 编译期内嵌 `web/dist` 的 SPA fallback |

## 7. 状态与并发模型

### 落库 vs 在内存

**配置（节点 / key / 路由 / 定价 / 工作流 / 设置）以 DB 为唯一事实源**，
运行时读 `ArcSwap<Snapshot>`，读路径零查库（`state.rs:1-5`）。
管理端写入在事务提交后 `reload()` 重建快照并广播——保存即生效，无需重启。

| 状态 | 位置 | 理由 |
| --- | --- | --- |
| 节点 / 路由 / 定价 / 工作流 | DB + 快照 | 事实源，配置要能查、能回滚 |
| 余额 `balance_micro` | DB | 账务必须持久、必须对得上 |
| 在途预扣 `held_micro` | DB | 跨崩溃可见，靠启动对账清理 |
| **key 冷却 / 隔离状态** | **DB + 内存** | **唯一例外**，见下 |
| key 轮换进度 `last_used` | 内存为主，DB 备份 | 排序要新鲜，见 §5 |
| 节点健康与冷却退避 | 内存 | 单进程内的临时判断，重启重新学 |
| ComfyUI 端点冷却 | 内存（每请求新建） | 单次请求内判定完，见 §4 |
| 并发占用 / 队列 | 内存 | 纯运行时态 |
| 软上限窗口计数 | 内存 | 高频更新，不值得每请求写库 |
| 调用明细 / 日汇总 | DB | 唯一被记录的请求元数据 |

**冷却状态落库是「运行时态只在内存」的唯一例外。** 5 小时冷却若因容器重启丢失，
重启后会立刻再撞一次限额——用户看到的是「刚重启就好了几秒，然后又坏了」。
`apply_verdict` 同时写内存和 DB，并把判定依据（命中的规则、信号、reset_source）
写进 `key_rotation_event`（`keys/mod.rs:286-304`），排障时能还原「哪次请求、命中哪条规则、
为什么冷却」。

### 配置热更新怎么生效

`reload()`（`state.rs:169-183`）做三件事：

1. 从 DB 重建 `Snapshot` 并 `store` 进 `ArcSwap` —— 读路径立刻看到新配置
2. `keys.sync()` 刷新 key 运行态（`cooldown_until` 以 DB 重算，`last_used` 故意保留）
3. 对每个节点和 ComfyUI 端点调 `nudge(max_concurrency)` —— **正在排队的请求按新限额
   重新评估一次**，不必等下一次槽位释放

管理端几乎每个写操作都以 `s.reload().await` 结尾。改节点地址时还会先
`mark_lan_up`（`admin.rs:795-798`）：标记按节点记，运营刚换上的新内网地址会被上一条
地址的旧标记压住，最长白等一个冷却周期。

### 并发原语

- `SlotGate` 内部：`Mutex<GateState>` + `Notify`。名额归还走 `SlotLease::drop`，
  释放幂等（`compare_exchange`）——panic 和取消路径都不会漏放
- `NodeHealth` 内部：`Mutex<HashMap<..>>`，冷却时刻用 **monotonic `Instant`** 而非
  系统时间，改系统时钟不会让退避错乱
- `KeyRing` 内部：`Mutex<HashMap<i64, KeyRuntime>>`
- 数据库：单事务，`WAL` 模式

## 8. 已知边界

**单进程单库，调度状态全在内存。** 这是最根本的一条。多副本部署会出现：两个进程各有
一份 `NodeHealth` 和各自的 `SlotGate`，一个进程把节点判离线，另一个照样往那儿发；
`held_micro` 虽然在 DB 里但预扣的并发正确性依赖单事务，跨进程没有互斥。
**调度不能水平扩展**，扩容的唯一正确方式是加节点（填更多 `llm_node`），
而不是加网关副本。

**崩溃窗口内的预扣会被全量释放**（`ledger.rs:207`）。进程在预扣和结算之间被 kill，
那笔冻结在下次启动时被清零，费用没收到。方向是刻意选的：少收一次比超收或永久冻结好查。

**节点的并发度是配置值，不是探测值。** 填 4 就是 4，网关不知道那台机器实际能跑几个。
`SlotGate` 保证的是「不超过 4」，不是「用满 4」。

**出图没有跨请求的端点健康积累**（§4），也没有 `stream`。ComfyUI 一次几十秒，
`POLL_TIMEOUT` 是 600 秒（`images.rs:39`），超了就是失败，不重投。

**流式一旦发出响应头就没法改状态码。** 所以流式路径上，上游在响应头之后才暴露的
错误只能以 SSE 帧的形式送给客户端（`UnifiedEvent::Error`），HTTP 状态码仍是 200。

**TLS 与反向代理不在范围内**，默认 HTTP，出图面和 LLM 面都不做 CORS 白名单
（`main.rs:205` 是 `CorsLayer::permissive()`）——家庭内网的前提是网络本身可信。
