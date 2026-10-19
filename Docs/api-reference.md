# API 参考

本文以 `crates/gateway/src/main.rs` 的路由表为准，覆盖网关面（`/v1/*`，调用方用 API Key）
与管理面（`/api/*`，管理台口令）。**先看错误码那一节**——这个网关里大部分线上事故
最后都落在 402 / 502 / 503 上，而不是 400。

- 网关面端点：7 个（`/v1/*`）
- 管理面端点：50 个方法（`/api/admin/*`，其中 1 个免鉴权）
- 健康检查：1 个（`/api/health`，免鉴权）

---

## 一、网关面 `/v1/*`

### 1.1 鉴权：两套完全独立的凭证

网关面和管理台**互不通用**。管理台口令不能调 `/v1/*`，API Key 也进不了管理面。

调用方 API Key 有两种写法，任选其一（`x-api-key` 优先于 `Authorization`）：

| 头 | 值 | 适用 |
| --- | --- | --- |
| `Authorization` | `Bearer sk-hac-xxx` | OpenAI 系 SDK |
| `x-api-key` | `sk-hac-xxx` | Anthropic 系 SDK |

Key 在库里**只存 SHA-256 哈希**（网关每请求验一次，故不用 Argon2），前缀是明文前 8 字符，
用于在日志和管理台里辨认。明文只在创建时返回一次。

鉴权失败的四种 401（`error.message` 原文）：

| message | 含义 |
| --- | --- |
| `缺少 API Key：请用 Authorization: Bearer <key> 或 x-api-key 头` | 两个头都没带或为空 |
| `API Key 无效` | 哈希查不到 |
| `账号已停用` | 所属账号 `enabled=0` |
| `API Key「xxx」已停用` | key 自身 `enabled=0` |

> **网关不会返回 403。** 上游节点返回的 401/403 会被判为「这把 key 死了」并隔离，
> 对调用方表现为之后的 **503**，不是 403。

### 1.2 准入链路（三个协议端点共用）

`chat/completions`、`responses`、`messages` 走**完全相同**的一条链路，差别只在入站解析与出站渲染：

```
鉴权 → 查价 → 预扣冻结 → key 并发闸门 → 节点调度 → 渲染上游请求 → 调上游 → 结算
```

其中「查价」和「预扣」是最常见的失败点：

- **没配价直接 400**，不允许免费白嫖。
- **预扣按最大可能费用冻结**，冻结不足即 **402**。这条是条件更新（`balance - held >= amount`），
  并发请求不会同时看到「余额充足」然后一起透支。
- 流式请求的并发名额**占满整个流**，结算挂在流尾部的析构上。流被读完、被掐断、报错，
  三条路径都会结算；预扣未结算就析构时兜底全额释放。

### 1.3 `POST /v1/chat/completions`

OpenAI Chat Completions 协议。

**请求体**

| 字段 | 类型 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- | --- |
| `model` | string | **是** | — | 缺失 → 400 `model 必填` |
| `messages` | array | **是** | — | 缺失 → 400 `messages 必填` |
| `messages[].role` | string | 否 | `user` | `system` / `developer` / `assistant` / `tool` / 其他 |
| `messages[].content` | string \| array | 否 | — | 数组时支持 `{"type":"text","text":…}` 与 `{"type":"image_url","image_url":{"url":"data:…;base64,…"}}` |
| `messages[].tool_call_id` | string | 否 | — | 仅 `role:"tool"` |
| `messages[].tool_calls` | array | 否 | — | 仅 `role:"assistant"`，回填工具结果时用 |
| `max_completion_tokens` / `max_tokens` | number | 否 | — | 前者优先，两个名字都收 |
| `temperature` / `top_p` | number | 否 | — | |
| `stop` | string \| array | 否 | — | 超过上游上限会截断，日志记 `stop_truncated` |
| `tools` | array | 否 | — | OpenAI 的 `{"type":"function","function":{name,description,parameters}}` |
| `tool_choice` | string \| object | 否 | — | `auto` / `none` / `required` / `{"function":{"name":…}}` |
| `stream` | bool | 否 | `false` | |
| `reasoning_effort` | string | 否 | — | 映射成内部 `thinking_effort` |
| `user` | string | 否 | — | 落库的 `metadata` |

