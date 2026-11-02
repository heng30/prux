# Prux 迁移指南：yoyo-evolve → prux（候选清单：1 项并入 `memory`，其余不做）

> **来源**：`yoyo-evolve`（Rust 写的「自我进化」终端 agent；本仓的本地只读检出是
> `yoyo-evolve -> /home/blue/wayshot-fork/yoyo-evolve`，检出点 `6fe4d33d`
> （`Day 246 (00:41): session-end state resets`，2026-11-01），`Cargo.toml` 的
> `version = "0.3.0"`，`LICENSE` 是 MIT）。
>
> **目标**：`src/extensions/` 下的内置扩展（本仓已有 `migration-subagents.md` /
> `migration-tasks.md` / `migration-kiss-v0.0.35.md` / `migration-jcode-v0.94.0.md` /
> `migration-jan.md` / `migration-openresearch.md`）。
> **性质**：先冻结判断，再逐条落地；每落地一项更新路线图并补「落地记录」。
>
> **判断口径**：沿用 `migration-jcode-v0.94.0.md` 的尺子（独立功能 / 自带用户可见价值 /
> 不靠守护进程）。这里额外加一条，因为 yoyo 有大量**叙事型产物**：
> **产出不可验证、或只能靠提示词维持的机制，一律不搬**——它们进了 prux 只会变成每轮
> 都要付 token 的 prompt 噪音。

---

## 〇、摘要

### 结论

yoyo 是一个「自己改自己」的 agent：GitHub Actions 每 3–8 小时跑一轮
A1 评估 → A2 规划 → B 实现 → B-eval 评审 → 构建/测试 → 写 journal → 推送，任何 gate 失败就
`git reset --hard`。它和 prux 的**结构差异大于功能差异**——prux 已有对应的记忆、技能、
子代理、worktree、git 集成与安全围栏，而且多数地方做得更稳。

真正值得拿的只有一条：**记忆的时间衰减合成**（§2.1）。其余的真机制要么 prux 已经更强
（§3.2 淘汰语义、§3.4 保护文件 gate），要么需要几年样本才成立（§3.3 风险预测闭环），
要么是 CI 流水线而不是扩展（§3.5）。

**候选清单到此收口：落地 1 项（并入 `memory` 扩展，不新起扩展），明确不做 8 项，无待做项。**

### 路线图

| # | 项 | 类型 | 状态 |
|---|---|---|---|
| 2.1 | 记忆的时间衰减合成（归档 → 三档压缩 → 生成式视图） | 并入 `memory` | 未落地（设计见 §2.1） |
| 3.1 | dream / IDENTITY / PERSONALITY / JOURNAL | — | 不做（叙事） |
| 3.2 | 记忆上限 + 只淘汰自动条目 | — | 不做（prux 已更强） |
| 3.3 | 风险预测-验证-权重学习闭环 | — | 不做（样本量不足，记翻案条件） |
| 3.4 | 受保护文件 gate + revert 回执 | — | 不做（`permissions` 已覆盖） |
| 3.5 | 自修改流水线（`evolve.sh` 一族） | — | 不做（CI 流水线，非扩展） |
| 3.6 | 分 phase 计费 / 盲测打分账本 | — | 不做（收益弱） |
| 3.7 | 枚举-登记-棘轮测试清单 | — | 不做为扩展（另记 4 条，见 §3.7） |
| 3.8 | `gasp` 可携带状态快照 | — | 不做（与 `rewind` / 会话树重叠） |

---

## 一、它的结构，与 prux 的重叠面

yoyo 的 `src/` 是**扁平单层**（94 个源文件 + 同名 `*_tests.rs`）。按功能归组：

