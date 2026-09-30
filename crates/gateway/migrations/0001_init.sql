-- home-ai-center 初始 schema
-- 金额一律 INTEGER 微元（1e-6 元），全程整数，杜绝浮点误差。
-- 时间戳一律 INTEGER unix 秒（UTC）。

CREATE TABLE account (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    name                TEXT    NOT NULL UNIQUE,
    api_key_hash        TEXT    NOT NULL,
    api_key_prefix      TEXT    NOT NULL,
    enabled             INTEGER NOT NULL DEFAULT 1,
    balance_micro       INTEGER NOT NULL DEFAULT 0,
    held_micro          INTEGER NOT NULL DEFAULT 0,
    llm_max_concurrency INTEGER NOT NULL DEFAULT 2,
    llm_max_queue       INTEGER NOT NULL DEFAULT 8,
    image_max_concurrency INTEGER NOT NULL DEFAULT 1,
    image_max_queue     INTEGER NOT NULL DEFAULT 4,
    rpm_limit           INTEGER,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL
);
CREATE INDEX idx_account_prefix ON account(api_key_prefix);

CREATE TABLE balance_txn (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id    INTEGER NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    kind          TEXT    NOT NULL CHECK (kind IN ('topup','adjust','settle','refund')),
    amount_micro  INTEGER NOT NULL,
    balance_after INTEGER NOT NULL,
    request_id    TEXT,
    note          TEXT,
    created_at    INTEGER NOT NULL
);
CREATE INDEX idx_balance_txn_account ON balance_txn(account_id, id DESC);

CREATE TABLE llm_node (
    id                      INTEGER PRIMARY KEY AUTOINCREMENT,
    name                    TEXT    NOT NULL UNIQUE,
    kind                    TEXT    NOT NULL DEFAULT 'openai',
    base_url                TEXT    NOT NULL,
    lan_base_url            TEXT,
    max_concurrency         INTEGER NOT NULL DEFAULT 2,
    default_max_output_tokens INTEGER,
    enabled                 INTEGER NOT NULL DEFAULT 1,
    sort_order              INTEGER NOT NULL DEFAULT 0,
    disabled_start          INTEGER,
    disabled_end            INTEGER,
    disabled_timezone       TEXT,
    extra_headers           TEXT    NOT NULL DEFAULT '{}',
    extra_body              TEXT    NOT NULL DEFAULT '{}',
    capabilities            TEXT    NOT NULL DEFAULT '{}',
    created_at              INTEGER NOT NULL,
    updated_at              INTEGER NOT NULL,
    CHECK (max_concurrency BETWEEN 1 AND 64)
);

-- 上游 API key 池。secret 明文存储（与 erotic_sci 的密钥策略一致：
-- 假脱敏只会制造「key 对不上」的排障成本）；API 列表只回 label + 末 4 位。
-- cooldown_until 持久化是「运行时态只在内存」的唯一例外：5 小时冷却若因
-- 容器重启丢失，重启后会立刻再撞一次限额，用户可见。
CREATE TABLE llm_node_key (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    node_id             INTEGER NOT NULL REFERENCES llm_node(id) ON DELETE CASCADE,
    label               TEXT    NOT NULL,
    secret              TEXT    NOT NULL,
    sort_order          INTEGER NOT NULL DEFAULT 0,
    enabled             INTEGER NOT NULL DEFAULT 1,
    state               TEXT    NOT NULL DEFAULT 'active'
                        CHECK (state IN ('active','cooling','quarantined','disabled')),
    cooldown_until      INTEGER,
    reset_source        TEXT,
    quota_class         TEXT,
    matched_rule        TEXT,
    matched_signal      TEXT,
    rate_limit_scope    TEXT    NOT NULL DEFAULT 'perKey'
                        CHECK (rate_limit_scope IN ('perKey','account')),
    soft_cap_window_ms  INTEGER,
    soft_cap_tokens     INTEGER,
    window_started_at   INTEGER,
    window_tokens_used  INTEGER NOT NULL DEFAULT 0,
    probe_failed        INTEGER NOT NULL DEFAULT 0,
    count_429           INTEGER NOT NULL DEFAULT 0,
    count_rotations     INTEGER NOT NULL DEFAULT 0,
    count_false_positive INTEGER NOT NULL DEFAULT 0,
    last_used_at        INTEGER,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    UNIQUE (node_id, label)
);
CREATE INDEX idx_node_key_node ON llm_node_key(node_id, sort_order);

