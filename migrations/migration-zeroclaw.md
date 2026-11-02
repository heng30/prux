# Prux 迁移评估：zeroclaw → prux 扩展

> **来源**：`zeroclaw`（`https://github.com/zeroclaw-labs/zeroclaw`，MIT OR Apache-2.0，Rust edition 2024；
> 本仓的本地只读检出是 `tmp/zeroclaw`，不在 git 索引内，检出点
> `[workspace.package] version = "0.8.7"`，`git log -1` = `df34caf`（2026-11-01）
> `feat(install): install the CLI-only build …`）。
> 规模：`crates/` 下 22 个 crate，`find crates -name '*.rs' | xargs wc -l` ≈ **117 万行**；
> `zeroclaw-tools/src` 90 个工具文件、`zeroclaw-channels/src` 51 个渠道模块，
> 另有 `apps/{zerocode,tauri,zerorelay}` 与 `crates/zeroclaw-sop-graph`。
>
> **目标**：判断 zeroclaw 的功能面里，哪些能收编成 `src/extensions/` 的 Rust 扩展
> （或直接并进现有扩展）。
>
> **性质**：**评估档，不是落地档**。本轮只做功能面扫描与成本判断，没有写任何代码；
> 真要动手时按选中项另开落地记录，并在本文档 §3 对应小节回填。
>
> **判断口径（与 `migration-oh-my-pi.md` 同一把尺子）**：一项东西算「可作为扩展」，
> 当且仅当它是**一项独立功能、自带用户可见价值**，且能只靠 `src/core/extensions.rs` 的
> `Extension` trait 面（tools / commands / keybindings / hooks / overlay / settings /
> virtual_models / cli_flags）实现。满足以下任一条的一律**明确不做**：
> 1. 它是**机制 / 策略**而不是功能，或者 prux 已有等价实现；
> 2. 它需要**渠道 / 服务端 / 守护进程 / 新 provider 协议 / 改 core**，扩展层够不到；
> 3. 它面向的是「个人助理」（IM、SaaS 待办、投票、语音通话），不是单机编码 TUI 的主线。
>
> **与既有迁移文档的关系**：本档与 `migration-smelt.md` / `migration-oh-my-pi.md` 同类
> （全量功能面扫描后的差集清单），结论多为「不做」，因此不写移植步骤，只写判据。
> pi 契约本身的差异以 `migration-v*.md` 为准；embedding 栈、多账号、市场等重依赖项
> 沿用 `migration-jcode-v0.94.0.md` / `migration-kiss-v0.0.35.md` 的既定结论。

---

## 〇、摘要

### 结论

zeroclaw 与 prux 的定位差得比 kiss / jcode / oh-my-pi 都远：它是**长驻的「个人助理 OS」**
（渠道永远在线 + 网关 + 调度器 + 硬件外设），prux 是**一次一进程的编码 TUI**。
所以 zeroclaw 的功能面里，最大的一块（51 个渠道模块、A2A 服务端、cron/routines 守护、
observability/OTel、control plane、peers/relay）**在 prux 里没有对应位置**，不是「工作量问题」。

能在 prux 扩展面里落地的候选共 **9 项**（下表），它们的共同特征是：**都是一个工具或一个写路径的加固，
跟渠道 / 守护进程无关**。本轮定案：只有 §3.3 落地，§3.1 / §3.2 / §3.4 明确不做。

