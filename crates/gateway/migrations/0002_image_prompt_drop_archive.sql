-- 出图提示词：独立成表，与调用明细分开。
--
-- 为什么不是给 request_log 加一列：
--   request_log 的定位是**元数据表**（建表时明写「不存 prompt / completion 正文」）。
--   出图提示词是正文性质的内容，混进去会让那张表的语义和保留策略都变含糊。
--
-- 为什么只记出图、不记 LLM：
--   LLM 的 prompt / completion 正文仍不落库（见 README「不做的事」），那才是
--   体积与隐私的主要来源。出图提示词很短，且是「这张图怎么来的」唯一的可追溯
--   线索，值得留。
--
-- 保留期跟随 log_retention_days（见 billing/usage::purge_older_than），不单独设开关：
-- 提示词无限增长与请求明细无限增长是同一个问题，分开设只会让人漏配一个。
--
-- 0001 里 request_log 上那句「不存 prompt / completion 正文」依然成立：
-- 本次是把**出图**提示词挪到独立的表，而不是往那张表里加列，
-- 免得它的定位从「元数据表」变成「一半元数据一半正文」。
--
-- request_id 外键到 request_log，级联删除：清明细时提示词跟着走，不留孤儿。
CREATE TABLE image_prompt (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id  TEXT    NOT NULL UNIQUE
                        REFERENCES request_log(request_id) ON DELETE CASCADE,
    account_id  INTEGER NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    workflow    TEXT    NOT NULL,
    prompt      TEXT    NOT NULL,
    created_at  INTEGER NOT NULL
);
CREATE INDEX idx_image_prompt_created ON image_prompt(created_at DESC);
CREATE INDEX idx_image_prompt_account ON image_prompt(account_id, created_at DESC);

-- 图片落盘归档整体移除：图片由调用方自己保存，网关只回 base64。
--
-- 这里删列而不是留一个失效开关，有两个具体原因：
--   1. 归档写出去的 url 指向 `/api/admin/images/...`，而这个路由**从来没有存在过**，
--      返回的链接本来就是 404。
--   2. archive_retention_days 配了从来没有执行者，data/images/ 只增不减。
ALTER TABLE workflow DROP COLUMN archive;
ALTER TABLE workflow DROP COLUMN archive_retention_days;
