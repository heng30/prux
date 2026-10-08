//! `codemode` 的 JS 沙箱：进程内 rquickjs
//!
//! 驱动方式沿用 `subagent/workflow/bridge.rs` 的既有模式：**宿主不 await 脚本的 Promise**
//! （`AsyncContext::async_with` 要求 closure 的 future 是 `Send`，而 rquickjs 的
//! `Function`/`Value` 都是 `!Send`），脚本结束/抛错时调宿主注入的 `__done`，
//! 宿主只 await tokio 通道；`WithFuture` 在 closure 挂起时会自动跑 JS 微任务队列。
//!
//! - 没有独立 worker 线程可 `terminate()`，由运行时中断处理器（取消标志 / 墙钟 / 脚本
//!   `timeout_ms`）+ 内存上限兜底；
//! - `timeout_ms` 未设置时的墙钟上限是 [`MAX_WALL_MS`]（30 分钟）；
//! - 脚本等待永不能 settle 的 Promise 时由本模块的「空转」检测判死

use futures_util::future::BoxFuture;
use rquickjs::{AsyncContext, AsyncRuntime, Ctx, Function, Object};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{
    Notify,
    mpsc::{UnboundedSender, unbounded_channel},
};

/// JS 预置源码（`include_str!` 进来，对 7 个参数的工厂函数求值）
const GLUE: &str = include_str!("glue.js");

/// 空转检测与取消检查的间隔
const TICK: Duration = Duration::from_millis(50);

/// 脚本的兜底墙钟上限（防「既不结束也不被中断」的脚本把会话挂死）
const MAX_WALL_MS: u64 = 30 * 60 * 1000;

/// QuickJS 堆上限：超出时脚本内抛 `InternalError: out of memory`。
const MEMORY_LIMIT_BYTES: usize = 256 * 1024 * 1024;

/// `store()` 单值 JSON 文本上限
const MAX_STORE_VALUE_CHARS: usize = 256 * 1024;

/// `store()` 全部值 JSON 文本上限
const MAX_STORE_TOTAL_CHARS: usize = 1024 * 1024;

/// 脚本输出（`text()` / `image()` / `console.*`）的字符数上限
const MAX_OUTPUT_CHARS: usize = 16 * 1024 * 1024;

/// 脚本输出条目数上限
const MAX_OUTPUT_ITEMS: usize = 100_000;

/// `image()` 参数不合法时的统一提示
const IMAGE_HELPER_EXPECTS: &str = "image expects a non-empty image URL string, an object with image_url, or a raw MCP image block";

/// 嵌套调用/全局调用的宿主实现（由 `codemode` 扩展提供，落到 `ctx.execute_tool`）。
pub trait ScriptHost: Send + Sync + 'static {
    /// 调一个工具：`Ok` 成为脚本里 `await tools.x()` 的值；`Err` 的消息成为抛出的 Error。
    fn call_tool(
        &self,
        name: String,
        args: Option<Value>,
    ) -> BoxFuture<'static, Result<Value, String>>;

    /// 调一个全局：`Ok` 成为 `await searchTools(...)` 等的值。
    fn call_global(
        &self,
        name: String,
        args: Vec<Value>,
    ) -> BoxFuture<'static, Result<Value, String>>;
}

/// 沙箱里脚本可调的一个工具。
#[derive(Debug, Clone)]
pub struct ScriptTool {
    /// 工具真名（宿主按它分发）
    pub name: String,
    /// 脚本里用的 JS 标识符（[`super::declarations::to_codemode_identifier`] 的结果）
    pub js_name: String,
    /// 进 `ALL_TOOLS` 的说明（通常是完整声明样例）
    pub description: String,
}

/// 沙箱里的一个全局（`searchTools` / `describeTool` / `models.*`）。
#[derive(Debug, Clone)]
pub struct ScriptGlobal {
    /// 全局名；含 `.` 时（`models.classify`）会归组成命名空间对象
    pub name: String,
    /// true = 把全部实参当数组传给宿主（`searchTools(query, opts)`）；false = 只传第一个实参
    pub spread: bool,
}

/// 脚本输出条目（`text()` / `console.*` / `image()` / 顶层 `return`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptOutput {
    /// 文本条目
    Text(String),
    /// 图片条目：base64 数据 + mime
    Image {
        /// base64 编码的图片数据（不含 `data:` 前缀）
        data: String,
        /// mime 类型（如 `image/png`）
        mime_type: String,
    },
}

/// 脚本失败的类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// 脚本抛错或语法错误（`name`/`message`/`stack` 来自脚本的错误对象）
    Script,
    /// 超过 `timeout_ms` 或兜底墙钟
    Timeout,
    /// 调用方取消（父级中断）
    Aborted,
    /// 沙箱自身失败（VM 出错等，非脚本可控）
    Sandbox,
}

