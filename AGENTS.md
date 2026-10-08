# AGENTS.md

本文件会被 prux 在启动时自动加载（项目级上下文）。动手前至少读「本地环境」与「跨模块硬约定」两节——
前者违反即不可恢复，后者违反能编译、能过测试，只有人眼会翻车。

## 本地环境：绝对不要动用户的 tmux server

- **禁止 `tmux kill-server`。** 它杀掉的是本机**所有** tmux 会话，包括用户正在使用的会话，
  且不可恢复（滚动历史、运行中的进程全部丢失）。曾因调试 TUI 执行它而误杀用户的会话。
- 禁止 `tmux kill-session -a`、`tmux kill-window -a` 这类“清理其它/全部”的批量命令。
- 不要用 tmux 调试 TUI。优先顺序：
  1. `cargo test` + `ratatui::backend::TestBackend`：对单帧缓冲做确定性断言；
  2. 独立 pty（pexpect / `script`）驱动二进制并重建屏幕；
  3. 万不得已用 tmux 时：先 `tmux ls` 查看已有会话，创建**唯一命名**的会话
     （如 `zz-debug-<pid>`），结束时只执行 `tmux kill-session -t zz-debug-<pid>`，
     绝不触碰其它会话，也绝不使用 `kill-server`。
- 不要假设 tmux 里没有别人的会话，也不要假设你可以随意重启/清空它。

## 仓库地图

本仓是 **`@earendil-works/pi-coding-agent`（TypeScript）的 Rust 重写**：单 crate（非 workspace），
git 历史以「同步 pi-vX.Y.Z」为节奏推进，与上游保持行为/文案对齐是本仓的主线。

| 位置 | 内容 | 注意 |
|---|---|---|
| `src/` | 全部源码（模块索引见下节） | 单 crate，改分层先看 `src/lib.rs` / `src/core.rs` 顶部注释 |
| `tests/` | 集成测试（按主题分文件：session / tools / compaction / mcp_oauth_e2e / proxy_gateway 等） | 经自身 dev-dependency 的 `test-support` feature 打开接缝，见「构建 / 测试」 |
| `assets/` | 编译期内嵌资源：`docs/`（中文官方文档）、`changelog/`、`models/`、`themes/`、`syntaxes/`、`banner/`、`export-html/` | 经 `embedded!` 宏 `include_str!` 内嵌，改文件即改二进制 |
| `migrations/` | 每次对齐 pi 版本一份迁移指南（`migration-vX.Y.Z.md`），另有按功能写的迁移（plugins / prompt / subagents / tasks） | 参照现状的历史账本，动手前先读最新那份 |
| `scripts/sync-models.py` | 从本地 pi 安装目录同步 `assets/models/` | 手改 `assets/models/**` 一定会被下次同步覆盖 |
| `vendor/mermaid-text` | `Cargo.toml` 里 `[patch.crates-io]` 的本地 vendored 依赖 | 是第三方代码，不适用本仓注释规范 |
| `examples/` | 可运行示例（`cargo run --example cli_demo`） | 兼作 API 用法演示，改公共 API 时留意 |
| `pi/` | **符号链接**，指向仓外的上游源码（TS / 参考实现），不在 git 索引内 | 只读参考：禁止写入、禁止纳入提交 |

运行期目录（`.prux/`、`.pi/`、`tmp/`、`target/`）均在 `.gitignore` 内，不属于源码。

## 模块索引（`src/`）

| 模块 | 职责 | 依赖方向 |
|---|---|---|
| `cli/` | CLI 业务：参数解析（clap）、文件参数、初始消息、会话选择、模型列表、项目信任、`auth` / `mcp` 子命令 | 依赖 `core`、`utils` |
| `core/` | 核心：模型调用、Agent 循环、会话、压缩、工具、扩展机制、设置 | 见下方三层 |
| `extensions/` | 内置扩展（编译期静态注册），每个模块一个功能 | 依赖 `core`（trait 面）、`utils` |
| `modes/` | 运行模式：交互 TUI（`interactive/` 下 `app.rs` / `panel.rs` / `render/` / `handlers/` / `worker.rs`） | **只向下**依赖 `core` 与 `cli` |
| `utils/` | 通用工具：diff、mime、路径、截断、终端、网络、图片、cron、zip 等 | **不依赖 `core`** |
| `test_support/` | 测试接缝：线程本地目录/HOME override、超时护栏 | 仅 `cfg(test)` 或 `test-support` feature 编译 |

`core` 内部三层（`src/core.rs` 顶部是唯一真源，此处摘要）：