| yoyo 侧 | 做什么 | prux 的对应物 |
|---|---|---|
| `memory.rs` / `commands_memory.rs` | `.yoyo/memory.json` 项目记忆：模糊子串打分召回、上限 20 条、只淘汰 auto 条目 | `memory` 扩展（图结构 + 词法打分 + 置信度/取代 + 1000 条上限） |
| `memory/*.jsonl` + `memory/active_*.md` | 全量归档 + 每日 LLM 生成的三档压缩视图 | **无**（§2.1） |
| `skills/<name>/SKILL.md` + `skill_fence.rs` | frontmatter（`name/description/tools/core/origin/keywords`），`core` / `origin: creator` 禁改，退役进 `skills_attic/` | `skills` 扩展 |
| `commands_spawn.rs` / `spawn_worktree.rs` | 子 agent + worktree | `subagent` 扩展（`isolation: worktree`） |
| `watch.rs` | 编辑后自动跑 build/test，从失败里写记忆 | `vet` 扩展（收工前跑检查命令并续跑） |
| `safety.rs` / `hard_deny.rs` / `cd_deny.rs` / `outside_read.rs` / `restricted.rs` | 目录/路径围栏 | `permissions` 扩展（`path` / `external_directory` 两层 + most-restrictive-wins） |
| `commands_risk*.rs`（12 个文件）+ `.yoyo/risk_weights.json` | 风险打分、预测 top-10 易坏文件、事后回填并学习 7 个信号权重 | 无（§3.3） |
| `prompt.rs` / `prompt_budget.rs` / `context.rs` | 系统提示拼装与预算裁剪 | `core::system_prompt` + 压缩 |
| `dreams/`、`journals/`、`IDENTITY.md`、`PERSONALITY.md` | 自我叙事：梦、日记、人格 | `recap` / `btw` 扩展（一行小结与旁问） |

数据目录里实际长这样（各一份样本）：

- `memory/learnings.jsonl`：一行一条 lesson（含 `day` / `source` / `takeaway`），全量不删；
  `memory/active_learnings.md` 是它的**压缩视图**（`## Lesson: … ` / `**Day:** N` / `**Takeaway:**`）；
- `dreams/dream_log.jsonl`：`{"type":"form","day":110,"dream":"…","spark":"…","milestone":"…","expected":"…"}`；
- `journals/JOURNAL.md`：`## Day 246 — 00:41 — 标题` + 散文。

---

## 二、值得移植

### 2.1 记忆的时间衰减合成（并入 `memory` 扩展，不新起扩展）

**问题**：prux 的 `memory` 扩展有 1000 条上限（`MAX_ENTRIES`）与「淘汰打分最低、优先已失效」
的淘汰策略（`graph.rs::prune_to_limit`）。这比 yoyo 的 FIFO 20 条好得多，但**长期使用仍然是
信息丢失**：超过上限后，旧的事实被直接删掉。用户可以接受的正确行为是「旧条目被压成主题摘要」，
而不是「消失」。

**yoyo 的做法**（`.github/workflows/synthesize.yml` 的合成 prompt +
`scripts/synthesize_state.sh`）：
1. 归档（`*.jsonl`，append-only，不删）与视图（`active_*.md`，生成物）分离；
2. 视图由**一次 LLM 调用**重建，三档时间权重：
   近 2 周全量 → 2–8 周压成 1–2 句 → 8 周以上按主题归并为 `## Wisdom: [theme]`；
3. **输入未变就短路**：`memory/.synthesize_inputs` 记着上次输入的 sha256，相同则整步跳过；
4. **产物体检**（`synth_output_ok`）：非空、首行是指定 header、且与上次的 `.bak` 逐字节不同——
   最后这条抓的是「模型什么都没改也交卷」；
5. 失败则回滚 `.bak`，没有 `.bak` 就删掉可疑产物（宁可缺视图，不可留半成品）。

**prux 侧的差别**：`memory` 的条目是**结构化图**（事实/偏好/实体/纠正，带置信度、取代关系），
不是散文 lesson。所以压缩不能照抄「重写一份 md」，要压成**同一 schema 的摘要条目**：
`kind = summary`，正文是主题归并的几句话，`supersedes` 指向被它吸收的那些原条目。
这样召回路径、注入路径、`memory` 工具都不需要改语义。

