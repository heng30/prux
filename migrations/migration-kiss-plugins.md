# KISS → Prux 插件移植分析

> 生成时间：2025-09-29
> 分析范围：kiss 全部 crates（kiss-agent, kiss-ai, kiss-coding, kiss-mcp, kiss-webmcp, kiss-workflow, kiss-tui, kiss-sdk, kiss 主 CLI）
> 已排除 prux 已有功能：workflow、subagent、web_access、tasks 等
>
> **注意**：prux 当前 **完全没有 MCP 实现**（仅注释中提及），kiss 的 MCP 是最成熟的参考实现。

---

## 🔴 高优先级（prux 完全缺失，价值高）

### 1. MCP 完整集成

**来源**：`kiss-mcp/`（config.rs, manager.rs, oauth.rs, tool.rs）+ `kiss/src/mcp_cli.rs`

**prux 现状**：**完全缺失**。仅在 `main.rs` 和 `plan_mode.rs` 注释中提到 MCP，无任何实现。

kiss 的 MCP 实现是完整的生产级方案，包含以下子系统：

#### 1a. 多作用域配置发现与合并

6 个配置位置（全局优先）：

| # | 路径 | 作用域 |
|---|------|--------|
| 1 | `~/.config/mcp/mcp.json` | 共享全局 |
| 2 | `~/.agents/mcp.json` | agents 全局 |
| 3 | `~/.agents/mcp/mcp.json` | agents 嵌套全局 |
| 4 | `~/.kiss/agent/mcp.json` | KISS 全局 |
| 5 | `<cwd>/.mcp.json` | 项目（需信任） |
| 6 | `<cwd>/.kiss/mcp.json` | 项目 KISS（需信任） |

- 支持 `//` 和 `/* */` 注释
- 递归深度合并（后覆盖前）
- 写入使用原子 rename + 排他文件锁，文件权限 0o600
- `ServerEntry` 支持：command/stdio、url/HTTP、args、env、headers、auth、oauth、bearer_token、include/exclude_tools、disabled、idle_timeout、request_timeout_ms、lifecycle
- 验证拒绝：command+url 同时存在、HTTP auth on stdio、空名称、非 ASCII 名称

#### 1b. McpManager 生命周期

- **懒连接**：构造时不连接，每次操作时 `ensure_connected()`
- **传输**：stdio（TokioChildProcess）或 HTTP Streamable（StreamableHttpClientTransport）
- **空闲断开**：默认 10 分钟无操作断开，`lifecycle: persistent` 跳过
- **超时/取消**：每个 RPC 通过 `tokio::select!` 竞争 timeout（默认 60s）和 CancellationToken
- **服务器状态**：Disabled → NotConnected → Cached → Connected → Failed

#### 1c. 工具/资源/Prompt 缓存

- 缓存文件 `~/.kiss/agent/mcp-cache.json`
- 以 SHA-256 指纹（ServerEntry 序列化哈希）为 key
- 首次访问直接返回缓存，无需连接
- 指纹不匹配时自动失效
- 每次 tools/list、resources/list、prompts/list 后原子持久化

#### 1d. OAuth 2.1 for MCP

- `FileCredentialStore` 持久化到 `~/.kiss/agent/mcp-oauth.json`，绑定服务器 URL
- Authorization code + PKCE（S256）：浏览器打开 + localhost 回调监听（127.0.0.1:3118/callback）
- Client credentials flow
- 自动 token 刷新（rmcp AuthorizationManager）
- `probe_oauth_challenge()` 探测 WWW-Authenticate

#### 1e. McpTool Agent 接口

单一 AgentTool 实现，9 个 action：

| Action | 参数 | 行为 |
|--------|------|------|
| `status` | -- | 每服务器状态、工具/资源/prompt 计数 |
| `list` | 可选 server | 懒连接所有非禁用服务器，返回工具列表 |
| `search` | `query` | 模糊多词搜索名称/描述 |
| `describe` | `server`, `name` | 返回单个工具详情 |
| `call` | `server`, `name`, arguments | 调用工具，返回内容块 |
| `resources` | `server` | 列出资源 |
| `read_resource` | `server`, `uri` | 读取资源 |
| `prompts` | `server` | 列出 prompts |
| `get_prompt` | `server`, `name`, arguments | 获取 prompt |