`role:"system"` / `"developer"` 的内容会被抽出来合成内部 `system` 字段（多条用空行拼接）。

**响应体**

| 字段 | 说明 |
| --- | --- |
| `id` / `object` / `created` / `model` | `object` 恒为 `chat.completion` |
| `choices[0].message.content` | 文本 |
| `choices[0].message.reasoning_content` | 思考内容，有才出现 |
| `choices[0].message.tool_calls[]` | `{id, type:"function", function:{name, arguments}}`，`arguments` 是 **JSON 字符串** |
| `choices[0].finish_reason` | `stop` / `length` / `tool_calls` / `content_filter`；未知值归一为 `stop` |
| `choices[0].logprobs` | 恒为 `null` |
| `usage.prompt_tokens` | **含** `cached_tokens`，不要相加 |
| `usage.completion_tokens` | **含** `reasoning_tokens` |
| `usage.total_tokens` | 上面两项之和 |
| `usage.prompt_tokens_details.cached_tokens` | >0 时出现 |
| `usage.completion_tokens_details.reasoning_tokens` | >0 时出现 |

```bash
curl http://192.168.1.10:8080/v1/chat/completions \
  -H "Authorization: Bearer sk-hac-xxx" \
  -H "content-type: application/json" \
  -d '{
    "model": "qwen3-8b",
    "messages": [{"role": "user", "content": "你好"}],
    "max_completion_tokens": 256
  }'
```

### 1.4 `POST /v1/responses`

OpenAI Responses 协议。

**请求体**

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `model` | string | **是** | |
| `input` | string \| array | 否 | 字符串等价于一条 user 消息 |
| `input[].type` | string | 否 | `function_call` / `function_call_output` / `reasoning` / 其余按 role 解析 |
| `input[].call_id` | string | 工具项必填 | 回填工具结果用 `call_id`，不是 `id` |
| `input[].content` | string \| array | 否 | 支持 `input_text` / `text` / `output_text` / `input_image` |
| `instructions` | string | 否 | 合成内部 `system` |
| `max_output_tokens` | number | 否 | |
| `temperature` / `top_p` | number | 否 | |
| `stop` | array | 否 | **只收数组**，不收字符串 |
| `tools` | array | 否 | 扁平结构 `{"type":"function","name":…,"parameters":…}`；非 `function` 类型会被跳过 |
| `tool_choice` | string \| object | 否 | |
| `stream` | bool | 否 | |
| `reasoning.effort` | string | 否 | |
| `metadata.user_id` | string | 否 | |
| `previous_response_id` | — | **禁止** | **传了直接 400** |

> **`previous_response_id` 显式拒绝**，不是忽略。网关无服务端会话状态，
> 静默忽略会返回「看起来成功但上下文丢了」的结果。请客户端自行携带完整历史。

`input` 里 `type:"reasoning"` 的项会被**丢弃**（Chat 方言无处安放），不伪造。

**响应体**

| 字段 | 说明 |
| --- | --- |
| `object` | 恒为 `response` |
| `status` | `completed` 或 `incomplete` |
| `incomplete_details.reason` | 截断时为 `max_output_tokens` |
| `output[]` | `{"type":"message",…}` 与 `{"type":"function_call","call_id":…,"arguments":…}` |
| `error` | 成功时恒为 `null` |
| `usage.input_tokens` / `output_tokens` / `total_tokens` | 同 Chat 的包含口径 |

**截断不是 `finish_reason`**，而是 `status:"incomplete"`。只看终止事件名会把截断当成功。

```bash
curl http://192.168.1.10:8080/v1/responses \
  -H "Authorization: Bearer sk-hac-xxx" \
  -H "content-type: application/json" \
  -d '{"model":"qwen3-8b","input":"用一句话介绍你自己","max_output_tokens":200}'
```

### 1.5 `POST /v1/messages`

