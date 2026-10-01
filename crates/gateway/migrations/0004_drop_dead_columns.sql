-- 清掉四个「只写不读」或「读了但没人用」的字段。
--
-- 共同点：**没有任何执行路径读它们**。留着它们会让后来的人以为
-- 限流、探测失败、误报计数、能力标记这些机制已经生效了。
--
-- 1. account.rpm_limit
--    早期设计是按 RPM 调度，后来改成按并发度调度（见 README），
--    准入路径（api::v1 / gate）从来没读过它。能写能读，但不设防。
--    重新引入 RPM 限流时再加回来，那时它会带真正的执行逻辑。
--
-- 2. llm_node_key.probe_failed
--    零引用：建表之后没有任何代码读写过它。
--
-- 3. llm_node_key.count_false_positive
--    只在 db/load.rs 被读进内存，三处结构体初始化又硬置 0，
--    **从不写库**，所以恒为 0。要统计误报得先有「误报」的判定，
--    那个判定至今不存在。
--
-- 4. llm_node.capabilities
--    只在写入路径出现，从不 SELECT、从不回 API、不进配置快照。
--    原注释写的是「预留位，将来接 /v1/responses 时不用改 schema」——
--    但一个没人读的预留位连「已填了什么」都查不到，起不到预留的作用；
--    真要接的时候加一列迁移的代价，远小于留一列误导性的死字段。
ALTER TABLE account DROP COLUMN rpm_limit;
ALTER TABLE llm_node_key DROP COLUMN probe_failed;
ALTER TABLE llm_node_key DROP COLUMN count_false_positive;
ALTER TABLE llm_node DROP COLUMN capabilities;