| # | zeroclaw 位置 | 一句话 | 落成 prux 什么 | 估体量 |
|---|---|---|---|---|
| 1 | `zeroclaw-memory/src/{redact,threat,decay}.rs` (160/274/163) | 记忆写盘前打码 + 注入类内容扫描 + 时间衰减 | **补进现有 `memory` 扩展**（`store.rs` 写路径、`recall.rs` 打分） | ~200 行 |
| 2 | `zeroclaw-tools/src/git_forge.rs` (1347) | GitHub/Gitea 的类型化 issue / PR / review / comment | 新扩展 `forge` | 600~800 行 |
| 3 | `zeroclaw-runtime/src/security/leak_detector.rs` (1402) | 外发内容里的凭据与高熵串检测 | 新扩展 `leak-guard`（或并进 `permissions`） | 精简 ~250 行 |
| 4 | `zeroclaw-runtime/src/skills/{creator,improver}.rs` (1956/692) | 任务完成后反思 → 生成 / 改进 `SKILL.md` | 增强现有 `skills` 扩展 | 300~400 行 |
| 5 | `zeroclaw-runtime/src/security/prompt_guard.rs` (351) + `external_content.rs` (592) | 提示注入检测 + 外部内容 provenance | 新扩展或并进 `web_access` | ~200 行 |
| 6 | `zeroclaw-tools/src/{claude_code.rs,coding_cli.rs}` (446/630) | 把任务委托给外部编码 CLI | 新扩展 `delegate-agent` | ~400 行 |
| 7 | `zeroclaw-channels/src/tts.rs` (2766) | 多 provider TTS | 新扩展 `speak`（砍到 ~200 行） | ~200 行 |
| 8 | `zeroclaw-tools/src/image_gen.rs` (1117) | fal.ai 出图 | 新扩展 `image-gen`（与 codemode 重叠） | ~300 行 |
| 9 | `zeroclaw-tools/src/hardware_*.rs` (267+215+265) | SWD/JTAG 板卡信息、内存映射、内存读写 | 新扩展 `hardware`（嵌入式专用） | ~500 行 |

### 定案（本轮）

- **已落地（v1.4.0）**：§3.3 `leak-guard`、§3.5 `injection-guard`、§3.9 `hardware`（只做 RAG 那半）。
- **明确不做**：§3.1 记忆写路径加固、§3.2 `forge`、§3.4 技能自动生成、§3.6 `delegate-agent`、
  §3.7 `speak`、§3.8 `image-gen`（本轮排除，不排期）。
- 本档至此**全部定案，无待做项**；若日后要动，另开一份迁移记录。

### 一句话结论

落地的三项（§3.3 / §3.5 / §3.9）有同一个特征：**零新依赖、不改信任模型**——两个是纯检测，
一个是本地只读检索。其余六项按上表定案执行，不再评估。

---

## 一、体量对照（先说清楚对象不同）

| | prux | zeroclaw |
|---|---|---|
| 形态 | 单 crate TUI + 可选 GUI，一次一进程 | workspace 22 crate，长驻 daemon + 网关 + 51 渠道 |
| 扩展/插件机制 | **编译期静态注册**（`linkme` 工厂切片 + `priority` 降序） | 运行期：`zeroclaw-plugins`（WASM component，WIT） |
| 扩展数量 | `src/extensions/*.rs` 41 个模块（含 `util.rs` 与子模块根） | `zeroclaw-tools/src` 90 个工具文件 |
| 用户面 | 编码会话、工具、会话历史、prompt cache | IM/SaaS 消息、跨设备、硬件外设、看板 |

**因此本档「不做」的比例比 kiss / jcode 更高**：zeroclaw 的一半功能依赖「有一个永远在线的
消息入口」这个前提，而 prux 没有这个前提，也不该有 —— 那是另一个产品。
另：`zeroclaw-plugins` 的 WASM 插件机制与 prux 的编译期注册**机制不兼容**，抄不了，
只能抄单个功能并用 Rust 重写成扩展（同 `migration-smelt.md` 对 Lua 插件的结论）。

---

## 二、已有能力对照（prux 已经覆盖，无需移植）

扫到 zeroclaw 有、prux 也有的，列出来是为了避免重复造：