Anthropic Messages 协议。这是三者里约束最紧的一套。

`anthropic-version` 头**不强制**：缺失只记一条 debug 日志，不阻断。

**请求体**

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `model` | string | **是** | |
| `messages` | array | 否 | 缺省空数组 |
| `messages[].content` | string \| array | 否 | 块类型：`text` / `image` / `tool_use` / `tool_result` / `thinking` |
| `messages[].content[].source` | object | 图片块必填 | `{"type":"base64","media_type":…,"data":…}` |
| `system` | string \| array | 否 | 顶层字段，**不在 messages 里** |
| `max_tokens` | number | 协议上必填 | 缺失时网关用节点的 `default_max_output_tokens` 补，再兜底 `4096`；不报错 |
| `stop_sequences` | array | 否 | |
| `tools` | array | 否 | **schema 字段叫 `input_schema`**，不是 `parameters` |
| `tool_choice` | object | 否 | `{"type":"auto"\|"any"\|"none"\|"tool","name":…}`；`any` 等价于 Chat 的 `required` |
| `thinking.effort` / `output_config.effort` | string | 否 | |
| `metadata.user_id` | string | 否 | |
| `stream` / `temperature` / `top_p` | | 否 | |

**响应体**

| 字段 | 说明 |
| --- | --- |
| `type` | 恒为 `message` |
| `content[]` | `{"type":"thinking","thinking":…,"signature":""}` / `{"type":"text","text":…}` / `{"type":"tool_use","id","name","input"}` |
| `stop_reason` | `end_turn` / `max_tokens` / `tool_use` / `refusal` |
| `usage.input_tokens` | **已减去**缓存部分 |
| `usage.output_tokens` | |
| `usage.cache_creation_input_tokens` | 恒为 `0` |
| `usage.cache_read_input_tokens` | 缓存命中数 |

> Anthropic 定义总输入 = `input_tokens` + `cache_creation` + `cache_read`（**三项相加**），
> 而上游 Chat 的 `prompt_tokens` 已经含了 `cached_tokens`。网关的换算是
> `input_tokens = prompt_tokens - cached_tokens`，三项相加恰好等于真实输入。
> `thinking` 块的 `signature` 只能是空串——上游 Chat 没有这个概念。

```bash
curl http://192.168.1.10:8080/v1/messages \
  -H "x-api-key: sk-hac-xxx" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{"model":"qwen3-8b","max_tokens":256,"messages":[{"role":"user","content":"你好"}]}'
```

### 1.6 `GET /v1/models`

列出**已配置路由的模型**（快照里 `model_routes` 的键，排序后返回）。

| 响应字段 | 说明 |
| --- | --- |
| `object` | 恒为 `list` |
| `data[].id` | 模型名，即路由用的名字 |
| `data[].object` | 恒为 `model` |
| `data[].created` | 恒为 `0`（网关不记录创建时间） |
| `data[].owned_by` | 恒为 `home-ai-center` |

**这里不列出出图模型。** 出图模型在 `GET /v1/workflows`。

### 1.7 `POST /v1/images/generations`

出图。**这是本接口最容易用错的地方**，规则先看四条：

1. `model` 放**请求体顶层**（与 `/v1/chat/completions` 及 OpenAI 出图端点一致）。
2. `params` 里**只允许** `prompt` / `size` / `seed` 三个键。
3. `size` 只有 `3x4`(768×1024) 与 `16x9`(1024×576) 两组，不传默认 `3x4`。
   **`width` / `height` 不在允许集**，传了直接 400。
4. 响应只有 `b64_json` 和 `filename`，**网关不存图**。

**请求体**

| 字段 | 类型 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- | --- |
| `model` | string | 条件必填 | — | 见下方「唯一性规则」 |
| `params.prompt` | string | 条件必填 | — | 模板引用了 `{{prompt}}` 而没传 → 400 |
| `params.size` | string | 否 | `3x4` | 只能是 `3x4` / `16x9` |
| `params.seed` | number | 条件必填 | — | 模板引用了 `{{seed}}` 而没传 → 400 |
| `n` | number | 否 | `1` | **仅作预扣额度提示**，clamp 到 `1..64`；实际返回张数由 ComfyUI 决定 |
| `mode` | string | 否 | — | `"raw"` 时走原始通道，`params.workflow` 需给完整 workflow |
| `workflow` | — | **禁止** | — | 顶层传这个直接 400 |