/// 脚本错误详情。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScriptError {
    /// 错误类型名（如 `TypeError`）
    pub name: String,
    /// 错误消息
    pub message: String,
    /// 带帧的完整文本（QuickJS 的 stack 只有帧，已由 prelude 补上 "Name: message" 头）
    pub stack: Option<String>,
}

/// 一次脚本运行的结局（无论成败都带回已产生的输出与 `store()` 写入）。
#[derive(Debug, Clone)]
pub struct ScriptOutcome {
    /// 脚本是否成功结束
    pub ok: bool,
    /// 顶层 `return` 的值（`exit()` 与无返回值时为 None）
    pub value: Option<Value>,
    /// 按产生顺序的输出条目
    pub output: Vec<ScriptOutput>,
    /// `store()` 的写入：`Some(json)` = 设值，`None` = 删除（只在成功时可信）
    pub store_writes: BTreeMap<String, Option<Value>>,
    /// 失败详情；`ok` 为 true 时为 None
    pub error: Option<ScriptError>,
    /// 失败类型；`ok` 为 true 时无意义
    pub error_kind: ErrorKind,
}

/// 一次脚本运行的输入。
pub struct SandboxRequest {
    /// 已剥掉 `// @options:` 行的脚本源码（行号与用户输入一致）
    pub code: String,
    /// 脚本可调的工具表
    pub tools: Vec<ScriptTool>,
    /// 脚本可见的全局
    pub globals: Vec<ScriptGlobal>,
    /// `load()` 的初始快照：key → 值
    pub store: BTreeMap<String, Value>,
    /// `// @options:` 里的 `timeout_ms`；None = 不限（只受 [`MAX_WALL_MS`] 约束）
    pub timeout_ms: Option<u64>,
    /// 父级取消标志（`ToolExecCtx::parent_abort`）
    pub cancel: Arc<AtomicBool>,
}

/// 宿主桥费的一次调用（JS 侧 `__call` 入队）。
struct Dispatch {
    /// JS 侧分配的调用 id（结算时原样交回）
    id: u64,
    /// `"call"`（工具）或 `"global"`（全局）
    kind: String,
    /// 工具名或全局名
    name: String,
    /// 实参的 JSON 文本；空串 = 脚本传了 `undefined`
    args: String,
}

/// `__done` 交给宿主的一次结算。
struct Done {
    /// 是否成功
    ok: bool,
    /// 成功时是返回值 JSON（空串 = undefined）；失败时是错误对象 JSON
    payload: String,
    /// 成功时的 `store()` 写入数组 JSON
    writes: String,
}

/// 跑一次脚本。脚本失败不返回 `Err`（都进 [`ScriptOutcome`]）。
pub async fn run(request: SandboxRequest, host: Arc<dyn ScriptHost>) -> ScriptOutcome {
    match run_inner(request, host).await {
        Ok(outcome) => outcome,
        Err(err) => ScriptOutcome {
            ok: false,
            value: None,
            output: Vec::new(),
            store_writes: BTreeMap::new(),
            error: Some(err),
            error_kind: ErrorKind::Sandbox,
        },
    }
}

