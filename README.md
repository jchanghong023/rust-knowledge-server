# Rust Knowledge Server

在网页创建知识库、批量导入 Markdown，并搜索已发布的文档。Claude Code、OMP 等客户端也可通过 MCP 查询同一份资料，取得原文片段、文件路径和行号。

## 启动

### Windows x64

在可执行文件所在目录打开 PowerShell：

```powershell
.\rust-knowledge-server.exe
```

数据保存在 `~/.rust-knowledge-server`，机器重启后保留。

### CentOS 7 x64

从 [GitHub Releases](https://github.com/jchanghong023/rust-knowledge-server/releases) 下载 `rust-knowledge-server-centos7-x86_64.tar.gz` 并解压。

```sh
./rust-knowledge-server
```

数据保存在 `/dev/shm/rust-knowledge-server`，重启服务保留，**重启机器清空**。请另存原始 Markdown。

本机 Web 地址：<http://127.0.0.1:9333/>。

## Web

- **知识库管理**：创建、删除知识库；支持上传 UTF-8 Markdown 文件或文件夹，也支持从服务器可读的绝对路径导入。重新导入会替换整个库。
- **导入状态**：显示上传、索引和发布进度；已发布的库可查看收录文件。
- **搜索**：支持按知识库和相对路径筛选，返回原文摘录、文件路径和行号。
- **活动记录**：操作按自报工号留痕，不验证身份。任何能访问网页的人都能查看最近 30 天的登记工号及其保留的操作记录。

服务器路径按服务进程权限读取，不检查访问者个人的文件权限。

## MCP

`/mcp` 提供两个只读工具，无需登记工号：

- `knowledge_search`：搜索已发布的知识库，返回原文和出处。
- `knowledge_fetch`：按搜索命中的 `ref` 补读附近原文。

**Claude Code**（在项目终端运行）：

```sh
claude mcp add --transport http knowledge http://127.0.0.1:9333/mcp
```

**OMP**：配套版本无需配置；其他版本在 OMP 会话内输入：

```text
/mcp add knowledge --url http://127.0.0.1:9333/mcp
```

远程部署时，两个命令中的地址都换成服务的实际 `/mcp` 地址。

[技术方案](openspec/changes/add-standalone-knowledge-server/design.md) · [部署与验收](openspec/changes/add-standalone-knowledge-server/tasks.md#4-部署验收) · [MCP 接口](openspec/changes/add-standalone-knowledge-server/specs/knowledge-mcp/spec.md)