| zeroclaw | prux 对应 |
|---|---|
| `agent/loop_detector.rs` (1056) | `loop_detect` 扩展 |
| `agent/history_{pruner,trim}.rs`、`zeroclaw-memory/src/consolidation.rs` | `core/compaction` + `shake` 扩展 |
| `agent/pricing_catalog.rs`、`cost/mod.rs` | `usage` / `cache_usage` 扩展 |
| `agent/memory_inject.rs` / `memory_recall.rs` | `memory` 扩展（BM25 召回 + 注入块） |
| `zeroclaw-memory/src/rerank.rs`、`knowledge_graph.rs` | `memory/graph.rs` + `recall.rs`（BM25 + `memory_score` + 取代 / link） |
| `tools/content_search.rs` (1706) | `grep` 工具（`rg --json` 流式） |
| `tools/screenshot.rs` (347) | `computer` 扩展（截图） |
| `tools/text_browser.rs` (1010) | `web_access`（fetch 侧） |
| `tools/sessions*.rs` (2376) | `session_search` + `rewind` |
| `runtime/src/cron/*`、`routines/*` | `utils/cron` + `subagent` 的 schedule |
| `runtime/src/doctor/*`、`health/*` | `doctor` 扩展 |
| `runtime/src/skills/{cache,service,skill_tool}.rs` | `skills` 扩展（`/skills` 面板） |
| `runtime/src/subagent/*` | `subagent` 扩展 |
| `runtime/src/mcp*`、`zeroclaw-tools/src/mcp_*.rs` | `mcp` 扩展（含 resources / prompts） |
| `runtime/src/security/{policy,sandbox*,landlock,seatbelt,bubblewrap}` | `permissions` 扩展 |
| `runtime/src/agent/safety_net.rs` | `hang_detect` 扩展 |
| `runtime/src/rag/*`（数据手册分块） | `markit` + `memory`（无向量栈，做成词法检索） |

---

## 三、候选：值得移植（按推荐顺序）

### 3.1 记忆写路径加固（`redact` / `threat` / `decay`）

- **是什么**
  - `zeroclaw-memory/src/redact.rs` (160)：写盘前按类别把匹配内容替换成 `[REDACTED:<category>]`
    （token / 私钥 / 邮箱 / 路径等类别，类别表可配）；
  - `threat.rs` (274)：扫描「把凭据变量写进 shell 命令」「带凭据的 URL」「读已知凭据文件」
    「改写 agent 指令文件」这几类内容，`On` / `Strict` 两档模式；
  - `decay.rs` (163)：按半衰期（默认 7 天）对非 Core 记忆降权。
- **为什么 prux 缺**：`src/extensions/memory/store.rs` 的写路径**原样落盘**，
  抽取出 `sk-ant-…` / `ghp_…` / 私钥片段时会直接写进 `agent_dir()/extensions/memory/global.json`，
  且这份文件会跟着会话被反复读回上下文。prux 目前全仓没有 redact / 凭据扫描逻辑
  （`grep -rn redact src/core src/extensions/memory` 只命中 Anthropic 的 `redacted_thinking`）。
- **落成什么**：不是新扩展，**补进现有 `memory` 扩展**：
  - `store.rs` 写入前过 `redact`（类别表给默认值，不新增设置项，需要时加一个 bool 开关）；
  - `extract.rs` 抽出条目后过 `threat` 的 `On` 档，命中就丢弃该条并在 debug 日志留痕；
  - `recall.rs` 的 `Ranked::score` 乘一个衰减因子（`decay` 的公式，与 `memory_score` 相乘）。
- **风险 / 改动点**：三处都落在已有文件的既有函数里，**不新增状态**；衰减因子会让既有召回
  测试的期望分变化，`recall.rs` 的 `#[cfg(test)]` 里那几条断言要跟着改。
- **建议**：**做**。成本最低、收益是安全边界（防止凭据随记忆外泄 / 反复回灌）。

- **定案（本轮）**：**不做**。

### 3.2 `forge` 扩展（GitHub / Gitea 类型化操作）

- **是什么**：`zeroclaw-tools/src/git_forge.rs` (1347)。一个工具，`action` 取
  `describe` / `raw` / 类型化调用；资源覆盖 milestone / label / issue / pull / review /
  reviewer / comment，且**校验超出 2xx 的响应语义**（不是「状态码 200 就算成功」）。
