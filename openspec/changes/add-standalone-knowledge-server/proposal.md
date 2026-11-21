# Proposal

## Why

团队需要可集中部署的知识库，让资料维护者通过浏览器统一管理企业知识和代码 Wiki，让 Agent 通过标准 MCP 接口搜索同一份发布资料、引用原文并按需补读上下文。使用独立 Rust HTTP 服务交付 Web 与 MCP 两个入口，便于离线部署和统一管理数据。

## What Changes

- 仅交付 Windows x64 和 CentOS 7 x64，各一个可执行文件，包含 Web 资源和进程内 Qdrant；不需要单独的数据库进程。不引入服务配置文件；运行设置通过命令行，CentOS 7 可只读复用已有 Claude Code 网关配置来自动启用模型，`--help` 提供说明。Windows 数据固定存于当前用户的 `~/.rust-knowledge-server`；CentOS 7 的所有服务数据固定存于 `/dev/shm/rust-knowledge-server`，不提供数据路径设置。开发阶段在 Windows 上验证；CentOS 7 由验收人员在目标服务器上手工验证。
- CentOS 7 同一可执行文件提供 `start`、`status`、`stop`，普通用户可后台启动、查看状态和正常停机。直接运行仍以前台启动；不安装系统服务，不自动拉起或开机自启。
- 浏览器以恰好 8 位数字的工号登记会话，不验证身份、不区分权限。浏览器可创建和删除知识库、整库导入或替换原始 Markdown、查看导入状态、测试搜索并查看原文与来源；服务记录登记和操作结果，数据根保留时可在进程重启后恢复结果。CentOS 7 机器重启后，服务从空库启动，操作者自行保管 Markdown 并手动重导。
- Web 访问者无需登记即可查看最近 30 天成功登记过的自报工号和最近登记时间，并按工号查看仍保留的知识库操作、时间及结果。部署须确认访问范围获准共享这些记录；MCP 不提供此功能。
- `/mcp` 使用标准 Streamable HTTP，仅提供只读的 `knowledge_search` 和 `knowledge_fetch`，免工号登记。搜索返回有界原文及出处，补读只需原样传回命中引用。Web 和 MCP 使用同一检索逻辑。基础全文检索不依赖模型；服务端可独立配置 Embedding 及 Rerank，分别补充语义召回和调整有限候选顺序。已有 `OMP_OFFLINE=1` 和有效 Claude Code 网关配置的 CentOS 7 环境可直接前台或后台启动，无需额外模型参数或凭据。开发测试使用本地 HTTP mock；目标 CentOS 7 验收实际网关兼容性，不以模型质量指标作为门槛。
- 从原始 Markdown 构建索引，保留原文及可核验的来源。检索优先返回查询中的完整技术名称；可选模型不可用时保留可用的基础检索结果。

## Capabilities

### New Capabilities

- `knowledge-service`: 独立单文件交付、本地嵌入式索引生命周期、文档导入和来源保真的检索。
- `knowledge-web`: 浏览器工号会话、管理、搜索测试、操作留痕及活动记录查看。
- `knowledge-mcp`: 面向 Agent 的标准只读 Streamable HTTP MCP，提供原文搜索与按引用补读。

### Modified Capabilities

无。独立仓库尚无主规格。

## Impact

本变更仅涉及 `rust-knowledge-server`，无需独立数据库服务、Claude Code 运行时或单独的前端分发文件。Windows 使用本地持久化存储；CentOS 7 使用可写的 `/dev/shm`（tmpfs），不读写 NFS 作为服务存储。部署方须限制 Web 和 MCP 的访问范围，并只导入获准在该范围共享的资料；自动启用模型的部署须确认查询和文档片段获准发送至所配置的网关。
