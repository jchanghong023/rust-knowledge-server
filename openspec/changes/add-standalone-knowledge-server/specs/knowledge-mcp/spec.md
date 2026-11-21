# Spec Delta

## Purpose

提供标准 Streamable HTTP MCP 接口，让 Agent 搜索已发布的企业知识和代码 Wiki，直接引用原文证据，或按命中引用补读上下文，无需专用集成或浏览器会话。

## ADDED Requirements

### Requirement: 标准 Streamable HTTP 接入

服务 MUST 在同一个 `/mcp` 地址提供标准 Streamable HTTP MCP，并支持客户端完成协议初始化、工具发现和工具调用。客户端 MUST NOT 依赖专用代理、自定义传输、登记工号、浏览器会话或登录凭据。

#### Scenario: 两个独立的原生客户端使用同一地址
- **WHEN** 两个独立的 MCP 客户端各自使用原生 Streamable HTTP 配置连接同一服务地址
- **THEN** 两者能完成协议握手，发现 `knowledge_search` 和 `knowledge_fetch`，执行搜索并用某条命中的 `ref` 补读连续原文

#### Scenario: 请求 Host 与来源校验
- **WHEN** 原生客户端连接服务，以实际服务端 IP 和端口作为 Host 且不发送 `Origin`，或请求携带不匹配的 Host 或不允许的 `Origin`
- **THEN** 前者可完成标准 MCP 初始化；后者返回 HTTP 403、不执行工具调用

#### Scenario: MCP 不提供管理操作
- **WHEN** 客户端发现工具或试图经 MCP 创建、导入或删除知识库
- **THEN** 不会发现这些写工具，调用也不能造成写入；MCP 请求不会被归到任何已登记的浏览器工号

### Requirement: 搜索输入及可选范围

`knowledge_search` MUST 接受非空的查询词或自然语言文本 `query`，以及可选的库名 `library` 和库内相对路径 `path`，MUST NOT 接受其他参数。`path` 使用导入路径格式，按完整相对路径限定文件或按 `/` 分段限定目录子树；省略 `library` 时仅查所有已发布库，省略 `path` 时不限路径。

#### Scenario: 只传查询或指定已有范围
- **WHEN** 客户端只传 `query`，或者额外指定库名、文件或目录
- **THEN** 前者不限制库和路径；后者仅在指定范围内检索已发布资料

### Requirement: 搜索命中可直接引用

成功结果 MUST 返回 `status`（`ok` 或 `no_published_index`）和 `matches`。每条命中 MUST 包含 `text`、`library`、`path`、`line_start`、`line_end`、`category`（`exact` 或 `related`）、不透明的 `ref` 和布尔值 `truncated`。`text` MUST 是同一来源的连续原文，行号 MUST 是文本实际覆盖的一基闭区间；缩短原命中片段时 MUST 标记 `truncated`。原文即使只覆盖长行的一部分，行号仍指向该行。完整名称命中的摘录 MUST 保留支持该分类的完整名称；相关候选不得表述为已证实事实。

#### Scenario: 长命中缩短后仍可核对
- **WHEN** 完整名称命中的原文过长，无法完整返回
- **THEN** 摘录仍包含支持命中类别的完整名称，行号按实际返回内容标记，并设 `truncated: true`

### Requirement: 搜索输出去重及预算

每次搜索 MUST 在共享检索排序后去除同一版本同一文件内重叠的证据，保留较高排名者；不同文件的相同正文不视为重叠。返回 MUST 最多 5 条命中，所有 `text` 的 UTF-8 字节数合计最多 4 KiB（4096 字节，不含元数据与序列化开销）。有价值的短摘录可与较完整的高排名摘录共存，不以重复或无关片段凑数。MCP MUST NOT 返回原文档 SHA-256、检索方式、精排状态或分数。

#### Scenario: 与 Web 共享排序而输出不同
- **WHEN** Web 与 MCP 对同一批发布版本提交等效查询，实际的召回与精排条件相同，排名候选中含同文件重叠的证据及长片段
- **THEN** MCP 使用同一底层排名，不改变保留项的相对顺序；只返回去重后有界的原文，保证条数、合计字节数和来源坐标准确；Web 响应不受 MCP 输出裁剪影响

#### Scenario: 单次请求独立降级
- **WHEN** 同一已发布版本的等效 Web 与 MCP 查询中，只有一个请求无法使用 Embedding 或 Rerank
- **THEN** Embedding 不可用的一方回退关键词，Rerank 不可用的一方保留基础排名；Web 仍如实标明当次状态，MCP 返回当次可用证据，不要求不同检索条件给出相同排序

#### Scenario: 无效输入、无已发布索引、零命中和故障
- **WHEN** 客户端传入空白查询、指定不存在的库或非法路径，或有效查询遇到无已发布索引、范围内没有匹配、底层索引不可用
- **THEN** 分别返回参数错误、`status: "no_published_index"` 且 `matches: []`、`status: "ok"` 且 `matches: []`，或存储错误；合法但未命中文件的路径范围在已有发布版本时属于零命中

### Requirement: 按命中引用补读同一版本的周边原文

`knowledge_fetch` MUST 只接受一个必填参数 `ref`，客户端从 `knowledge_search` 命中原样传回。`ref` MUST 绑定原命中所在的库、发布版本、文件及命中位置；不得把它作为访问凭据或允许其指定本机文件。补读 MUST 不发起新搜索、不调用模型，只读请求开始时仍有效的对应发布版本。

成功结果 MUST 返回 `text`、`library`、`path`、`line_start`、`line_end` 和 `truncated`，MUST NOT 返回原文档 SHA-256。`text` MUST 是包含命中位置的单段连续原文，最多覆盖 60 个源文件行且最多 4 KiB UTF-8 字节（不含元数据与序列化开销）；在该范围内尽量保留有用的周边上下文。行号 MUST 描述实际返回文本覆盖的一基闭区间；周边原文因上限被截去时 MUST 标记 `truncated`，不可因此拆成多个不连续段。

#### Scenario: 跨索引片段补读
- **WHEN** 客户端传回有效命中的 `ref`，可读的周边上下文横跨索引片段边界
- **THEN** 返回同文件同版本的连续原文，不重复重叠字节、不跳行；两项上限均满足，坐标与截断标记准确

#### Scenario: 旧引用与故障可以区分
- **WHEN** `ref` 无效、对应版本在补读开始前已被替换或删除，或仍发布的版本无法读取必要的索引数据
- **THEN** 分别报告参数错误、引用失效或存储错误；不得从同名库或文件的新版本取相同行号，也不伪装为成功空响应

### Requirement: Agent 可发现并读取有类型的证据

两个工具 MUST 声明只读与非破坏性提示，并在 `tools/list` 公布 `inputSchema` 与 `outputSchema`。成功结果的 `structuredContent` MUST 符合声明的输出结构，`content` MUST 同时提供同一成功结果的 JSON 文本，不再另行缩短原文。

#### Scenario: 仅消费文本的客户端仍可引用与补读
- **WHEN** MCP 客户端只向 Agent 暴露本次工具调用的文本内容
- **THEN** Agent 仍能从同一份有界证据获得完整的返回摘录、出处、行号和搜索命中的 `ref`