- **为什么 prux 缺**：prux 没有任何 forge 工具（`grep -rn api.github.com src/` 只命中
  `update_check` / `copilot` / `tools_manager`），模型只能靠 `bash` 手搓 `gh`。
  对一个编码 agent，这是最常用的一类外部写操作。
- **落成什么**：新扩展 `forge`，一个工具 + `/forge` 面板（token / 端点 / 默认仓库）。
  - **关键偏离**：zeroclaw 走它自己的「git channel」抽象（`zeroclaw-channels/src/git/`，
    3182 行，含 router / providers / polling）。prux 没有渠道概念，直接 `reqwest` 打 REST，
    PAT 从环境变量（`GITHUB_TOKEN` / `GITEA_TOKEN`）或 settings 读；
  - `describe` 自描述那套保留 —— 它让模型不用记 schema，是 zeroclaw 这工具最值的设计。
- **风险**：写操作（建 PR、发 review、改 issue）属信任边界，必须接 `permissions` 的审批，
  不能照抄 zeroclaw 的 `SecurityPolicy` 判定；`raw` 兜底要限定方法与路径前缀。
- **定案（本轮）**：**不做**。

### 3.3 `leak-guard` 扩展（凭据外发检测）

- **是什么**：`zeroclaw-runtime/src/security/leak_detector.rs` (1402)：正则 + 高熵 token 检测，
  对 `sk-` / `ghp_` / `AKIA` / 私钥块 / 长随机串在**外发前**打码或拒绝，返回
  `Redaction` 列表便于审计。
- **为什么 prux 缺**：`permissions` 只做「这个工具能不能跑」的门控，不检查**内容**。
  一次 `bash` 里 `curl -H "Authorization: Bearer $TOKEN"` 或把 `.env` 贴进 prompt，没人拦。
- **落成什么**：新扩展 `leak-guard`，两个 hook 点：请求前（拼好的消息）与 `tool_call` 参数。
  命中即按模式 warn（UI notify）或拦（返回给模型一条错误说明为什么被拦）。
  patterns 是精华，1400 行里大部分是类别枚举与测试，实际搬的规则表约 250 行。
- **风险**：误报会打断正常排障（比如用户就是要传一个测试用假 token）。默认**只 warn 不拦**，
  拦截需显式开设置项。这与 zeroclaw 默认 `GuardAction::Warn` 一致。
- **落地记录**（v1.4.0）：`src/extensions/leak_guard.rs`，单文件扩展，**零新依赖**
  （`regex` 已是依赖）。
  - 规则表是**一条合并正则**，命名分组即类别（`leftmost-first` 保证 `sk-ant-` 先于 `sk-`），
    覆盖 `private-key` / `anthropic-key` / `openai-key` / `github-token` / `gitlab-token` /
    `aws-access-key` / `slack-token` / `google-api-key` / `stripe-key` / `jwt`；高熵串单独扫
    （≥32 字符、大小写与数字齐全、Shannon 熵 ≥4 bits/char，纯 hex 跳过）。
  - 两个钩子：`transform_context`（每条消息的 `Text` 块；`block` 档替换成
    `[REDACTED:<category>]`，只改本次请求的副本）+ `before_tool_call_ext`（扫入参 JSON，
    `block` 档拒绝执行并把类别作为原因回给模型）。
  - **不碰 thinking 块**：思考块带供应商签名，改文本会让签名失效（请求直接报错），只扫不报。
  - 档位 `warn`（默认）/ `block` / `off`，落盘 `agent_dir()/extensions/leak-guard.json`，
    `/leak-guard [warn|block|off]` 设档、裸 `/leak-guard` 打开扩展设置面板；默认不启用
    （`Dev` / `Creator`）。
  - 去重：同一 `(类别, 片段)` 只告警一次（上限 64 条），避免每轮重扫历史消息时刷屏。
  - 与 `permissions` **保持解耦**：本扩展只看内容，不做「这个工具能不能跑」的门控。