1. **底层** `provider`（对应 pi-ai）：统一消息/模型/工具类型 + 流式调用，**禁止依赖本层其它模块**；
2. **中层** `agent_session` / `session_manager` / `compaction`：Agent 循环、状态、事件、会话历史、上下文压缩，不知道具体业务；
3. **顶层业务** `tools` / `skills` / `prompt_templates` / `system_prompt` / `context` / `project_trust` / `settings_manager` / `model_resolver` / `export_html` / `extensions` 等。

## 扩展系统：注册靠声明，不靠注册表

`src/extensions.rs` 用 `#[linkme::distributed_slice]` 声明三类工厂切片（扩展 / footer / banner），
启动时 `register_all` 遍历切片按 `priority` **降序**注册。因此：

- **新增内置扩展 = 声明 `pub mod x` + 在模块内放一个工厂静态**，不要去改 `main.rs` 的注册函数；
- `priority` 值越大越先注册，影响两类语义：footer 三布局互斥（*最后注册的启用者生效*，
  故默认底栏 `footer(normal)` 的 priority 最小）、面板条目/同名命令的先到先得顺序；
- 多数扩展**注册后为禁用态**（如 `doc-helper`），需用户在 `/extension` 面板开启——新扩展默认不要抢行为。

## 跨模块硬约定（违反即复现历史 bug）

- **依赖方向单向向上**：`core::provider` 不依赖本层其它模块；`utils` 不依赖 `core`；`modes` 只向下依赖 `core` / `cli`。
  这些约束同 crate，跨层互调照样能编译，没有任何机器检查会拦住你。
- **锁序固定，禁止反向**：注册表 → 扩展 state（`extensions.rs` / `plan_mode.rs` 均如此），
  配置缓存恒为「RwLock → 缓存 Mutex」。扩展回调里**不要跨 `push`/`retain` 持锁**，
  也不要在持锁时回调注册表（`hooks()`）——已有 ABBA 死锁与「锁不可重入」的教训在 `test_support` 与各 config 模块顶部。
- **测试隔离只能用线程本地 override**：测试曾用 `set_var("PRUX_AGENT_DIR")` + 进程级 `Mutex`，
  导致跨测试竞态、锁重入死锁、跨进程踩同一目录。同步测试用 `AgentDirGuard::temp()` / `HomeGuard`；
  有 `spawn` 的异步测试用 `pin_test_agent_dir`（叶级锁串行，无嵌套）；网络/服务器测试套 `run_with_timeout`。
  **回调体内禁止再取任何锁**。
- **`assets/` 是编译期内嵌的唯一真源**：`doc_sync` 会把内嵌文档**覆盖式**同步到 `agent_dir()/docs`（清单内文件被外部修改会被覆写，
  清单外用户文件保留）。所以改文档要改 `assets/docs/**`，改 `agent_dir()/docs` 里的副本无效。
- **changelog 与版本绑定**：`assets/changelog/v.<Cargo.toml version>.md` 缺失会**直接编译失败**（`include_str!` 编译期路径）。
  这是有意的机制，别改成运行时读取。
- **模型数据不手改**：`assets/models/**` 由 `make sync-models` 生成，范围由 `src/core/model_resolver.rs` 的
  `SUPPORTED_PROVIDERS` 决定；手改会在下次同步被覆盖。
- **与 pi 对齐是契约**：错误文案、参数校验、协议形状多处注释写着「对齐 pi / 文案与 pi 一致」。
  改动用户可见的报错与提示前，先确认上游怎么写的；偏差要能说出理由。
- **符号链接只读**：`pi/` 指向仓外目录，不在版本控制内。读没问题，写=改别人的仓。


## 代码要求

- 如果 `enum` 类型需要和字符串相互转换，需要使用`strum`库实现这个功能，不要手动实现转换函数。

## 注释：`///` 与它修饰的 item 绑定，移动代码时必须连注释一起核对

文档注释是**位置绑定**的，编译器、clippy、rustfmt 都不会校验“这段注释说的到底是不是下面这个东西”。
复制/粘贴/抽取函数/调整顺序时，很容易只搬了代码、注释留在原地（或反之），留下一条张冠李戴的注释，
而它照样能编译、照样通过 CI，只有人眼会翻车。曾出现过：`pub struct App` 顶上挂着
“按全局行 g 与选中区间提取该行片段到 out，返回是否写入了内容”（`append_sel_line` 的文档）。

因此，凡是对代码块做移动、复制、抽取、重排，提交前逐条核对：