**这两条 400 是故意的，报错里会告诉你实际会用哪条：**

- 顶层传 `workflow`：调用方以为在选工作流，实际拿到的是默认那条——**静默吞掉比报错更糟**。
  裸 JSON 多余字段默认不报错，宁可 400 也要说。
- `model` 放进 `params`：`params` 是模板占位符白名单，`model` 不是占位符。

**模型唯一性规则**（没有 `default_image_model` 这种设置）：

| 情况 | 行为 |
| --- | --- |
| 恰好一个启用的模板模型 | 不传 `model` 也能用 |
| 多于一个 | **400**，报错列出全部可选项 |
| 同模型有两条启用工作流 | **400**（重复模板，需停用一条或改 checkpoint） |
| 有启用模板推不出模型名 | **400**（不静默跳过） |
| 无启用的出图工作流 | **400** |

模型名不是存的列，而是 `derive_model` 从模板的 checkpoint 文件名推出来的
（剥掉 `.safetensors` / `.gguf` 等后缀）。只认主模型加载器，VAE / CLIP / LoRA 不算。
**改 checkpoint 文件名就等于改 model**，现有调用方会立刻 404。

**响应体**

| 字段 | 说明 |
| --- | --- |
| `created` | unix 秒 |
| `data[].b64_json` | 图片 base64（标准编码） |
| `data[].filename` | ComfyUI 给的文件名 |
| `usage.images` | 实际张数 |
| `usage.cost_micro` | 本次结算微元（1 元 = 1,000,000 微元） |

```bash
# 只有两个模型之一时，model 可以省
curl http://192.168.1.10:8080/v1/images/generations \
  -H "Authorization: Bearer sk-hac-xxx" \
  -H "content-type: application/json" \
  -d '{
    "model": "z_image_turbo-Q8_0",
    "params": {"prompt": "一只猫", "size": "16x9", "seed": 42}
  }'

# 典型 400：把 model 放进了 params
curl http://192.168.1.10:8080/v1/images/generations \
  -H "Authorization: Bearer sk-hac-xxx" \
  -H "content-type: application/json" \
  -d '{"params":{"model":"z_image_turbo-Q8_0","prompt":"一只猫"}}'
```

**为什么 `width` / `height` 不开放**：`size` 会被展开成 `width` / `height` 填进模板的
`{{width}}` / `{{height}}` 占位符，所以模板照旧写占位符、不用跟着改；而且**只有模板真的
引用到宽高占位符时才注入**——无脑注入会让没有宽高占位符的图生成工作流
在你只传了 `size` 时报「没有消费这些参数：width, height」。

**占位符填不上时的两类处理**（`check_unresolved`）：

- 属于允许集（`prompt` / `size` / `seed`）→ **出图前直接 400**。否则 `{{seed}}` 字面量
  会送到 ComfyUI，调用方拿到的是 `Prompt outputs failed validation`，看不出是自己漏传。
- 不属于允许集（`negative_prompt` 等，调用方永远传不进来）→ **放行**，只记 warn。
  那是模板该写 `{{negative_prompt|}}` 的事，不该让调用方为模板背锅。

反向提示词、采样步数、denoise 之类全部固定在模板里，**不对外开放成参数**。

**出图的 failover 边界**：只有**提交之前**能换 ComfyUI 端点（最多 3 次）。
`/prompt` 一旦成功，任务就绑死在那个端点上——重投会双倍扣费并产生重复图。

### 1.8 `GET /v1/workflows`

出图侧的「模型列表」。列的是**模型**，不是工作流。