执行模式：Parallel。内容块转换：Text 截断 100KB，Image 直传，Audio 转占位符。

#### 1f. Tool Include/Exclude 过滤

- `include_tools` 非空时只保留匹配的工具名
- `exclude_tools` 始终过滤掉匹配的工具名
- 精确字符串匹配

#### 1g. CLI（mcp_cli.rs）

命令：`list`、`get`、`add`、`update`、`remove`、`enable`、`disable`、`login`、`logout`、`test`

作用域 flag：`--user` / `--project`

**移植方案**：作为 prux 核心模块 `core/mcp/`（config.rs, manager.rs, oauth.rs, tool.rs）+ CLI `cli/mcp.rs`。这是最优先的移植项，因为 MCP 是 AI agent 生态的标准工具协议。

**复杂度**：⭐⭐⭐⭐（但可分阶段：先 config+manager，再 oauth，再 tool agent interface）

---

### 2. Voice 语音输入

**来源**：`kiss/src/voice.rs`

- 三个后端：local（whisper.cpp）、Deepgram、ElevenLabs
- ffmpeg 采集 16kHz PCM16，2 分钟限制
- 实时预览事件，Esc 取消
- `/voice` slash command 管理（tap 模式适配不支持 key-release 的终端）
- 所有后端要求 `ffmpeg` in PATH；local 需要 whisper.cpp 的 `whisper-cli` + GGML 模型
- 云后端使用用户自己的 `DEEPGRAM_API_KEY` 或 `ELEVENLABS_API_KEY`，凭证永不保存到 settings

**移植方案**：`extensions/voice.rs`，注册 slash command 和 editor hook。依赖外部二进制（ffmpeg、whisper-cli）和可选 API key。

**复杂度**：⭐⭐⭐

---

### 3. WebMCP 浏览器集成

**来源**：`kiss-webmcp/`（lib.rs, client.rs）

- 通过 Chrome DevTools Protocol WebSocket 连接浏览器
- 追踪页面 `toolsAdded`/`toolsRemoved` 事件
- 允许/拒绝 origin 策略（`allowedOrigins`/`disallowedOrigins`）
- 暴露为 agent tool（list/describe/call），输出标记为不可信
- 调用超时 60s，发送浏览器取消请求
- 限制页面输出 100,000 bytes

**移植方案**：`extensions/webmcp.rs`，与现有 web_access extension 并列。依赖 `tokio-tungstenite`（prux 已有）。

**复杂度**：⭐⭐⭐

---

### 4. Doctor 健康检查

**来源**：`kiss/src/doctor.rs`

- 并发探测所有 provider 端点（SSE、WebSocket、login URL），10s 超时
- 检查：安装路径、model catalog、auth 凭证数量、proxy 环境变量
- `--summary` 模式压缩为每 provider 一行
- 输出可直接发给网络团队做防火墙白名单
- 任何 HTTP 状态码都表示可达（401 = 正常未认证）；SKIP = 需要本地配置

**移植方案**：`cli/doctor.rs`，作为 CLI subcommand。

**复杂度**：⭐⭐

---

### 5. Jev 动态推理选择

**来源**：`kiss-coding/src/jev.rs`

- **Compaction**：外部分类 API 按 tool-interaction 决定 keep/truncate/drop（概率 0.5 阈值）
- **Dynamic reasoning**：选择 ThinkingLevel + generation lease（1-10 代自动调整推理深度），无需用户干预
- 需要 TypeSafe API key

**移植方案**：`extensions/jev.rs`，可选的 compaction/reasoning 增强。需要外部 API 依赖。

**复杂度**：⭐⭐⭐⭐

---

## 🟡 中优先级

