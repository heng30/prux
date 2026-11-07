## English

### Added

- **`/debug context` shows what is filling the context window**: the status bar reports one
  number — the tokens the provider last measured — and it says nothing before a turn has reported
  usage, when the system prompt and the tool schemas are already tens of thousands of tokens.
  `/debug context` splits the window by source instead: the system prompt (with `base`, project
  context, skills and MCP sections counted apart), the tool schemas actually declared for this
  request (name + description + parameter JSON; the hidden ones do not count), and the
  conversation. With a measured usage the known parts are counted and the difference goes to the
  conversation; before the first turn all three parts are estimates and the total is their sum, so
  it never reads 0. The rest of the window prints as `free`. Part of the `debug` extension, which
  stays off by default (Dev mode).

## 中文

### 新增

- **`/debug context` 说明上下文窗口被什么占着**：状态栏只给一个数——供应商上次实测的 token——
  而在第一轮落地 usage 之前它什么都不说，可那时系统提示词与工具 schema 已经占了几万 token。
  `/debug context` 改成按来源拆开：系统提示词（`base`、项目上下文、技能、MCP 段分别计数）、
  本次请求真正声明的工具 schema（名字 + 描述 + 参数 JSON，被隐藏的不算）、以及会话本身。
  有实测 usage 时已知部分按词表数出来、差额归会话；第一轮之前三块都是估算、总额取三块之和，
  因此不会再显示 0。窗口剩下的部分打成 `free`。属于 `debug` 扩展，仍然默认不启用（Dev 模式）。

