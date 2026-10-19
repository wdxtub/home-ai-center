# 自托管运维手册

面向自己部署这套网关的人：怎么装、怎么备份、怎么恢复、怎么升级、坏了怎么查。

本文所有默认值、环境变量、路径、备份文件名都以源码为准，主要依据：

| 内容 | 权威位置 |
| --- | --- |
| 环境变量与默认值 | `crates/gateway/src/config.rs` |
| 日志过滤 | `crates/gateway/src/main.rs`（`init_tracing`） |
| 备份 / 保留 / 探针 | `crates/gateway/src/tasks/{backup,retention,probe}.rs` |
| SQLite 连接参数 | `crates/gateway/src/db/mod.rs` |
| 迁移 | `crates/gateway/src/db/migrate.rs` + `crates/gateway/migrations/*.sql` |
| 镜像构建 | `Dockerfile` |

本文不含任何真实地址与密钥。示例里的内网地址用 RFC1918 保留段（`192.168.1.20`），
机器名写 `<node-a>`，口令写 `change-me-to-something-long`。

---

## 1. 部署

### 1.1 主路径：docker compose

```bash
cp .env.example .env
# 编辑 .env，把 HOME_AI_ADMIN_TOKEN 改成长口令
docker compose up -d --build
docker compose logs -f gateway
```

启动日志应当依次出现（`crates/gateway/src/main.rs`，顺序即启动装配顺序）：

```
home-ai-center 启动
数据库迁移完成
已用 HOME_AI_ADMIN_TOKEN 初始化管理员账号     ← 仅首次
运行时快照已装载
监听 0.0.0.0:8080
后台任务已启动
下一次备份 at=...
```

打开 <http://localhost:8080>，用 `admin` + `.env` 里的口令登录。用户名固定是 `admin`
（`api/auth.rs` 写死 `INSERT ... VALUES (1,'admin',...)`）。

**`.env` 只有 `docker compose` 会读。** 代码侧一律 `std::env::var`（`config.rs:34`），
没有 dotenv 依赖。所以 `docker compose up` 正常；而直接在宿主机 `cargo run` 时
**必须自己导入**：

```bash
set -a && . ./.env && set +a
cargo run -p home-ai-center
```

漏了这步的**症状是静默的**：进程照常起来，只是日志里多一行
`尚未初始化管理员：请设置 HOME_AI_ADMIN_TOKEN 环境变量后重启`（`api/auth.rs:225`），
管理员口令**不会被创建**，于是永远登不进去。见排障 §6.2。

> 源码直跑时 `HOME_AI_DATA_DIR` 缺省是 `./data`，和 compose 挂载的是同一个目录。
> **别让容器和本地进程同时跑**，两套调度器会互相抢库。

### 1.2 管理员口令只初始化一次

`ensure_admin` 在 `admin_user` 表已有一行时**直接返回**（`api/auth.rs:218`），所以改
`HOME_AI_ADMIN_TOKEN` **不会**改已存的口令；改口令走「设置 → 管理员」
（`PUT /api/admin/auth/password`）。忘了口令又进不去管理台，见 §6.2。

### 1.3 数据目录

容器内只有一个持久化入口 `/data`（`Dockerfile:43` 的 `VOLUME`），compose 挂到宿主机 `./data`：

```
data/
├── home-ai-center.db        # 全部配置与数据
├── home-ai-center.db-wal    # WAL 在线存在时才有
├── home-ai-center.db-shm    # 同上
└── backups/                 # 每日备份
    ├── hac-20260115-033000.db
    └── ...
```

网关**不存图**。出图的图以 base64 回到调用方，由调用方自己落盘。
容器内不存在 `images/`；启动时若发现遗留的 `data/images/`，**空目录（含只有 `.DS_Store`
的点文件）会被自动删掉，有真实文件的只打一条 warn 绝不删**（`config.rs:90`）。见排障 §6.9。

容器以非 root 运行（uid/gid 均为 `10001`，`Dockerfile:36-37`），`HEALTHCHECK` 就以这个
身份执行 `sqlite3 /data/home-ai-center.db "SELECT 1"`（`Dockerfile:56-57`）。备份文件本来
就是网关进程建的、属主已经正确；从 tar 包或 root 侧拷进来的才需要修属主
（`§4.4`）——否则**可验证症状**是应用日志一切正常但容器一直 `unhealthy`，
health log 写 `unable to open database file`。