| 响应字段 | 说明 |
| --- | --- |
| `id` | **调用方传回来的就是这个值** |
| `object` | 恒为 `model` |
| `workflow` | 哪条工作流实现的，排障用，调用方不用管 |
| `params` | 全局固定的可传参数面：`["prompt","size","seed"]` |
| `sizes` | `[{size,width,height}]`，两组 |
| `default_size` | `"3x4"` |
| `default` | **布尔值**：为 `true` 表示「不传 `model` 会被接受」（仅当只有一个模型） |
| `price_micro` | 单价，微元 |

推不出模型名的启用工作流**不进这个列表**，保证这里报出来的与实际能选的一致。

### 1.9 `GET /v1/usage`

查自己的余额与今日用量。**余额是账号的（共用），并发额度是这把 key 的（独立）**——
一个账号多把 key 时，每把都看得到全量余额但只有自己的 key 用量。

| 响应字段 | 说明 |
| --- | --- |
| `account` | 账号名 |
| `balance_micro` / `held_micro` / `available_micro` | 余额 / 冻结中 / 可用 |
| `key.id` / `name` / `prefix` | 当前这把 key |
| `key.llm_max_concurrency` / `image_max_concurrency` | 这把 key 的并发上限 |
| `today` | `{cost_micro, tokens, requests, day}`，**按账号**，本地时区 |
| `today_by_key` | **四元数组** `[cost_micro, tokens, requests, images]`，**按这把 key** |
| `requests_by_protocol_30d` | 近 30 天按协议的请求数（出图记为 `llm`） |

> `today_by_key` 是数组不是对象，字段顺序固定为
> `[费用微元, token 数, 请求数, 出图张数]`。

### 1.10 流式要点

响应头：`content-type: text/event-stream`、`cache-control: no-cache`、
`x-accel-buffering: no`（关掉 nginx 缓冲，否则流式退化成阻塞）。

三套协议的 SSE 形态完全不同，**混用会挂起**：

| | OpenAI Chat | OpenAI Responses | Anthropic Messages |
| --- | --- | --- | --- |
| 事件名 | 无（只有 `data:`） | 具名 `event:` + `data:` | 具名 `event:` + `data:` |
| 索引 | `choices[].index` | `sequence_number`（流内**全局单调**计数器） | `content_block.index` |
| 终止信号 | `data: [DONE]` | `response.completed` / `.incomplete` / `error`，**无 `[DONE]`** | `message_stop`，**无 `[DONE]`** |
| usage 位置 | 倒数第二个 chunk，`choices: []` | **只在**终止事件的 `response` 对象里 | 出现**两次**（见下） |
| 流中错误 | `{"error":{message,type}}` 后跟 `[DONE]` | `error` 事件，**扁平**（不嵌套在 `error` 键下） | `error` 事件，`{"type":"error","error":{…}}` |

要点：

- **Chat 末块的 `choices` 是空数组且只带 usage**，这是 OpenAI 的既定形状，客户端照此解析。
- **Responses 每个事件都必须带 `sequence_number`**，不是块索引。
- **Anthropic 的 usage 出现两次**：`message_start` 立刻带 `input_tokens`（且 `output_tokens: 1`），
  `message_delta` 带**累积**的 `output_tokens`（取最后一次，累加会虚增）。
- **usage 可能是估算值。** 上游没给 usage 时（老后端常见），网关按已收到的字符数估算并
  **绝不记 0**；此时日志里 `tokens_source` 为 `estimated`（正常是 `upstream`）。
  记 0 等于白嫖，且客户端看到的 usage 会和账上扣的对不上。
  Anthropic 的 `message_start` 里的输入 token **总是估算的**——上游只在流结束时才给。
- **流式请求的计费在流结束时才发生。** 客户端提前断开也会走结算（按已收到的正文估算）。
- 截断在 Chat 是 `finish_reason:"length"`，在 Responses 是 `status:"incomplete"`。

```bash
curl -N http://192.168.1.10:8080/v1/chat/completions \
  -H "Authorization: Bearer sk-hac-xxx" \
  -H "content-type: application/json" \
  -d '{"model":"qwen3-8b","stream":true,"messages":[{"role":"user","content":"你好"}]}'
```

---

## 二、管理面 `/api/admin/*`