### 3.4 技能自动生成 / 改进（并进 `skills` 扩展）

- **是什么**：`zeroclaw-runtime/src/skills/creator.rs` (1956) 的 reflection 路径 —— 用一次
  一次性模型请求把「刚完成的任务」转成一份规范 `SKILL.md`；`improver.rs` (692) 带冷却的
  自我改进；`audit.rs` (947) 对技能目录做静态审计。
- **为什么 prux 缺**：`src/extensions/skills.rs` 只有 **273 行**，且 `tools()` 返回空
  —— 只做「列出 / 启停 / 装内嵌技能」，模型无法自己产出技能。而 prux 的
  `memory/extract.rs` 已经把「一次性请求 + 结构化抽取」这套机器跑通了，可以复用。
- **落成什么**：不新起扩展，给 `skills` 加两个口子：
  - 工具：`skill_create`（模型显式要求「把刚才的过程记成技能」）；
  - hook：turn_end 反思（默认关，`default_enabled` 保持现在的语义），照 `memory` 的
    「每 N 轮 / 每轮结算」节流写法；
  - 落盘到 `agent_dir()/skills/<name>/SKILL.md`，写前走 §3.1 的 redact 与 `audit` 的
    路径包含检查（`audit.rs` 里 `Component` 路径校验那套必须保留）。
- **偏离**：去掉 `creator.rs` 的 embedding 去重（prux 无向量栈），改用名字 + 标题词法近似
  （`utils/text::terms` 已在 `memory/recall.rs` 用过）。
- **定案（本轮）**：**不做**。

### 3.5 提示注入防护（`prompt_guard` + `external_content`）

- **是什么**：`prompt_guard.rs` (351) 用模式表给消息打 `Safe` / `Suspicious(patterns, score)` /
  `Blocked`；`external_content.rs` (592) 给外部来源内容加 provenance 标记后再进上下文。
- **为什么 prux 缺**：`web_access`（fetch / search）、`markit`（读 PDF/docx）、`ssh_remote`
  （远端输出）都在把**不可信文本**灌进上下文，没有任何标记或检测。
- **落成什么**：一个纯检测扩展（无工具）：在 `tool_result` 钩子上对来自上述工具的结果做标记
  + 打分，命中就在 UI 上提示（`UiNotifyLevel`），并保留现有内容不变（不改写，避免破坏
  diff / 代码块的精确性）。
- **注意**：这一项的价值取决于阈值调校，误报会变成噪音。默认只做「标记来源」不做「拦截」。
- **落地记录**（v1.4.0）：`src/extensions/injection_guard.rs`，单文件纯检测扩展，**零新依赖**。
  - 挂在 `AfterToolCall` 上（`after_tool_call_ext`）：**所有**工具结果都扫，不只外部来源工具——
    `bash` 里的 `curl` 输出同样是不可信文本，按工具名开白名单只会漏。
  - 规则表是**一条合并正则**（命名分组 + 权重）：`ignore/disregard previous instructions`(3)、
    `reveal your system prompt`(3)、角色劫持(3)、聊天模板标记 `<|im_start|>` / `[INST]` /
    `<<SYS>>`(3)、HTML 注释里的指令(3)、行首 `System:` 之类角色标记(2)、`new instructions:`(2)、
    `do not tell the user`(2)、外传数据(2)。**总分 ≥3** 才提示。
  - 同一规则重复出现**只计一次权重**：否则一篇正常技术文档里三次 `System:` 就能把分数堆过阈值。
  - **不改写结果**：只发 `UiNotifyLevel::Warning` 系统消息（不落库、不进模型上下文），原文照常
    交给模型——打码会破坏 diff / 代码块，且模型需要原文自己判断。
  - 去重：同一工具 + 同一段结果（前 200 字符）只提示一次，上限 32 条。
  - `external_content.rs` 的 provenance 标记**没有搬**：它要求改写工具结果，与上面「不改写」
    的取舍冲突；真正需要的是「用户知道被喂了什么」，UI 提示已经做到。

