-- API Key 从账号上拆出来：一个账号多把 key，各自限流、各自统计。
--
-- 之前 `account` 表同时承担了「钱」和「调用方身份」两件事，key 只是它的一列，
-- 于是「同账号多把 key 各自限流」在结构上就做不到。而「一台设备一把 key」是
-- 家庭网关的常态：手机、笔记本、跑批脚本要能互相限流、互相看到用量。
--
-- 三件事：
--
-- 1. 新增 `api_key`，**并发额度从 account 下沉到 key**。额度不再是账号的总量，
--    而是每把 key 各有一份 —— 建 3 把 key 就是 3 份并发，账号层面不再单独设限
--    （两层闸门会配出「key 限 100、账号总 20」这种没人看得懂的拒绝）。
--    **余额仍然留在 account**：钱是账号的，key 只管频率和统计。
--
-- 2. `request_log` 记下 `api_key_id`，`usage_daily` 的主键加上它 ——
--    「每把 key 的 rpm / tpm / 出图张数」只能从这两个表切出来。
--    顺带把原来的 `key_id` 改名成 `node_key_id`：它指的是**上游**节点 key
--    （llm_node_key），和调用方的 api_key 放在同一张表里，不改名以后必混。
--
-- 3. 老数据归属：迁移前每个账号只有一把 key，把这些请求归到该账号的
--    「默认」key 名下。**这是近似** —— 轮换过 key 的账号，历史请求实际来自
--    几把不同的 key，但那时没记，无从区分。归到「默认」是唯一说得通的近似，
--    总比留个 NULL 造成统计断层好。

-- ⚠ **不要在这里写 `PRAGMA foreign_keys = OFF`**：
-- `sqlx::migrate!` 把每个迁移文件包在一个事务里执行，而 SQLite 的
-- `PRAGMA foreign_keys` 在事务内是**空操作**。写在这里不但没用，
-- 还会掩盖下面 `DROP TABLE account` 触发的级联删除：
-- 外键仍开着 → 删 account 时 `ON DELETE CASCADE` 把 `api_key` 和
-- `request_log` 的数据一并带走 → 迁移「成功」，但**数据全没了**。
-- `_sqlx_migrations` 只记文件执行完毕，不记每条语句是否生效。
--
-- 安全的做法是**不删父表**：保留 `account` 原样，只删它上面的列。
-- `ALTER TABLE … DROP COLUMN` 要 SQLite 3.35+，本仓库的 sqlx 内置
-- SQLite 满足；`foreign_keys` 开着也不影响 DROP COLUMN。

-- ── 1. api_key 表 ──────────────────────────────────────────────────────────
CREATE TABLE api_key (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id            INTEGER NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    -- 给人看的名字，同账号内唯一
    name                  TEXT    NOT NULL,
    -- 只存 SHA-256。库里被读走也无法直接冒用。
    key_hash              TEXT    NOT NULL UNIQUE,
    key_prefix            TEXT    NOT NULL,
    enabled               INTEGER NOT NULL DEFAULT 1,
    -- 并发额度：每把 key 独立一份
    llm_max_concurrency   INTEGER NOT NULL DEFAULT 2,
    llm_max_queue         INTEGER NOT NULL DEFAULT 8,
    image_max_concurrency INTEGER NOT NULL DEFAULT 1,
    image_max_queue       INTEGER NOT NULL DEFAULT 4,
    created_at            INTEGER NOT NULL,
    updated_at            INTEGER NOT NULL,
    UNIQUE (account_id, name)
);

-- 额度随 key 一起搬过去，行为不变（原来限几现在还是限几）
INSERT INTO api_key (account_id, name, key_hash, key_prefix, enabled,
                     llm_max_concurrency, llm_max_queue,
                     image_max_concurrency, image_max_queue,
                     created_at, updated_at)
SELECT id, '默认', api_key_hash, api_key_prefix, enabled,
       llm_max_concurrency, llm_max_queue,
       image_max_concurrency, image_max_queue,
       created_at, updated_at
FROM account
-- 空哈希的账号本来就没法鉴权，不要给它造一把假的
WHERE api_key_hash IS NOT NULL AND api_key_hash <> '';

