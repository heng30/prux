# vet（收工前的项目检查闭环）

`vet` 扩展把「改完了吗？」变成「检查真的过了吗？」：agent 打算结束一个回合时，自动跑你配置的
检查命令（测试 / lint / 类型检查），有失败就把输出喂回去并让它继续修，直到检查全绿、用满重试
额度，或者发现它在原地打转。

它不提供任何模型可见的工具——只在回合结算前动手，所以对模型的工具列表和系统提示没有任何影响。

**默认不启用**，且仅在 `Dev` 模式（及 `All`）下可用——`Minimal` / `Creator` 模式里它不会出现。
在 `/extension` 面板里开启，或写 `settings.json`：

```json
{ "enabledExtensions": ["vet"] }
```

## 配置

两处配置，项目级覆盖全局级：

| 位置 | 作用 |
|---|---|
| `agent_dir()/extensions/vet.json` | 全局配置（所有项目共享） |
| `<项目>/.prux/extensions/vet.json` | 项目配置，**仅当项目受信任时读取与写入** |

合并规则是「键级覆盖 + 数组整体替换」：项目里写了哪个键就覆盖全局同名键，`checks` 整个换掉
（不会一半来自全局、一半来自项目）。

```json
{
  "autoRun": true,
  "maxIterations": 2,
  "timeoutSecs": 300,
  "announce": "fail",
  "requireEdits": true,
  "checks": [
    { "name": "test", "command": "cargo test" },
    { "name": "clippy", "command": "cargo clippy --all-targets" },
    { "name": "e2e", "command": "npm run e2e", "optional": true }
  ]
}
```

| 键 | 缺省 | 含义 |
|---|---|---|
| `autoRun` | `true` | 结算时是否自动跑检查。关掉后只剩 `/vet run` 手动触发 |
| `maxIterations` | `2` | 一个回合最多自动续跑几次。`0` = 只检查、只报告，不续跑 |
| `timeoutSecs` | `300` | 单条检查的超时。超时的检查按失败处理（连同子进程一起杀掉） |
| `announce` | `"fail"` | 聊天区通知口径：`off`（不通知）/ `fail`（只报失败与开始）/ `always`（通过也报） |
| `requireEdits` | `true` | 只在本回合调用过 `edit` / `write` / `bash` / `powershell` 时才跑（纯问答轮不跑） |
| `checks` | `[]` | 检查列表。为空时整个扩展什么也不做 |

每条检查：

| 键 | 必填 | 含义 |
|---|---|---|
| `name` | 是 | 展示名，也是 `/vet remove` 的操作键。同名项**后者覆盖前者** |
| `command` | 是 | 交给 shell 执行的命令（Linux / macOS `sh -c`，Windows `cmd /C`）。空串整项丢弃 |
| `optional` | 否 | `true` 时失败只报告、不阻止收工（缺省 `false`） |

命令在你的项目根目录下执行，输出**尾部**最多保留 4000 字符、汇总进模型上下文的预算为
12000 字符——失败信息通常就在尾部，前面的大段编译日志会被丢掉。

## 命令

| 命令 | 作用 |
|---|---|
| `/vet` | 打开（或关闭）本扩展的设置面板 |
| `/vet settings` | 同上（面板已打开时关闭） |
| `/vet run` | 立刻在后台跑一遍全部检查，结果回聊天区（不触发续跑） |
| `/vet status` | 生效配置、配置来源、检查列表、本回合进度、最近一次结果 |
| `/vet detect` | 按项目里的标记文件给出候选命令（不写入） |
| `/vet detect --write` | 把候选检查直接写进全局配置 |
| `/vet add [--project] [--optional] <name> <command>` | 新增或覆盖一条检查（缺省写全局） |
| `/vet remove [--project] <name>` | 删除一条检查 |
| `/vet reset` | 清空本回合的续跑计数与失败指纹 |

`/vet detect` 看这些标记：`Cargo.toml`（`cargo test` / `cargo clippy`）、`package.json` 的
`test` / `lint` / `typecheck` 脚本（按 lockfile 选 `npm` / `pnpm` / `yarn` / `bun`）、`justfile`
或 `Makefile` 的 `check` 配方、`pyproject.toml` 或 `pytest.ini`（`pytest`，pyproject 里提到
`ruff` 时再加 `ruff check .`）、`go.mod`、`deno.json`。

设置面板提供五个开关：`autoRun`、`maxIterations`、`timeout`、`announce`、`requireEdits`。
面板只能表达离散选项，检查列表本身走配置文件或 `/vet add`。

## 一个回合里发生什么

1. 主 agent 准备结算（`agent_before_settle`）。子代理结算不跑检查。
2. 条件不满足就放行：`autoRun` 关、没有检查、本回合没改过文件、用户刚中断（Esc / Ctrl+C）、
   或者还有排队的用户输入（让位给用户）。
3. 跑完全部检查，聊天区给一行结论。
4. **全部通过**（或只有 optional 失败）：不续跑，本回合正常结束。
5. **有阻塞失败**：把失败检查的名字、命令、退出码、输出尾部拼成一条 user 消息注入上下文
   继续跑一轮。消息里会明确告诉模型「不要再自己跑一遍检查」，避免它重复执行同一条命令。

停止续跑的三种情况：

| 情况 | 行为 |
|---|---|
| 已续跑 `maxIterations` 次 | 停止，提示「仍然失败」，交回用户 |
| 失败指纹与上一次相同（名字 + 输出） | 停止——同一条错误再喂一遍不会变成通过 |
| 用户提交新输入 | 计数与指纹复位，从零开始 |

用户再次提交输入（包括回合进行中的 steer）会把计数清零，所以「同一个回合」的语义是
「用户上一次发话之后」。

## 安全与代价

- 项目配置里的命令**只在项目受信任时**才读取：一个陌生仓库提交一份 `vet.json` 不会让
  `prux` 执行任意命令。
- 检查命令通过 shell 执行，等同于你自己在项目目录里敲它。只配置你信任的命令。
- 跑检查期间回合是阻塞的（界面显示工作中，聊天区会先提示「running N checks」）。测试慢的
  项目建议把 `timeoutSecs` 调小、把慢检查标成 `optional`，或者干脆只留 lint 这类秒级检查。
- 同一时刻只允许一组检查在跑：自动路径正在进行时 `/vet run` 会直接告诉你「已在运行」。
