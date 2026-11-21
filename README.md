# Rust Knowledge Server

面向公司内网的独立 Rust 文档知识库，使用嵌入式 Qdrant Edge，无需另行部署数据库进程。

**状态：Windows 开发任务已通过验证，尚未完成发布验收。** CentOS 7 由验收人员在目标服务器上手工验证。

## 使用方式

- **Web 管理**：用 8 位 ASCII 数字工号登记后建库、上传 Markdown、查看导入状态、测试搜索或确认删除。工号只用于操作留痕，不验证身份；已登记用户的权限相同。新建库名不得为空白或含控制字符，最多 1,024 个 UTF-8 字节。
- **导入和重导**：准备 UTF-8 `.md` 文件（扩展名不区分大小写）并保留库内相对路径。可多选文件或选择文件夹；PDF、Word 等先转为 Markdown。每批上传上限 1 GiB（含 multipart 元数据），超限返回 HTTP `413`；每条相对路径上限 16 KiB（UTF-8）。重导以整批内容替换该库；成功后未上传的旧文档不再可查，失败时保留上次发布版本。若导入结果未知，同一浏览器标签页刷新或重新登记后仍可核对操作编号，确认终态前不要盲目重导。删除只影响服务中的库，不修改源文件。
- **搜索**：Web 和只读 `/mcp` 共用检索；支持 Streamable HTTP 的 MCP 客户端无需登记工号。证据包含原文、库名、相对路径、行号及文档 SHA-256。全文检索无需模型；Embedding 和 Rerank 独立选配，分别补充语义召回和调整有限候选顺序。

## 部署要求

- 仅交付 Windows x64（最低 Windows 11 25H2）和 CentOS 7 x64（glibc 2.17），各一个离线可运行的文件。`OMP_CONFIG_ROOT` 设为绝对路径时使用其 `rust-knowledge-server/` 子目录，否则使用 `~/.rust-knowledge-server/`。服务数据必须放在持久化块存储；不可用的显式路径、NFS 和其他网络文件系统不作回退。
- 使用本地持久化块存储：Windows 接受固定 NTFS/ReFS 卷；CentOS 7 的实现限制为可识别本地块设备上的 ext 或 XFS。`versions/`、`imports/`、`audit/` 必须留在数据根所在卷，不得另行挂载或链接。网络盘、易失存储和无法识别的存储会被拒绝，不回退。同一数据目录只能运行一个实例；正常停机释放目录锁。若 `catalog.json` 缺失且仍有服务数据，启动会拒绝继续并保留文件；应从备份恢复目录表，而不是清空索引。
- 只在已批准的内网范围部署 Web 和 MCP，并且只导入获准向该范围共享的资料。能访问服务不代表已验证员工身份。

默认只监听 `127.0.0.1:7336`。在 Windows 上可用 `cargo build --release --locked` 构建，再运行 `target\release\rust-knowledge-server.exe`；打开启动时打印的地址。若经反向代理或监听内网地址，在服务数据目录的 `config.json` 写明 `listen`、浏览器实际访问的 `web_origin`（方案、主机、端口）和 `mcp_hosts`（允许的部署主机名或主机名加端口）。例如：

```json
{
  "listen": "127.0.0.1:7336",
  "web_origin": "https://knowledge.intranet.example",
  "mcp_hosts": ["knowledge.intranet.example"]
}
```

反向代理应保留许可的 `Host`，并把 Web 和 `/mcp` 一起限制在获准的内网范围。`mcp_origins` 默认空：原生客户端不发 `Origin` 可接入；发送 `Origin` 的请求必须匹配显式许可的源。浏览器登记及管理操作记录位于数据根的 `audit/`，工号为自报，记录不包含搜索词或文档原文。

### MCP 客户端

将支持 Streamable HTTP 的 MCP 客户端连接到服务的 `/mcp` 地址，例如 `https://knowledge.intranet.example/mcp`。服务提供只读工具 `knowledge_search`：必填 `query`；可选 `library` 与库内 `/` 分段相对路径 `path`，可指完整文件或目录子树。