### 6. Mutation Queue 文件锁

**来源**：`kiss-agent/src/tools/mutation_queue.rs`

- 全局 per-path async mutex，防止并发 write/edit 竞争
- Arc 引用计数自动清理死条目
- prux 现状：prux 的 tools 无此保护

**移植方案**：提取为 `utils/mutation_queue.rs`，集成到 tools/write.rs 和 tools/edit.rs。

**复杂度**：⭐

---

### 7. JSON Salvage 截断修复

**来源**：`kiss-ai/src/json_salvage.rs`

- 修复不完整 tool-call 参数流中的截断 JSON
- prux 现状：无此机制

**移植方案**：提取为 `utils/json_salvage.rs`，集成到 agent loop 的 tool call 解析路径。

**复杂度**：⭐

---

### 8. Incremental SSE Parser

**来源**：`kiss-ai/src/sse.rs`

- 处理 chunked bytes、multi-line data、CRLF、split UTF-8
- 10K events 仅 0.93ms
- prux 现状：prux 有 SSE 解析但可能不如 kiss 精细

**移植方案**：可替换 prux 现有 SSE 解析器，或作为参考优化。

**复杂度**：⭐⭐

---

### 9. Context Files 自动发现

**来源**：`kiss-coding/src/context_files.rs`

- 自动发现 AGENTS.md/CLAUDE.md/AGENTS.override.md，从全局到祖先链
- 加载 SYSTEM.md/APPEND_SYSTEM.md 做 prompt 替换/追加
- prux 现状：prux 有 AGENTS.md 加载但机制较简单

**移植方案**：增强 `core/system_prompt.rs`，增加 override/append 变体和祖先链发现。

**复杂度**：⭐⭐

---

### 10. Trust 项目信任

**来源**：`kiss-coding/src/trust.rs`

- 持久化 per-directory 信任决策到 `~/.kiss/agent/trust.json`
- 门控项目本地资源加载（settings、MCP servers、workflows、skills）
- prux 现状：prux 已有 `project_trust.rs`，kiss 实现更系统化

**移植方案**：参考 kiss 的信任范围设计，增强 prux 的 `core/project_trust.rs`。

**复杂度**：⭐⭐

---

### 11. Skills 增强

**来源**：`kiss-coding/src/skills.rs`

- agentskills.io 风格 Markdown skills + YAML frontmatter
- 通过 `$name`、`/name`、`/skill:name` 调用
- 展开为 XML 注入 system prompt
- prux 现状：prux 已有 skills 但格式可能不同

**移植方案**：参考其 YAML frontmatter 设计，增强 `core/skills.rs`。

**复杂度**：⭐⭐

---

## 🟢 低优先级

### 12. Export HTML 增强

**来源**：`kiss/src/export.rs`

- 样式化 HTML 导出（暗色主题、角色彩色卡片、可折叠 tool 结果）
- JSONL 可移植往返格式
- prux 现状：prux 已有 `export_html`，kiss 的折叠 tool 结果功能可参考

**移植方案**：参考折叠 tool 结果的设计，增强 `core/export_html/`。

**复杂度**：⭐⭐

---

### 13. Cache Usage 统计

**来源**：slash command `/cache-usage`

- 显示当前 session 缓存命中率趋势
- 跨 session 比较、按 provider 比较
- 图表可视化
- prux 现状：prux 有 `cache_warmer` 但无用户可见统计

**移植方案**：作为 extension，注册 `/cache-usage` slash command。

**复杂度**：⭐⭐

---

### 14. Autoresearch 自动研究

**来源**：`kiss-coding/src/iterative.rs` 中的 autoresearch 模式

- 建立 baseline → 逐个微调 → 保留改进 → 回滚退化
- 与 loop 共享基础设施
- prux 现状：prux 有 subagent/workflow 基础但无此特定模式

**移植方案**：作为 workflow 模板或 iterative job 的一种模式。

**复杂度**：⭐⭐⭐

---

### 15. File Search @ mention

