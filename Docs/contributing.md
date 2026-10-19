# 贡献指南

## 环境要求

| 工具 | 版本 | 说明 |
| --- | --- | --- |
| Rust | ≥ 1.82 | 由 `Cargo.toml` 的 `rust-version` 约束。仓库**没有** `rust-toolchain` 文件 |
| Node.js | CI 用 22，本地 25 实测可用 | vite 6 |
| Python 3 | 系统自带即可 | 只有 `scripts/` 下的脚本和 SQL 审计要用；`audit_sql.py` 不依赖第三方包 |

## 本地开发

```bash
# 后端 → :8080
set -a && . ./.env && set +a    # 必需，见下
cargo run -p home-ai-center

# 前端 → :5173，已把 /api 与 /v1 代理到 127.0.0.1:8080
cd web && npm run dev
```

> ⚠️ **代码不读 `.env` 文件，只认进程环境变量。** `config.rs` 用 `std::env::var`，
> 没有 dotenv 依赖。忘了 `set -a && . ./.env && set +a` 就起服务，**管理员不会被初始化**，
> 只在日志里留一句 warn。
> `docker compose up -d` 是唯一会自动读 `.env` 的路径。

`.env` 含管理员口令，已在 `.gitignore`，**从未进入过 git 历史**；可提交的那份是 `.env.example`。
口令只在**首次启动**时用于初始化，改这个变量不会改已存的口令（要改走「设置 → 管理员」）。

## 前后端构建顺序（硬约束）

`include_dir!` 在 `cargo build` 时把 `web/dist` 打进二进制（`crates/gateway/src/webui.rs`），
**`web/dist` 不存在会直接 panic**。

所以**任何涉及前端的改动，顺序固定为「先 `npm run build`，再 `cargo build`」**，
反了会编不过或内嵌旧产物。`web/dist` 是 gitignore 的产物、不是源码。
Dockerfile 与 CI 都是两段构建，不要为了「省一步」合并成一段。

## 验证

按改动范围跑，CI 跑的是全量：

```bash
cd web && npm run build          # 改了前端
cargo test --all                 # lib 测试（无集成测试、无 doctest）
python3 scripts/audit_sql.py     # 改了 SQL 才需要
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
```

**完成判据是「原始复现消失」**：改动前能复现的失败，在同一条命令下不再出现。
只跑新加的测试、或只确认编译通过，都不算完成。

`audit_sql.py` 查的两类错误编译期查不出、运行时才炸，症状都是
「某个 NOT NULL 列莫名其妙是 NULL」：SQL 里 `?` 数量与 `.bind()` 次数不符、
INSERT 列数与 VALUES 值数不符。**改完 SQL 要跑它**，`cargo test` 覆盖不到。

### fmt / clippy 基线是红的

`cargo fmt --all --check` 和 `cargo clippy --all-targets -- -D warnings` 在 main 上就已失败，
**与你的改动无关**。

不要为了让检查变绿去跑 `cargo fmt --all` 或批量修 lint —— 那会混进几十个文件的
无关 diff，其中「函数参数过多」一类往往是有意的设计取舍。正确做法：只保证
**自己碰过的行**干净（`rustfmt --check <file>` 可单文件核对），交付时说明
「基线本就红，失败项与本次改动无关」。修基线是独立的、值得单独提的改动。

## 数据库迁移

**迁移只能追加，不能改已应用的文件。**

`sqlx::migrate!` 会把每个迁移文件的 checksum 记进 `_sqlx_migrations`，
**改动已跑过的迁移文件会让下次启动直接失败**（checksum mismatch），
已有的 `./data` 库和所有部署都起不来。

要改已上线的结构，只能新加 `000N_xxx.sql`；想纠正旧迁移的**注释**也一样，
新写一条迁移或写进代码注释里，别回去动 `0001`。

两个已踩过的 SQLite 陷阱，写新迁移前务必知道：