/// 建运行时并注入宿主函数，再跑脚本；沙箱级失败返回 `Err`。
async fn run_inner(
    request: SandboxRequest,
    host: Arc<dyn ScriptHost>,
) -> Result<ScriptOutcome, ScriptError> {
    let SandboxRequest {
        code,
        tools,
        globals,
        store,
        timeout_ms,
        cancel,
    } = request;

    let start = Instant::now();
    let deadline = timeout_ms.map(Duration::from_millis);
    let hard_limit = Duration::from_millis(
        timeout_ms
            .map(|ms| ms.min(MAX_WALL_MS))
            .unwrap_or(MAX_WALL_MS),
    );

    let hard = hard_limit;
    let cancel_h = cancel.clone();
    let rt = AsyncRuntime::new().map_err(|e| sandbox_error(format!("codemode runtime: {e}")))?;
    rt.set_interrupt_handler(Some(Box::new(move || {
        cancel_h.load(Ordering::Relaxed) || start.elapsed() > hard
    })))
    .await;
    rt.set_memory_limit(MEMORY_LIMIT_BYTES).await;

    let ctx = AsyncContext::full(&rt)
        .await
        .map_err(|e| sandbox_error(format!("codemode context: {e}")))?;

    let notify = Arc::new(Notify::new());
    let dispatch_q: Arc<Mutex<Vec<Dispatch>>> = Arc::new(Mutex::new(Vec::new()));
    let outputs: Arc<Mutex<Vec<ScriptOutput>>> = Arc::new(Mutex::new(Vec::new()));
    let (done_tx, mut done_rx) = unbounded_channel::<Done>();
    let (settle_tx, mut settle_rx) = unbounded_channel::<(u64, bool, String)>();

    let tools_json = serde_json::to_string(
        &tools
            .iter()
            .map(|t| json!({"name": t.name, "jsName": t.js_name, "description": t.description}))
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| "[]".to_string());

    let globals_json = serde_json::to_string(
        &globals
            .iter()
            .map(|g| json!({"name": g.name, "spread": g.spread}))
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| "[]".to_string());

    let store_json = serde_json::to_string(
        &store
            .iter()
            .map(|(k, v)| (k.clone(), v.to_string()))
            .collect::<BTreeMap<_, _>>(),
    )
    .unwrap_or_else(|_| "{}".to_string());

    let limits_json = json!({
        "maxStoreValueChars": MAX_STORE_VALUE_CHARS,
        "maxStoreTotalChars": MAX_STORE_TOTAL_CHARS,
        "maxOutputChars": MAX_OUTPUT_CHARS,
        "maxOutputItems": MAX_OUTPUT_ITEMS,
        "imageHelperExpects": IMAGE_HELPER_EXPECTS,
    })
    .to_string();

    ctx.async_with(async move |ctx| -> Result<ScriptOutcome, ScriptError> {
        // 沙箱级失败也要按「取消 / 超时 / 真沙箱错误」归类：中断处理器打断 JS 时，
        // 错误会以 `InternalError: interrupted` 的形式冒出来。
        macro_rules! or_fail {
            ($e:expr, $kind:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(err) => {
                        return Ok(guard_failure(
                            &outputs, &cancel, start, hard_limit, err, $kind,
                        ));
                    }
                }
            };
        }

        or_fail!(
            install_host_globals(&ctx, &dispatch_q, &notify, &outputs, done_tx)
                .map_err(sandbox_error),
            ErrorKind::Sandbox
        );

        let api = or_fail!(
            eval_glue(&ctx, &tools_json, &globals_json, &store_json, &limits_json),
            ErrorKind::Sandbox
        );

        // 语法错误/求值期抛错都算脚本失败（中断处理器打断时会被上面的取消/超时分支接管）。
        or_fail!(run_script_body(&ctx, &api, &code), ErrorKind::Script);

        let mut inflight: usize = 0;
        let mut idle_ticks: u32 = 0;

        loop {
            if cancel.load(Ordering::Relaxed) {
                return Ok(aborted_outcome(&outputs));
            }

            if start.elapsed() > hard_limit {
                return Ok(timeout_outcome(&outputs, hard_limit));
            }

            // 先把这一轮 JS 产生的调用派发出去（并发跑，结果经 settle 回灌）。
            let pending: Vec<Dispatch> = {
                let mut q = dispatch_q.lock().unwrap();
                std::mem::take(&mut *q)
            };

            for d in pending {
                inflight += 1;
                let host = host.clone();
                let tx = settle_tx.clone();
                tokio::spawn(async move {
                    let (ok, payload) = dispatch(&*host, &d).await;
                    _ = tx.send((d.id, ok, payload));
                });
            }

            let tick = TICK.min(hard_limit.saturating_sub(start.elapsed()));
            tokio::select! {
                done = done_rx.recv() => {
                    if let Some(done) = done {
                        return Ok(finish(done, &outputs));
                    }
                }
                Some((id, ok, payload)) = settle_rx.recv() => {
                    inflight = inflight.saturating_sub(1);
                    let settle: Function = ctx
                        .globals()
                        .get("__settle")
                        .map_err(|e| sandbox_error(format!("get __settle: {e}")))?;

                    if let Err(e) = settle.call::<_, ()>((id, ok, payload)) {
                        return Ok(guard_failure(
                            &outputs,
                            &cancel,
                            start,
                            hard_limit,
                            sandbox_error(format!("__settle failed: {e}")),
                            ErrorKind::Sandbox,
                        ));
                    }
                    idle_ticks = 0;
                    continue;
                }
                _ = notify.notified() => {}
                _ = tokio::time::sleep(tick) => {}
            }

            // 空转判定：JS 微任务已全部跑完（每次 poll 都会推进），既没有在飞的调用、
            // 也没有新调度，脚本就卡在一个永远不会 settle 的 Promise 上。
            if inflight == 0 && dispatch_q.lock().unwrap().is_empty() && done_rx.is_empty() {
                idle_ticks += 1;
                if idle_ticks >= 2 {
                    return Ok(stalled_outcome(&outputs));
                }
            } else {
                idle_ticks = 0;
            }

            if let Some(d) = deadline
                && start.elapsed() > d
            {
                return Ok(timeout_outcome(&outputs, d));
            }
        }
    })
    .await
}