CREATE TABLE key_rotation_event (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    key_id        INTEGER NOT NULL REFERENCES llm_node_key(id) ON DELETE CASCADE,
    node_id       INTEGER NOT NULL,
    request_id    TEXT,
    quota_class   TEXT    NOT NULL,
    matched_rule  TEXT    NOT NULL,
    matched_signal TEXT,
    cooldown_ms   INTEGER NOT NULL,
    reset_source  TEXT    NOT NULL,
    created_at    INTEGER NOT NULL
);
CREATE INDEX idx_rotation_event_key ON key_rotation_event(key_id, id DESC);
CREATE INDEX idx_rotation_event_time ON key_rotation_event(created_at DESC);

CREATE TABLE llm_route (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    model_name     TEXT    NOT NULL,
    node_id        INTEGER NOT NULL REFERENCES llm_node(id) ON DELETE CASCADE,
    upstream_model TEXT    NOT NULL,
    enabled        INTEGER NOT NULL DEFAULT 1,
    priority       INTEGER NOT NULL DEFAULT 0,
    UNIQUE (model_name, node_id)
);
CREATE INDEX idx_route_model ON llm_route(model_name, enabled);

CREATE TABLE model_price (
    model_name                  TEXT    PRIMARY KEY,
    input_micro_per_1k          INTEGER NOT NULL DEFAULT 0,
    output_micro_per_1k         INTEGER NOT NULL DEFAULT 0,
    cached_input_micro_per_1k   INTEGER
);

CREATE TABLE comfy_node (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    name               TEXT    NOT NULL UNIQUE,
    base_url           TEXT    NOT NULL,
    lan_base_url       TEXT,
    username           TEXT    NOT NULL DEFAULT '',
    password           TEXT    NOT NULL DEFAULT '',
    max_concurrency    INTEGER NOT NULL DEFAULT 1,
    enabled            INTEGER NOT NULL DEFAULT 1,
    sort_order         INTEGER NOT NULL DEFAULT 0,
    disabled_start     INTEGER,
    disabled_end       INTEGER,
    disabled_timezone  TEXT,
    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL,
    CHECK (max_concurrency BETWEEN 1 AND 64)
);

CREATE TABLE workflow (
    id                     INTEGER PRIMARY KEY AUTOINCREMENT,
    name                   TEXT    NOT NULL UNIQUE,
    mode                   TEXT    NOT NULL DEFAULT 'template'
                           CHECK (mode IN ('template','raw')),
    node_ids               TEXT    NOT NULL DEFAULT '[]',
    comfy_workflow         TEXT    NOT NULL DEFAULT '{}',
    param_slots            TEXT    NOT NULL DEFAULT '[]',
    price_micro            INTEGER NOT NULL DEFAULT 0,
    enabled                INTEGER NOT NULL DEFAULT 1,
    archive                INTEGER NOT NULL DEFAULT 0,
    archive_retention_days INTEGER,
    created_at             INTEGER NOT NULL,
    updated_at             INTEGER NOT NULL
);

-- 调用明细：只存元数据，**不存 prompt / completion 正文**。
CREATE TABLE request_log (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id        TEXT    NOT NULL UNIQUE,
    account_id        INTEGER NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    kind              TEXT    NOT NULL CHECK (kind IN ('llm','image')),
    protocol          TEXT    CHECK (protocol IN ('chat','responses','messages')),
    target            TEXT    NOT NULL,
    node_id           INTEGER,
    node_name         TEXT,
    key_id            INTEGER,
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
CREATE INDEX idx_request_log_created ON request_log(created_at DESC);
CREATE INDEX idx_request_log_account ON request_log(account_id, created_at DESC);
CREATE INDEX idx_request_log_status ON request_log(status, created_at DESC);

CREATE TABLE usage_daily (
    day              TEXT    NOT NULL,
    account_id       INTEGER NOT NULL,
    target           TEXT    NOT NULL,
    node_id          INTEGER NOT NULL DEFAULT 0,
    requests         INTEGER NOT NULL DEFAULT 0,
    errors           INTEGER NOT NULL DEFAULT 0,
    prompt_tokens    INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    image_count      INTEGER NOT NULL DEFAULT 0,
    cost_micro       INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (day, account_id, target, node_id)
);
CREATE INDEX idx_usage_daily_day ON usage_daily(day DESC);

CREATE TABLE setting (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE backup_record (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    path        TEXT    NOT NULL,
    size_bytes  INTEGER NOT NULL DEFAULT 0,
    status      TEXT    NOT NULL CHECK (status IN ('ok','error')),
    error       TEXT,
    started_at  INTEGER NOT NULL,
    finished_at INTEGER
);

CREATE TABLE admin_user (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    username      TEXT    NOT NULL DEFAULT 'admin',
    password_hash TEXT    NOT NULL,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);

CREATE TABLE session (
    token      TEXT PRIMARY KEY,
    expires_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