---

## 2. 环境变量参考表

以 `config.rs::from_env` 为准，括号里是容器镜像里的 `ENV` 覆盖值（`Dockerfile:48-52`）。

| 变量 | 默认值 | 作用 | 改后要重启？ |
| --- | --- | --- | --- |
| `HOME_AI_DATA_DIR` | `./data`（容器 `/data`） | 数据目录：SQLite、备份 | 是 |
| `HOME_AI_BIND` | `0.0.0.0:8080` | 监听地址 | 是 |
| `HOME_AI_ADMIN_TOKEN` | 无（未设置 / 空串视为未设置） | **仅首次启动**初始化 `admin` 口令 | 是（且只对空库生效） |
| `HOME_AI_TIMEZONE` | `Asia/Shanghai` | 备份调度、节点禁用时段、每日分桶 | 是 |
| `HOME_AI_BACKUP_AT` | `03:30` | 每日备份时刻（该时区的本地时间） | 是 |
| `HOME_AI_PROBE_INTERVAL` | `15` | 节点健康复检间隔，**单位是分钟**，代码内 `max(10) * 60` | 是 |
| `HOME_AI_QUEUE_TIMEOUT` | `120` | 单次请求排队等待上限（秒） | 是 |
| `HOME_AI_MAX_KEY_ROTATION` | `3` | 单个请求内最多轮换几把 key | 是 |
| `HOME_AI_BACKGROUND_JOBS` | `4` | 见下方说明 | 是 |
| `HOME_AI_LOG` | `info,hac=debug,sqlx=warn` | 日志过滤，见 §7 | 是 |
| `RUST_BACKTRACE` | 容器内 `1` | panic 时打栈 | 是 |

三点必须知道：

1. **`HOME_AI_BACKGROUND_JOBS` 实际不控制并发。** `tasks/mod.rs:12-23` 读它只是启动时
   打印一行 `后台任务已启动 workers=N`，三类任务（备份 / 探针 / 明细清理）各起**一个**
   独立循环，不按这个数扩缩。填什么都只改日志。
2. **填错格式不报错，只会静默回退默认值。** 除 `ADMIN_TOKEN` 外所有数值都是
   `.parse().unwrap_or(默认值)`，写成 `HOME_AI_QUEUE_TIMEOUT=2m` 会被当成 120。
   `HOME_AI_BACKUP_AT` 解析失败则回退到 3:30（`tasks/backup.rs:212`）。
3. **环境变量没有一个是热改的**，全部在 `Config::from_env()` 读一次存进 `AppState`。
   只有存在数据库里、能从管理台改的那些（定价、并发额度、保留期）才是即时生效。

compose 只显式透传了 7 个变量（`HOME_AI_ADMIN_TOKEN` / `TIMEZONE` / `BACKUP_AT` /
`PROBE_INTERVAL` / `QUEUE_TIMEOUT` / `MAX_KEY_ROTATION` / `LOG`）。要改数据目录或监听
地址，得自己在 `docker-compose.yml` 的 `environment:` 里补：

```yaml
    environment:
      HOME_AI_DATA_DIR: /data          # 只有这一个值是容器内路径
      HOME_AI_BIND: 0.0.0.0:8080
```

---

## 3. 备份机制

### 3.1 备份是怎么做的

`tasks/backup.rs::run_once` 每次备份做四件事：

1. 用 SQLite 的 **`VACUUM INTO`** 生成**一致性快照**，只取读锁，不阻塞在线写入
   （`db/mod.rs:18` 开着 WAL，所以读不阻塞写、写不阻塞读）；
2. 先写到 `hac-<时间戳>.db.tmp`；
3. 成功后 **原子改名** 成 `hac-<时间戳>.db`；
4. 无论成败都往 `backup_record` 表写一行（`status` 为 `ok` 或 `error`）。

先 `.tmp` 再改名是刻意的：中途崩溃或磁盘写满，目录里只会多一个 `.tmp`，
不会留下「看起来正常、其实是半截」的备份。

调度用「下一次触发时刻」而不是「睡 24 小时」，所以机器休眠或容器重启不会让备份时刻
整体漂移。备份跑完立刻 `purge(7)` 清掉 7 天前的文件，并顺手删掉 30 天前的 `backup_record` 行。

