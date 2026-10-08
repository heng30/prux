# 出站 HTTP 代理（HTTP Proxy）

prux 通过标准代理环境变量为**出站**模型 API 请求配置 HTTP(S) 代理，并支持
`NO_PROXY` 让指定目标直连。

> 方向提醒：本篇讲的是出站（prux → 网络）。把外部客户端接进 prux 的**入站**
> OpenAI 兼容网关是另一件事，见 [扩展（Extensions）](extensions.md) 的 `proxy` 章节。

## 环境变量

| 变量 | 说明 |
|------|------|
| `HTTPS_PROXY`, `https_proxy` | https 目标使用的代理 |
| `HTTP_PROXY`, `http_proxy` | http 目标使用的代理 |
| `ALL_PROXY`, `all_proxy` | 兜底代理：目标协议没有对应的 `<scheme>_proxy` 时使用 |
| `NO_PROXY`, `no_proxy` | 直连名单：命中该列表的目标不走代理 |

大小写变体均支持；同名变量同时存在时**小写优先**。
变量值首尾空白会被忽略；值为空视同未设置。

## 行为

每个出站请求按**目标 URL** 逐请求决策（构建 HTTP 客户端时挂载
`reqwest::Proxy::custom`，而非全局套用一个代理）：

1. 目标命中 `NO_PROXY` → **直连**，不查代理变量。
2. 未命中 → https 目标依次取 `https_proxy` → `all_proxy`；http 目标依次取
   `http_proxy` → `all_proxy`。
3. 都没有可用代理 → 直连。

这与 curl 等常见工具的按协议选择一致：例如只设置了 `HTTPS_PROXY` 时，`http://`
目标不会被代理。

### 代理 URL 格式

- 代理值缺省 scheme（不含 `://`）时，按目标协议补全：
  `https_proxy=proxy.corp:8080` 对 https 目标等价于
  `https://proxy.corp:8080`。
- 支持的代理协议：`http`、`https`，以及 `socks4`/`socks4a`/`socks5`/`socks5h`
  （reqwest 的 socks feature）。
- 无法解析或协议不支持的代理值不会生效，该协议的目标**直连**（注意：`<scheme>_proxy` 已给出但不可解析时
  **不会**再回退到 `all_proxy`），也不会报错。

### NO_PROXY 语法

列表以**逗号或空格**分隔，条目大小写不敏感：

| 写法 | 含义 |
|------|------|
| `*` | 全部直连（仅当整个列表就是 `*`） |
| `example.com` | 匹配 `example.com` 及其所有子域（`api.example.com`） |
| `.example.com` | 等价于 `example.com`（前导点） |
| `*.example.com` | 等价于 `example.com`（子域通配） |
| `127.0.0.1` / `::1` | 匹配 IP（IPv6 可裸写） |
| `[2001:db8::1]` | 匹配带方括号写法的 IPv6 |
| `host:port` | 仅匹配该 host 的指定端口（如 `127.0.0.1:8080`） |

不支持 CIDR（如 `10.0.0.0/8`）。`example.com` 不会误伤
`notexample.com`。

## 示例

公司代理 + 本地/内网直连：

```bash
export HTTPS_PROXY=http://proxy.corp.example:8080
export HTTP_PROXY=http://proxy.corp.example:8080
export NO_PROXY="localhost,127.0.0.1,::1,*.internal.example"
```

全部直连（关闭代理）：

```bash
export NO_PROXY="*"
```

仅放行企业 GitHub，其余走代理：

```bash
export HTTPS_PROXY=http://proxy.corp.example:8080
export NO_PROXY="github.com,githubusercontent.com"
```

## 生效范围

代理决策挂在 `utils::http::client_builder` 上，**用它构建的所有 HTTP 客户端**都生效，包括：

- 所有协议实现的模型 API 调用（completions / responses / anthropic / google 等）；
- OAuth 登录 / 令牌刷新等认证请求（`core::oauth::*`）；
- 远程模型目录同步、更新检查、MCP 的 HTTP 传输与 MCP OAuth（均经 `client_builder`）。

反之，显式传了 `proxy` 选项的调用（如 `web-access` 扩展的抓取）优先用调用方给定的代理；
未用该 builder 的本地进程/stdio 通道（如 MCP stdio 服务器子进程）不受影响。