**建议形态**（尽量少写代码）：
- 新文件 `src/extensions/memory/synthesize.rs`，只在 `memory` 扩展内加一条通路，**不新起扩展**；
- 触发：手动 `/memory digest` 优先（可预览、可丢弃）；自动触发挂在已有 `turn_end` 计数之外，
  **额外加冷却**（例如距上次成功合成 ≥ 7 天且条目数 > 阈值），默认关闭——
  它会花一次额外模型请求，不该由框架替用户决定；
- 短路键：条目集合的哈希（id + updated_at 排序后），存在 `agent_dir()/extensions/memory/` 下，
  与配置同目录、不写进 `global.json`；
- 产物门槛照抄 yoyo 的第 4 条：非空、参与压缩的条目数减少、`supersedes` 至少指向一条真实存在的
  条目；不满足就丢弃并保留原图；
- **注入块必须仍然稳定**：摘要条目按「分数 → 更新时间 → id」确定性排序，与现有召回同一条路径，
  一轮之内只算一次（否则打掉 prompt cache）。

**风险与边界**：
1. 摘要会丢细节，所以**只压冷条目**：`MemoryEntry` 现有 `access_count` / `strength` /
   `updated_at` / `confidence` 四个信号足够判冷热，不新增 `last_recalled_at` 字段；
2. 摘要与原文**不能并存注入**——被吸收的原条目降权/失效，否则上下文翻倍；
3. 与 `rewind` 无交互（记忆不在会话历史里），但与 `subagent` 的 `MEMORY.md` 是**两套**：
   那是用户手写、整份注入；这里是自动抽取、按相关性挑选，合成只碰后者。

**验证**：
1. 把三档分档与主题归并抽成纯函数（输入条目集 → 输出分组），单测断言边界（2 周 / 8 周）与
   确定性排序；
2. 短路键单测：同一集合两次 → 第二次不发请求（用一个可注入的请求计数点）；
3. 产物门槛单测：空输出 / 首行不对 / 条目数没减少 → 一律不落盘；
4. 集成层面确认 `/memory digest` 在忙碌期（流式输出中）是拒绝执行的，与 `rewind` 同一处理。

**落地记录**：（未落地）

---

## 三、明确不做

### 3.1 dream / IDENTITY / PERSONALITY / JOURNAL

`scripts/dream.sh` 是写得挺认真的（7 天冷却、900 秒预算、产物只允许写 `DREAM.md` 与
`dreams/dream_log.jsonl`、post-diff 守卫越界即 revert），但**产出物本身不可验证**：
dream 的 milestone 喂给下一轮开发目标，人格与日记进系统提示。在 prux 里这等价于
「用用户的 token 换一段法则无法检查的散文」。而且 prux 已有 `recap`（后台一行小结）与
`btw`（用户开口才动的只读旁问），定位更窄、更省。

### 3.2 记忆上限与淘汰语义

yoyo 的 `MAX_AUTO_MEMORIES = 20` + 「只淘汰 auto 条目 + 全串去重 + 读取端过滤空壳条目」
是针对它自己的历史 bug（曾因「满了就永久拒收」冻结了 11 周）打的补丁。
prux 侧已有 1000 条上限 + 打分淘汰 + 置信度/取代机制，直接搬只会降级。

### 3.3 风险预测-验证-权重学习闭环

这是全文最聪明的机制：`/risk` 每次预测 top-10 最可能出问题的文件，事后用真实失败/CI 结果
回填（`risk_validations.jsonl` 的 `accuracy_pct` / `hits` / `surprises`），再反向学习 7 个信号
的权重（churn / recency / size / complexity / test_density / coupling / revert_history，
`learned_from: 401` 条样本，权重已归一）。

**不做的理由**：闭环要成立需要**几百次真实失败样本**，这是 yoyo 每天自跑 3 轮换来的。
prux 是用户驱动的工具，一年也攒不到这个量；权重学不出来，剩下的只是一个「猜哪些文件易坏」
的启发式，而 `vet` 已经用「跑真实检查命令」把这个需求解决得更直接。
（顺带一提，它也没逃过过拟合：7 个权重里 `size` 排第二高，`revert_history` 最低——
在没有版本控制事件流的数据集上本来就该这样。）