### 3.2 为什么绝对不能 `cp` 数据库文件

**WAL 模式下 `cp data/home-ai-center.db` 得到的是坏副本。** 活跃写入时，最近的事务
还在 `home-ai-center.db-wal` 里没并回主库；你只拷到主库，等于拿一个缺了最近若干笔
提交、可能还页号错位的文件。症状是恢复后启动就报 `database disk image is malformed`，
或者更阴的：能起来，但账少了最近几天的流水。compose 里那行 `VOLUME ./data:/data`
保护不了你——它保护的是**整个目录**，而 `cp` 只拷了其中一个文件。

安全的临时快照只有两种：走网关自己的备份，或**先停容器再拷**（干净关闭会 checkpoint
并删掉 `-wal`）。

### 3.3 备份文件名

```
hac-YYYYMMDD-HHMMSS.db          # 时间戳是「本地时间」，即备份时刻按 HOME_AI_TIMEZONE 折算
hac-YYYYMMDD-HHMMSS.db.tmp      # 进行中或失败残留；只可能是这一个后缀
```

见 `tasks/backup.rs:26-39`。滚动清理拿文件名里的时间戳跟 `keep_days` 比
（`parse_stamp`），所以**往 `backups/` 里塞别的文件不会被删**（不以 `hac-` 开头直接跳过），
但也永远不会被用。

### 3.4 手工备份 / 改保留期

管理台「设置 → 备份」里的按钮对应三个接口（都需要管理员会话）：

```bash
# 立刻备份一次（与每日任务同一条代码路径）
curl -X POST http://localhost:8080/api/admin/backups/run \
  -H "Authorization: Bearer <管理台会话 token>"

# 历史记录 + 备份目录绝对路径
curl -s http://localhost:8080/api/admin/backups \
  -H "Authorization: Bearer <管理台会话 token>"

# 立刻按当前保留期清理一次
curl -X POST http://localhost:8080/api/admin/backups/purge \
  -H "Authorization: Bearer <管理台会话 token>"
```

**备份保留天数在源码里写死 7 天**（定时任务 `tasks/backup.rs:185` 与手动清理
`api/admin.rs:1762` 都传字面量 `7`），既没有环境变量也没有管理台开关；想要更长窗口，
只能在两次每日备份之间手动把旧的 `hac-*.db` 拷到别处。能配的是**调用明细保留期**：
管理台「设置 → 数据保留」，落在 `setting` 表的 `log_retention_days` 键上，默认 90
（`tasks/retention.rs:14`），只清 `request_log` 与 `image_prompt`，`usage_daily` 永久保留。

### 3.5 备份文件本身就是敏感数据

`llm_node_key.secret` 是**明文**存储的（上游节点的 key；调用方 key 相反，只存 SHA-256）。
所以 `backups/*.db` 等同明文密钥库，异地存放要按密钥的标准加密。

---

## 4. 恢复 runbook

目标：把 `backups/hac-<ts>.db` 恢复成运行库。全程假设在仓库根目录（`data/` 与之平级）。

### 4.1 停容器

```bash
docker compose stop gateway
```

**必须停。** 运行中直接替换主库文件，WAL 与主库会错配，SQLite 可能拒绝打开。

### 4.2 先给当前库留一份

```bash
cp -a data/home-ai-center.db "data/home-ai-center.db.before-restore.$(date +%Y%m%d-%H%M%S)"
ls -l data/home-ai-center.db*
```

干净停机后 `-wal` / `-shm` 通常已被 checkpoint 掉；还在列表里就一起备份。这一步是为了
「恢复完发现选错了备份」时还能退回来。

### 4.3 放置备份

```bash
SRC=data/backups/hac-20260115-033000.db
ls -l "$SRC"                                    # 确认存在、大小不是 0
sqlite3 "$SRC" "PRAGMA integrity_check;"        # 期望输出 ok
cp "$SRC" data/home-ai-center.db
rm -f data/home-ai-center.db-wal data/home-ai-center.db-shm
```

`integrity_check` 别省：它能在**覆盖当前库之前**就告诉你这份备份是不是坏的。宿主机没装
`sqlite3` 的话，运行时镜像自带（`Dockerfile:35`），之后可用
`docker compose exec gateway sqlite3 ...` 复核。