以下为两个可选客户端的配置示例。OMP 使用 `mcp.json`（参见 [OMP HTTP 配置](https://github.com/can1357/oh-my-pi/blob/main/docs/mcp-config.md#http-transport)）：

```json
{
  "mcpServers": {
    "knowledge": {
      "type": "http",
      "url": "https://knowledge.intranet.example/mcp"
    }
  }
}
```

Claude Code（参见 [原生 HTTP MCP 配置](https://code.claude.com/docs/en/mcp)）：

```sh
claude mcp add --transport http knowledge https://knowledge.intranet.example/mcp
```

## 可选模型

在服务专用数据目录的 `config.json` 中独立配置 `embedding` 或 `rerank`；省略相应配置即关闭，两者默认均关闭。例如：

```json
{
  "embedding": {
    "base_url": "https://embedding.intranet.example",
    "model": "company-embedding",
    "dimensions": 1024
  },
  "rerank": {
    "base_url": "https://rerank.intranet.example",
    "model": "company-rerank"
  }
}
```

两项各自可指定 `api_key`，请限制配置文件的读权限。服务只调用配置的地址，分别使用 `/v1/embeddings` 和 `/v1/rerank`；不会探测其他服务。开启 Embedding 后需从原始 Markdown 重导才能为已有库生成兼容向量。查询向量或兼容代际不可用时保留全文检索；精排只处理有限候选，并且不会把相关命中移到完整名称之前。Web 和 MCP 分别报告当次实际使用的检索方式，以及是否采用精排；精排响应失败时保留基础排序。

## 对比混合检索效果

启动参数 `--fusion` 同时作用于 Web 和 MCP，启动日志打印所选模式：

| 参数 | 每个知识库内的处理方式 |
| --- | --- |
| `--fusion grouped`（默认） | 全文和向量分别由 Qdrant Edge 按文件分组，每路每个文件最多取两条候选，再由服务做 RRF。 |
| `--fusion edge` | Qdrant Edge 原生 RRF 先融合全文和向量候选，再按文件分组，每个文件最多取两条融合候选。 |

两种模式都独立召回完整技术名，并保留完整名称优先和现有跨库排序规则。没有可用向量时，两种模式使用相同的全文检索路径。切换模式不改索引，也不需要重导。

对比时，在同一个已配置 Embedding 且有兼容向量的索引上，正常停止服务后依次用两种参数启动，保留其他启动参数、配置和查询范围不变；不要让两个进程同时打开同一数据目录。先关闭可选 Rerank，确认返回 `retrieval: hybrid`，再用相同查询记录首条是否相关、前 5 条是否包含正确证据，以及前 5 条来自多少个文件。只对比全文回退结果不能区分两种方案。

## 本期验收范围

在 Windows 11 25H2 x64 上，平台探针在不授予联网能力的 Windows AppContainer 中成功绑定回环监听。`--edge-probe write <新目录>` 写入探针数据，另起进程使用 `--edge-probe read <同目录>` 验证持久化及稀疏 IDF 语料范围：相同候选过滤下，范围内 IDF 的排序为 `1, 2`，全局 IDF 为 `2, 1`。

Windows 本机发布文件验证（Chrome 157.0.8103.80）使用独立目录中的单个可执行文件。以 250 毫秒间隔采样文件逻辑长度，服务数据与版本索引的观测峰值分别为 537,915,969 和 415,095,509 字节。重启后复查的原文、相对路径、行号和 SHA-256 一致，导入开始与结果审计文件不变。本机重启不证明断电持久性，也不代替干净 Windows、CentOS 7 或内网验收。

另用最终 Windows 构建在浏览器建库并整批重导。OMP 原生 MCP 客户端通过独立项目的 `mcp.json` 连接同一回环实例，发现并调用 `knowledge_search`。关闭模型功能时，Web 与 MCP 的命中顺序、原文、相对路径、行号、SHA-256 和命中类型一致。此项不计入公司内网或 Claude Code 验收。

CentOS 7 的 glibc 2.17 不导出 Qdrant Edge 0.9.0 使用的 `pwritev2`。在 CentOS 7 x64 或等价 glibc 2.17 的构建机上运行 `bash release/build-centos7.sh`。脚本在构建临时目录针对锁定源码使用原始 Linux 系统调用，不改变 Edge 的追加语义、范围 IDF 实现或 Windows 依赖。构建需要 Rust、C 编译工具、`patch`、Python 3 和可用的 Cargo 依赖缓存或构建期仓库访问；交付文件为 `target/x86_64-unknown-linux-gnu/release/rust-knowledge-server`，不附带补丁、Python 或数据库进程。补丁可应用性和脚本语法已在 Windows 检查；未运行 CentOS 7 构建或测试，须由验收人员在目标服务器上确认。

真实 Markdown 语料由验收人员提供，仅用于全文实测，流程见[任务 4.1](openspec/changes/add-standalone-knowledge-server/tasks.md#4-部署验收)。Embedding 和 Rerank 使用本地 HTTP mock 验证请求、结果处理和失败回退；无需真实模型 API，不评测模型效果。

### 同语料性能对比（Windows）

旧 OMP 安装版与本服务均使用同一台 Windows 工作站、同一批 22 份 Markdown（约 117 MiB，逐文件 SHA-256 一致），只运行全文检索，关闭 Embedding 和 Rerank。

| 指标 | 旧 OMP 安装版 `18.10.4+fork.314` | 本服务 |
|---|---:|---:|
| 整批导入 | 19.06–19.07 秒（CLI） | 19.88 秒（回环 HTTP）；20.43–21.78 秒（Chrome） |
| 冷查询：首批结果 | 3.27–7.59 秒 | 1.41–1.64 秒 |
| 热查询：立即重复同一请求 | 18.6–51.8 毫秒 | 11.6–13.0 毫秒 |

查询使用相同的 5 个条件，每个条件各测 3 次取中位数；表中为各条件中位数的范围。冷查询在新启动的 worker 或服务进程上执行，未清空操作系统缓存。逐条件对比，本服务首批返回的延迟缩短至旧版的约 1/2.0–1/5.1（冷）和 1/1.5–1/4.3（热）。

两侧计时与结果范围不同：旧版查询走 worker IPC，支持完整排名及分页；本服务走 Web HTTP，包含审计落盘，最多返回 16 条候选。因此这些数据仅比较首批结果的返回延迟，不代表全量检索或引擎性能。导入计时也使用各自入口，包含不同的上传和状态确认开销。

### 待完成的发布与部署验收

| 门禁 | 仍需留存的证据 |
|---|---|
| 4.1 双平台发布 | 干净 Windows x64 离线单文件验证；CentOS 7 目标服务器验证 glibc 2.17、运行依赖、离线启动、范围 IDF、NFS 与独立挂载拒绝，以及版本与目录表的持久性。两端留存整批重导、重启复查、耗时和峰值占用。 |
| 4.2 内网验收 | 批准的网络范围与语料共享许可；用 Web 和原生 MCP 客户端（OMP、Claude Code）在同一实例查到原文与来源；范围外 Web 和 MCP 均不可达；记录重导峰值与重启结果。 |

## 规划文档

- [变更提案](openspec/changes/add-standalone-knowledge-server/proposal.md)
- 需求：[服务](openspec/changes/add-standalone-knowledge-server/specs/knowledge-service/spec.md) · [Web](openspec/changes/add-standalone-knowledge-server/specs/knowledge-web/spec.md) · [MCP](openspec/changes/add-standalone-knowledge-server/specs/knowledge-mcp/spec.md)
- [技术方案](openspec/changes/add-standalone-knowledge-server/design.md) · [实施与验收任务](openspec/changes/add-standalone-knowledge-server/tasks.md)