鉴权用**管理员口令**换来的会话 token，与网关面的 API Key **互不通用**：

- 登录后拿到 `hac_session` Cookie（`HttpOnly; SameSite=Strict; Path=/`，有效期 7 天），
  浏览器管理台用它；
- 也可把 token 放 `x-admin-token` 头，供脚本化调用。

`/api/admin/auth/login` **免鉴权**，其余全部需要会话。会话失效返回 401
（`未登录管理台` / `会话已过期，请重新登录`）。改口令会**作废所有会话**。

一次写入 = 一个事务 + 一次快照重建，**保存即生效，无需重启**。

### 2.1 会话

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| POST | `/api/admin/auth/login` | 登录（免鉴权）。body `{username, password}`，返回 `{token, expires_at}` 并下发 Cookie。口令错也走完校验再返回，避免用响应时间探测用户名 |
| POST | `/api/admin/auth/logout` | 登出，清 Cookie |
| GET | `/api/admin/auth/me` | 当前管理员 `{id, username, created_at}` |
| PUT | `/api/admin/auth/password` | 改口令。body `{old_password, new_password}`，新口令至少 8 字符 |

### 2.2 账号与调用方 Key

账号只管**钱**；key 和并发额度是另一套。一个账号可有多把 key，余额共用。

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| GET | `/api/admin/accounts` | 账号列表 |
| POST | `/api/admin/accounts` | 建账号（会顺带发一把初始 key，明文只出现一次） |
| PUT | `/api/admin/accounts/{id}` | 改账号（改名 / 调余额 / 启停） |
| DELETE | `/api/admin/accounts/{id}` | 删账号 |
| POST | `/api/admin/accounts/{id}/api-keys` | 在账号下再建一把 key（明文只出现一次） |
| PUT | `/api/admin/api-keys/{id}` | 改 key（并发上限、启停） |
| DELETE | `/api/admin/api-keys/{id}` | 删 key |
| POST | `/api/admin/api-keys/{id}/rotate` | 轮换这把 key（只换这一把，不动账号其他 key） |
| GET | `/api/admin/api-keys/{id}/stats` | 这把 key 的用量读数；带 `minutes` 看分钟趋势，不带只要今日汇总 |
| POST | `/api/admin/accounts/{id}/topup` | 充值 |
| GET | `/api/admin/accounts/{id}/txns` | 账目流水 |

### 2.3 节点与节点 Key

节点 key 是**上游那把 LLM key**，与上一节的「调用方 key」是两回事。

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| GET | `/api/admin/nodes` | 节点列表 |
| POST | `/api/admin/nodes` | 建节点（base_url 要填到**端点前缀**，如 `http://<node-a>:7001/v1`） |
| PUT | `/api/admin/nodes/{id}` | 改节点（含 `extra_body`、节点级默认参数） |
| DELETE | `/api/admin/nodes/{id}` | 删节点 |
| GET | `/api/admin/nodes/{id}/keys` | 节点下的 key 列表 |
| POST | `/api/admin/nodes/{id}/keys` | 建节点 key |
| PUT | `/api/admin/keys/{id}` | 改节点 key |
| DELETE | `/api/admin/keys/{id}` | 删节点 key |
| POST | `/api/admin/keys/{id}/clear-cooldown` | 手动清冷却（限额要等人点一下才能恢复） |
| GET | `/api/admin/keys/{id}/events` | 这把 key 的冷却/轮换事件 |

### 2.4 路由、定价、ComfyUI

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| GET | `/api/admin/routes` | 路由列表（模型名 → 节点 + 上游模型名） |
| POST | `/api/admin/routes` | upsert 路由 |
| DELETE | `/api/admin/routes/{id}` | 删路由 |
| GET | `/api/admin/prices` | 定价列表 |
| POST | `/api/admin/prices` | upsert 定价（**不能填 0**：未配价会被直接拒绝） |
| DELETE | `/api/admin/prices/{model}` | 删定价 |
| GET | `/api/admin/comfy-nodes` | ComfyUI 端点列表 |
| POST | `/api/admin/comfy-nodes` | 建 ComfyUI 端点 |
| PUT | `/api/admin/comfy-nodes/{id}` | 改端点 |
| DELETE | `/api/admin/comfy-nodes/{id}` | 删端点 |