- `PRAGMA foreign_keys` **在事务内是空操作**，而 `sqlx::migrate!` 把每个迁移包在事务里。
  写了不但没用，还会掩盖重建父表触发的级联删除。
- `ALTER TABLE … DROP COLUMN` 会连带校验索引。**索引还引用着被删的列就会直接报错**，
  必须先 `DROP INDEX`。

## 别加「只写不读」的列

加新字段前先问一句：**谁读它？** 读不到就等于没加。

`0004` 一次性清了四个字段，共同点就是**没有任何执行路径读它们**：
`account.rpm_limit`（早期按 RPM 调度时代的遗留）、`llm_node_key.probe_failed`（零引用）、
`llm_node_key.count_false_positive`（从不写，恒为 0）、`llm_node.capabilities`（只写不查）。

留着它们会让后来的人以为限流、探测失败、误报统计这些机制已经生效了。
确实需要预留，就把理由写进迁移注释并让它进配置快照（能查），否则下一个人会当死字段删掉。

## 测试的两个反模式

**不要写死绝对日期。** key 重置时间要过 `clamp_reset`，它**拒绝已经过去的时刻**
（生产行为正确）。写死 `2026-10-01` 这种日期的测试，会在某天零点过后必然失败。
用 `keys::future_utc_date(2)` 取相对日期，它保证落在未来且在 8 天合理区间内。

**不要恒真断言。** `assert!(cond || true)` 恒通过、什么都没验，
是「把红测试变绿」最常见的写法。key 轮换、并发闸门这类承重模块的回归守卫一旦空转，
后面真出问题时没有任何东西挡着。assert 写不下去说明这条性质没想清楚，
该改测试设计而不是加 `|| true`。

## 安全约定（开源项目，硬要求）

- **绝不提交任何真实密钥、token、口令。** 可提交的只有 `.env.example`。
- **不要把真实内网域名、机器名、私有 IP 写进代码或文档。** 举例一律用占位符：
  API Key 用 `sk-hac-xxx`，私有地址用 RFC1918 保留段（`192.168.1.20` / `10.0.0.5`），
  机器名用 `<node-a>`。
- 上游节点 key **明文存库是有意设计**（见 [data-model.md](data-model.md)），
  但这不构成「可以把真实 key 提交进代码」的理由——它只该存在于运行中的 `./data`。
- `scripts/import_erotic_sci.py` 的设计原则是**脚本里不含任何明文密钥**，全部运行时从源仓库读，
  所以可以安全提交。改动它时别破坏这个性质。
- 提交前扫一遍：`git grep -nE "(sk-[A-Za-z0-9]{16,}|BEGIN .*PRIVATE KEY|192\.168\.|10\.[0-9]+\.)"`

## 文档分工

避免写出两份会漂移的真相：

| 内容 | 维护在 |
| --- | --- |
| 计费口径、协议层设计、并发闸门、key 轮换规则 | `README.md`（**权威**） |
| 动手前必须知道的流程与坑 | `AGENTS.md`（**权威**） |
| 端到端请求生命周期与设计理由 | [architecture.md](architecture.md) |
| 端点、请求响应结构、错误码 | [api-reference.md](api-reference.md) |
| schema 与迁移史 | [data-model.md](data-model.md) |
| 部署、备份恢复、升级、排障 | [operations.md](operations.md) |
| 凭证模型、威胁边界、加固清单 | [security.md](security.md) |

改了上面某一项的**行为**，记得同步对应那份文档。

## `scripts/import_erotic_sci.py` 是数据迁移，不是环境配置

它依赖系统 python3 的 pyyaml 和隔壁 `../erotic_sci` 仓库，**要求网关已启动**，
会往库里写节点 / key / 路由 / 定价 / 账号。预演用 `--dry-run`。

它幂等（按名字 upsert），但仍属于**会改变数据的操作**，执行前先和用户确认。
