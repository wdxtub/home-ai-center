# 家庭内部 AI 网关

把家里散落的 LM Studio / Ollama / vLLM / ComfyUI 收进一个统一的 OpenAI 兼容网关，
顺便管住**谁在用、用多少、并发排到谁头上**。

- **单容器部署**，数据全在一个可挂载的 SQLite 里
- **三种客户端协议**：OpenAI Chat、OpenAI Responses、Anthropic Messages，各含流式与非流式
- **按节点并发度调度与排队**，而不是只按 RPM
- **同 provider 多把 API Key 自动轮换**，撞了 5 小时限额自动切下一把
- **预付费余额 + 并发额度**，账目精确到 token
- **每日自动备份**，滚动 7 天

---

## 快速开始

```bash
cp .env.example .env
# 改掉 HOME_AI_ADMIN_TOKEN
docker compose up -d
```

打开 <http://localhost:8080>，用 `admin` + `.env` 里的口令登录。

**先配单价再调模型**：没配单价的模型会被直接拒绝（402），这是刻意的。

---

## 三分钟配置

管理台里有七页，按这个顺序配：

### 1. LLM 节点

填名称、上游地址、最大并发。

> **上游地址要填到「端点前缀」，和 OpenAI SDK 的 `base_url` 一个意思**：
> 网关只在其后拼 `/chat/completions`。所以 LM Studio 填 `http://n1-lms.wdxmzy.com:7001/v1`，
> **带 `/v1`**；只填到端口会变成 `POST /chat/completions`。
>
> 这个坑不好察觉：LM Studio 对错误路径回的是 **200 + `{"error":"Unexpected endpoint..."}`**，
> 不是 404。网关会把它认出来并报 502（见「上游 2xx 里裹着错误信封」），但第一次配的时候
> 还是照着填全。

> 上游必须是 OpenAI Chat Completions 方言。LM Studio / Ollama / OMLX / vLLM 都满足。
> 部分后端不支持 `/v1/responses`，所以网关统一走上游 Chat，再渲染成客户端要的协议。

**「内网地址」是内网优先 + 自动回退。** 家里的机器之间走局域网：外网要绕运营商再绕回来，
家里宽带的上行通常远小于局域网，那一跳能把流式首字延迟拖到没法用。

- 请求先打内网；**只有连接级失败**（连不上 / 超时）才改走公网
- 连上了但限额、模型没加载、内容报错 → **不换地址**，换地址只会把同一个错再犯一遍
- 内网不通只把「该节点的内网地址」标记为不可用 **2 分钟**，**不会**把节点拖进冷却——
  同一台机器的公网地址可能好好的，一次换网络不该把所有节点下线
- 健康探针走同一套地址优先级，不会出现「探针说活着、请求连不上」

两个地址填成一样时只试一遍。ComfyUI 端点的 `lan_base_url` 是**无条件**优先（没有回退），
给 ComfyUI 配内网地址前先想清楚那台机器关机时会发生什么。

**「每日禁用时段」**用于「白天不用、晚上才开机」这类场景，跨午夜直接填 `23 → 7`。
起止相同会被拒绝（那等于永久禁用，请用启用开关）。

**「节点级额外请求体」**用来关思考。各后端参数名不统一，这里原样合并进请求：

| 后端 | 写法 |
| --- | --- |
| LM Studio | `{"reasoning_effort":"low"}` |
| DeepSeek | `{"thinking":{"type":"disabled"}}` |

不做自动探测——猜错了比不配更难查。

### 2. API Key

一个节点可以挂**任意多把** key，用来跑不同组织 / 订阅账号。

- 轮换策略是**最久未用优先**，不是随机
- 某把撞上额度上限 → 进冷却（默认 5 小时）→ 自动切下一把
- 冷却状态**落库**：容器重启不会立刻再撞一次
- 「软上限」可以提前切换，主动避开 429
- 5 小时限额是「等」；401 / 欠费是「修」，后者要在这里手动解除