### 2.5 出图工作流

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| GET | `/api/admin/workflows` | 工作流列表（含推导出来的 `model`） |
| POST | `/api/admin/workflows` | 建工作流（`mode` 为 `template` 或 `raw`） |
| GET | `/api/admin/workflows/{id}` | 工作流详情 |
| PUT | `/api/admin/workflows/{id}` | 改工作流 |
| DELETE | `/api/admin/workflows/{id}` | 删工作流 |
| POST | `/api/admin/workflows/{id}/dry-run` | **试跑**：只做填充不提交，报出 `unresolved` 占位符 |

> 试跑与真实提交**必须走同一套 `prepare_slots` 校验**。两份校验会出现
> 「试跑通过、实际调用被拒」的排障噩梦。

### 2.6 用量、概览、设置、备份

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| GET | `/api/admin/logs` | 调用明细日志 |
| GET | `/api/admin/usage/summary` | 用量汇总 |
| GET | `/api/admin/overview` | 总览卡片：今天 / 本月 / 余额 / 在线节点 |
| GET | `/api/admin/runtime` | 运行时面板：闸门实时占用 |
| GET | `/api/admin/settings` | 读设置 |
| PUT | `/api/admin/settings` | 写设置（保存即生效） |
| GET | `/api/admin/backups` | 备份列表 |
| POST | `/api/admin/backups/run` | 立即备份 |
| POST | `/api/admin/backups/purge` | 按保留期清理 |

### 2.7 健康检查

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| GET | `/api/health` | **免鉴权**。返回 `{ok, version, uptime_secs, nodes, comfy_nodes, models, workflows, frontend}`；DB 不可用时 503 |

---

## 三、错误与状态码

### 3.1 统一错误结构（按客户端协议渲染）

同一份 `ApiError`，响应体形状跟着调用方用的协议走：

```jsonc
// OpenAI Chat
{ "error": { "message": "…", "type": "invalid_request_error", "param": null, "code": null } }

// OpenAI Responses（HTTP 层）
{ "error": { "message": "…", "type": "invalid_request_error", "code": null } }

// Anthropic Messages
{ "type": "error", "error": { "type": "invalid_request_error", "message": "…" }, "request_id": "…" }
```

响应头可能带 `retry-after`（429 / 503）与 `x-request-id`（便于对日志）。

`type` 的词表两套不重合：

| HTTP | OpenAI `type` | Anthropic `error.type` |
| --- | --- | --- |
| 400 | `invalid_request_error` | `invalid_request_error` |
| 401 | `authentication_error` | `authentication_error` |
| 402 | `insufficient_quota` | `billing_error` |
| 404 | `invalid_request_error` | `not_found_error` |
| 429 | `rate_limit_error` | `rate_limit_error` |
| 500 / 502 | `api_error` | `api_error` |
| 503 | `server_error` | `overloaded_error` |

> 404 在 OpenAI 侧也是 `invalid_request_error`，且 404 走的是 `openai_type()`——
> Anthropic 的 `not_found_error` 只在 `/v1/messages` 上出现。

### 3.2 逐码说明