/// 注入 `__call` / `__output` / `__done` 三个宿主函数。
fn install_host_globals(
    ctx: &Ctx<'_>,
    dispatch_q: &Arc<Mutex<Vec<Dispatch>>>,
    notify: &Arc<Notify>,
    outputs: &Arc<Mutex<Vec<ScriptOutput>>>,
    done_tx: UnboundedSender<Done>,
) -> Result<(), String> {
    let q = dispatch_q.clone();
    let n = notify.clone();
    let call = Function::new(
        ctx.clone(),
        move |id: u64, kind: String, name: String, args: String| {
            q.lock().unwrap().push(Dispatch {
                id,
                kind,
                name,
                args,
            });
            n.notify_one();
        },
    )
    .map_err(|e| format!("inject __call: {e}"))?;

    ctx.globals()
        .set("__call", call)
        .map_err(|e| format!("set __call: {e}"))?;

    let out = outputs.clone();
    let output = Function::new(ctx.clone(), move |kind: String, a: String, b: String| {
        let item = if kind == "image" {
            ScriptOutput::Image {
                data: a,
                mime_type: b,
            }
        } else {
            ScriptOutput::Text(a)
        };
        out.lock().unwrap().push(item);
    })
    .map_err(|e| format!("inject __output: {e}"))?;

    ctx.globals()
        .set("__output", output)
        .map_err(|e| format!("set __output: {e}"))?;

    let done = Function::new(
        ctx.clone(),
        move |ok: bool, payload: String, writes: String| {
            _ = done_tx.send(Done {
                ok,
                payload,
                writes,
            });
        },
    )
    .map_err(|e| format!("inject __done: {e}"))?;

    ctx.globals()
        .set("__done", done)
        .map_err(|e| format!("set __done: {e}"))?;

    Ok(())
}

/// 求值 JS 预置并返回 `{settle, run, stalled}` 这三件（`__settle` 已由预置装到 globalThis）。
fn eval_glue<'js>(
    ctx: &Ctx<'js>,
    tools_json: &str,
    globals_json: &str,
    store_json: &str,
    limits_json: &str,
) -> Result<Object<'js>, ScriptError> {
    let mut options = rquickjs::context::EvalOptions::default();
    options.strict = true;
    options.filename = Some("codemode-glue.js".to_string());

    let factory: Function = ctx
        .eval_with_options(GLUE.as_bytes(), options)
        .map_err(|e| caught_js_error(ctx, e, "codemode sandbox failed to start"))?;

    factory
        .call::<_, Object>((tools_json, globals_json, store_json, limits_json))
        .map_err(|e| caught_js_error(ctx, e, "codemode sandbox failed to start"))
}

/// 把脚本体包成 `async (tools, console) => { ... }` 并交给预置的 `run()`。
fn run_script_body<'js>(ctx: &Ctx<'js>, api: &Object<'js>, code: &str) -> Result<(), ScriptError> {
    let source = format!("(async (tools, console) => {{{code}\n}})");
    let mut options = rquickjs::context::EvalOptions::default();
    options.strict = false;
    options.filename = Some("codemode.js".to_string());

    let body: Function = ctx
        .eval_with_options(source.into_bytes(), options)
        .map_err(|e| caught_js_error(ctx, e, "Script failed to parse"))?;

    let run: Function = api
        .get("run")
        .map_err(|e| sandbox_error(format!("get run: {e}")))?;

    run.call::<_, ()>((body,))
        .map_err(|e| caught_js_error(ctx, e, "Script failed to start"))
}

/// 处理一次 JS 侧调用，产出交回 JS 的 `(ok, payload)`。
async fn dispatch(host: &dyn ScriptHost, d: &Dispatch) -> (bool, String) {
    let args: Option<Value> = if d.args.is_empty() {
        None
    } else {
        match serde_json::from_str(&d.args) {
            Ok(v) => Some(v),
            Err(e) => return (false, format!("invalid arguments from the script: {e}")),
        }
    };

    let result = if d.kind == "global" {
        let args = match args {
            Some(Value::Array(items)) => items,
            Some(other) => vec![other],
            None => Vec::new(),
        };
        host.call_global(d.name.clone(), args).await
    } else {
        host.call_tool(d.name.clone(), args).await
    };

    match result {
        Ok(value) => (true, value.to_string()),
        Err(message) => (false, message),
    }
}

