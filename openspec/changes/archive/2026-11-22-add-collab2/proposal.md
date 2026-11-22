# Proposal

## Why

内网同事需要用一个房间号共同操作主持人的 OMP 会话，而不是交换原生长链接或登记工号。复用知识库的部署入口，可让 Windows 和 CentOS 7 用户使用同一套命令协作，无需单独部署中继。

## What Changes

- 在知识库现有 Rust/Axum 监听上增加房间号登记、查询和原生 Collab WebSocket 中继。房间号仅保存在内存；同号只允许一个主持登记成功，首次主持连接结束即关房，不自动恢复。
- 配套 OMP TUI 用 `/collab2 <房间号>` 分享当前会话；多位同事用 `/collab2 join <房间号>` 直接加入，无需工号或确认。主持人和来宾都能向主持人的 Agent 发消息，来宾权限与原生 Collab 可写加入一致。
- `/collab2 stop` 只关闭本实例通过 `/collab2` 开的房间；主持人切换会话也关闭旧房。保留原有 `/collab`、`/join`、知识库 Web 和 MCP 行为。
- 知识库 Web、MCP 和房间登记、查询共用现有 HTTP 监听，协作中继使用同端口 WS，不部署 HTTPS/WSS 或 TLS 代理。OMP 默认连接 `127.0.0.1:9333`，可用 `OMP_COLLAB2_IP` 指定服务端 IP，端口不变。保留原生会话帧加密，不新增独立进程、应用监听端口、数据库、身份配置、鉴权、审批或加密协议。

## Capabilities

### New Capabilities

- `collab-room-relay`：同端口房间号登记与查询、原生会话中继、关房撤销及部署契约。

### Modified Capabilities

## Impact

本变更仅在知识库仓库实施：增加独立的 Axum 协作路由与内存状态，并接入现有停机流程。OMP 命令、生命周期接入与客户端发行由 OMP 仓库单独交付，本变更定义与其联调的契约。房间号授予控制能力，登记和查询走明文 HTTP，仅在可信隔离内网开放跨机协作入口。
