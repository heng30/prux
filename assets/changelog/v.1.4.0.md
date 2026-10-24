## English

### Added

- **The `ponytail` extension writes the smallest change that works**: enabling it appends a
  "lazy senior developer" ruleset to the system prompt before every request — nothing is written
  into the session, so turning it off leaves no residue in the context. The ruleset is one
  ladder: does this need to exist, is it already in this repo, does the standard library or the
  platform do it, is there an installed dependency, can it be one line, and only then the minimum
  code that works. What it never cuts: validation at trust boundaries, error handling that
  prevents data loss, security, accessibility, and anything the user asked for. New logic with a
  branch, a loop, a parser, money or security leaves one small test behind, and a deliberate
  shortcut is marked with a `shortcut: <ceiling>, <when to upgrade>` comment. Each reply ends
  with one or two lines naming what was skipped or not checked and any risk the user should know.

  The level shapes the strength: `/ponytail off` turns it off, `lite` builds what was asked and
  names the smaller option in one line, `full` (the default) applies the ladder, and `ultra`
  also pushes back on parts of the request that the need does not justify. A bare `/ponytail`
  turns it on at the default level, or reports the current level when it is already on;
  `/ponytail status` reports the session level together with the default, and
  `/ponytail default <level>` writes `defaultMode` in `agent_dir()/extensions/ponytail.json`.
  The session level lives in a session entry (`ponytail-mode`), so resuming or switching a
  session replays it. Typing "stop ponytail" or "normal mode" as the whole message turns it off —
  the match is the entire message, so an ordinary request such as "add a normal mode toggle"
  does not.

- **Five companion commands ship with the extension**: `/ponytail-review` (quality review of a
  change: bugs, security, load, missing tests, speed, what to cut — each finding says what the
  code does, what goes wrong, how to fix it and what happens if we skip it), `/ponytail-audit`
  (the same review across the whole repo, ranked), `/ponytail-debt` (harvest `shortcut:`
  comments into a debt ledger), `/ponytail-gain` (the published benchmark scoreboard) and
  `/ponytail-help` (level and command reference). Their text is embedded in the binary and
  delivered as a `<skill>` block when invoked, so nothing is written to disk and no skill
  description leaks into the system prompt while the extension is off.

  Off by default and available in `Dev` and `Creator` modes: it rewrites how the model writes
  in every turn, so it has to be switched on explicitly.

- **`remote_default` answers which machine the remote tools hit**: the ssh-remote family gained a
  seventh (deferred) tool reporting the target used when `host` is omitted — its `user@host:port`,
  remembered cwd, note, jump host and identity, the remembered target list and the active
  forwards. It only reads the local config, so it never opens a connection and never waits on the
  network; with no default set it says so instead of guessing a host. The model can therefore stop
  asking which server to use, and users need not run `/ssh status` for it.

## 中文

### 新增

- **新增 `ponytail` 扩展：只写「最小的完整改动」**：启用后每次请求前把一套「懒资深开发者」
  规则追加到系统提示末尾——**不写进会话**，所以关掉之后上下文里不留残渣。规则是一条梯子：
  这东西需要存在吗、仓库里已经有了吗、标准库或平台原生有没有、已装依赖里有没有、能不能一行写清，
  最后才是「能跑的最小实现」。它绝不省的东西：信任边界上的校验、防丢数据的错误处理、安全、
  无障碍，以及用户明确要求的部分。带分支 / 循环 / 解析 / 钱 / 安全的新逻辑要留一个小测试，
  刻意的简化用 `shortcut: <上限>, <何时升级>` 注释标注。每轮回复结尾用一两行点明「跳过了什么、
  没检查什么、有什么风险」。

  档位决定强度：`/ponytail off` 关闭，`lite` 按用户要求做但用一句话点名更小的做法，`full`
  （默认）走完整梯子，`ultra` 额外在动手前反驳需求里不成立的部分。裸 `/ponytail` 在关闭时按
  默认档开启、在已开启时报告当前档；`/ponytail status` 同时报本会话档位与默认档；
  `/ponytail default <档位>` 把 `defaultMode` 写进 `agent_dir()/extensions/ponytail.json`。
  本会话档位记在会话自定义条目（`ponytail-mode`）里，恢复或切换会话时重放。输入整句
  “stop ponytail” 或 “normal mode” 即关闭——按**整句**匹配，所以「add a normal mode toggle」
  这种正常请求不会被误伤。

- **扩展自带五个附属命令**：`/ponytail-review`（改动审查：bug、安全、真实负载、缺测试、
  性能、可删的代码——每条都说明这段代码在做什么、哪里会出错、怎么修、不修会怎样）、
  `/ponytail-audit`（同一套审查跑全仓，按严重度排序）、`/ponytail-debt`（把 `shortcut:`
  注释收成债务账本）、`/ponytail-gain`（已发布的 benchmark 记分牌）、`/ponytail-help`
  （档位与命令速查）。这些正文随二进制内嵌，执行时作为 `<skill>` 块投递给模型，不落盘；
  扩展关闭时也不会有技能描述漏进系统提示。

  默认不启用，`Dev` / `Creator` 模式下可用：它改写的是模型每一轮的写作风格，必须由用户显式开启。

- **新增 `remote_default`：省略 `host` 时到底连哪台机器**：ssh-remote 工具族补上第七个（延迟）
  工具，报出默认目标的 `user@host:port`、记住的 `cwd`、备注、跳板机与私钥、已记住的目标列表、
  活跃的端口转发。它只读本地配置，不建连接、不等网络；没设默认目标时如实说「没有」，不猜主机。
  模型因此不必再问「用哪台服务器」，用户也不必为这件事跑 `/ssh status`。