**翻案条件**：本仓有 ≥200 次真实的「测试失败 → 定位到文件」样本，且能以离线方式重放。

### 3.4 受保护文件 gate + revert 回执

yoyo 有受保护文件清单（agent 不可改 `evolve.sh` 之类）、gate 失败先存 diff 到 audit 分支再
`git reset --hard`、并自动开一个 `agent-revert` issue 当回滚回执。
prux 侧：`permissions` 扩展的 `path` 层用 pattern + `deny` 就能表达「这些文件不许写」，
而且 `ask` 是 fail-closed；diff 保全与回滚是 git 的本职。再叠一层只会多一套用户要学的规则。

### 3.5 自修改流水线

`scripts/evolve.sh`（235 KB）驱动 A1/A2/B/B-eval 五阶段，配套几十个 shell 与
`.github/workflows/*`。这是**仓库运维流水线**，不是扩展；prux 没有「无人值守自己改自己」
的需求（也不该有）。

### 3.6 分 phase 计费 / 盲测打分账本

`format/cost.rs` + `--cost-warn` + 各 phase 的 `spend.jsonl`：把一次运行按阶段记账。
prux 有 `usage`（provider 侧配额）与 `cache-usage`（本机会话缓存命中率），足以回答
「钱花在哪」。要在 prux 里做 per-phase 成本得先给 `core` 加记账点，收益不成比例。

### 3.7 枚举-登记-棘轮测试清单（不是扩展）

yoyo 攒了一批「枚举即登记、数量只许降」的测试 gate：`module_size`（单文件行数上限）、
`orphan_modules`（未被 `mod` 引用的文件）、`doc_version_claims`（文档里写的版本号必须与
`Cargo.toml` 一致）、`global_state_races`（`static mut` / 全局可变状态的登记表）、
`cargo_spawning_tests`（禁止测试里 spawn 子进程）。

它们不是扩展，但**有两条对 prux 直接有用**：
1. `doc_version_claims`：prux 的版本唯一真源是 `Cargo.toml`，而 `assets/docs`、
   `assets/changelog`、`sync.md` 里都出现过版本号——把「三者一致」写成一个测试是低成本的；
2. `orphan_modules`：prux 的 `src/extensions.rs` 是手写 `pub mod` 清单，漏登记不会编译失败，
   只会让扩展悄然消失。

其余两条（module_size / cargo_spawning_tests）与 prux 的 `test_support` 纪律重叠
（AGENTS.md 要求测试用线程本地 override、不得 spawn 外部程序），价值不大。
**本档把它们记为可选的工程项，不作为扩展候选。**

### 3.8 `gasp` 可携带状态快照

把 agent 的「会话 + 工作状态」打包成可携带文件。prux 已有会话树（`/tree`）、
`rewind`、子代理 worktree 隔离三套机制覆盖「回到某个状态」。再加一种容器格式没有新能力。

---

## 四、落地后的收尾检查（通用清单）

> 本节是**通用清单**，不是完成记录。

1. 改 `memory` 扩展后，确认注入块**逐字节稳定**（同图同查询同文本，一轮只算一次）——
   这是现有实现的硬约束，合成功能不能破坏它；
2. 合成失败必须留原图，不允许出现「摘要写了、原文删了」的中间态；
3. 新增的 `const` / `struct` / 字段 / 函数按 AGENTS.md 写中文 `///`（字段写「是什么 + 取值/`None` 含义」）；
4. 不在回调里跨 `push` / `retain` 持锁，也不在持锁时回调 `hooks()`；
5. 自动合成默认关闭；默认开启会改变用户的 token 消耗曲线；
6. 提交前 `cargo fmt` + `cargo clippy` + `cargo test`，并同步 `assets/changelog/v.<version>.md`。