**`-wal` / `-shm` 一定要删。** 留着旧库的 WAL 文件，新主库配上它照样是坏副本。

### 4.4 修属主

只在文件不是网关自己创建的时候需要（理由见 §1.3）：

```bash
sudo chown 10001:10001 data/home-ai-center.db
sudo chmod 644 data/home-ai-center.db
```

宿主机 `cargo run` 的话跳过这步。

### 4.5 启动并验证

```bash
docker compose up -d gateway
docker compose logs -f gateway
```

依次确认：

1. `数据库迁移完成` —— 迁移是内嵌在二进制里的，启动时自动跑（`main.rs:43`）。
   用**旧备份 + 新二进制**恢复是安全的，缺的迁移会自动补上；
2. `运行时快照已装载`，且 `nodes=N` 不为 0；
3. 免鉴权健康检查：

```bash
curl -fsS http://localhost:8080/api/health
# {"ok":true,"version":"...","uptime_secs":3,"nodes":3,"comfy_nodes":1,
#  "models":5,"workflows":2,"frontend":true}
```

4. 账目对得上：

```bash
docker compose exec gateway sqlite3 /data/home-ai-center.db \
  "SELECT COUNT(*) FROM request_log; SELECT COUNT(*) FROM account; PRAGMA integrity_check;"
```

5. 浏览器登录管理台，抽查一个节点和一条路由还在不在。

### 4.6 恢复旧备份后仍失败

| 现象 | 判断 | 处理 |
| --- | --- | --- |
| `integrity_check` 报 `malformed` | 这份备份本身坏了（多半是历史上手工 `cp` 出来的） | 换更早的一份；见 §3.2 |
| 启动报迁移 `VersionMissing` | 备份来自**更新的版本**，当前二进制不认识那些迁移 | 换匹配的二进制版本，见 §5.4 |
| 起来了但节点全空 | 选中的备份早于建节点的时间 | 换晚一点的备份 |
| 容器一直 `unhealthy` | 属主不对 | 回 §4.4 |

---

## 5. 升级 runbook

### 5.1 铁律：先前端，后端

Rust 侧用 `include_dir!` 在**编译期**把 `web/dist` 打进二进制
（`crates/gateway/src/webui.rs:23`），目录不存在时直接 panic——刻意的，
免得起一个所有人都看到 404 的服务。所以是 `npm run build` **在前**、`cargo build` 在后。

用 Dockerfile 就自动对了，它是三段构建：`web`（node:22-alpine 跑 `npm run build`）
→ `api`（把 `/tmp/webdist` 拷到 `/build/web/dist` 后才 `cargo build --release`）
→ 运行时。`web/dist` 是 gitignore 的**产物**不是源码，别手动往构建上下文里加东西
（`.dockerignore` 也因此不能忽略它）。

### 5.2 步骤

```bash
# 0. 先备份（见 §3.4），别省
curl -X POST http://localhost:8080/api/admin/backups/run \
  -H "Authorization: Bearer <管理台会话 token>"

# 1. 拉新代码
git fetch --all
git checkout <目标 tag 或 commit>

# 2. 构建并启动
docker compose up -d --build

# 3. 看日志，确认迁移跑完
docker compose logs -f gateway
curl -fsS http://localhost:8080/api/health
```

迁移是自动的：`sqlx::migrate!` 把 `migrations/` 目录编译进二进制，启动时无条件执行
（`db/migrate.rs:8`、`main.rs:43`）。**不需要手工跑任何 SQL。** 迁移发生在监听端口
之前（`main.rs:43` 早于 `main.rs:80`），所以升级期间容器有一段起不来的窗口，
这是正常的；任何一步失败进程就直接退出，不做降级启动（`main.rs` 头注释）。

### 5.3 迁移只能追加，不能改已应用的文件

`sqlx` 会把每个迁移文件的 checksum 记进 `_sqlx_migrations` 表。**改动任何已经跑过的
迁移文件（哪怕只是改注释），下次启动就会 checksum mismatch 直接失败**，
所有已有部署的 `./data` 都会起不来。要改已上线的结构只能**新加
`crates/gateway/migrations/000N_xxx.sql`**；想纠正旧迁移的注释，写新迁移或写进代码注释，
别回去动 `0001`；合并/删除迁移文件 = 破坏所有既有部署。