| 码 | 触发条件 | 排查方向 |
| --- | --- | --- |
| **400** | `model` / `messages` 必填缺失；`previous_response_id` 被拒；**模型未配置单价**；出图 `params` 里有非允许键（含 `model`）、`size` 非法或传了 `width`/`height`；顶层传 `workflow`；`model` 无路由/有歧义/模板推不出名字；允许集占位符没填；ComfyUI 判定工作流本身非法 | 报错的 `message` 里会列出**实际能传什么**。按它改请求体，不要猜 |
| **401** | 缺 key、key 无效、账号或 key 停用；管理面未登录/会话过期/口令不对 | 检查 `Authorization` / `x-api-key` 头；管理面重新登录 |
| **402** | **余额不足**。预扣阶段条件更新失败，报 `可用 X 微元，本次需要 Y 微元（余额 … / 冻结 …）` | 充值，或等在途请求结算释放冻结 |
| **404** | 模型没有可用节点（未配路由，或节点停用 / 落在禁用时段）；工作流指定的 ComfyUI 端点一个都没有 | 补路由或启用节点；检查工作流的 `node_ids` |
| **429** | 队列已满或排队超时（key 并发闸门、节点闸门、ComfyUI 闸门三处都算） | 带 `retry-after`，退避重试。默认 LLM 排队超时 120s（`HOME_AI_QUEUE_TIMEOUT`），出图为 600s |
| **500** | 内部错误（数据库错误等）。**日志里有真实原因，响应里统一是「内部错误」** | 看网关日志 |
| **502** | 上游在重试与 key 轮换后仍失败 | 见下方 3.3 |
| **503** | 当前无可用资源：无节点，或**全部 key 已限额**。报 `最早恢复：<时间>（约 N 分钟后）`，带 `retry-after` 与 `overloaded_error` | 等限额恢复，或手工在管理台清冷却 |

### 3.3 502 详解：上游在 2xx 里裹着错误信封

**这是最容易踩的坑：HTTP 状态是 200，响应体却是一段错误。**

LM Studio 在 base_url 少写 `/v1` 时会返回 200 +

```json
{ "error": "Unexpected endpoint or method. (POST /chat/completions)" }
```

不挑出来的话，它会被当成「一个没有 `choices` 的正常补全」——**客户端收到空回答，
网关还照常按估算 token 计费**，配置错了却什么都看不出来。

网关会检查 2xx 响应体里的 `error` 字段，认出后当作**软失败**：不冷却节点、不轮换 key
（上游是通的，只是这次没给结果），但把原因**响亮地**报成 502，message 形如：

```
上游返回 2xx 但响应体是错误：… 。若 base_url 只填到了端口，请补上 /v1 —— 网关会在其后拼 /chat/completions。
```

其他 502 来源：

- `模型正在加载，请重试` —— 节点活着但模型在冷启动，原地等 5 秒后重试，不动冷却。
- 瞬时错误按 `Retry-After` 或 5 秒（clamp 到 1–30）等待后重试，**不换 key**。
- 节点硬故障 → 冷却该节点并换点，最多轮换 `HOME_AI_MAX_KEY_ROTATION`（默认 3）次。
- 出图侧：所有 ComfyUI 端点提交前都失败（超时 / 端点不可达），或 ComfyUI 没返回任何图片。

**结论：2xx 不代表成功。** 网关面拿到 502 时，第一反应应是「检查 `base_url` 有没有写全
端点前缀」，而不是盲目重试。

### 3.4 排查速查

| 现象 | 先看 |
| --- | --- |
| 所有模型都 400 且提示「未配置单价」 | 去管理台配价（LLM 与出图**分开**配） |
| 偶发 402，但余额看着够 | 有在途请求冻结着（`/v1/usage` 的 `held_micro`） |
| 一直 503 带「最早恢复」 | 上游 key 限额，等恢复或管理台清冷却 |
| 200 但内容为空 / 502 提示 base_url | `base_url` 缺 `/v1` 前缀 |
| 出图 400 说「没有消费这些参数」 | 参数与模板槽位不匹配；宽高只按模板占位符注入 |
| 出图 400 说「没传 seed」 | 模板引用了 `{{seed}}` 但请求没给 |
| 改了模板后调用方 404 | 改了 checkpoint 文件名 = 改了 model 名 |

---

## 附：本文档的核对方式

端点清单逐条对照 `crates/gateway/src/main.rs` 的 `router()` 路由表；
错误码与响应结构对照 `crates/gateway/src/error.rs`；
请求/响应字段对照 `crates/gateway/src/protocol/{chat,responses,messages}.rs`
与 `crates/gateway/src/api/{v1,images,auth}.rs`；
出图规则对照 `crates/gateway/src/domain/workflow.rs`。
未逐一验证的细节请以源码为准。
