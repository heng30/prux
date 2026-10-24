# SSH 远程工具与隧道（ssh-remote）

`ssh-remote` 给 prux 加上「另一台机器」：模型可以用 `remote_exec` 在远端跑命令，
用 `remote_read` / `remote_ls` / `remote_grep` 读远端文件，用 `remote_sync` 搬文件；
用户可以用 `/tunnel` 把远端端口映射到本机。

**它不改写任何内置工具。** 内置 `read`、`write`、`edit`、`ls`、`find`、`grep`、`bash`
始终只操作本地，`@` 文件补全、`!` 命令、git 分支、LSP、子代理看到的也始终只有本地。
远程是「第二个文件系统 + 一台能跑命令的机器」，不是工作区——这是刻意的：

- 不存在「一半穿透、一半不穿透」的裂缝（LSP / MCP / 子代理不可能跑到远端）；
- 本地与远端可以同时用，不需要先「切过去」；
- 断开远端不会影响本地会话。

**默认不启用**：它会建立出站连接、并可能改动远端文件。在 `/extension` 面板开启，
或把 `ssh-remote` 加进 `settings.json` 的 `enabledExtensions`。

## 工具

| 工具 | 作用 |
|---|---|
| `remote_exec` | 在远端跑一条 shell 命令（查日志、进程、磁盘、容器） |
| `remote_read` | 读远端文本文件，支持 `offset` / `limit` 行范围 |
| `remote_ls` | 列远端目录（目录在前，文件带大小） |
| `remote_grep` | 远端检索文件内容（有 ripgrep 用 ripgrep，否则退回 `grep -rn`） |
| `remote_sync` | 本地 ↔ 远端复制文件或目录树，支持 `dry_run` |
| `remote_pwd` | 报出该目标的远端工作目录（其他 `remote_*` 工具默认就在这个目录里操作）与它在远端解析出的绝对路径 |
| `remote_default` | 报出省略 `host` 时默认命中的目标（含 `cwd`、跳板机、私钥、已记住的目标、活跃转发）；只读本地配置，不建连接 |

除 `remote_default` 外都有 `host` 参数：`[user@]host[:port]`（省略 `user` 时用当前用户名），或 `~/.ssh/config` 里的别名。
省略时使用 `/ssh <target>` 设置的默认目标——想知道那是哪台机器就用 `remote_default`，不必问用户。

这七个工具是**延迟工具**：它们不在开箱可见的工具列表里，模型要先调用 `tool_search`
检索（`query: "ssh remote"` 即可命中整族），命中后从下一次调用起直接可用。所以
`tool-search` 扩展需要处于启用状态；`/extension` 面板里 ssh-remote 的详情页会标出这一前置关系。

`remote_pwd` 报的是「这套工具当前在哪儿操作」：`cwd` 是 `/ssh cd` 记住的目录，
`absolute` 是它在这台机器上解析出的绝对路径（`~` 会展开成登录用户的家目录），`home` 是家目录。
其余工具默认就用这个目录，所以拼相对路径不必先调它；要写远端绝对路径时调一次，别靠猜。

## 工作目录

每个调用都用该目标的「记住目录」（`/ssh cd` 设置的 `cwd`，默认 `~`）：

- `remote_exec` 省略 `cwd` 时在其中执行；传了 `cwd` 就以它为准，相对路径相对记住目录；
- `remote_read` / `remote_ls` / `remote_grep` / `remote_sync` 的相对路径都相对它展开；
- `~`、`~/project` 这类写法可用，工具会主动在远端把它解析成绝对路径再操作；
- 解析出的绝对路径会写在结果首行（如 `user@host:/srv/app/log.txt`），不用再猜目录。

SFTP 没有「当前目录」概念、也不会展开 `~`，所以读 / 列 / 传文件前工具会先问远端一次
真实路径（记住目录本身已经是绝对路径时不额外往返）。记住目录进不去时直接报错，
不会默默回退到登录目录。

