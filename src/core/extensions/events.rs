//! 扩展↔扩展事件总线（契约见 `migration-subagents.md` §5.17）。
//!
//! 核心只提供**按字符串通道名广播 `(name, payload)`** 的哑总线：
//!
//! - 订阅 = 扩展在 `hooks()` 里声明 [`ExtensionHook::ExtensionEvent`]；
//! - 收发都受**扩展生命周期**门控（`registered()` = 已启用 + 当前模式可用）；
//! - 发送方必须自报 [`Extension::name`]，核心校验它是"已注册且启用"的扩展（未启用的扩展不能借别人的名字发）；
//! - `payload` 是 [`Value`]；总线**不认识** `requestId`/成功失败信封——那是扩展之间的线协议。
//!
//! # 重入
//!
//! handler 内再 `emit` 时事件进队列，等**当前 dispatch 返回后**按 FIFO drain；
//! handler 永不嵌在另一个 handler 里跑（锁序固定 `registry → 扩展 state`）。
//! 非 handler 上下文（后台任务）里 `emit` 立即投递。一次连锁有上限（[`MAX_CHAIN`]）防事件风暴。

use super::{ExtensionHook, registered};
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
};

/// 一次连锁最多投递的事件数（超出即丢弃剩余，防事件风暴）。
pub const MAX_CHAIN: usize = 1024;

thread_local! {
    /// 本线程待投递队列（重入时先入队，外层 drain 消费）。
    static QUEUE: RefCell<VecDeque<(String, Value)>> = const { RefCell::new(VecDeque::new()) };
    /// 本线程是否正在 drain（重入检测）。
    static DRAINING: Cell<bool> = const { Cell::new(false) };
}

/// RAII：保证 handler panic / 提前返回时 `DRAINING` 复位、队列不死锁。
struct DrainGuard;
impl Drop for DrainGuard {
    /// 复位线程本地的 `DRAINING` 标志，使 drain 过程中的 panic/提前返回
    /// 不会让队列永久卡在“正在 drain”状态。
    fn drop(&mut self) {
        DRAINING.with(|d| d.set(false));
    }
}

/// 广播一个扩展事件。
///
/// `from` 是发送方扩展名（通常 `self.name()`）：核心校验它是已注册且启用的扩展，
/// 否则本调用被忽略（未启用的扩展不许发）。发送方若订阅了也会收到自己的事件。
///
/// 在 handler 内调用会被**队列化**，待当前 dispatch 返回后投递（见模块文档）。
pub fn emit(from: &str, name: &str, payload: Value) {
    if !sender_is_live(from) {
        return;
    }
    QUEUE.with(|q| q.borrow_mut().push_back((name.to_string(), payload)));
    drain();
}

/// 把事件投递给所有已启用、当前模式可用且声明了
/// [`ExtensionHook::ExtensionEvent`] 的扩展（按注册顺序）。
///
/// 一般经 [`emit`] 调用（带发送方校验与重入队列）；直接调用会绕过发送方门控，
/// 仅供测试与核心自用。
pub fn dispatch(name: &str, payload: &Value) {
    for ext in registered() {
        if ext.hooks().contains(&ExtensionHook::ExtensionEvent) {
            ext.on_extension_event(name, payload);
        }
    }
}

/// 发送方是否为"已注册且启用"的扩展。
fn sender_is_live(from: &str) -> bool {
    registered().iter().any(|e| e.name() == from)
}