> 轮换决策会写 `key_rotation_event` 表，排障时能看到「哪次请求、命中哪条规则、为什么冷却」。

### 3. 模型路由

把**对外模型名**映射到「节点 + 上游模型名」。同一个模型可以挂多个节点，
调度按**并发占用率最低**选——所以「1 台 1 并发 + 1 台 4 并发」时不会把慢机器一直塞满。

「上游模型名」和对外名可以不同（对外叫 `fast`、上游叫 `qwen3.8-27b`）。
调度选中哪条路由，就按那条路由的上游名发请求。多数情况下两边填一样就行。

### 4. 设置 → 定价

按模型配「输入 / 输出 / 缓存输入」的单价，单位**微元 / 1k token**（1 元 = 1,000,000 微元）。
缓存命中按 `cached_input` 单价计，不配就按普通输入价。

> 1.2 元 / 百万 token = **1200 微元 / 1k**，输入输出同价就这么填。

### 5. 账号

给每个使用者建一个账号，设余额和并发额度。**明文 key 只在创建时显示一次**，
库里只存 SHA-256 哈希。

### 6. 从 erotic_sci 一次性导入

仓库里带了个导入脚本，把已有的 LM Studio / ComfyUI 配置搬过来：

```bash
# 用一个装了 pyyaml 的解释器（erotic_sci 自带的 .venv 就行）
/path/to/erotic_sci/.venv/bin/python scripts/import_erotic_sci.py \
  --password "$HOME_AI_ADMIN_TOKEN"

# 只看要写什么，不落库
... --dry-run
```

它会读 `config/ingest.yaml`、`.env`、`config/comfyui/*.json`，建好
**节点 + key + 路由 + 定价 + ComfyUI 端点 + 工作流 + 一个自用账号**。

- **脚本里没有任何明文密钥**，全部运行时从源仓库读，可以安全提交
- **幂等**：按名字 upsert，可以反复跑；已存在的 key 不会覆盖
- 工作流的参数槽是**从模板里扫出来的**，不手写，所以模板改了不会对不上
- 含 `image` 槽的工作流（img2img）导入后**停用**——网关只做「提交 + 轮询取图」，
  没有 `/upload/image`，那个槽要的是 ComfyUI 机器上已经存在的文件名
- 出图单价默认 **100000 微元 = 0.1 元/张**，用 `--image-price-micro` 改
  （**不能填 0**：网关对未配价的工作流直接拒绝出图）

---

## 调用

网关面用调用方的 API Key 鉴权，和管理台口令是两套完全独立的凭证。

```bash
# OpenAI Chat
curl http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer sk-hac-xxx" \
  -H "content-type: application/json" \
  -d '{"model":"qwen3-8b","messages":[{"role":"user","content":"你好"}]}'

# Anthropic（用 x-api-key，和 OpenAI SDK 一样直接换 base_url）
curl http://localhost:8080/v1/messages \
  -H "x-api-key: sk-hac-xxx" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{"model":"qwen3-8b","max_tokens":256,"messages":[{"role":"user","content":"你好"}]}'

# 出图
curl http://localhost:8080/v1/images/generations \
  -H "Authorization: Bearer sk-hac-xxx" \
  -H "content-type: application/json" \
  -d '{"workflow":"txt2img","params":{"prompt":"一只猫","seed":42}}'
```

换成现成客户端只要改 `base_url`：

```python
from openai import OpenAI
client = OpenAI(base_url="http://localhost:8080/v1", api_key="sk-hac-xxx")
```

| 路径 | 用途 |
| --- | --- |
| `POST /v1/chat/completions` | OpenAI Chat |
| `POST /v1/responses` | OpenAI Responses |
| `POST /v1/messages` | Anthropic Messages |
| `POST /v1/images/generations` | 出图 |
| `GET /v1/models` | 可用模型 |
| `GET /v1/workflows` | 可用工作流 |
| `GET /v1/usage` | 自己的余额与今日用量 |
| `GET /api/health` | 健康检查（免鉴权） |

