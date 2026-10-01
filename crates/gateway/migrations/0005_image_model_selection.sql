-- 出图按 **model** 选工作流，不再有「默认工作流」这回事。
--
-- 缺省值来自唯一性：只有一个启用的模型时不传 model 也能用，多于一个就报错
-- 并列出可选值。`default_image_workflow` 那种设置正是本仓库要根除的
-- 「配了不生效」——库里残留了什么就让出图结果变成什么。
--
-- 这次只删数据，不动表结构：模型名不是存出来的列，而是从 workflow 的
-- comfy_workflow JSON 里推导（domain::workflow::derive_model），
-- 单一事实来源，不存在「改了 checkpoint 忘了改名字」。
--
-- model → workflow 的映射与工作流表在同一个表里，所以没有新表。

DELETE FROM setting WHERE key = 'default_image_workflow';