/// 成功/失败的 `__done` 载荷 → 结局。
fn finish(done: Done, outputs: &Arc<Mutex<Vec<ScriptOutput>>>) -> ScriptOutcome {
    let output = take_outputs(outputs);
    if !done.ok {
        let parsed: Value = serde_json::from_str(&done.payload).unwrap_or(Value::Null);
        let name = parsed
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("Error")
            .to_string();
        let message = parsed
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("script failed")
            .to_string();
        let stack = parsed
            .get("stack")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        return ScriptOutcome {
            ok: false,
            value: None,
            output,
            store_writes: BTreeMap::new(),
            error: Some(ScriptError {
                name,
                message,
                stack,
            }),
            error_kind: ErrorKind::Script,
        };
    }

    let value = if done.payload.is_empty() {
        None
    } else {
        serde_json::from_str(&done.payload).ok()
    };

    let mut writes: BTreeMap<String, Option<Value>> = BTreeMap::new();
    if !done.writes.is_empty()
        && let Ok(entries) = serde_json::from_str::<Vec<Value>>(&done.writes)
    {
        for entry in entries {
            let Some(items) = entry.as_array() else {
                continue;
            };
            let Some(key) = items.first().and_then(|v| v.as_str()) else {
                continue;
            };
            // 写入值以 JSON 文本过桥，这里再解析回结构化的值。
            let value = items.get(1).and_then(|v| v.as_str()).map(|json| {
                serde_json::from_str(json).unwrap_or_else(|_| Value::String(json.to_string()))
            });
            writes.insert(key.to_string(), value);
        }
    }

    ScriptOutcome {
        ok: true,
        value,
        output,
        store_writes: writes,
        error: None,
        error_kind: ErrorKind::Script,
    }
}

/// 取走已收集的输出条目。
fn take_outputs(outputs: &Arc<Mutex<Vec<ScriptOutput>>>) -> Vec<ScriptOutput> {
    std::mem::take(&mut *outputs.lock().unwrap())
}

/// 取消：`Aborted`。
fn aborted_outcome(outputs: &Arc<Mutex<Vec<ScriptOutput>>>) -> ScriptOutcome {
    ScriptOutcome {
        ok: false,
        value: None,
        output: take_outputs(outputs),
        store_writes: BTreeMap::new(),
        error: Some(ScriptError {
            name: "Error".to_string(),
            message: "Execution aborted".to_string(),
            stack: None,
        }),
        error_kind: ErrorKind::Aborted,
    }
}

/// 超时：`Timeout`（`limit` 是实际生效的上限，毫秒）。
fn timeout_outcome(outputs: &Arc<Mutex<Vec<ScriptOutput>>>, limit: Duration) -> ScriptOutcome {
    ScriptOutcome {
        ok: false,
        value: None,
        output: take_outputs(outputs),
        store_writes: BTreeMap::new(),
        error: Some(ScriptError {
            name: "Error".to_string(),
            message: format!("Execution timed out after {} ms", limit.as_millis()),
            stack: None,
        }),
        error_kind: ErrorKind::Timeout,
    }
}

/// 空转（等一个永远不会 settle 的 Promise）
fn stalled_outcome(outputs: &Arc<Mutex<Vec<ScriptOutput>>>) -> ScriptOutcome {
    ScriptOutcome {
        ok: false,
        value: None,
        output: take_outputs(outputs),
        store_writes: BTreeMap::new(),
        error: Some(ScriptError {
            name: "Error".to_string(),
            message:
                "The script is waiting on a promise that can never settle: no tool call is pending, and timers do not exist here."
                    .to_string(),
            stack: None,
        }),
        error_kind: ErrorKind::Script,
    }
}

/// 沙箱级失败按当前状态归类：取消标志 → `Aborted`，超过墙钟/脚本超时 → `Timeout`，
/// 其余才是 `Sandbox`（中断处理器打断 JS 时错误文案是 `InternalError: interrupted`）。
fn guard_failure(
    outputs: &Arc<Mutex<Vec<ScriptOutput>>>,
    cancel: &Arc<AtomicBool>,
    start: Instant,
    hard_limit: Duration,
    error: ScriptError,
    default_kind: ErrorKind,
) -> ScriptOutcome {
    if cancel.load(Ordering::Relaxed) {
        return aborted_outcome(outputs);
    }

    if start.elapsed() > hard_limit {
        return timeout_outcome(outputs, hard_limit);
    }

    ScriptOutcome {
        ok: false,
        value: None,
        output: take_outputs(outputs),
        store_writes: BTreeMap::new(),
        error: Some(error),
        error_kind: default_kind,
    }
}

/// 构造一个沙箱级错误。
fn sandbox_error(message: String) -> ScriptError {
    ScriptError {
        name: "Error".to_string(),
        message,
        stack: None,
    }
}