当前目录里是 `0001` 到 `0006`（以目录中实际存在的编号为准）。`0004` 一次性删了四个
「只写不读」的死列，`0006` 把调用方 key 从 `account` 拆成独立的 `api_key` 表——
都是「结构只能前进」的典型。

### 5.4 回滚

**二进制和数据库必须成对回滚**，因为迁移没有 down 脚本：

```bash
# 二进制回滚：回到旧代码重新构建
git checkout <升级前记下的 commit>
docker compose up -d --build
```

分三种情况：

| 场景 | 做法 |
| --- | --- |
| 新版本启动就挂（迁移不兼容 / checksum 问题） | 只回滚二进制，**保留新库**通常不行：新迁移已经写进 `_sqlx_migrations`，旧二进制不认识 → `VersionMissing`。**二进制和库一起回滚**（把 §4.2 留的那份 `.before-restore.*` 拷回去）。 |
| 新版本能跑但有 bug，数据没问题 | 只回滚二进制，库保留。新增的表/列留着无害，旧代码不读它们。 |
| 升级后数据被写坏 | 走完整的 §4 恢复 runbook，选升级前那次备份。 |

回滚后**不要**再直接跳回新版本：迁移只前进不后退，跳回去会二次失败。先搞清楚差异再升级。

---

## 6. 排障手册

先看结论：绝大多数「服务好好的但用不了」都能用**免鉴权**的
`curl -fsS http://localhost:8080/api/health` 和下面几张表定位，不用翻日志。

### 6.1 起来就 checksum mismatch 启动失败

- **症状**：`docker compose logs gateway` 里迁移报错退出，容器反复重启。
- **原因**：`crates/gateway/migrations/` 里某个**已经跑过**的文件被改了内容。
  sqlx 记的是文件 checksum，不是语句是否生效。
- **确认**：

```bash
docker compose exec gateway sqlite3 /data/home-ai-center.db \
  "SELECT version, success, LEFT(checksum,8) FROM _sqlx_migrations ORDER BY version;"
# 再和本地文件算出的 checksum 对比
```

- **修**：把被改动的迁移文件 `git checkout` 还原成已发布版本；真正要改的结构
  **新加一个 `000N_xxx.sql`**（§5.3）。已经跑坏的那台，只能恢复升级前的备份；或手工删掉
  `_sqlx_migrations` 里那几行让迁移重跑——**危险**：DDL 不会回滚，
  只在确认该迁移在目标库上没产生实际变更时才这么做。

### 6.2 管理台登录不了 / 管理员没初始化

- **症状**：登录框提示凭据错误；或日志里 `尚未初始化管理员：请设置 HOME_AI_ADMIN_TOKEN 环境变量后重启`。
- **原因**，两种：
  1. **没设变量就启动了。** `ensure_admin` 只在 `admin_user` 为空时用
     `HOME_AI_ADMIN_TOKEN` 建账号（`api/auth.rs:224-227`），没有它就只 warn 一行、继续启动。
     源码直跑时最容易犯——代码不读 `.env` 文件。
  2. **口令早就初始化过了**，现在改环境变量当然不生效。
- **确认**：

```bash
docker compose exec gateway sqlite3 /data/home-ai-center.db \
  "SELECT id, username, created_at FROM admin_user;"
# 有行 = 账号已存在，问题在口令不对；空表 = 从没初始化过
```

- **修**：空表 → 补上变量再重启（compose：写进 `.env` 后 `docker compose up -d`；
  源码：`set -a && . ./.env && set +a && cargo run -p home-ai-center`）。
  有行但忘了口令 → 走「设置 → 管理员」（`PUT /api/admin/auth/password`）；
  进不去页面就只能停容器、用 sqlite3 把 `password_hash` 换成一个自己算好的 Argon2 哈希，
  格式以 `api/auth.rs::hash_password` 为准。验证：日志里 `初始化管理员` 只会在**首次**出现。

### 6.3 402

- **症状**：`{"error":{"message":"余额不足：可用 … 微元，本次需要 … 微元"}}`。
- **原因**：只有一种——**余额不足**。准入时按「最大可能费用」冻结，条件更新
  `WHERE balance_micro - held_micro >= reserved` 影响 0 行即 402（`billing/ledger.rs:59`）。
