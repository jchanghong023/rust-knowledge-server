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

`knowledge_search` MUST 接受非空的查询文本 `query`，以及可选的库名 `library`、库内相对路径 `path`、布尔值 `strict`、整数 `limit` 和不透明的 `cursor`，MUST NOT 接受其他参数。`path` 按完整相对路径限定文件或按 `/` 分段限定目录子树；省略 `library` 时查所有已发布库，省略 `path` 时不限路径。`strict: true` 只返回核验通过的完整字面匹配，不调用模型，不以相关候选补足名额。`limit` 默认 5，允许 1–16。

#### Scenario: 只传查询或指定已有范围
- **WHEN** 客户端只传 `query`，或者额外指定库名、文件或目录
- **THEN** 前者不限制库和路径；后者仅在指定范围内检索已发布资料

#### Scenario: 继续搜索结果页
- **WHEN** 调用方将 `next_cursor` 原样作为 `cursor` 传回，查询文本、库、路径、`strict` 和已发布版本未变
- **THEN** 从上页消费的结果之后继续，能够翻到首页窗口之外的已核验字面匹配。后续页复用首请求的候选选择及精排，不重新调用模型。还有可遍历候选时给出 `next_cursor`，耗尽时省略。结果去重不跨页；普通检索的相关候选仍受召回窗口限制。

#### Scenario: 无效或过时的搜索游标
- **WHEN** 游标无效、查询或发布范围不再匹配，或其进程内排名上下文已被回收或因重启失效
- **THEN** 返回明确错误并要求重新开始搜索，不静默返回新版本的首页或跳过证据。

### Requirement: 搜索命中可直接引用

成功结果 MUST 返回 `status`（`ok` 或 `no_published_index`）和 `matches`。每条命中 MUST 包含 `text`、`library`、`path`、`line_start`、`line_end`、`category`（`exact` 或 `related`）、不透明的 `ref` 和布尔值 `truncated`。`text` MUST 是同一来源的连续原文，行号 MUST 是文本实际覆盖的一基闭区间；缩短原命中片段时 MUST 标记 `truncated`。原文即使只覆盖长行的一部分，行号仍指向该行。`exact` 的摘录 MUST 保留完整核验区间；若该区间超出返回预算则不返回该条。`related` 不表示已证实事实。

`match_ranges` MUST 列出实际核验的原文命中行区间（一基闭区间），不以摘录的全部行代替命中行；没有已核验的字面位置时返回空数组。`section_title` MUST 来源于原文中包含该命中或片段的 Markdown 小节标题，无法可靠确定时省略，不从路径或代码块中的伪标题推断。响应不公开内部相关性分数。

#### Scenario: 长文档中的小节标题
- **WHEN** 命中位于大文档后部，原文片段的开头可由相邻索引片段验证为真实标题，或可从该真实标题向后恢复上下文
- **THEN** 返回该处 `section_title`；不得因命中距文件开头较远而省略，也不得把围栏代码中的同形行当作标题。

#### Scenario: 长命中缩短后仍可核对
- **WHEN** 字面命中的原文片段过长，但完整核验区间可放入当前预算
- **THEN** 摘录保留完整核验区间，行号按实际返回内容标记，并设 `truncated: true`

### Requirement: 搜索输出去重及预算

每页搜索 MUST 在共享检索排序后去除同一版本同一文件内重叠的证据，保留较高排名者；逐字节相同的片段与只在大小写、空白上不同的长共同开头可合为一条。`duplicates` 中的 `library`、`path`、`line_start`、`line_end` 和 `ref` MUST 指向各出处的原文；共享开头的出处标记 `match: "shared_prefix"`，不保证其后正文相同。返回 MUST 不超过 `limit`，每条最多覆盖 60 个源文件行，每页所有 `text` 的 UTF-8 字节数合计最多 4 KiB（4096 字节，不含元数据与序列化开销）。单条能放入一页但当前页剩余预算不足时， MUST 把该条留给后续页。MCP MUST NOT 返回原文档 SHA-256、检索方式、精排状态或分数。

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

`knowledge_fetch` MUST 只接受一个必填参数 `ref`，客户端原样传回搜索命中、重复出处或上一页补读给出的引用。`ref` MUST 绑定原命中所在的库、发布版本、文件和原文区间或相邻页边界；不得把它作为访问凭据或允许其指定本机文件。补读 MUST 不发起新搜索、不调用模型，只读请求开始时仍有效的对应发布版本。

成功结果 MUST 返回 `text`、`library`、`path`、`line_start`、`line_end`、`truncated`、`previous_ref` 和 `next_ref`，MUST NOT 返回原文档 SHA-256。`text` MUST 是最多 60 个源文件行且最多 4 KiB UTF-8 字节（不含元数据与序列化开销）的单段连续原文。以搜索引用补读时，MUST 完整保留该引用指向的原文，再按预算增加上下文；共享开头的出处保留其自身的原文，不复制主结果的字节。从补读结果续读时，MUST 返回同文件中对应方向紧邻的原文字节，不重复、不跳过、不切开 UTF-8 字符或 CRLF。页前或页后没有原文时对应的引用为 `null`；有任一方向可继续时 `truncated` 为 `true`。行号 MUST 描述实际返回文本覆盖的一基闭区间。

服务进程重启不影响仍已发布版本的引用。

#### Scenario: 跨索引片段补读
- **WHEN** 客户端传回有效命中的 `ref`，可读的周边上下文横跨索引片段边界
- **THEN** 返回同文件同版本的连续原文，不重复重叠字节、不跳行；两项上限均满足，坐标与截断标记准确

#### Scenario: 旧引用与故障可以区分
- **WHEN** `ref` 无效、对应版本在补读开始前已被替换或删除，或仍发布的版本无法读取必要的索引数据
- **THEN** 分别报告参数错误、引用失效（工具错误结果的 `error` 为 `stale_ref`，客户端须重新搜索）或存储错误；不得从同名库或文件的新版本取相同行号，也不伪装为成功空响应

### Requirement: Agent 可发现并读取有类型的证据

两个工具 MUST 声明只读与非破坏性提示，并在 `tools/list` 公布 `inputSchema` 与 `outputSchema`。成功结果的 `structuredContent` MUST 符合声明的输出结构，`content` MUST 同时提供同一成功结果的 JSON 文本，不再另行缩短原文。

`knowledge_search` 的 `tools/list` 描述与输入 schema MUST 公布 `limit` 的 1–16 范围、默认值 5，以及将 `next_cursor` 传回 `cursor` 时不变的查询字段。旧连接若缓存工具列表，重新发现工具定义时 MUST 取得当前契约。

#### Scenario: 仅消费文本的客户端仍可引用与补读
- **WHEN** MCP 客户端只向 Agent 暴露本次工具调用的文本内容
- **THEN** Agent 仍能从同一份有界证据获得完整的返回摘录、出处、行号和搜索命中的 `ref`