- **搬代码 = 搬注释**：连同上方紧邻的 `///` 一起搬；搬完回头确认原位置没有残留孤儿注释。
- **语义自检**：注释里提到的形参名 / 返回值 / “字段” 必须能在紧随其后的 item 签名里找到。
  对不上就是错位——例如 `///` 里出现“返回”“参数”“写入 out”这类函数语义词，
  而下面挂的是 `struct` / `enum` / `const` / `type`，基本可以断定是贴错了。
- **不要留下重复文档**：同一段 `///` 出现在两处（复制时没删原处），说明至少有一处是错的。
- **新增公共 item 必须有 `///`**：`struct` / `enum` / 其字段与变体 / `const` / `static` / `type` 都要有，
  字段注释写“是什么 + 单位/取值/None 的含义”，不要复述字段名。

## 注释覆盖范围：类型、成员、全局量与函数都要有 `///`

下列 item 一律要有中文 `///` 文档注释，**新建时就写，不要留待事后补**：

- `struct` / `enum`：说明它表示什么、什么场景下用；
  - 每个**字段**、每个**变体**也都要写：是什么 + 单位 / 取值 / `None` 的含义 / `true` 表示什么，
    不要复述名字（`name` 不要写成“名字”）；
  - 枚举变体若是内联结构体形式（`Variant { ... }`），变体本身和其内部字段都要写。
- `const` / `static`：说明用途与取值来源；成组的同类常量（如同一前缀的一组）要**逐个**写，
  不要只给第一个写一条组注释就算完。
- `fn`：说明它做什么、关键参数的语义与副作用；返回 `Option` / `Result` 时说明何时为 `None` / 何时出错。
- `type` 别名：说明它代表什么、与底层类型的区别（若有）。

例外（不要求）：`vendor/` 下的第三方代码、`tests/` 目录与 `#[cfg(test)]` 块内的测试代码。

## 提交纪律

- 提交信息用中文；按改动性质**逐行**加前缀：新增 `[+]`、删除 `[-]`、修改 `[*]`。
- 一行一件事，行与行之间**不留空行**；一个提交含多类改动就写多行，顺序为 新增 > 删除 > 修改。
- 用**显式路径**暂存（`git add <具体文件>`），**不要** `git add -A` / `git add .`；
  提交前 `git status` 确认暂存区只含本次改动的文件。
- 纯格式化改动（`cargo fmt` 结果）与逻辑改动**分开提交**。

## 格式化

`rustfmt` 已在 `rust-toolchain.toml` 的 `components` 里，仓库无 `rustfmt.toml`、无 git hook，所以**格式只能靠自觉**：

```bash
cargo fmt            # 提交前跑一次
cargo fmt --check    # 只想核对
```

## 构建 / 测试

```bash
make                 # cargo build --release --bin prux（Makefile 的 all 目标即 release）
make build           # 仅编译，debug 目标
make debug           # cargo run
make check           # cargo check
make sync-models-check   # 校验 assets/models 与本地 pi 安装是否一致（不写文件）
cargo test           # 全量测试（单元 + tests/ 集成）
cargo clippy         # toolchain 已带 clippy 组件
```

- CI（`.github/workflows/{linux,macos,windows}.yml`）在 `push` 时跑 `make`；**只有 Linux 跑 `cargo test`**
  （注释写明：全量测试含死锁/竞态回归护栏）。⚠️ macOS / Windows 上测试不过不会被 CI 拦下，
  跨平台改动要自己在本地补跑。
- `tests/` 下的集成测试依赖 `test-support` feature（`Cargo.toml` 里自身 dev-dependency 打开）。
  新增测试接缝时同步更新 `src/test_support/mod.rs` 的用法说明。
- 涉及 TUI 的验证优先 `ratatui::backend::TestBackend`（见「本地环境」）；涉及网络的用
  `run_with_timeout` 包住测试体，避免挂起。

## 版本与 pi 对齐

- **版本号唯一真源 = `Cargo.toml` 的 `version`**：`core::changelog::VERSION` 与 doc_sync、bug report、
  `--version` 全部取自它。发版时同步新增 `assets/changelog/v.<version>.md`（缺文件编译失败）。
- **`sync.md` 记录当前对齐的 pi-ai 版本**（`scripts/sync-models.py` 会读取并在不一致时提示），
  **`migrations/migration-vX.Y.Z.md` 是对齐某一版 pi 的迁移账本**（含「明确不做」的决策记录）。
  下一轮同步时先读最新那份迁移指南，再看上游 `packages/*/CHANGELOG.md`。
- 发布：推 tag 触发三平台 workflow，各自 `make` 后打成 `prux-<tag>-x86_64-<os>.tar.gz`
  （`linux` / `darwin` / `windows`，平台标记在各自 workflow 里写死），**仅在 tag 构建时**上传到 GitHub Release。
