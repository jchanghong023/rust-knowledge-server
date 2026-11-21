# Proposal

## Why

公司内网需要可集中部署的文档知识库，让资料维护者通过浏览器统一管理文档，让兼容客户端通过标准 MCP 接口检索同一份发布资料。使用独立 Rust HTTP 服务交付 Web 与 MCP 两个入口，便于离线部署和统一管理数据。

## What Changes

- 仅交付 Windows x64 和 CentOS 7 x64，各一个可执行文件，包含 Web 资源和进程内 Qdrant；服务按需打开或创建数据库。用户不安装、下载、启动或配置独立数据库进程。`OMP_CONFIG_ROOT` 统一指定文件根，所有服务管理的文件归入专用子目录。开发阶段只在 Windows 上验证；CentOS 7 由验收人员在目标服务器上手工验证，不作为开发阶段门禁。
- 浏览器以恰好 8 位数字的工号登记会话，不验证身份、不区分权限。浏览器可创建和删除知识库、整库导入或替换原始 Markdown、查看导入状态、测试搜索并查看原文与来源；服务持久记录登记和操作结果，并在重启后恢复可确认的结果。
- `/mcp` 使用标准 Streamable HTTP，仅开放只读知识库搜索，免工号登记。Web 和 MCP 使用同一检索逻辑。基础全文检索不依赖模型；服务端可独立配置 Embedding 及 Rerank，分别补充语义召回和调整有限候选顺序。本期仅实测全文；模型 API 用本地 HTTP mock 验证功能，不验真实模型效果。
- 从原始 Markdown 构建索引，保留原文及可核验的来源。检索优先返回查询中的完整技术名称；可选模型不可用时保留可用的基础检索结果。

## Capabilities

### New Capabilities

- `knowledge-service`: 独立单文件交付、本地嵌入式索引生命周期、文档导入和来源保真的检索。
- `knowledge-web`: 浏览器工号会话、管理、搜索测试及操作留痕。
- `knowledge-mcp`: 面向通用客户端的标准只读 Streamable HTTP MCP。

### Modified Capabilities

无。独立仓库尚无主规格。

## Impact

本变更仅涉及 `rust-knowledge-server`，运行时使用嵌入式 Qdrant Edge，无需独立数据库服务或单独的前端分发文件。公司内网须提供服务可访问的持久化块存储，限制 Web 和 MCP 的访问范围，并只导入获准在该范围共享的资料。