- **注意**：**未配置单价返回的是 400，不是 402**（`api/v1.rs:234`，报文
  `模型 X 未配置单价`）。README 里写「没配单价的模型会被直接拒绝（402）」与当前代码不符。
- **确认**：

```bash
docker compose exec gateway sqlite3 /data/home-ai-center.db \
  "SELECT id,name,balance_micro,held_micro FROM account;"
docker compose exec gateway sqlite3 /data/home-ai-center.db \
  "SELECT kind,COUNT(*),SUM(amount_micro) FROM balance_txn GROUP BY kind;"
```

  `held_micro` 明显大于 0 且长时间不降，说明有预扣没释放（进程崩溃残留的会在**下次启动
  对账**时清掉，日志有 `已清理崩溃残留的预扣冻结`）。
- **修**：管理台「账号」充值 / 调额度；或调小 `default_max_output_tokens`——
  预扣按最大输出估，最大输出配到 65536 的话预扣会离谱地大。

### 6.4 502：上游 2xx 里裹着错误信封

- **症状**：502，报文形如
  `上游返回 2xx 但响应体是错误：Unexpected endpoint or method. (POST /chat/completions)。若 base_url 只填到了端口，请补上 /v1`。
- **原因**：OpenAI 兼容后端的老毛病——**路径写错也回 200**，body 却是
  `{"error":"..."}`（LM Studio 原样如此）。网关会把它挑出来报 502，而不是当成
  「一个没有 choices 的空补全」照常计费（`upstream/openai.rs:242-278`）。
  **同类静默失败还有两处，也都是 502**：非流式请求收到非 JSON body；流式请求收到不是 SSE
  的内容（`Content-Type` 不是 `text/event-stream` 且内容不像 SSE 帧）。
- **确认**：直接打上游，别隔着网关猜：

```bash
curl -sS -o /dev/null -w '%{http_code}\n' \
  http://192.168.1.20:7001/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"<上游模型名>","messages":[{"role":"user","content":"hi"}],"max_tokens":1}'
```

  回 200 但 body 里是 `error`，就是路径错（见 §6.5）。
- **修**：见 §6.5；若是真 5xx，在管理台「LLM 节点」看该节点是否进了冷却。

### 6.5 上游地址填错前缀

- **症状**：所有模型一律 502，报 `Unexpected endpoint`。
- **原因**：`base_url` 必须填到**端点前缀**（等价于 OpenAI SDK 的 `base_url`），
  网关只在其后拼 `/chat/completions`。填 `http://192.168.1.20:7001` 会变成
  `POST /chat/completions`——**缺了 `/v1`**。
- **确认**：

```bash
docker compose exec gateway sqlite3 /data/home-ai-center.db \
  "SELECT id,name,base_url,lan_base_url FROM llm_node;"
```

- **修**：改成 `http://192.168.1.20:7001/v1`，内网地址填法相同；两个地址填成一样时
  只试一遍。ComfyUI 端点的 `lan_base_url` 是**无条件**优先（没有回退），
  配之前先想清楚那台机器关机会发生什么。

### 6.6 内网地址不通

- **症状**：内网那台机器关机 / 换网络后请求变慢，绕公网出流量。
- **行为**（刻意设计，别当 bug 修）：只有**连接级失败**（连不上 / 超时）才改走公网；
  连上了但限额、模型没加载、内容报错则**不换地址**。内网不通只把
  **该节点的内网地址**标记为不可用 **2 分钟**（`gate/node_gate.rs:74` 的
  `LAN_RETRY_AFTER`），**不会把节点拖进冷却**——同一台机器的公网地址可能好好的。
  探针走同一套地址优先级，不会出现「探针说活着、请求连不上」。
- **确认**：日志里有 `内网不可用` 级别的 warn（`mark_lan_down`），且节点本身**不在**冷却列表里。
- **修**：多数不用修，2 分钟后自动重试内网。频繁 warn 说明那台机器的网段/IP 变了。

### 6.7 排队超时