### 3.6 `delegate-agent` 扩展（外部编码 CLI）

- **是什么**：`claude_code.rs` (446) + `coding_cli.rs` (630) 把任务交给 `claude` / `codex` /
  `gemini` / `opencode` 子进程执行，统一超时、环境、输出截断。
- **为什么 prux 缺**：`subagent` 用 prux 自己的模型跑子代理；用户手上若有别的 agent 订阅，
  现在只能 `bash` 手拼命令。
- **必须遵守的硬约定**：子进程输出**绝不继承终端**（AGENTS「外部命令的输出必须有去处」），
  走管道 + 截断；长命的应用用 `utils::process::spawn_detached`。
- **判断**：**可选**。收益取决于用户是否真有外部 CLI 订阅；不做也不缺手。
- **定案（本轮）**：**不做**。

### 3.7 `speak` 扩展（TTS 朗读回复）

- **是什么**：`zeroclaw-channels/src/tts.rs` (2766)：OpenAI / ElevenLabs / Google 三家 +
  ffmpeg 转码 + 渠道相关的格式选择。
- **为什么 prux 缺**：全仓零音频（`grep -rn tts src/` = 0）。
- **落成什么**：砍到 ~200 行 —— 只留 OpenAI 与 ElevenLabs 两家（直接出 mp3/opus，不转码），
  `notify` 那套「整轮 settle 后触发」的时机直接复用，播放交给系统命令
  （`afplay` / `paplay` / `ffplay`，走 `spawn_detached`）。
- **判断**：**可选**。跟编码主线关系弱，属「锦上添花」；ponytail 视角下这是典型的
  「不做也完整」的功能。
- **定案（本轮）**：**不做**。

### 3.8 `image-gen` 扩展

- **是什么**：`image_gen.rs` (1117)，fal.ai 出图，带 SSRF 防护（`domain_guard` + 解析地址校验）
  与 20MB 上限。
- **为什么优先级最低**：prux **已有** image 型虚拟模型，且 `codemode` 的
  `models.generateImages` 已经能让模型出图（`with_image_impl` 有 7 处调用）。
  再做独立工具只是省掉一层 JS。
- **判断**：**暂不做**，除非实际用起来发现 codemode 绕。
- **定案（本轮）**：**不做**。

### 3.9 `hardware` 扩展（嵌入式）

- **是什么**：`hardware_board_info.rs` / `hardware_memory_map.rs` / `hardware_memory_read.rs`
  (267/215/265) + `crates/zeroclaw-hardware`（GPIO/I2C/SPI/USB）+ `runtime/src/rag`
  （数据手册分块检索）+ 引脚别名映射。
- **判断**：**看用户群**。prux 是编码 agent，嵌入式开发者确实会需要「读芯片手册 + 查引脚 +
  读内存」，且别处没有替代；但对非嵌入式用户是纯负重。真要做得**先只做 RAG 那半**
  （手册分块 + 引脚别名，零硬件依赖），probe 那半留后。