-- ── 2. request_log：加 api_key_id，key_id → node_key_id ─────────────────────
CREATE TABLE request_log_new (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id        TEXT    NOT NULL UNIQUE,
    account_id        INTEGER NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    -- 调用方那把 key。删除时置 NULL：key 轮换后历史记录还要留着，
    -- ON DELETE CASCADE 会把别人的历史账一起删掉。
    api_key_id        INTEGER REFERENCES api_key(id) ON DELETE SET NULL,
    kind              TEXT    NOT NULL CHECK (kind IN ('llm','image')),
    protocol          TEXT    CHECK (protocol IN ('chat','responses','messages')),
    target            TEXT    NOT NULL,
    node_id           INTEGER,
    node_name         TEXT,
    -- 上游节点 key（llm_node_key），不是调用方的
    node_key_id       INTEGER,
    rotated_count     INTEGER NOT NULL DEFAULT 0,
    stream            INTEGER NOT NULL DEFAULT 0,
    status            TEXT    NOT NULL CHECK (status IN ('ok','error','rejected')),
    error_kind        TEXT,
    error_message     TEXT,
    queue_wait_ms     INTEGER NOT NULL DEFAULT 0,
    latency_ms        INTEGER NOT NULL DEFAULT 0,
    prompt_tokens     INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    cached_tokens     INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens  INTEGER NOT NULL DEFAULT 0,
    tokens_source     TEXT,
    image_count       INTEGER NOT NULL DEFAULT 0,
    cost_micro        INTEGER NOT NULL DEFAULT 0,
    created_at        INTEGER NOT NULL
);

INSERT INTO request_log_new (
    id, request_id, account_id, api_key_id, kind, protocol, target,
    node_id, node_name, node_key_id, rotated_count, stream, status,
    error_kind, error_message, queue_wait_ms, latency_ms, prompt_tokens,
    completion_tokens, cached_tokens, reasoning_tokens, tokens_source,
    image_count, cost_micro, created_at
)
SELECT r.id, r.request_id, r.account_id,
       (SELECT k.id FROM api_key k WHERE k.account_id = r.account_id LIMIT 1),
       r.kind, r.protocol, r.target,
       r.node_id, r.node_name, r.key_id, r.rotated_count, r.stream, r.status,
       r.error_kind, r.error_message, r.queue_wait_ms, r.latency_ms, r.prompt_tokens,
       r.completion_tokens, r.cached_tokens, r.reasoning_tokens, r.tokens_source,
       r.image_count, r.cost_micro, r.created_at
FROM request_log r;

DROP TABLE request_log;
ALTER TABLE request_log_new RENAME TO request_log;

CREATE INDEX idx_request_log_created ON request_log(created_at DESC);
CREATE INDEX idx_request_log_account ON request_log(account_id, created_at DESC);
-- 每把 key 的 rpm / tpm / 张数就是靠这个索引切出来的
CREATE INDEX idx_request_log_api_key ON request_log(api_key_id, created_at DESC);
CREATE INDEX idx_request_log_status ON request_log(status, created_at DESC);

-- ── 3. usage_daily：主键加上 api_key_id ────────────────────────────────────
CREATE TABLE usage_daily_new (
    day               TEXT    NOT NULL,
    account_id        INTEGER NOT NULL,
    api_key_id        INTEGER NOT NULL DEFAULT 0,
    target            TEXT    NOT NULL,
    node_id           INTEGER NOT NULL DEFAULT 0,
    requests          INTEGER NOT NULL DEFAULT 0,
    errors            INTEGER NOT NULL DEFAULT 0,
    prompt_tokens     INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    image_count       INTEGER NOT NULL DEFAULT 0,
    cost_micro        INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (day, account_id, api_key_id, target, node_id)
);

INSERT INTO usage_daily_new (day, account_id, api_key_id, target, node_id,
                             requests, errors, prompt_tokens,
                             completion_tokens, image_count, cost_micro)
SELECT u.day, u.account_id,
       COALESCE((SELECT r.api_key_id FROM request_log r
                  WHERE r.account_id = u.account_id
                    AND r.target = u.target
                    AND COALESCE(r.node_id, 0) = u.node_id
                  ORDER BY r.id LIMIT 1), 0),
       u.target, u.node_id, u.requests, u.errors, u.prompt_tokens,
       u.completion_tokens, u.image_count, u.cost_micro
FROM usage_daily u;

DROP TABLE usage_daily;
ALTER TABLE usage_daily_new RENAME TO usage_daily;

CREATE INDEX idx_usage_daily_day ON usage_daily(day DESC);

-- ── 4. account：去掉已下沉的列 ────────────────────────────────────────────
-- 用 DROP COLUMN 而不是「建新表再 DROP 旧的」：重建 account 会触发
-- ON DELETE CASCADE 把子表数据清空（见文件头的警告）。DROP COLUMN 不碰数据。
--
-- **索引必须先删**：0001 在 account 上建了 `idx_account_prefix`，
-- 而 DROP COLUMN 会连带校验索引，索引还引用着被删的列就会直接报错
-- （`error in index … after drop column: no such column`）——
-- 迁移失败，网关起不来。先把索引删掉。
DROP INDEX IF EXISTS idx_account_prefix;

ALTER TABLE account DROP COLUMN api_key_hash;
ALTER TABLE account DROP COLUMN api_key_prefix;
ALTER TABLE account DROP COLUMN llm_max_concurrency;
ALTER TABLE account DROP COLUMN llm_max_queue;
ALTER TABLE account DROP COLUMN image_max_concurrency;
ALTER TABLE account DROP COLUMN image_max_queue;
