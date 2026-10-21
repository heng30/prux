# 权限门控（permissions）

`permissions` 扩展把「模型能做什么」从提示词约束变成**执行前的确定性判定**：每次工具调用
（内置工具、扩展工具、MCP 代理工具）都在执行前过一遍策略，命中 `deny` 直接拦下，命中
`ask` 同样拦下但告诉你如何放行。

它在 `/extension` 面板里默认是关闭的，用空格开启后立即生效。

## 配置文件

| 范围 | 路径 | 生效条件 |
|---|---|---|
| 全局 | `~/.prux/extensions/permissions/config.json` | 总是加载 |
| 项目 | `<工作目录>/.prux/extensions/permissions/config.json` | **仅项目受信任时**加载 |

项目配置覆盖全局配置：两边都是对象时按键递归合并，类型不同时（一边字符串、一边对象）
高优先级的那份整个生效。未受信的项目里那份配置会被忽略并在 `/permissions show` 里告警——
一个仓库不能靠提交一份配置放宽你本机的策略。

`/permissions init` 会写入一份保守的缺省配置（见[缺省配置](#缺省配置)），已存在时不覆盖，
加 `--force` 才覆盖。

## 策略语法

`permission` 对象下每一层都接受三种值：

```jsonc
{
  "permission": {
    "*": "allow",
    "read": "allow",
    "write": { "*": "deny", "src/*": "allow" },
    "bash": { "*": "allow", "rm -rf /*": "deny" },
    "path": { "*": "allow", "*.env": "deny", "~/.ssh/*": "deny" },
    "external_directory": "ask"
  }
}
```

三态语义：

| 值 | 行为 |
|---|---|
| `allow` | 静默放行 |
| `deny` | 拦下，把命中的规则与理由交回模型 |
| `ask` | 弹确认面板；拿不到答复则拦下（见 [`ask` 意味着什么](#ask-意味着什么)） |

值也可以写成对象带理由：`{"action": "deny", "reason": "这里不允许跑 npm，请用 pnpm"}`，
理由会出现在模型的工具结果里。

### 模式匹配

- `*` 匹配**零个或多个任意字符，包括 `/`**（没有单段/多段之分，`**` 与 `*` 等价）；
- `?` 匹配恰好一个字符；
- 一张表内 **last-match-wins**：最后一条命中的规则生效。所以**兜底写在前面，例外写在后面**：

```jsonc
{ "path": { "*": "allow", "*.env": "deny" } }   // .env 被拒
{ "path": { "*.env": "deny", "*": "allow" } }   // .env 会被放行——兜底在后面吃掉了例外
```

- 路径规则同时匹配**按原样的拼写**与**绝对/符号链接解析后的拼写**，所以用软链接换个名字
  指过去也拦得住；
- 模式开头可以用 `~`、`$HOME`、`${HOME}`。

### 四层与组合

| 层 | 回答的问题 | 作用面 |
|---|---|---|
| `path` | 这个路径模式允许吗？ | 所有工具 + bash 的路径参数 |
| `external_directory` | 访问工作目录之外可以吗？ | 所有工具 + bash |
| 工具名（`read` / `write` / `bash` / `mcp` / 任意扩展工具名） | 这个工具允许吗？ | 对应工具 |
| `bash` 命令模式 | 这条命令允许吗？ | bash |

组合是 **most-restrictive-wins**：`deny` > `ask` > `allow`。

- 任何一层给出 `deny`，调用直接拒绝且不会再询问，拒绝文案里写明是哪一层的哪条规则；
- 任何一层给出 `ask`，调用就被收紧成「需要确认」，即使别的层是 `allow`；
- 一层都没命中时落到 `permission["*"]` 兜底（没写就是 `ask`）。

这条规则解释了两件常见的事：

- `path` 上的 `allow` **压不过** `external_directory: "ask"`——想在项目外放行某个目录，
  规则要写在 `external_directory` 上；
- `path` 上的 `deny` **盖过** `external_directory: "ask"`——被 `path` 拒掉的路径不会先弹
  一次确认再拒。

某个工具表面的**所有**模式都是 `deny` 时，该工具会从模型可见工具表里整个摘掉（省 token、
省一次注定失败的尝试）。只 deny 一部分模式时工具保留，模型仍可尝试，被拒的命令在执行时拦下。

### 方向：只读 / 只写

`path` 与 `external_directory` 各有单向变体：`path_read` / `path_write`、
`external_directory_read` / `external_directory_write`。裸键是「两个方向都算」的简写。

```jsonc
{
  "permission": {
    "external_directory": { "*": "ask" },
    "external_directory_read": { "~/sdk/*": "allow" }
  }
}
```

这里读 `~/sdk` 下面的文件是静默的，而写同一个目录仍要确认。

方向由工具决定：`read` / `grep` / `find` / `ls` 是读，`write` / `edit` 是写；bash 的重定向
目标按自身方向（`>` 写、`<` 读），其余路径参数按命令效果判（`rm` / `cp` / `mv` / `tee` /
`sed -i` 这类视为写，其余视为读）。

## `ask` 意味着什么

命中 `ask` 时会弹一个确认面板：顶行是标题（哪个工具、要执行什么），下面三个选项，
操作提示在底部。

| 选项 | 效果 |
|---|---|
| `Approve once` | 只放行这一次，策略不变 |
| `Approve for this session` | 把这条 `(surface, pattern)` 记进会话放行表，本次运行后续同样命中这条规则的调用都直接放行（策略文件不变） |
| `Deny` | 拒绝本次调用 |

按 `Esc` 关闭面板、面板等待超时（10 分钟）、或者**没有可交互的界面**（非交互运行、启动早期），
都按 `Deny` 处理——`ask` 永远不静默放行，最坏情况是「多拦一次」。

拒绝时交回模型的消息里写明命中的规则和放行办法：

```
Blocked by permissions: bash — bash(ask) matched rule `sudo *`.
The approval panel was not answered (cancelled, timed out, or another prompt was already open),
so 'ask' fails closed. Grant it for this session with `/permissions allow bash "sudo *"`,
or change the rule in ~/.prux/extensions/permissions/config.json.
```

`surface` 与 `pattern` 可以直接复制进 `/permissions allow`。这条命令是**非交互场景的唯一出路**，
也是面板误拒后的补救手段：它只把 `ask` 变成放行，**不会覆盖 `deny`**——底线不因为一次放行而消失。

`yoloMode: true`（或在 `/extension` 的设置面板里把它打开）会把所有 `ask` 当 `allow`，连面板都不弹。
那是显式的逃生门，缺省关闭。

面板一次只开一个：如果已经有别的确认在等（比如另一个子代理先撞上同一条规则），后来的调用
直接按 `Deny` 处理而不会再弹一个——否则新面板会把旧面板顶掉，旧的那个白白等到超时。

## bash 会被怎么分析

bash 是最难判的一层，这里说清边界：**看不透就按 `ask` 处理，绝不猜着放行**。

会被逐条判定的：

- 按 `;` `&&` `||` `|` `|&` `&` 与换行切成语句，**每条各判一次**，最严格的结果决定整条命令；
- 命令替换 `$(…)`、反引号、子 shell `( … )` 里的命令**也判**，并且标注来源
  （`inside command substitution` 之类），所以 `echo $(rm -rf x)` 拦得住；
- 字面 `cd <目录>` 会移动后续命令的工作目录，路径参数同时按原文与绝对拼写匹配；
  `cd "$DIR"` 这种目标不是字面量的，之后不再做绝对拼写匹配（但仍按原文检查）；
- 重定向目标（`> .env`、`2> log`）与路径形参数都进 `path` / `external_directory` 层；
- 前导环境变量赋值（`FOO=1 cmd`）在匹配前剥掉，规则判的是真正的命令；
- 透明包装会被剥掉后继续判：`env` / `nohup` / `command` / `exec` / `time` / `nice` /
  `timeout` / `setsid` / `stdbuf` / `flock`。所以 `timeout 30 npm publish` 按
  `npm publish` 匹配。

一律按 `ask`（也就是拦下）处理的：

- `bash -c` / `sh -c` / 其它 shell 的 `-c`，以及不带 `-c` 从 stdin 读脚本的调用；
- `eval`、`sudo` / `doas` / `su`、`xargs` / `parallel`、`find … -exec`、
  `ssh` / `nsenter` / `unshare` / `chroot` / `systemd-run` / `script` / `watch`、
  `docker` / `podman` / `kubectl`；
- 解释器的 `-c`（`python` / `node` / `ruby` / `perl` / `php` / `lua` / `deno`）与
  `awk` / `sed` 的脚本体；
- **命令没能完整解析**时（引号没闭合、括号不配对）——整条命令按 `ask` 处理。

heredoc：`<<'EOF'`（分隔符带引号）的 body 是纯文本，不判；`<<EOF` 的 body 里的命令替换会被
判。`if` / `while` / `for` / `case` 与 `{ … }` 的内容不做结构解析，按外层语句的文本一起匹配。

## 命令

| 命令 | 作用 |
|---|---|
| `/permissions`、`/permissions show` | 打印生效策略：来源文件、四层规则、会话放行、告警 |
| `/permissions settings` | 打开本扩展的设置面板（再执行一次关闭），见[设置面板](#设置面板) |
| `/permissions allow <surface> <pattern>` | 会话级放行，`surface` 为 `path` / `external_directory` / `bash` / 工具名，也可以是 `*` |
| `/permissions drop <surface> <pattern>` | 撤销一条会话放行 |
| `/permissions allowlist` | 列出会话放行 |
| `/permissions init [--force]` | 写一份保守缺省配置 |
| `/permissions log [n]` | 打印最近 n 条 review log（缺省 10 条） |
| `/permissions reset` | 清空全部会话放行 |

这些命令在 agent 正在跑的时候也能用（典型场景：模型被 `ask` 拦住，你当场放行后让它重试）。

## 设置面板

`/permissions settings`（或 `/extension` 面板里选中 `permissions`）打开设置面板，改动立即写回全局配置：

| 开关 | 含义 |
|---|---|
| `permissionReviewLog` | 是否把每次判定写进 review log（缺省开） |
| `yoloMode` | 是否把 `ask` 当 `allow`（缺省关） |

## review log

每次判定追加一行 NDJSON 到
`~/.prux/extensions/permissions/logs/permissions-review.jsonl`：

```json
{"ts":"2026-02-11T09:31:02Z","tool":"bash","surface":"bash","pattern":"sudo *","action":"ask","value":"sudo ls","reason":null,"sessionApproved":false}
```

`action` 是最终动作，`sessionApproved` 表示这次是不是被会话放行转成了放行。
写不进日志不会影响门控结果（日志是旁路）。

日志里保留被检查的原始值（bash 命令或路径），不需要时把 `permissionReviewLog` 关掉。

## 缺省配置

`/permissions init` 写入的内容：

```jsonc
{
  "permissionReviewLog": true,
  "yoloMode": false,
  "permission": {
    "*": "allow",
    "path": {
      "*": "allow",
      "*.env": "deny",
      "*.env.*": "deny",
      "*.env.example": "allow",
      "~/.ssh/*": "deny",
      "~/.aws/*": "deny",
      "~/.gnupg/*": "deny",
      "*.pem": "deny",
      "*.key": "deny",
      "*id_rsa*": "deny"
    },
    "bash": {
      "*": "allow",
      "rm -rf /*": "deny",
      "rm -fr /*": "deny",
      "rm -rf ~*": "deny",
      "mkfs*": "deny",
      "dd if=*": "ask",
      "shutdown*": "deny",
      "reboot*": "deny",
      "git push --force*": "ask",
      "git push -f*": "ask",
      "git reset --hard*": "ask",
      "git clean -f*": "ask",
      "chmod 777 *": "ask",
      "chown *": "ask",
      "sudo *": "ask",
      "eval *": "ask",
      "bash -c *": "ask",
      "sh -c *": "ask",
      "xargs *": "ask"
    }
  }
}
```

取向是「不打断正常工作流」：只拦敏感文件与灾难性命令，**不设** `external_directory`
边界（不配置 = 不限制），否则第一次访问 `~/.cargo` 就要放行一次。

## 配方

### 保护敏感文件

```jsonc
{ "permission": { "path": {
  "*": "allow",
  "*.env": "deny",
  "*.env.*": "deny",
  "*.env.example": "allow",
  "~/.ssh/*": "deny"
} } }
```

`cat .env`、`cp .env .env.bak`、`echo x > .env` 与 `read` 工具都会被 `path` 拦下。

### 只读 agent

```jsonc
{ "permission": {
  "read": "allow",
  "grep": "allow",
  "find": "allow",
  "ls": "allow",
  "write": "deny",
  "edit": "deny",
  "bash": { "*": "allow", "rm *": "deny", "cp *": "deny", "mv *": "deny", "tee *": "deny" }
} }
```

### 允许项目外的某个目录

```jsonc
{ "permission": {
  "external_directory": { "*": "ask", "~/.cargo/registry/*": "allow", "~/src/shared/*": "allow" }
} }
```

`~/.cargo/registry/*` 的尾部 `*` 是贪婪的，会跨目录层级；不带尾部的
`~/.cargo/registry` 只匹配目录本身。

### 全局禁用某个工具

```jsonc
{ "permission": { "web_search": "deny", "mcp": { "*": "deny" } } }
```

`web_search` 会从模型可见工具表里消失；`mcp` 因为所有模式都是 `deny` 也会消失。

## 边界

- 只认工具参数里的 `path` / `command`。MCP 工具真正的工作参数在 `arguments` 里，
  不解析（只按工具名表面判定），所以 `mcp` 里再套一层路径参数不会被 `path` 层兜住。
- bash 分析是静态的：动态拼接出来的命令（`eval "$CMD"`、从文件读脚本执行）看不透，
  一律按 `ask`。
- 不支持 `permission` 之外的层级（例如按 agent/子代理分别配策略）：所有会话、子代理
  共用同一份策略与同一张会话放行表。
- 确认面板一次只开一个。主 agent 与子代理同时命中 `ask` 时，后到的那个直接按 `Deny` 处理
  （不会排队，也不会顶掉前一个面板）。
