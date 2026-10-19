# 文档索引

`home-ai-center` 的完整文档。**先读根目录的 [README.md](../README.md)**——
那是上手和设计决策的权威来源，本目录是往下钻的细节。

## 从这里开始

| 你想做的事 | 看哪篇 |
| --- | --- |
| 第一次装起来 | [README 快速开始](../README.md#快速开始) → [operations.md](operations.md) 部署 |
| 让现有客户端接进来 | [api-reference.md](api-reference.md) |
| 出图接不好 | [README 出图工作流](../README.md#出图工作流) → [api-reference.md](api-reference.md) 出图端点 |
| 排查线上问题 | [operations.md](operations.md) 排障手册 |
| 备份 / 恢复 / 升级 | [operations.md](operations.md) |
| 搞清楚一个请求怎么走完全程 | [architecture.md](architecture.md) |
| 改数据库相关代码 | [data-model.md](data-model.md) |
| 提交代码 | [contributing.md](contributing.md) |
| 评估安全风险 | [security.md](security.md) |

## 各篇定位

### [architecture.md](architecture.md) — 架构与请求生命周期

一个 `/v1/chat/completions` 从进进程到返回，经过哪些模块、判断了什么、失败怎么走。
含模块职责表、状态与并发模型、关键设计取舍的理由。

### [api-reference.md](api-reference.md) — API 参考

网关面 `/v1/*`（调用方 API Key 鉴权）与管理面 `/api/*`（管理台口令鉴权）**两套独立凭证**下的
全部端点：请求响应字段、流式事件形态、错误码与出现条件。

### [data-model.md](data-model.md) — 数据模型

六个迁移全部应用之后的 schema，按域分组说明。含**迁移史里删掉的东西**，
解释当前表结构为什么「缺字段」。

### [operations.md](operations.md) — 自托管运维

部署、完整环境变量参考、备份与**恢复 runbook**、**升级 runbook**、
按「症状 → 原因 → 确认 → 修」组织的排障手册。

### [security.md](security.md) — 安全

两套凭证模型、威胁边界与信任关系、数据留存与隐私、加固清单、
以及**贡献者安全约定**（提交前自查清单）。

### [contributing.md](contributing.md) — 贡献指南

环境要求、前后端构建顺序、验证命令、迁移硬约束，以及几个反复踩过的坑
（基线红、恒真断言、写死日期、只写不读的列）。

## 文档分工（避免两份真相）

```
README.md      权威：上手路径 + 计费口径 + 协议层设计 + 并发闸门 + key 轮换
AGENTS.md      权威：动手前必须知道的流程与坑
Docs/          往下钻的细节
```

**改了行为要同步文档**：README 管口径，`Docs/` 管细节。
新增一篇文档时也请在上表登记。