/// 把 JS 抛出的异常转成 [`ScriptError`]（读 `catch()` 的异常对象；缺字段时用 `err` 的文案）。
fn caught_js_error(ctx: &Ctx<'_>, err: rquickjs::Error, fallback: &str) -> ScriptError {
    let caught = ctx.catch();
    let obj = caught.as_object();
    let name = obj
        .and_then(|o| o.get::<_, String>("name").ok())
        .unwrap_or_else(|| "Error".to_string());
    let message = obj
        .and_then(|o| o.get::<_, String>("message").ok())
        .unwrap_or_else(|| format!("{fallback}: {err}"));
    let stack = obj.and_then(|o| o.get::<_, String>("stack").ok());
    ScriptError {
        name,
        message,
        stack,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// 记录调用并按名字返回固定结果的测试宿主。
    struct FakeHost {
        /// `(kind, name, args)` 的调用流水
        seen: Mutex<Vec<(String, String, Value)>>,
    }

    impl ScriptHost for FakeHost {
        fn call_tool(
            &self,
            name: String,
            args: Option<Value>,
        ) -> BoxFuture<'static, Result<Value, String>> {
            let args = args.unwrap_or(Value::Null);
            self.seen
                .lock()
                .unwrap()
                .push(("call".to_string(), name.clone(), args.clone()));
            Box::pin(async move {
                match name.as_str() {
                    "boom" => Err("tool exploded".to_string()),
                    "echo" => Ok(args),
                    other => Ok(json!(format!("{other}-ok"))),
                }
            })
        }

        fn call_global(
            &self,
            name: String,
            args: Vec<Value>,
        ) -> BoxFuture<'static, Result<Value, String>> {
            self.seen
                .lock()
                .unwrap()
                .push(("global".to_string(), name.clone(), json!(args)));
            Box::pin(async move { Ok(json!(format!("{name}!"))) })
        }
    }

    /// 跑一段脚本：工具表固定为 `echo`/`boom`/`plain`。
    async fn run_script(
        code: &str,
        store: BTreeMap<String, Value>,
    ) -> (ScriptOutcome, Arc<FakeHost>) {
        let host = Arc::new(FakeHost {
            seen: Mutex::new(Vec::new()),
        });
        let request = SandboxRequest {
            code: code.to_string(),
            tools: ["echo", "boom", "plain"]
                .iter()
                .map(|n| ScriptTool {
                    name: n.to_string(),
                    js_name: super::super::declarations::to_codemode_identifier(n),
                    description: format!("{n} tool"),
                })
                .collect(),
            globals: vec![
                ScriptGlobal {
                    name: "searchTools".to_string(),
                    spread: true,
                },
                ScriptGlobal {
                    name: "models.generateImages".to_string(),
                    spread: true,
                },
            ],
            store,
            timeout_ms: Some(10_000),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let outcome = run(request, host.clone()).await;
        (outcome, host)
    }

    /// 脚本能调工具、拿到返回值，并顶层 `return`。
    #[tokio::test]
    async fn calls_tools_and_returns_value() {
        let (out, host) = run_script(
            "const a = await tools.plain({x: 1});\nreturn a + '!';",
            BTreeMap::new(),
        )
        .await;
        assert!(out.ok, "{out:?}");
        assert_eq!(out.value, Some(json!("plain-ok!")));
        assert_eq!(host.seen.lock().unwrap().len(), 1);
    }

    /// 2.6：读不存在的 `tools` / 命名空间成员时报错并点名近似名；`in` 探测与原型链成员不受影响。
    #[tokio::test]
    async fn missing_tool_member_names_close_matches() {
        async fn attempt(expression: &str) -> String {
            let (out, _) = run_script(&format!("return {expression};"), BTreeMap::new()).await;
            if out.ok {
                out.value.map(|v| v.to_string()).unwrap_or_default()
            } else {
                out.error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .unwrap_or_default()
            }
        }

        // 大小写/非字母数字不敏感地给出近似名
        assert_eq!(
            attempt("tools.Echo").await,
            "tools.Echo does not exist. Did you mean tools.echo? ALL_TOOLS lists every tool; searchTools(query) finds tools by topic. Check for a member with \"Echo\" in tools."
        );
        // 成员数不超过 20 时列出全部
        assert!(
            attempt("tools.nothing")
                .await
                .contains("Available: echo, boom, plain.")
        );
        // `in` 探测、原型链成员、JSON 化照旧
        assert_eq!(
            attempt("['echo' in tools, 'nothing' in tools, String(tools.toString), JSON.stringify(tools)]").await,
            "[true,false,\"undefined\",\"{}\"]"
        );
        // 全局命名空间同样守护：`models` 下不存在的成员给出近似名
        assert!(
            attempt("models.generateImage").await.contains(
                "models.generateImage does not exist. Did you mean models.generateImages?"
            )
        );
    }

    /// 工具失败 → 脚本里的 Error 携带工具错误文本，脚本失败但保留输出。
    #[tokio::test]
    async fn tool_error_rejects_in_script() {
        let (out, _) = run_script(
            "text('before');\ntry { await tools.boom(); } catch (e) { text(e.message); }\n",
            BTreeMap::new(),
        )
        .await;
        assert!(out.ok, "{out:?}");
        assert_eq!(
            out.output,
            vec![
                ScriptOutput::Text("before".to_string()),
                ScriptOutput::Text("tool exploded".to_string())
            ]
        );
    }

    /// `text()` / `console.log` / `image()` 的输出条目与顺序；顶层 `return` 追加为条目（由扩展侧做）。
    #[tokio::test]
    async fn collects_output_items() {
        let (out, _) = run_script(
            "text('a');\nconsole.log('b', 2);\nimage('data:image/png;base64,iVBORw0KGgo=');\n",
            BTreeMap::new(),
        )
        .await;
        assert!(out.ok, "{out:?}");
        assert_eq!(
            out.output,
            vec![
                ScriptOutput::Text("a".to_string()),
                ScriptOutput::Text("b 2".to_string()),
                ScriptOutput::Image {
                    data: "iVBORw0KGgo=".to_string(),
                    mime_type: "image/png".to_string()
                }
            ]
        );
    }

    /// `image()`：按签名推导 MIME、剥掉换行；非法 base64 或非支持格式 → `TypeError`。
    #[tokio::test]
    async fn image_output_validates_base64_and_signature() {
        // 换行包裹的 PNG base64 被剥离；声明的 jpeg 被忽略，MIME 由签名决定
        let (out, _) = run_script(
            "image('data:image/jpeg;base64,iVBO\\nRw0KGgo=');",
            BTreeMap::new(),
        )
        .await;
        assert!(out.ok, "{out:?}");
        assert_eq!(
            out.output,
            vec![ScriptOutput::Image {
                data: "iVBORw0KGgo=".to_string(),
                mime_type: "image/png".to_string(),
            }]
        );

        // 非法 base64（长度不是 4 的倍数）
        let (out, _) = run_script("image('data:image/png;base64,AAAAA');", BTreeMap::new()).await;
        assert!(!out.ok, "{out:?}");
        let err = out.error.unwrap();
        assert_eq!(err.name, "TypeError");
        assert!(err.message.contains("not valid base64"), "{err:?}");

        // 合法 base64 但不是 PNG/JPEG/GIF/WebP
        let (out, _) =
            run_script("image('data:image/png;base64,QUJDRA==');", BTreeMap::new()).await;
        assert!(!out.ok, "{out:?}");
        let err = out.error.unwrap();
        assert_eq!(err.name, "TypeError");
        assert!(
            err.message.contains("not a PNG, JPEG, GIF, or WebP"),
            "{err:?}"
        );
    }

    /// 输出超过字符上限 → `RangeError`，脚本失败（对齐 pi #10283）。
    #[tokio::test]
    async fn output_limit_is_enforced() {
        let (out, _) = run_script("text('x'.repeat(16777217));", BTreeMap::new()).await;
        assert!(!out.ok, "{out:?}");
        let err = out.error.unwrap();
        assert_eq!(err.name, "RangeError");
        assert!(err.message.contains("script output exceeded"), "{err:?}");
    }

    /// `store()` / `load()`：快照可见、写入回报。
    #[tokio::test]
    async fn store_and_load_round_trip() {
        let mut store = BTreeMap::new();
        store.insert("k".to_string(), json!(41));
        let (out, _) = run_script(
            "const v = load('k');\nstore('k', v + 1);\nstore('gone', undefined);\nreturn v;",
            store,
        )
        .await;
        assert!(out.ok, "{out:?}");
        assert_eq!(out.value, Some(json!(41)));
        assert_eq!(out.store_writes.get("k"), Some(&Some(json!(42))));
        assert_eq!(out.store_writes.get("gone"), Some(&None));
    }

    /// `store()` 超限时报错文案说清 store 的用途（对齐 pi 的 STORE_HINT）。
    #[tokio::test]
    async fn store_limits_explain_what_the_store_is_for() {
        let value = "x".repeat(MAX_STORE_VALUE_CHARS);
        let (out, _) = run_script(
            &format!(
                "try {{ store('big', '{value}'); }} catch (e) {{ text(e.name + ': ' + e.message); }}"
            ),
            BTreeMap::new(),
        )
        .await;
        assert!(out.ok, "{out:?}");
        let ScriptOutput::Text(text) = &out.output[0] else {
            panic!("got {:?}", out.output);
        };
        assert!(
            text.starts_with("RangeError: store(\"big\") value has "),
            "{text}"
        );
        assert!(
            text.contains("store() is for small state such as IDs or summaries"),
            "{text}"
        );
    }

    /// 全局以 spread 方式收全部实参。
    #[tokio::test]
    async fn globals_receive_all_arguments() {
        let (out, host) = run_script(
            "return await searchTools('q', {limit: 3});",
            BTreeMap::new(),
        )
        .await;
        assert!(out.ok, "{out:?}");
        assert_eq!(out.value, Some(json!("searchTools!")));
        assert_eq!(
            host.seen.lock().unwrap()[0],
            (
                "global".to_string(),
                "searchTools".to_string(),
                json!(["q", {"limit": 3}])
            )
        );
    }

    /// 脚本抛错 → `Script` 类型 + 错误名/消息/栈。
    #[tokio::test]
    async fn script_throw_reports_error() {
        let (out, _) = run_script("throw new TypeError('nope');", BTreeMap::new()).await;
        assert!(!out.ok);
        assert_eq!(out.error_kind, ErrorKind::Script);
        let err = out.error.unwrap();
        assert_eq!(err.name, "TypeError");
        assert_eq!(err.message, "nope");
        assert!(err.stack.unwrap().starts_with("TypeError: nope"));
    }

    /// 语法错误也算 `Script`（不是沙箱失败）。
    #[tokio::test]
    async fn syntax_error_is_a_script_failure() {
        let (out, _) = run_script("const = ;", BTreeMap::new()).await;
        assert!(!out.ok);
        assert_eq!(out.error_kind, ErrorKind::Script);
    }

    /// 等一个永远不会 settle 的 Promise → 卡死判定（文案与 pi 一致）。
    #[tokio::test]
    async fn stalled_promise_is_reported() {
        let (out, _) = run_script("await new Promise(() => {});", BTreeMap::new()).await;
        assert!(!out.ok);
        assert_eq!(out.error_kind, ErrorKind::Script);
        let message = out.error.clone().unwrap().message;
        assert!(message.contains("promise that can never settle"), "{out:?}");
    }

    /// `exit()` 立刻成功结束（返回值 None）。
    #[tokio::test]
    async fn exit_finishes_successfully() {
        let (out, _) = run_script("text('a');\nexit();\ntext('b');", BTreeMap::new()).await;
        assert!(out.ok, "{out:?}");
        assert_eq!(out.value, None);
        assert_eq!(out.output, vec![ScriptOutput::Text("a".to_string())]);
    }

    /// 超时 → `Timeout`。
    #[tokio::test]
    async fn timeout_is_reported() {
        let host = Arc::new(FakeHost {
            seen: Mutex::new(Vec::new()),
        });
        let request = SandboxRequest {
            code: "await tools.plain({});\nawait new Promise(() => {});".to_string(),
            tools: vec![ScriptTool {
                name: "plain".to_string(),
                js_name: "plain".to_string(),
                description: String::new(),
            }],
            globals: Vec::new(),
            store: BTreeMap::new(),
            timeout_ms: Some(1),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let out = run(request, host).await;
        assert!(!out.ok);
        assert!(
            matches!(out.error_kind, ErrorKind::Timeout | ErrorKind::Script),
            "{out:?}"
        );
    }

    /// 取消标志已置位 → `Aborted`。
    #[tokio::test]
    async fn cancel_flag_aborts() {
        let host = Arc::new(FakeHost {
            seen: Mutex::new(Vec::new()),
        });
        let cancel = Arc::new(AtomicBool::new(true));
        let request = SandboxRequest {
            code: "await new Promise(() => {});".to_string(),
            tools: Vec::new(),
            globals: Vec::new(),
            store: BTreeMap::new(),
            timeout_ms: None,
            cancel,
        };
        let out = run(request, host).await;
        assert!(!out.ok);
        assert_eq!(out.error_kind, ErrorKind::Aborted);
    }

    /// 并发：`Promise.all` 的多次调用并发发出。
    #[tokio::test]
    async fn concurrent_calls_run() {
        let (out, host) = run_script(
            "const all = await Promise.all([tools.plain({}), tools.echo({a: 1}), tools.plain({})]);\nreturn all;",
            BTreeMap::new(),
        )
        .await;
        assert!(out.ok, "{out:?}");
        assert_eq!(out.value, Some(json!(["plain-ok", {"a": 1}, "plain-ok"])));
        assert_eq!(host.seen.lock().unwrap().len(), 3);
    }

    /// `tools` 表冻结：脚本覆盖不掉，且原工具仍可调。
    #[tokio::test]
    async fn tools_table_is_frozen() {
        let (out, _) = run_script(
            "tools.plain = 1;\nreturn [Object.isFrozen(tools), typeof tools.plain, await tools.plain({})];",
            BTreeMap::new(),
        )
        .await;
        assert_eq!(out.value, Some(json!([true, "function", "plain-ok"])));
    }
}