- **症状**：429（带 `Retry-After: 5`），报文按**卡在哪一层**不同，认字就能定位：

  | 报文 | 卡在哪 |
  | --- | --- |
  | `API Key「<key 名>」并发已满：排队已满，请稍后再试` | 调用方那把 key 的并发额度用完了 |
  | `API Key「<key 名>」出图并发已满：…` | 同上，出图侧（`api/images.rs:197`） |
  | `节点 <节点名> 的并发已排满，请稍后再试` | 节点级闸门队列满（`gate/node_gate.rs:278`） |
  | `等待节点 <节点名> 空闲超时` | 单个节点等超过 `HOME_AI_QUEUE_TIMEOUT` |
  | `模型 <model> 的所有节点都满载，排队超时` | 调度器把所有节点轮了一遍仍无空位（`gate/node_gate.rs:314`） |
  | `所有 ComfyUI 端点都满载，排队超时` | 出图侧同义（`gate/comfy_gate.rs:135`） |

- **原因**：请求在并发闸门里等了超过 `HOME_AI_QUEUE_TIMEOUT` 秒（默认 120），
  或队列已满被立即拒绝。
- **确认**：

```bash
curl -fsS http://localhost:8080/api/admin/runtime -H "Authorization: Bearer <会话 token>"
docker compose exec gateway sqlite3 /data/home-ai-center.db \
  "SELECT target, AVG(queue_wait_ms) q, MAX(queue_wait_ms) m, COUNT(*) FROM request_log
   WHERE created_at > strftime('%s','now')-86400 GROUP BY target;"
```

- **修**，按卡住的那一层分别处理：
  - 卡在 **API Key** → 调大该 key 的 LLM / 出图「并发」与「排队」额度（管理台「账号」），
    或换一把 key；
  - 卡在 **节点** → 加节点，或调大节点「最大并发」（`llm_node.max_concurrency`，1–64）。
    节点级队列深度是**推导值** `HOME_AI_QUEUE_TIMEOUT × 4 + 4`（`gate/node_gate.rs:275`），
    没有独立开关；调大超时同时也放宽了节点队列。
  - 全部节点都满 → 加机器，或把 `HOME_AI_QUEUE_TIMEOUT` 调大后重启。
  - 交互请求会插到批量请求前面（双优先级队列），所以长跑的批量任务占着槽位时，
    优先调并发而不是无脑调超时。

### 6.8 key 一直轮换 / 冷却不解除

- **症状**：某节点永远不出活，或明明修好了还是 503。
- **背景**：
  - 轮换策略是**最久未用优先**；
  - 冷却状态**落库**（`llm_node_key.cooldown_until`），容器重启不会立刻再撞一次限额；
  - **5 小时限额是「等」；401 / 欠费 / key 泄露是「修」**，后者不会自己好，必须人工解除。
- **确认**（这一条必须查表，日志里看不到全部字段）：

```bash
docker compose exec gateway sqlite3 /data/home-ai-center.db \
  "SELECT id,label,state,cooldown_until,quota_class,matched_rule FROM llm_node_key ORDER BY node_id,sort_order;"
docker compose exec gateway sqlite3 /data/home-ai-center.db \
  "SELECT created_at,key_id,quota_class,matched_rule,cooldown_ms,reset_source
   FROM key_rotation_event ORDER BY id DESC LIMIT 20;"
```

  `quota_class` 为 `KeyDead`（规则 `R5_payment_required` / `R5_key_leaked`）= 要修，不是要等。
- **修**：修好上游凭据后，在管理台点该 key 的**解除冷却**
  （`POST /api/admin/keys/{id}/clear-cooldown`）。限额类的等 `cooldown_until` 到点即可。
- **反面教材**：408 / 409 / 500 / 502 / 503 / 529 / `slow_down` 这些**容量信号绝不轮换
  key**，5xx 只会把节点标故障。轮换事件异常频繁时先看 `matched_rule` 列是哪条。

### 6.9 磁盘满 / 备份没生成

- **症状**：`data` 所在分区满了，或 `backups/` 一直是空的。
- **确认**：

```bash
df -h ./data
docker compose exec gateway sqlite3 /data/home-ai-center.db \
  "SELECT datetime(started_at,'unixepoch','localtime') t, status, size_bytes, error
   FROM backup_record ORDER BY id DESC LIMIT 10;"
ls -l data/backups/
```

  `status='error'` 时 `error` 列会写明是 `VACUUM INTO 失败: …` 还是 `改名失败: …`
  （磁盘满通常表现为后者或前者的 IO error）。