- **落地记录**（v1.4.0）：`src/extensions/hardware.rs`，单文件扩展，**零新依赖**（只做 RAG 那半）。
  - 资料目录 `agent_dir()/extensions/hardware/datasheets/`（可递归分芯片子目录），收
    `.md/.markdown/.txt/.csv/.tsv/.json/.rst/.adoc`，单文件上限 4 MB，跳过隐藏目录与二进制。
  - 分块：按空行或 1200 字符上限切；块记 `file:line`，返回时给 `file:line` + 最多 800 字符正文。
  - 检索：`utils::text::terms` 切词（与 `memory` / `session_search` 同一套，中日韩按二字组合），
    词频开方累加，**路径命中额外 +2/词元**——搜 `usart` 时 `stm32f4/…` 会排在无关文件前面。
    排序确定为「分数降序 → 路径升序 → 行号升序」。
  - 工具 `hardware_search`（`query` / `path` 前缀过滤 / `limit` 1..=20，默认 8）是**延迟工具**
    （`ToolExposure::Deferred`），声明 `prerequisite_tools = tool_search`；`/hardware` 打印资料
    目录与文件/块计数；扩展声明 `Dev` 单档（嵌入式是细分场景，不给 `Creator` 放行）。
  - **引脚别名不另起机制**：`pins.csv` 的一行（`PA9,USART1_TX,TIM1_CH2`）就是一块，搜引脚名或
    复用功能名都能命中。zeroclaw 的独立别名表在这里是多余的。
  - **probe 半（板卡信息 / 内存映射 / 内存读写）没做**：需要 `probe-rs` 这类依赖 + 真实硬件
    才能验证，而本仓的原则是不做无法验证的硬件通道。要做的话它是独立立项，不是「顺手移植」。
  - 每次调用重扫目录、不建索引缓存（手册目录通常几十个文件，扫盘毫秒级；已留 `shortcut:` 注释）。

---

## 四、明确不做（附理由）

| zeroclaw 功能 | 位置 | 不做的理由 |
|---|---|---|
| SOP 图引擎 | `crates/zeroclaw-sop-graph` + `runtime/src/sop/`（23 文件，含 step contract / routing / trigger registry / approval / procedural memory） | 与 `skills` / `plan_mode` / `tasks` / `goal` 大面积重叠，且是「一个有状态的流程运行时」；移植成本远超收益。想借的话只借「procedural memory」的概念，不借代码 |
| 51 个渠道模块 | `zeroclaw-channels/src/*`（discord / slack / matrix / telegram / whatsapp / signal / imessage / nostr / twitch / irc / wechat …） | 需要「永远在线的消息入口」。prux 是进程内 TUI，`notify` 已覆盖「跑完通知我」这类需求 |
| `poll` / `escalate_to_human` / `reaction` / `send_via` / `channel_room` / `ask_user` 的渠道分发 | `zeroclaw-tools/src/*` | 全是渠道概念的下游产物；prux 的 `ask_user_question` 已是 TUI 内的等价物 |
| A2A（agent-to-agent） | `zeroclaw-tools/src/a2a_client.rs` (5942) + `zeroclaw-api/src/a2a_wire.rs` | 需要服务端 + agent card 发现 + 对端凭证管理；单机 CLI 无对端。`subagent` 已解决「自己管一批 agent」 |
| routines / heartbeat / cron daemon | `runtime/src/{routines,heartbeat,cron,daemon}/` | prux 已有 `utils/cron` + `subagent` 的 schedule（含一次性 / 间隔 / cron 三形态、持久化） |
| gateway / dashboard / RPC / control plane / peers / relay | `zeroclaw-gateway`、`runtime/src/{rpc,control_plane,peers,relay}` | 是「多客户端连一个 runtime」的架构，正好与 prux「一次一进程」相反 |
| observability（OTel / Prometheus / audit 日志） | `runtime/src/observability/*`、`zeroclaw-log` | 运维面；prux 的 `usage` / `cache_usage` / crash_log 已覆盖开发者关心的部分 |
| SaaS 集成（jira / notion / google_workspace / microsoft365 / linkedin / composio / email IMAP / discord_search） | `zeroclaw-tools/src/{jira,notion,google_workspace,microsoft365,linkedin,composio,email_*,discord_search}.rs` | 面向个人助理与办公自动化，不是编码主线；每一个都是独立的大工程（OAuth + 增量同步），要做应单独立项而不是「顺手移植」 |
| `zeroclaw-plugins`（WASM component） | `crates/zeroclaw-plugins` | 机制不兼容：prux 扩展是编译期静态注册，没有运行期插件宿主，也不打算有 |
| `tool_receipts`（HMAC 回执防幻觉） | `runtime/src/agent/tool_receipts.rs` (364) | 思路可取（工具结果带可校验回执），但它要改**工具执行路径 + 消息序列化**，属 core 改动；且对「模型声称做过某事」的造假，prux 的 `vet`（green loop）从结果侧校验更实际 |
| `estop`（急停 / 分级 kill switch） | `runtime/src/security/estop.rs` (419) | 依赖长驻 daemon 的全局 kill 语义；prux 有 `hang_detect` 与 Ctrl-C，`permissions` 可禁工具 |
| `verifiable_intent`（VI 规范实现） | `runtime/src/verifiable_intent/` | 企业授权链场景（mandate 链、凭证验证），与编码 TUI 无关 |
| `tool_call_parser`（无原生 tool-call 的模型回退解析） | `crates/zeroclaw-tool-call-parser` | prux 的 provider 层已按原生 tool-use 协议实现（Anthropic / OpenAI / Responses 三套），不需要从文本里抠 tool call；真要支持「不会 tool use 的模型」是 core 的事 |
| `calculator` (940) / `weather` / `backup` / `data_management` / `inventory` / `project_intel` / `report_template` / `llm_task` | `zeroclaw-tools/src/*` | `calculator`+`weather`：`bash` / `web_access` 已够；`llm_task`：`codemode` 的 classifier 与 `subagent` 已覆盖「一次性结构化子调用」；其余是项目管理/报表场景，与编码无关 |
| `sessions_*` 工具 (2376) | `zeroclaw-tools/src/sessions.rs` | `session_search` + `rewind` + `/sessions` 已覆盖 |