/// 取出队列并 FIFO 投递；重入时直接返回（外层 drain 会消费新入队的事件）。
fn drain() {
    if DRAINING.with(|d| d.get()) {
        return;
    }
    DRAINING.with(|d| d.set(true));
    let _guard = DrainGuard;

    let mut count = 0usize;
    loop {
        let next = QUEUE.with(|q| q.borrow_mut().pop_front());
        let Some((name, payload)) = next else {
            break;
        };
        count += 1;
        if count > MAX_CHAIN {
            // 事件风暴：丢弃剩余，避免无限连锁。
            QUEUE.with(|q| q.borrow_mut().clear());
            break;
        }
        dispatch(&name, &payload);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::{
        Extension, ExtensionTool, register_extension, unregister_extension,
    };
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    /// 收集事件的测试扩展；可选地在收到第一个事件时再 emit 一个。
    struct Collector {
        name: String,
        seen: Arc<Mutex<Vec<(String, Value)>>>,
        reemit: Option<(String, Value)>,
        reemitted: AtomicUsize,
    }

    impl Extension for Collector {
        fn name(&self) -> &str {
            &self.name
        }
        fn tools(&self) -> Vec<ExtensionTool> {
            Vec::new()
        }
        fn hooks(&self) -> Vec<ExtensionHook> {
            vec![ExtensionHook::ExtensionEvent]
        }
        fn on_extension_event(&self, name: &str, payload: &Value) {
            self.seen
                .lock()
                .unwrap()
                .push((name.to_string(), payload.clone()));
            if let Some((n, p)) = &self.reemit
                && self.reemitted.fetch_add(1, Ordering::SeqCst) == 0
            {
                emit(&self.name, n, p.clone());
            }
        }
    }

    /// 未声明 ExtensionEvent 的扩展：绝不应收到事件。
    struct NonSubscriber {
        seen: Arc<AtomicUsize>,
    }
    impl Extension for NonSubscriber {
        fn name(&self) -> &str {
            "non-subscriber"
        }
        fn tools(&self) -> Vec<ExtensionTool> {
            Vec::new()
        }
        fn on_extension_event(&self, _n: &str, _p: &Value) {
            self.seen.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn unique(tag: &str) -> String {
        format!("evt-test-{tag}-{}", std::process::id())
    }

    #[test]
    fn emit_reaches_subscribers_in_registration_order() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let a = unique("a");
        let b = unique("b");
        let seen_a = Arc::new(Mutex::new(Vec::new()));
        let seen_b = Arc::new(Mutex::new(Vec::new()));
        register_extension(Collector {
            name: a.clone(),
            seen: seen_a.clone(),
            reemit: None,
            reemitted: AtomicUsize::new(0),
        });
        register_extension(Collector {
            name: b.clone(),
            seen: seen_b.clone(),
            reemit: None,
            reemitted: AtomicUsize::new(0),
        });

        emit(&a, "topic", serde_json::json!({"n": 1}));

        assert_eq!(seen_a.lock().unwrap().len(), 1);
        assert_eq!(seen_b.lock().unwrap().len(), 1);
        assert_eq!(seen_b.lock().unwrap()[0].0, "topic");
        assert_eq!(seen_b.lock().unwrap()[0].1["n"], 1);

        unregister_extension(&a);
        unregister_extension(&b);
    }

    #[test]
    fn non_subscriber_never_receives() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let sender = unique("sender2");
        let count = Arc::new(AtomicUsize::new(0));
        register_extension(Collector {
            name: sender.clone(),
            seen: Arc::new(Mutex::new(Vec::new())),
            reemit: None,
            reemitted: AtomicUsize::new(0),
        });
        register_extension(NonSubscriber {
            seen: count.clone(),
        });

        emit(&sender, "topic", Value::Null);

        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "未声明 hook 的扩展不应收到"
        );
        unregister_extension(&sender);
        unregister_extension("non-subscriber");
    }

    #[test]
    fn unknown_sender_is_ignored() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sub = unique("sub3");
        register_extension(Collector {
            name: sub.clone(),
            seen: seen.clone(),
            reemit: None,
            reemitted: AtomicUsize::new(0),
        });

        emit("not-a-registered-extension", "topic", Value::Null);

        assert!(seen.lock().unwrap().is_empty(), "未知发送方不应广播");
        unregister_extension(&sub);
    }

    #[test]
    fn reentrant_emit_is_queued_not_nested() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let a = unique("a4");
        let b = unique("b4");
        let seen_a = Arc::new(Mutex::new(Vec::new()));
        let seen_b = Arc::new(Mutex::new(Vec::new()));
        // b 收到第一个事件后再发一个；a 与 b 都应看到第二个（说明队列被 drain）。
        register_extension(Collector {
            name: a.clone(),
            seen: seen_a.clone(),
            reemit: None,
            reemitted: AtomicUsize::new(0),
        });
        register_extension(Collector {
            name: b.clone(),
            seen: seen_b.clone(),
            reemit: Some(("second".to_string(), serde_json::json!({"k": 2}))),
            reemitted: AtomicUsize::new(0),
        });

        emit(&a, "first", Value::Null);

        // 两个事件都到达（第一个 + b 重入的第二个）
        let a_names: Vec<String> = seen_a
            .lock()
            .unwrap()
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        let b_names: Vec<String> = seen_b
            .lock()
            .unwrap()
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        assert_eq!(a_names, vec!["first", "second"]);
        assert_eq!(b_names, vec!["first", "second"]);

        unregister_extension(&a);
        unregister_extension(&b);
    }

    #[test]
    fn disabled_sender_cannot_emit() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let sub = unique("sub5");
        let seen = Arc::new(Mutex::new(Vec::new()));
        register_extension(Collector {
            name: sub.clone(),
            seen: seen.clone(),
            reemit: None,
            reemitted: AtomicUsize::new(0),
        });
        // 把 sub 禁用后，它不再是"已注册且启用"，因此借它名义发的事件被丢弃。
        crate::core::extensions::set_extension_enabled(&sub, false);
        emit(&sub, "topic", Value::Null);
        assert!(seen.lock().unwrap().is_empty(), "禁用扩展不应能广播");
        crate::core::extensions::set_extension_enabled(&sub, true);
        unregister_extension(&sub);
    }
}