- **排查顺序**：日志里 `下一次备份 at=…` 出现过（启动时一次，之后每次跑完一次）说明定时任务活着；
  `HOME_AI_BACKUP_AT` 写坏了会静默回退 3:30（§2）；手工触发一次拿即时错误（§3.4）；
  目录里只残留 `hac-….db.tmp` = 上次写崩了，直接删掉，不影响下次。
- **省空间**：`request_log` 一天能上万行，是主要增长源。调小管理台「设置 → 数据保留」
  的 `log_retention_days`（`usage_daily` 永久保留，不会一起清）。备份滚动 7 天，常数级占用。
- **遗留 `images/`**：**空目录（含只有 `.DS_Store`）启动时会自动清掉**；里面还有真实文件的
  只打一条 warn、**绝不删**（那可能是唯一一份图），确认不需要了再自己 `rm -rf`。

---

## 7. 可观测性

### 7.1 `HOME_AI_LOG` 语法

`tracing_subscriber` 的 `EnvFilter` 语法，逗号分隔，**后面的覆盖前面的**：

```bash
# 默认
HOME_AI_LOG=info,hac=debug,sqlx=warn

# 只看备份与探针，其余压到 warn
HOME_AI_LOG=warn,hac::tasks=debug

# 追一次上游故障
HOME_AI_LOG=info,hac::upstream=debug,hac::gate=debug,reqwest=debug

# 看到 SQL（很吵，只在排查迁移/锁问题时用）
HOME_AI_LOG=info,hac=debug,sqlx=query
```

- 库名是 `hac`（`[lib] name = "hac"`），所以 target 前缀是 `hac::模块::函数`：
  `hac::tasks`、`hac::upstream`、`hac::gate`、`hac::keys`、`hac::billing`、`hac::api`。
- 格式：`EnvFilter::try_from_env("HOME_AI_LOG")` 失败会回退到
  `info,hac=debug,sqlx=warn`，再失败才是纯 `info`（`main.rs:97-100`）。
  **写错不会报错，只会安静地回退**，排查时先确认自己想要的级别真的生效。
- **级别只能在启动时定**：代码没有保留 `reload` handle，改完变量要重启容器。
  日志行还**不打印 target**（`.with_target(false)`）——`hac::gate=debug` 依然能过滤，
  只是输出里看不到模块名。

### 7.2 关键表

容器内自带 `sqlite3`，直接查：

```bash
docker compose exec gateway sqlite3 /data/home-ai-center.db "<SQL>"
```

| 表 | 排障时看什么 |
| --- | --- |
| `request_log` | 每次调用的元数据：状态、错误类型与文案、命中哪个节点、轮换了几次、排队与延迟、token 与费用。**不存 prompt / completion 正文**（出图提示词例外，存在 `image_prompt`）。`queue_wait_ms` 定位排队问题，`rotated_count > 0` 说明发生过 key 轮换。 |
| `key_rotation_event` | 每次轮换的「命中哪条规则、为什么冷却、冷却多久、重置时间从哪来」。key 为什么被换掉，只有这里说得清。 |
| `llm_node_key` | 上游 key 的实时状态：`state`、`cooldown_until`、`quota_class`、`matched_rule`。`secret` 是**明文**。 |
| `llm_node` / `llm_route` / `comfy_node` | 节点地址、并发、路由映射。地址填错看这里。 |
| `model_price` | 定价。缺价会在 `/api/admin/prices` 的 `models_without_price` 里列出来。 |
| `balance_txn` | 每一笔充值 / 调整 / 结算 / 退差，带 `balance_after` 与 `request_id`，对账以它为准。 |
| `account` / `api_key` | 账号余额、冻结金额、并发额度。`held_micro` 长期不降就是有预扣没释放。 |
| `backup_record` | 每次备份的路径、大小、`status`、错误文案。备份没生成时先看它。 |
| `setting` | `log_retention_days` 等管理台可改项。 |
| `usage_daily` | 按天分桶的汇总，**永久保留**，做趋势分析用它而不是 `request_log`。 |
| `_sqlx_migrations` | 迁移版本与 checksum。启动失败先看它。 |

一条好用的起手式（近一小时失败请求按错误类型分组）：

```sql
SELECT error_kind, error_message, COUNT(*)
FROM request_log
WHERE status <> 'ok' AND created_at > strftime('%s','now') - 3600
GROUP BY error_kind, error_message
ORDER BY 3 DESC
LIMIT 20;
```