---

## 五、路线图

| 阶段 | 内容 | 状态 |
|---|---|---|
| 1 | §3.1 记忆写路径加固（redact / threat / decay 并入 `memory`） | 🚫 不做（本轮明确排除） |
| 2 | §3.2 `forge` 扩展 | 🚫 不做（同上） |
| 3 | §3.3 `leak-guard` 扩展 | ✅ 已落地（v1.4.0） |
| 4 | §3.4 技能自动生成（并入 `skills`） | 🚫 不做（同上） |
| 5 | §3.5 提示注入防护 → `injection-guard` | ✅ 已落地（v1.4.0） |
| 6 | §3.6 `delegate-agent` 外部编码 CLI | 🚫 不做（同上） |
| 7 | §3.7 `speak` TTS | 🚫 不做（同上） |
| 8 | §3.8 `image-gen` | 🚫 不做（同上） |
| 9 | §3.9 `hardware`（只做 RAG 半） | ✅ 已落地（v1.4.0）；probe 半未做 |

本档至此**全部定案**：9 项候选里 3 项落地、6 项明确不做、零待做项。若日后要动其中任何一项
（包括 §3.9 的 probe 半），另开一份迁移记录，不在本档上打补丁。

---

## 六、口径声明与未核项

- **体量是读代码估的**，不是实测。zeroclaw 的工具都实现自己的 `Tool` trait
  （`zeroclaw_api::tool::{Tool, ToolOutput, ToolResult}` + `Arc<SecurityPolicy>`），
  移植到 prux 要整体改写成 `ExtensionTool` / `ToolExecCtx` 的形状，这部分重写成本没算进上表。
- **信任边界必须重新接线**：`git_forge` 与 `leak_detector` 都强依赖 zeroclaw 的
  `SecurityPolicy::enforce_tool_operation`（`Act` / `Read` 分级）与 `domain_guard`（SSRF）。
  照抄会把 zeroclaw 的默认策略一起搬进来 —— prux 侧必须重新接到 `permissions` 扩展上，
  并对 `raw` / 任意 URL 这类兜底入口显式收紧。
- **没有编译验证任何一项**：本档只读代码与 `head`/`grep`，未拉依赖、未跑 `cargo check`。
- **未逐一核对 zeroclaw 的 `Cargo.lock` 依赖**：例如 `image_gen` 需要 fal key、
  `hardware` 需要 probe-rs，实际引入的依赖成本未评估。