```text
用户：看看线上机器 /var/log/app 里最近的报错
→ tool_search { query: "ssh remote" }
→ remote_exec { command: "tail -n 100 /var/log/app/error.log", host: "prod-web-1" }
→ remote_grep { pattern: "panic|ERROR", path: "/var/log/app", host: "prod-web-1" }
```

## 命令

### `/ssh`

| 用法 | 作用 |
|---|---|
| `/ssh [user@]host[:port]` | 设为默认目标（也接受 `ssh -i key -J jump ...` 这类写法）并立刻试连一次；只有这一下真正登录成功，目标才会被记进默认值与已知列表。重新登录已记住的目标时，它保存的远端目录与备注会保留。端口只写在 `host:port` 里，`-p` 会报错 |
| `/ssh list` | 列出已连接端点与已记住的目标，各带当前远端工作目录（`cwd`）；默认目标标 `(default)` |
| `/ssh status` | 默认目标、已知目标数、活跃隧道数、超时与每轮输出预算 |
| `/ssh cd <path>` | 设置默认目标的远端工作目录；已建立的连接当场跟着换，此后工具调用都用它。支持 `~` 与 `~/sub`，也支持相对路径 |
| `/ssh note <text>` | 给默认目标加备注（显示在停靠面板） |
| `/ssh disconnect [target\|all]` | 断开连接 |
| `/ssh remove <target\|all>` | 彻底移除目标：删掉 `config.json` 里的记录（含默认目标项）、它的主机密钥与缓存的密码 / 口令 |
| `/ssh forget <host[:port]\|all>` | 只删主机密钥（目标仍留在列表里，用于指纹变化后重新信任） |
| `/ssh help` | 打印目标写法（`-i` / `-J` / `-o PasswordFile=`）与全部子命令 |

首个词与子命令同名会被当成子命令；`~/.ssh/config` 里的别名正好叫 `help` / `list` 时，
用带 `ssh` 前缀的写法连它：`/ssh ssh help`。

### `/tunnel`

| 用法 | 作用 |
|---|---|
| `/tunnel add [host:]本地端口:远端主机:远端端口` | 把远端服务映射到 `127.0.0.1:本地端口` |
| `/tunnel remove <本地端口>` | 停掉一条转发 |
| `/tunnel list` | 列出活跃与已配置的转发 |
| `/tunnel stop-all` | 停掉全部转发（配置保留，下次启动会重新拉起） |
| `/tunnel remove-all` | 停掉并删除全部转发（含配置里的条目，下次启动不再拉起） |

```text
/tunnel add 15432:localhost:5432
```

之后本机的数据库客户端、脚本、MCP server 全部连 `127.0.0.1:15432` 就行——
这是访问远端服务最干净的路径：远端只是「资源」，本地工具一个都不用改。

## 认证

先试公钥认证，失败后再试密码。私钥候选顺序：

1. `-i <key>` 或 `~/.ssh/config` 里该主机的 `IdentityFile`；
2. 否则依次尝试 `~/.ssh/id_ed25519`、`~/.ssh/id_ecdsa`、`~/.ssh/id_rsa`（缺文件跳过）。

显式 `-i` 保持「排他」：只有在同时提供密码来源时才回退密码。私钥有口令时会提示输入口令。

密码来源按顺序：

1. 进程内缓存（同一次运行里重连、再次连接直接复用）；
2. `-o PasswordFile=<path>`（文件权限必须是 0600，否则拒绝读取并报出 `chmod 600` 提示）；
3. 环境变量 `PRUX_SSH_PASSWORD`；
4. TUI 掩码输入框（逐键回传，屏幕上只显示 `*`）。

服务器只接受 keyboard-interactive 而不是 `password` 时（常见于 PAM 配置），会用同一个密码
自动再试一次。私钥口令可用 `PRUX_SSH_KEY_PASSPHRASE` 提供，或直接在弹出的输入框里键入。