---

## 出图工作流

两种模式：

- **模板**（默认）：服务端存 ComfyUI workflow JSON，客户端只传参数
- **raw**：客户端直接提交完整 workflow

模板里用占位符：

```json
{
  "1": { "class_type": "TextEncode", "inputs": { "text": "{{prompt}}" } },
  "6": { "class_type": "EmptyLatentImage", "inputs": { "width": "{{width|1024}}" } }
}
```

两条规则：

1. **整串就是占位符**时保留 JSON 类型——`{{seed}}` 变成数字 `42` 而不是字符串 `"42"`，
   否则 ComfyUI 校验直接报错。
2. **传了模板没消费的参数会报错**。静默丢弃会让「设了 seed 却每次出图一样」
   这类问题永远查不出来。

改工作流时先用「试跑」验证填充结果——它和真实提交走同一套校验规则。

---

## 计费口径

**预扣 → 实扣 → 退差**三段式：

1. 准入时按**最大可能费用**冻结余额（走条件更新，余额不足直接 402）
2. 调用结束后按真实 usage 扣费
3. 差额自动还回

只用结束时扣款的话，并发请求会同时看到「余额充足」然后一起透支。

**账目的唯一口径**是 `UnifiedUsage`：`input_tokens` **含** `cached_tokens`，
`output_tokens` **含** `reasoning_tokens`（两个子集不可相加）。
它和回给客户端的 usage 字段**无关**——Anthropic 没有 `total_tokens`、
Chat 末块 `choices` 是空数组，跟着协议走账就错了。

上游不认 `stream_options.include_usage` 时按字符估算，并在明细里标 `estimated`，
**不会记成 0**。

---

## 配置

全部走环境变量：

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `HOME_AI_DATA_DIR` | `./data` | 数据目录：SQLite、备份、图片归档 |
| `HOME_AI_BIND` | `0.0.0.0:8080` | 监听地址 |
| `HOME_AI_ADMIN_TOKEN` | — | **首次启动**用于初始化管理员口令（用户名 `admin`） |
| `HOME_AI_TIMEZONE` | `Asia/Shanghai` | 禁用时段与每日分桶用的时区 |
| `HOME_AI_BACKUP_AT` | `03:30` | 每日备份时刻（本地时间） |
| `HOME_AI_PROBE_INTERVAL` | `15` | 节点健康复检间隔（分钟） |
| `HOME_AI_QUEUE_TIMEOUT` | `120` | 单次请求排队等待上限（秒） |
| `HOME_AI_MAX_KEY_ROTATION` | `3` | 单个请求内最多轮换几把 key |
| `HOME_AI_LOG` | `info,hac=debug,sqlx=warn` | 日志过滤 |

改 `HOME_AI_ADMIN_TOKEN` 不会改已存的口令（初始化只跑一次），要改口令到「设置 → 管理员」。

---

## 数据与备份

```
/data
├── home-ai-center.db        # 全部配置与数据
├── backups/                 # 每日备份，滚动 7 天
│   └── hac-20260115-033000.db
└── images/<request_id>/     # 出图归档（仅工作流开启归档时）
```

备份用 SQLite 的 `VACUUM INTO`：**一致性快照，且只取读锁，不阻塞在线写入**。

> WAL 模式下直接 `cp` 数据库文件会得到「主库与 WAL 不匹配」的坏副本，
> 这是最常见的备份翻车方式。写入顺序固定为先写 `.tmp` 再原子改名，
> 中途失败只会多一个 `.tmp`，不会留下「看起来正常其实是半截」的文件。

调用明细默认保留 90 天（`setting` 表里的 `log_retention_days`），`usage_daily` 永久保留。

---

## 本地开发

```bash
# 后端
export PATH="$HOME/.cargo/bin:$PATH"
cargo test -p home-ai-center --lib      # 179 项
cargo run -p home-ai-center

# 前端（另开一个终端）
cd web && npm install && npm run dev    # 5173，已配好代理到 8080
```

