//! 工作流执行内核（rquickjs 沙箱 + 宿主桥 + 后台运行）。
//!
//! - [`meta`]：脚本 `meta` 块的字面量提取与校验；
//! - `glue.js`：注入沙箱的 JS 侧全局（`agent`/`parallel`/`pipeline`/`phase`/`log`/`args`/`budget`）；
//! - [`bridge`]：宿主驱动循环（`AsyncRuntime` + `__dispatch`/`__settle` + 配额/中止 + 进度条目）；
//! - [`structured_output`]：`agent({schema})` 注入 child 的 `StructuredOutput` 工具；
//! - [`inspector`]：运行检查器（两栏）；
//! - [`journal`]：可重放前缀的记录与读取；
//! - [`saved`]：具名工作流的解析；
//! - [`progress`]：进度日志的折叠模型；
//! - [`task`]：后台运行注册表；
//! - [`notify`]：运行完成的模型向通知信封；
//! - [`card`]：运行完成卡片（人向）。

pub(crate) mod bridge;
pub(crate) mod card;
pub(crate) mod inspector;
pub(crate) mod journal;
pub(crate) mod meta;
pub(crate) mod notify;
pub(crate) mod progress;
pub(crate) mod saved;
pub(crate) mod structured_output;
pub(crate) mod task;
pub(crate) mod tool;