**来源**：`kiss/src/file_search.rs`

- 缓存、ignore-aware 目录索引（最多 500K 条目，30s TTL）
- 模糊匹配 `@` 前缀 mention
- 多线程排名 + prefix-reuse 优化
- prux 现状：prux 无此功能

**移植方案**：作为 extension，提供文件搜索和 @ mention 补全。

**复杂度**：⭐⭐⭐

---

### 16. DiffRenderer 差分渲染

**来源**：`kiss-tui/src/renderer.rs`

- 维护上一帧，只重绘变化行
- synchronized output、viewport scrolling、cursor marker APC
- prux 现状：prux 使用 ratatui，架构不同，不直接移植

**移植方案**：不直接移植，但差分渲染理念可参考优化 ratatui 渲染。

**复杂度**：不适用

---

### 17. Large Paste 占位符

**来源**：`kiss-tui/src/editor.rs`

- >1000 字符的粘贴变成 `[Pasted text #N]` token，提交时展开
- prux 现状：无此功能

**移植方案**：作为 editor extension，拦截粘贴事件。

**复杂度**：⭐⭐

---

## 📋 推荐移植路线

| 阶段 | 功能 | 类型 | 难度 | 依赖 |
|------|------|------|------|------|
| Phase 1 | MCP 完整集成 | `core/mcp/` + CLI | ⭐⭐⭐⭐ | rmcp crate |
| Phase 1 | Mutation Queue | `utils/` 模块 | ⭐ | 无 |
| Phase 1 | JSON Salvage | `utils/` 模块 | ⭐ | 无 |
| Phase 1 | Doctor | CLI command | ⭐⭐ | reqwest |
| Phase 2 | Voice 语音输入 | Extension | ⭐⭐⭐ | ffmpeg, whisper-cli (可选) |
| Phase 2 | WebMCP | Extension | ⭐⭐⭐ | tokio-tungstenite |
| Phase 2 | Cache Usage 统计 | Extension | ⭐⭐ | 无 |
| Phase 3 | Jev 动态推理 | Extension | ⭐⭐⭐⭐ | TypeSafe API |
| Phase 3 | File Search @ mention | Extension | ⭐⭐⭐ | ignore crate |
| Phase 3 | Context Files 增强 | Core 增强 | ⭐⭐ | 无 |

---

## 📁 kiss Crate 架构参考

```
kiss/
├── crates/
│   ├── kiss-agent/          # Agent loop, tools (bash/edit/write/read), mutation queue, validation
│   ├── kiss-ai/             # Multi-provider LLM streaming, auth, model registry, SSE, JSON salvage
│   ├── kiss-coding/         # Sessions, compaction, subagents, workflows, skills, settings, trust
│   ├── kiss-mcp/            # MCP server lifecycle, OAuth, tool proxy
│   ├── kiss-webmcp/         # Chrome CDP WebSocket, browser MCP tools
│   ├── kiss-workflow/       # JS-like DSL interpreter for workflow orchestration
│   ├── kiss-tui/            # Diff rendering, editor, fuzzy search, markdown, theme
│   ├── kiss-sdk/            # Rust/Python/TS SDK, RPC protocol, session management
│   ├── kiss-bench/          # Benchmarking harness
│   └── kiss/                # CLI binary, slash commands, voice, export, doctor, jobs UI
```

### 关键设计模式值得参考

1. **Provider Registry**（kiss-ai）：44 个内置 provider catalog JSON + 用户 overlay，模型解析支持 exact/substring/glob
2. **Auth Cascade**（kiss-ai）：stored file → env vars → external auto-import → declared catalog keys，OAuth auto-refresh with lock dedup
3. **Session JSONL Tree**（kiss-coding）：append-only JSONL with leaf tracking, branching, fork, child sessions
4. **Workflow DSL**（kiss-workflow）：strict JS subset, arena-based AST, deterministic resume via journal
5. **Streaming Markdown Cache**（kiss-tui）：incremental re-render for streaming LLM output