改前端后要 `npm run build`，Rust 侧用 `include_dir!` 在**编译期**把 `web/dist`
内嵌进二进制——所以**必须先构建前端再编译后端**，顺序反了会编不过。

### 代码结构

```
crates/gateway/src/
├── protocol/     # 协议层：单上游方言 + 3×2 薄适配器
│   ├── ir.rs     # 中间表示，计费的唯一口径
│   └── upstream_chat.rs   # 唯一的上游渲染 + ChunkState 流式解析
├── gate/         # 并发闸门：自研 SlotGate（双优先级队列、可热改限额）
├── keys/         # key 轮换：分类决策表、重置时间解析、软上限窗口
├── billing/      # 预扣结算退差 + 调用明细
├── upstream/     # 上游客户端：OpenAI Chat、ComfyUI、模板填充
├── api/          # v1（网关面）、admin（管理面）、images（出图）
├── tasks/        # 健康复检、每日备份、日志保留
└── state.rs      # 配置快照 + 热更新
```

### 几条容易被改坏的地方

- **协议层是「一个上游方言 + 薄适配器」，不是 N×N 互转矩阵。**
  协议由**请求路径**决定，不嗅探请求体。
- **并发闸门用自研 `SlotGate`**，不是 `Semaphore`：需要双优先级队列、
  可热改的限额、拿不到就快速失败。
- **NodeHealth 是进程级共享的**（在 `AppState` 上）。每个请求新建一份的话，
  「标记离线 + 冷却」在下一个请求眼里根本不存在。
- **额度事件要先查后置**：`finish_reason` 块早于 `usage` 块到达，
  在前者就发 `Done` 会把后者丢掉——每次流式请求都退化成估算计费。
- **key 轮换的排序键取自内存**：`last_used_at` 一个请求才写一次，
  从 DB 读永远是旧值，排序会退化成「永远选第一把」。
- **冷却状态落库**是「运行时态只在内存」的唯一例外。
- **绝不轮换 5xx / 529 / `slow_down`**：它们是上游容量信号，不是限额信号。
  （litellm 正是在此踩坑：按状态码一律 5 秒冷却。）
- **2xx 不代表成功。** OpenAI 兼容服务端有个坑习惯：路径写错也回 200，body 却是
  `{"error":"..."}`（LM Studio 原样如此）。`upstream_chat::parse_response` 遇到没有
  `choices` 的 body 会安静地解析出「一个空补全」，于是客户端收到空回答、网关照常按
  估算 token 计费。同类的静默失败一共三处，都得各自拦住：
  2xx 里裹着错误信封、非流式请求收到非 JSON、流式请求收到不是 SSE 的内容。
- **`upstream_model` 必须在请求路径上生效**。调度器选中哪条路由，就按那条路由的
  上游名发 `model` 字段；只按对外名发的话，别名 / 灰度全是摆设。
- **回退必须自带连接超时。** 内网那台关机时内核往往不是 refuse，而是把 SYN 丢进黑洞，
  TCP connect 要挂到系统级超时（macOS 75 秒）。所以「还有兜底地址」的那次尝试走
  `OpenAiClient` 里的短连接超时实例；写成普通的整体超时会把长生成也砍掉。

改完 SQL 后跑一遍审计：

```bash
python3 scripts/audit_sql.py
```

它查两类**编译期查不出、运行时才炸**的错误：漏绑 `?`、INSERT 列数与 VALUES 不符。
症状都是「某个 NOT NULL 列莫名其妙是 NULL」。

---

## 不做的事

- **多机 / 多副本分布式调度**。单进程单库，调度状态在内存里。
- **TLS 终止与反向代理**。默认 HTTP，套 Nginx / Caddy 即可。
- **记录 prompt / completion 正文**。产品决策：正文最占空间，而排查问题需要的
  「谁、何时、调了哪个模型、多少钱」都在明细里。
- **自动探测后端参数**。关思考的参数各家不统一，逐节点配比猜更可靠。
