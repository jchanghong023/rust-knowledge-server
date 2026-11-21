---
name: 知识库工作台
description: 蓝晒索引；用于管理共享文档和核对活动的操作界面。
colors:
  ink: "#172941"
  quiet: "#50637c"
  surface: "#ffffff"
  canvas: "#edf3fb"
  wash: "#f3f7fd"
  divider: "#d5e0ee"
  control: "#8598b0"
  rail: "#142945"
  rail-ink: "#eaf2ff"
  rail-quiet: "#b6c7df"
  rail-active: "#254976"
  primary: "#244fc4"
  primary-hover: "#1d3e9e"
  success: "#21634b"
  danger: "#9a303e"
  warning: "#79521b"
typography:
  body:
    fontFamily: 'system-ui, "Microsoft YaHei", "PingFang SC", sans-serif'
    fontSize: "1rem"
    lineHeight: 1.55
  page-title:
    fontSize: "2.125rem"
    lineHeight: 1.3
    letterSpacing: "-.025em"
  section-title:
    fontSize: "1.1875rem"
    lineHeight: 1.3
  control-label:
    fontSize: ".875rem"
    fontWeight: 650
rounded:
  control: ".5rem"
  panel: ".75rem"
spacing:
  panel-inset: "1.5rem"
  panel-gap: "1.25rem"
components:
  button-primary:
    backgroundColor: "{colors.primary}"
    textColor: "{colors.surface}"
    rounded: "{rounded.control}"
    padding: ".45rem .85rem"
  button-secondary:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.ink}"
    rounded: "{rounded.control}"
    padding: ".45rem .85rem"
  panel:
    backgroundColor: "{colors.surface}"
    rounded: "{rounded.panel}"
    padding: "{spacing.panel-inset}"
---

## Overview

蓝晒索引用深蓝导航定位工作区域，用浅蓝底区分页面与白色主面。工号和时间对齐；操作历史在展开的工号下方原位显示，不跳转到另一个查询页面。

## Colors

白色承载表单与清册，浅蓝承载解释与明细。钴蓝用于主操作和焦点，深蓝区域反转焦点颜色。结果靠文字表达，绿色、红色和棕色分别辅助识别成功、失败及中断或未知。

## Typography

沿用同一系统无衬线字体。工号和时间使用 `tabular-nums`，只有摘要中的哈希和行号等代码数据用等宽字。手机端页面标题为 `1.75rem`。

## Layout

桌面使用 `14rem` 左导航，登记清册和范围说明并排；宽度不超过 `68rem` 时范围说明下移，不超过 `55rem` 时导航移到顶部。`42rem` 以内缩小面板内距、将工作台和搜索筛选改为单列。操作时间、目标库与结果允许换行，不隐藏字段。

## Elevation & Depth

不使用阴影。用 `1px` 分隔线和色面区分层级；仅确认对话框使用遮罩保护当前操作。

## Shapes

面板统一 `12px` 圆角，控件 `8px`。用细分隔线组织表格，不给每一行再加独立卡片。

## Components

- 主导航用 `aria-current="page"` 表达当前位置；工作台与活动记录分开。
- 活动清册使用原生展开控件。标题和列保持原位，展开内容铺浅蓝底；箭头仅旋转 150ms，减少动态效果时不播放。
- 实色主按钮用于登记、导入和搜索；删除以红色标识并要求确认。键盘焦点有清楚的 `2px` 描边。

## Do's and Don'ts

- 让空态、加载、失败和数量出现于所属区域，不在页面顶层堆状态。
- 不将自报工号视觉化为身份凭证或在线指示，不引入不会帮助核对记录的指标或插图。