凭据只保存在进程内存：不写文件、不进命令行参数、不进会话记录。`/ssh remove <target>`
清掉该目标的缓存（目标本身也从列表里移除），`/ssh remove all` 清空全部并删掉所有已记住的
目标；`/ssh disconnect` 保留缓存以便重连。

不支持 SSH agent（`SSH_AUTH_SOCK`）。

## 主机密钥

首次连接采用 TOFU：把服务器公钥记进 `agent_dir()/extensions/ssh-remote/known-hosts.json`。
之后公钥不匹配会**直接拒绝**，并提示运行 `/ssh forget <host:port>`（不带端口时删该主机
所有端口的记录）。`/ssh forget` 只动 `known-hosts.json`；要连目标记录一起清掉用 `/ssh remove`。
这挡的是中间人与「同 IP 换了机器」，不是可以随手跳过的提示。

## 跳板机与代理

- `-J user@bastion:22` 或 `-o ProxyJump=bastion`：经跳板机连接（跳板机自己也走完整认证）；
- `~/.ssh/config` 的 `Host` / `HostName` / `User` / `Port` / `IdentityFile` / `ProxyJump` 会被应用；
- 出站代理由 `config.json` 的 `proxy` 指定（`socks5://` / `socks5h://` / `http://` / `https://`，
  可带 `user:pass@`）；留空或省略即直连。**它不读 `ALL_PROXY` 等环境变量**，代理写错会直接报错，
  不会静默退回直连。

## 输出预算

远端输出动辄几十万行，直接回灌会瞬间吃掉上下文。因此有三道闸：

- 单次 `remote_exec` 默认 8 KiB / 200 行，超出的部分只保留尾部并明确告知「还剩多少行」；
- `remote_exec` 支持 `tail_lines`，在**远端**就用 `tail -n N` 截断，省带宽；
- 每轮所有远程输出合计默认 32 KiB，用尽后工具会直接说明「本轮预算已用尽」，不静默丢内容。

`remote_read` 对大文件（大于 2 MiB）走「远端 `wc -l` + `sed -n` 取范围」，不会把整个文件拉回本地。

## 配置

`agent_dir()/extensions/ssh-remote/config.json`：

```json
{
  "default": { "host": "gpu1", "user": "root", "port": 22, "cwd": "/srv/app" },
  "known": [ { "host": "gpu1", "user": "root", "port": 22 } ],
  "tunnels": [ { "host": "", "localPort": 15432, "remoteHost": "localhost", "remotePort": 5432 } ],
  "proxy": "socks5://127.0.0.1:1080",
  "timeoutSeconds": 30,
  "execMaxLines": 200,
  "execMaxBytes": 8192,
  "readMaxLines": 400,
  "readMaxBytes": 16384,
  "turnMaxBytes": 32768
}
```

- `default`：工具省略 `host` 时用的目标；
- `known`：连过一次就记住，省掉重述参数与 `~/.ssh/config` 查询；
- `tunnels`：扩展启用时自动拉起（失败只提示，不阻断启动）；
- `proxy`：出站代理 URL，省略 = 直连；格式不合法时连接直接失败并说明原因；
- 其余字段是上面「输出预算」里的上限，可手改。

主机密钥单独存在 `agent_dir()/extensions/ssh-remote/known-hosts.json`，与配置分开是为了能安全删除。

## 已知边界

- 不支持 SSH agent（`SSH_AUTH_SOCK`）；密码与私钥口令走「认证」一节的来源。
- 远端没有 `rg` 时 `remote_grep` 退回 `grep -rn`，语义略有差异（`grep` 的 `-m` 是每文件上限）。
- `remote_sync` 是逐文件 SFTP 传输，不做增量对比、不删目标端多余文件；单次上限 2000 个文件。
- 端口转发只监听 `127.0.0.1`，不对外暴露。
- 远端 LSP / 远端 MCP / 远端子代理不在本扩展范围内。
