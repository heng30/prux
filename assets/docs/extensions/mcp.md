# MCP（Model Context Protocol）

`mcp` 扩展把**外部 MCP 服务器**接入 prux：一个模型可见的 `mcp` 代理工具，加上 `/mcp`
命令与 `prux mcp` CLI，用来管配置、诊断连接、走 OAuth 登录。

- 扩展名：`mcp`（settings 的 `enabledExtensions` / `/extension` 面板切换）
- 代码位置：`src/extensions/mcp/`（`config` / `manager` / `oauth` / `error`）
- 来源：移植 kiss 的 `kiss-mcp` v0.0.18（`/extension` 二级详情里的 fork 来源）
- **默认关闭**：`default_enabled() == false`，只在 `Dev` / `Creator` 扩展模式注册。
  开启方式：`/extension` 面板，或 `settings.json` 的 `"enabledExtensions": ["mcp"]`。

> 这是标准 MCP 客户端，**不是 WebMCP**（浏览器 CDP 集成）。传输只支持
> **stdio**（`command`）与 **Streamable HTTP**（`url`）两种。

## 目录

- [心智模型](#心智模型)
- [开启与前置条件](#开启与前置条件)
- [配置发现](#配置发现)
- [服务器条目字段](#服务器条目字段)
- [`mcp` 工具](#mcp-工具)
- [配置例子](#配置例子)
- [生命周期与缓存](#生命周期与缓存)
- [OAuth 2.1](#oauth-21)
- [命令](#命令)
- [安全](#安全)
- [与其他模块的关系](#与其他模块的关系)

---

## 心智模型

MCP 服务器的工具**不会**被逐个注册成 prux 工具。无论配了多少台服务器，模型始终只看到
**一个** `mcp` 工具，靠 `action` 参数去检索、查看、调用远端能力：

```
远端 MCP 服务器（stdio 子进程 / HTTP 端点）
        │  连接、列举、调用
        ▼
   McpManager（懒连接 + 空闲断开 + 元数据缓存）
        │
        ▼
  单个 mcp 工具  ← 模型只看见它
   action = status | list | search | describe | call
            | resources | read_resource | prompts | get_prompt
```

好处是工具表不会因为远端服务器而膨胀；代价是模型必须先 `search`/`list` 再 `call`，多两步。

`mcp` 工具**仅在当前 cwd 至少有一台已启用服务器时才注入**。没有可用服务器时，工具直接
不出现在工具表里（不会让模型白调）。

## 开启与前置条件

```jsonc
// ~/.prux/settings.json
{ "enabledExtensions": ["mcp"] }
```

前置条件：

1. 已有一份能解析出至少一台未禁用服务器的配置（见下节）。
2. 使用 HTTP 服务器时能连到端点；stdio 服务器的 `command` 在 PATH 中可执行。
3. OAuth 服务器需先 `/mcp login <server>` 取得凭据。

## 配置发现

**全局优先，后覆盖前**：按 1→6 的顺序读取，深度合并（对象递归合并、标量与数组整体替换），
后面的文件覆盖前面的。

| # | 路径 | 作用域 | 何时生效 |
|---|------|--------|----------|
| 1 | `~/.config/mcp/mcp.json` | 与他工具共享的全局配置 | 总是 |
| 2 | `~/.agents/mcp.json` | agents 全局 | 总是 |
| 3 | `~/.agents/mcp/mcp.json` | agents 嵌套全局 | 总是 |
| 4 | `~/.prux/extensions/mcp/mcp.json` | prux 全局（`prux mcp --scope user` 写这里） | 总是 |
| 5 | `<cwd>/.mcp.json` | 项目（`--scope project` 写这里） | **仅项目受信任** |
| 6 | `<cwd>/.prux/mcp.json` | 项目 prux | **仅项目受信任** |

要点：

- `~/.prux` 就是 `agent_dir()`，可用 `PRUX_AGENT_DIR` 环境变量改。
- 项目级两个文件在**项目不受信任**时完全不加载——不信任的项目塞不进恶意 MCP 服务器。
- 顶层键是 `mcpServers`：服务器名 → 条目。服务器名只允许 ASCII 字母数字与 `._-`。
- 另一个顶层键是 `autoEnableCodemode`（bool，缺省开启）：`codemode` / `codemode-deferred` 暴露的服务器接入时是否自动启用 codemode 扩展。
- 允许 `//` 与 `/* */` 注释（字符串字面量内不受影响），方便写 JSONC。
- 深层合并发生在**校验之前**，所以高优先级文件可以只写一小块覆盖字段（见例子）。
- 未识别的顶层 / 服务器字段会原样保留，回写时不丢配置。

## 服务器条目字段

`ServerEntry`（camelCase）：

| 字段 | 类型 | 说明 |
|------|------|------|
| `command` | string | stdio：可执行文件。与 `url` **互斥** |
| `args` | string[] | stdio：命令行参数 |
| `env` | object | stdio：追加到子进程的环境变量 |
| `cwd` | string | stdio：子进程工作目录（支持 `~/` 展开） |
| `url` | string | HTTP：Streamable HTTP 端点，必须 `http`/`https` |
| `headers` | object | HTTP：随每个请求发送的额外请求头 |
| `auth` | `"oauth"` \| `"bearer"` \| `false` \| `{"provider": "<name>"}` | 显式指定认证方式；`false` 表示显式禁用（**不能写 `true`**）；`provider` 形式用该 provider 的当前登录 token 作 bearer（**每次请求现取**，换凭据/刷新后无需重连） |
| `bearerToken` | string | bearer：token 字面值，支持 `${ENV}` 展开 |
| `bearerTokenEnv` | string | bearer：存放 token 的环境变量**名** |
| `oauth` | object | OAuth 配置块（见下文） |
| `lifecycle` | `"persistent"` | 设 `persistent` 则跳过空闲断开 |
| `idleTimeout` | number | 空闲多少分钟后断开，默认 `10` |
| `requestTimeoutMs` | number | 单次 RPC 超时（毫秒），默认 `60000` |
| `includeTools` | string[] | 白名单：非空时只暴露列出的工具（精确名） |
| `excludeTools` | string[] | 黑名单：优先级高于白名单 |
| `disabled` | bool | `true` 则完全不连接、不暴露 |
| `description` | string | 人类可读的说明：进 `mcp` 工具说明与 `/mcp status\|list`，也参与 tool search 检索 |
| `exposure` | `"direct"` \| `"codemode"` \| `"codemode-deferred"` \| `"deferred"` \| `"hidden"` | 该服务器工具的默认暴露方式，缺省 `direct`（直接可见）：`codemode*` 只在 codemode 脚本里可调、`deferred` 需经 `tool_search` 激活、`hidden` 保留配置但不暴露 |
| `toolExposure` | object | 单个工具的暴露方式覆盖（键为工具名，`*` 匹配任意字符；精确名优先，其次按声明顺序取第一个匹配的通配模式） |

校验规则：

- `command` 与 `url` 必须**恰好有一个**，两者都有或都无都报错。
- stdio 不能带任何 HTTP 认证字段（`auth` / `oauth` / `bearerToken` / `bearerTokenEnv`）。
- `auth: true` 非法。
- `oauth.grantType == "client_credentials"` 必须同时给 `clientId` 与 `clientSecret`。
- `oauth.clientRegistration == "cimd"` 必须给 https 且路径非根的 `clientMetadataUrl`。
- `oauth.authServerMetadataUrl` 必须是 `http`/`https`。
- `auth: {"provider": "..."}` 的 provider 名不能为空；非 loopback 端点必须 `https`；
  该形式**只允许出现在用户级配置或扩展声明里**，项目级配置文件里出现直接报错。
  该形式的 token 在**每个请求**前重新解析（OAuth 凭据优先，过期先刷新），
  所以中途 `/login`、登出或凭据轮换都会在下一发请求生效，不需要 `/mcp reload`；
  凭据缺失时请求直接报错（附 `/login <provider>` 指引），不会退化成匿名请求。
- `${VAR}` 展开适用于 `command`、`args`、`env` 值、`cwd`、`url`、`headers` 值、`bearerToken`；
  注意语法只有 `${VAR}`，不识别裸 `$VAR`；变量不存在时展开为空串。

## `mcp` 工具

单个工具、9 个 action，参数：`action`（必填）、`server`、`name`、`query`、`uri`、`arguments`。

| action | 必填参数 | 作用 |
|--------|----------|------|
| `status` | — | 每台服务器的连接状态与缓存计数（不连接） |
| `list` | 可选 `server` | 懒连接并列举工具；`server` 省略时覆盖全部非禁用服务器 |
| `search` | `query`，可选 `server` | 多词模糊匹配工具名 / 标题 / 描述 / 服务器名（词间 AND） |
| `describe` | `server` + `name` | 返回单个工具的输入 schema；命中缓存则免连接 |
| `call` | `server` + `name`，可选 `arguments` | 调用远端工具 |
| `resources` | `server` | 列举资源 |
| `read_resource` | `server` + `uri` | 读取资源 |
| `prompts` | `server` | 列举 prompt |
| `get_prompt` | `server` + `name`，可选 `arguments` | 获取 prompt |

行为细节：

- 工具执行模式为 `Parallel`；每个 RPC 与父级 abort 联动（取消令牌），并受 `requestTimeoutMs` 约束。
- `call` 返回的内容：Text 超过 100 KB 截断并附提示；Image 作为附件直传；Audio 转成占位文本；
  `isError` 的结果转成工具错误。
- 服务器状态取值：`disabled`（配置禁用）、`not-connected`、`cached`（无连接但有缓存）、
  `connected`、`failed`（含 `error`）。
- `includeTools` / `excludeTools` 在列举、搜索与 `describe` 处过滤，被排除的工具不会出现在结果里；但 `call` 不额外拦截名单，直接透传给远端服务器。

典型用法（模型侧）：

```
mcp action=search query="create issue"
mcp action=describe server=github name=create_issue
mcp action=call server=github name=create_issue arguments={"title":"..."}
```

### 系统提示里的 `mcp_servers` 段

exposure 为 `codemode` / `codemode-deferred` / `deferred` 的服务器，它们的工具**不会**直接向模型声明，
因此在系统提示末尾追加一段：

```
<mcp_servers>
MCP servers whose tools are not declared to you. Call the tools of `codemode` servers from codemode scripts. Load the tools of `tool_search` servers with `tool_search`.
- docs-server (codemode): Documentation lookup
</mcp_servers>
```

每台一行 `- <服务器名> (<获取方式>)`，写了 `description` 的附一句摘要（只取第一行，按剩余预算截断）；
按服务器名排序，整段超过 4096 字符时从末尾省略并补一行计数。默认的 `direct` exposure 工具本就随 `mcp`
工具声明可见，不在这段里；MCP 扩展被禁用、或没有任何这类服务器时，整段不出现。

## 配置例子

### 1. 最小 stdio（本地子进程）

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
    }
  }
}
```

### 2. stdio + 环境变量 + 工作目录

```json
{
  "mcpServers": {
    "postgres": {
      "command": "uvx",
      "args": ["mcp-server-postgres"],
      "env": { "DATABASE_URL": "postgres://user:pass@localhost:5432/app" },
      "cwd": "~/work/db"
    }
  }
}
```

### 3. 最小 HTTP

```json
{
  "mcpServers": {
    "local": { "url": "http://127.0.0.1:9527/mcp" }
  }
}
```

### 4. HTTP + 请求头（`${ENV}` 注入密钥）

```json
{
  "mcpServers": {
    "internal": {
      "url": "https://mcp.internal.example.com/mcp",
      "headers": {
        "X-Org": "platform",
        "Authorization": "Bearer ${INTERNAL_MCP_TOKEN}"
      }
    }
  }
}
```

### 5. HTTP + bearer（字面值 / 环境变量名两种写法）

```jsonc
{
  "mcpServers": {
    // 直接写 token（会随配置落盘，注意别提交进 git）
    "a": { "url": "https://a.example.com/mcp", "auth": "bearer", "bearerToken": "sk-xxx" },
    // 从环境变量取；配置里只有变量名，token 留在进程环境
    "b": {
      "url": "https://api.githubcopilot.com/mcp/",
      "auth": "bearer",
      "bearerTokenEnv": "GITHUB_TOKEN"
    }
  }
}
```

> `bearerToken` / `bearerTokenEnv` 存在时即使不写 `auth` 也会推断为 bearer；写 `auth` 更明确。

### 6. HTTP + OAuth 授权码（浏览器 + PKCE）

```json
{
  "mcpServers": {
    "linear": {
      "url": "https://mcp.linear.app/mcp",
      "auth": "oauth",
      "oauth": {
        "grantType": "authorization_code",
        "scope": "read write",
        "redirectUri": "http://127.0.0.1:3118/callback"
      }
    }
  }
}
```

`clientId` / `clientSecret` 可省略——缺省时走动态客户端注册。配好后需 `/mcp login linear`
（或 `prux mcp login linear`）完成一次授权。

### 7. HTTP + OAuth 客户端凭据（无浏览器）

```json
{
  "mcpServers": {
    "analytics": {
      "url": "https://api.example.com/mcp",
      "auth": "oauth",
      "oauth": {
        "grantType": "client_credentials",
        "clientId": "my-client",
        "clientSecret": "${ANALYTICS_CLIENT_SECRET}",
        "scope": "metrics:read"
      }
    }
  }
}
```

### 8. 工具白/黑名单 + 超时 + 常驻连接

```json
{
  "mcpServers": {
    "github": {
      "url": "https://api.githubcopilot.com/mcp/",
      "bearerTokenEnv": "GITHUB_TOKEN",
      "includeTools": ["search_code", "get_file_contents"],
      "excludeTools": ["delete_file"],
      "lifecycle": "persistent",
      "idleTimeout": 30,
      "requestTimeoutMs": 120000
    }
  }
}
```

### 9. 显式禁用某台服务器

```json
{
  "mcpServers": {
    "noisy": { "disabled": true }
  }
}
```

### 10. 显式断开认证

服务器要求认证但你自有办法（例如走 `headers` 手动带 token）时，用 `auth: false` 关掉
OAuth/bearer 推断：

```json
{
  "mcpServers": {
    "gateway": {
      "url": "https://gateway.example.com/mcp",
      "auth": false,
      "headers": { "X-Api-Key": "${GATEWAY_KEY}" }
    }
  }
}
```

### 11. JSONC 注释

```jsonc
{
  // 团队共享的 MCP 端点
  "mcpServers": {
    "docs": { "url": "https://mcp.example.com/mcp" } /* 只读 */
  }
}
```

### 12. 用户 + 项目两文件合并（覆盖与新增）

`~/.prux/extensions/mcp/mcp.json`（user 作用域，`prux mcp --scope user`）：

```json
{
  "mcpServers": {
    "shared": { "url": "https://mcp.example.com/mcp", "requestTimeoutMs": 60000 }
  }
}
```

`<project>/.mcp.json`（project 作用域，需项目受信任）：

```json
{
  "mcpServers": {
    "shared": { "requestTimeoutMs": 120000 },
    "local": { "command": "node", "args": ["./mcp/server.js"] }
  }
}
```

合并结果：`shared` 保留 user 的 `url`、超时被项目覆盖为 `120000`；`local` 新增。
同样的手法可以只写 `{ "mcpServers": { "shared": { "disabled": true } } }` 在项目里临时禁掉
某个全局服务器。

## 生命周期与缓存

- **懒连接**：管理器构造时**不**启动子进程 / 不建连接，`tools()` 只看配置数量决定是否注入工具。
- **空闲断开**：一次调用结束后按 `idleTimeout`（默认 10 分钟）安排断开；`lifecycle: "persistent"`
  跳过该调度。已连接时再次调用会复用连接。
- **元数据缓存**：tools / resources / prompts 写到
  `~/.prux/extensions/mcp/cache.json`，key 是 `ServerEntry` 序列化的 SHA-256 指纹，
  配置一改即失效。`search` / `describe` / `/mcp status` 可完全免连接命中缓存。
- **超时与取消**：单次 RPC 受 `requestTimeoutMs`（默认 60s）约束，并通过取消令牌与父级
  abort 联动——Esc 中断 agent 也会取消进行中的 MCP 调用。
- **配置刷新**：管理器缓存按 `cwd` + 信任状态 + 读盘时刻（TTL 3 秒）复用；`/mcp reload`
  会清掉缓存与连接并请求重建工具表。
- **子进程日志**：stdio 服务器的 `stderr` 不继承 prux 的终端（那会直接写进 inline TUI 的
  滚动区、撕碎正在渲染的界面），而是重定向到 `~/.prux/extensions/mcp/logs/<server>.log`
  （追加写，单文件超过 1 MiB 时下次连接清空）；打不开日志文件时退化为丢弃。

## OAuth 2.1

- 支持 **authorization code + PKCE（S256）** 与 **client credentials** 两种流程。
- 凭据落在 `~/.prux/extensions/mcp/oauth.json`，按**服务器名**索引并**绑定 URL**：
  URL 变了旧凭据即作废。
- token 刷新由 rmcp 的 `AuthorizationManager` 负责；缺省走动态客户端注册，也可预注册
  `clientId` / `clientSecret`。
- 登录：`/mcp login <server>` 会在后台起任务、打开浏览器，并在
  `http://127.0.0.1:3118/callback` 等回调；结果经聊天区系统消息回执。也可写
  `/mcp login <server> <redirect-url>` 手动传回调（**仅 `client_credentials` 流程支持**：
  `authorization_code + PKCE` 的 state 在内存里，手动回调会报
  `Manual redirect-URL completion is not supported for authorization_code`）。CLI 用 `prux mcp login <server> [--no-browser]`，
  `--no-browser` 时在终端粘贴回调 URL，PKCE 状态同进程完成。
- 登出：`/mcp logout <server>` / `prux mcp logout <server>` 删除已保存凭据。
- 服务器若需要 OAuth 而未登录，调用会报
  `needs OAuth login. Run '/mcp login <server>'`。
- `skipIssuerMetadataValidation: true` 可跳过 issuer 元数据校验，用于不严格实现规范的服务器。

`oauth` 块字段（camelCase，除 `grantType` 外均可省略）：

| 字段 | 说明 |
|------|------|
| `grantType` | `authorization_code`（默认）或 `client_credentials` |
| `clientId` / `clientSecret` | 预注册的客户端凭据；缺省走动态客户端注册 |
| `scope` | 请求的 scope，空格分隔 |
| `redirectUri` | 回调地址，默认 `http://127.0.0.1:3118/callback` |
| `applicationType` | 动态客户端注册上报的 OIDC `application_type`（MCP SEP-837）：`native` 或 `web`；缺省时按 `redirectUri` 派生（自定义 scheme 或 loopback 为 `native`，否则 `web`） |
| `authorizationParams` | 追加到授权 URL 的自定义查询参数（不可覆盖流程自有参数） |
| `clientName` / `clientUri` / `logoUri` | 动态客户端注册时上报的信息 |
| `clientRegistration` | `dynamic`（默认，RFC 7591 动态注册）或 `cimd`（Client ID Metadata Document） |
| `clientMetadataUrl` | `cimd` 用的 metadata 文档地址；同时充当 client_id，须为 https 且路径非根 |
| `authServerMetadataUrl` | 授权服务器元数据文档地址：配了就跳过发现流程，直接用该文档 |
| `skipIssuerMetadataValidation` | 跳过 issuer 元数据校验 |

## 命令

### `/mcp`（交互式）

```
/mcp [status|list|tools [server]|test <server>|login <server> [redirect-url]|logout <server>|reload|enable|disable <server> [user|project]]
```

- 裸 `/mcp` / `/mcp status`：每台服务器状态 + 缓存计数（不连接）。
- `/mcp list`：列出已配置服务器及其**来源文件**。
- `/mcp tools [server]`：列出缓存工具，不连接。
- `/mcp test <server>`：后台连接并列举工具，结果回聊天区。
- `/mcp enable|disable <server> [user|project]`：写覆盖条目启用/禁用；默认作用于**当前项目**
  （项目不受信任时会提示覆盖暂不生效）。
- `/mcp login|logout <server>`：见 OAuth 一节。
- `/mcp reload`：重读配置、丢连接、重建工具表。

`busy_safe`：这些子命令只读配置/缓存或起后台任务，agent 忙碌时也能执行。

### `prux mcp`（CLI）

```
prux mcp list [--json]
prux mcp get <name> [--json]
prux mcp add    <name> [--scope user|project] [--url URL | -- <command> [args...]]
                       [--env K=V]... [--header K=V]... [--cwd DIR] [--auth oauth|bearer|none]
                       [--auth-provider NAME] [--bearer-token TOKEN] [--bearer-token-env VAR]
                       [--oauth-scope S] [--client-id ID] [--client-secret SECRET]
                       [--client-credentials] [--client-registration dynamic|cimd]
                       [--client-metadata-url URL] [--auth-server-metadata-url URL]
                       [--redirect-uri URI] [--timeout-ms MS] [--exposure MODE] [--description TEXT]
prux mcp update <name> [同 add 的 flag]
prux mcp remove <name> [--scope user|project]
prux mcp enable|disable <name> [--scope user|project]
prux mcp test <name>
prux mcp login <name> [--no-browser]
prux mcp logout <name>
```

作用域：`--scope user` = `~/.prux/extensions/mcp/mcp.json`；`--scope project` = `<cwd>/.mcp.json`。
`enable` / `disable` 只写**覆盖条目**（如 `{"disabled": true}`），不会把用户级的 command/url/凭据
抄进项目文件；项目文件里也可以手写只有 `disabled` / `exposure` 的条目来覆盖用户级服务器。
`add` 遇到同名直接报错（提示改用 `update`）；`update` 在已有条目上套用同样的 flag。
`list` / `get` 输出**脱敏**配置（token、密钥、可疑 header 变 `[REDACTED]`）并附来源。

常用示例：

```bash
# stdio：`--` 之后是命令与参数
prux mcp add fs --scope user -- npx -y @modelcontextprotocol/server-filesystem /tmp

# HTTP + 环境变量 bearer
prux mcp add github --url https://api.githubcopilot.com/mcp/ \
  --auth bearer --bearer-token-env GITHUB_TOKEN

# HTTP + OAuth（动态注册）
prux mcp add linear --url https://mcp.linear.app/mcp --auth oauth --oauth-scope "read write"

# HTTP + OAuth 客户端凭据
prux mcp add analytics --url https://api.example.com/mcp --auth oauth \
  --client-credentials --client-id my-client --client-secret "$SECRET"

# 写进项目 .mcp.json（需项目受信任才会被加载）
prux mcp add local --scope project -- node ./mcp/server.js

# 用已登录 provider 的 token 作 bearer（token 每个请求都重新读取）
prux mcp add jira --url https://mcp.example.com/mcp --auth-provider deepseek --description "Issue tracker"

prux mcp list --json
prux mcp test github
prux mcp login linear
```

写入是**原子 rename + 跨进程文件锁**，文件权限 `0o600`。

## 安全

- 项目级配置（`.mcp.json` / `.prux/mcp.json`）只在项目受信任时加载，防止克隆来的仓库
  偷偷挂一个 MCP 服务器。
- 配置文件权限 `0o600`；`list` / `get` 与 `/mcp` 展示会脱敏。
- `${VAR}` 展开发生在运行时，密钥可以只留在环境变量里，不落盘。
- server 名严格白名单校验，避免路径注入。
- stdio 服务器是**任意代码执行**：配置里写什么命令，prux 就会启动什么进程。别往不信任的
  项目配置里放你没看过的命令。

## 测试服务端

`examples/mcp_test_server/` 是一个真实的 HTTP MCP 服务端（rmcp server 侧 + hyper），用来做端到端验证：

```bash
# 起一个要求 bearer 的服务端
cargo run --example mcp_test_server -- --token sk-test
# 授权服务器 metadata 发现端点：missing（默认 404）| broken（缺 token_endpoint）| valid（完整文档）
cargo run --example mcp_test_server -- --discovery valid
```

它同时充当最小 OAuth 授权服务器（`/authorize`、`/register`、`/token`、`/.well-known/*`），
`tests/mcp_oauth_e2e.rs` 复用同一份实现，覆盖 `auth: {"provider": ...}`、`oauth.clientRegistration: "cimd"`
与 `oauth.authServerMetadataUrl` 的运行时路径。服务端实现放 `examples/` 而不是 `src/`：rmcp 的 server 侧
feature 与 `tower` 只在 dev-dependencies 里，不该进 lib 的依赖图。

## 与其他模块的关系

- 与 `subagent`：子代理的工具表默认继承父级全集，因此子代理也能用 `mcp` 工具（除非被
  frontmatter 白名单或 `isolated` 收窄）。MCP 工具本身是 `Parallel` 执行，不影响并发。
- 与 `web-access`：两者都提供联网/检索能力但机制不同——`web-access` 是 prux 内置的
  REST 直连，`mcp` 是走外部 MCP 协议。按需开启其一或并存。
- 与出站 HTTP 代理（[http-proxy.md](../http-proxy.md)）：方向不同。`mcp` 是**客户端**接外部
  服务器；`proxy` 扩展是**入站**网关。
- 与会话/取消：MCP 工具调用走统一的工具执行路径（`ToolExecCtx`，含 `cwd` 与父级 abort），
  因此 Esc 中断 agent 会连带取消进行中的 MCP 调用。plan-mode 只移除 `edit`/`write`，不影响 `mcp`。
