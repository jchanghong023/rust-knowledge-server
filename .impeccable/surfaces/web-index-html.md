---
version: 1
slug: "web-index-html"
primary_target: "web/index.html"
related_targets: ["web/app.css","web/app.js"]
---

# 网页工作台与活动记录

主入口：`/#activity`。模式：Operate。团队成员无需登记即可核对最近登记的工号与仍保留的操作；回到工作台才登记、管理知识库和搜索。

## Direction contract

**THESIS** — 活动记录是一张可逐行追溯的清册，而不是登记表下的附属折叠区。

**OWN-WORLD** — 蓝晒索引：深蓝导航、浅蓝工作底、白色主面。界面对齐而非装饰；工号和时间统一等宽数字；反馈色只表达实际操作结果。

**STORY** — 到达活动记录就看见可核对的工号及时间。展开原行，下方出现浅蓝底的操作表；刷新继续保留展开。无需为浏览登记。

**FIRST VIEWPORT** — 桌面左侧导航、右侧页面标题，下一块白色表面列出工号和最后登记时间；刷新在名单工具栏。右侧解释登记的时间范围和意义，手机端导航移到顶部，清册先于说明。工作台沿用同一套页面结构和控件。

**FORM** — 蓝晒接触印相，候选 7；seed:c5f78ba77f921e61。用户从表单选择此方向，采用 code-led。

**FINISH** — unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance
