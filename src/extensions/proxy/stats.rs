//! proxy 网关的运行时统计（dock 面板 + 请求日志）。
//!
//! 计数器全是原子量，`dock_lines()` 每帧采集时**不取锁**；只有「速度滑动窗口」
//! 与「请求日志环形缓冲」各持一把短锁，避免拖慢 TUI 渲染。统计不落盘，服务器每次启动清零。

use crate::utils::time::now_ms;
use std::{
    collections::VecDeque,
    sync::{
        Mutex, MutexGuard, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// 速率滑动窗口长度：窗口内字节和 / 窗口秒数，空闲随即回落 0（不做累计平均）。
const WINDOW: Duration = Duration::from_secs(5);

/// 请求日志环形缓冲容量（面板只显示最近几条）
const LOG_CAP: usize = 50;

/// 当前活跃连接数（已接受、未关闭）
static CONNS_ACTIVE: AtomicU64 = AtomicU64::new(0);
/// 累计接受连接数（单调递增，不随关闭回退）
static CONNS_TOTAL: AtomicU64 = AtomicU64::new(0);
/// 在途请求数（已开始、未结束）
static REQUESTS_ACTIVE: AtomicU64 = AtomicU64::new(0);
/// 成功请求数（HTTP < 400）
static REQUESTS_OK: AtomicU64 = AtomicU64::new(0);
/// 失败请求数（HTTP >= 400，含客户端断连的 499）
static REQUESTS_FAILED: AtomicU64 = AtomicU64::new(0);
/// 累计入站字节（请求体）
static BYTES_IN: AtomicU64 = AtomicU64::new(0);
/// 累计出站字节（响应体 / SSE 帧）
static BYTES_OUT: AtomicU64 = AtomicU64::new(0);
/// 累计输入 token（上游 usage）
static TOKENS_IN: AtomicU64 = AtomicU64::new(0);
/// 累计输出 token（上游 usage）
static TOKENS_OUT: AtomicU64 = AtomicU64::new(0);
/// 服务器启动时刻（epoch ms）；0 = 未运行
static STARTED_AT: AtomicU64 = AtomicU64::new(0);

/// 取互斥锁；锁被 poison（持锁线程 panic）时取回内部数据，避免统计永久不可用。
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 滑动窗口：记录窗口内的字节增量样本，速率 = 窗口内总和 / 窗口秒数。
struct ByteWindow {
    /// `(采样时刻, 字节数)` 队列，读取时按 [`WINDOW`] 淘汰过期样本
    samples: Mutex<VecDeque<(Instant, u64)>>,
}

impl ByteWindow {
    /// 空的滑动窗口（无样本）。
    fn new() -> Self {
        Self {
            samples: Mutex::new(VecDeque::new()),
        }
    }

    /// 记录一次字节增量并淘汰过期样本。
    fn add(&self, bytes: u64) {
        let now = Instant::now();
        let mut g = lock(&self.samples);
        g.push_back((now, bytes));
        prune(&mut g, now);
    }

    /// 当前速率（字节/秒）：窗口内样本总和 ÷ 窗口秒数；窗口空时返回 0。
    fn rate(&self) -> f64 {
        let now = Instant::now();
        let mut g = lock(&self.samples);
        prune(&mut g, now);
        let total: u64 = g.iter().map(|(_, n)| n).sum();
        total as f64 / WINDOW.as_secs_f64()
    }

    /// 清空全部样本（服务器启动时归零）。
    fn clear(&self) {
        lock(&self.samples).clear();
    }
}

/// 从队首淘汰超出 [`WINDOW`] 的过期样本（队列按时刻递增，遇未过期即停）。
fn prune(g: &mut VecDeque<(Instant, u64)>, now: Instant) {
    while let Some((at, _)) = g.front() {
        if now.duration_since(*at) > WINDOW {
            g.pop_front();
        } else {
            break;
        }
    }
}

/// 入站字节滑动窗口的进程级单例。
fn window_in() -> &'static ByteWindow {
    /// 入站字节的滑动窗口统计单例。
    static W: OnceLock<ByteWindow> = OnceLock::new();
    W.get_or_init(ByteWindow::new)
}

/// 出站字节滑动窗口的进程级单例。
fn window_out() -> &'static ByteWindow {
    /// 出站字节的滑动窗口统计单例。
    static W: OnceLock<ByteWindow> = OnceLock::new();
    W.get_or_init(ByteWindow::new)
}

/// 一条已完成的请求日志（最新在前展示）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// 完成时刻（epoch ms）
    pub at_ms: u64,
    /// 请求的 model id（解析失败时为客户端原始值或 `(none)`）
    pub model: String,
    /// 耗时（毫秒）
    pub ms: u64,
    /// HTTP 状态码
    pub status: u16,
    /// 输出 token 数（无 usage 时为 0）
    pub tokens_out: u32,
}

/// 请求日志环形缓冲的进程级单例。
fn log_ring() -> &'static Mutex<VecDeque<LogEntry>> {
    /// 最近已完成请求日志的环形缓冲单例。
    static L: OnceLock<Mutex<VecDeque<LogEntry>>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// dock 展示用的统计快照
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    /// 运行时长（秒）；0 = 未运行
    pub uptime_s: u64,
    /// 服务是否在运行（[`reset`] 后置真，[`mark_stopped`] 后置假）
    pub running: bool,
    /// 当前活跃连接数
    pub conns_active: u64,
    /// 累计接受连接数
    pub conns_total: u64,
    /// 在途请求数
    pub requests_active: u64,
    /// 成功请求数
    pub requests_ok: u64,
    /// 失败请求数
    pub requests_failed: u64,
    /// 累计入站字节
    pub bytes_in: u64,
    /// 累计出站字节
    pub bytes_out: u64,
    /// 入站速率（字节/秒，5 秒滑动窗口）
    pub bytes_in_rate: f64,
    /// 出站速率（字节/秒，5 秒滑动窗口）
    pub bytes_out_rate: f64,
    /// 累计输入 token
    pub tokens_in: u64,
    /// 累计输出 token
    pub tokens_out: u64,
}

/// 服务器启动：清零统计并记启动时刻
pub fn reset() {
    CONNS_ACTIVE.store(0, Ordering::Relaxed);
    CONNS_TOTAL.store(0, Ordering::Relaxed);
    REQUESTS_ACTIVE.store(0, Ordering::Relaxed);
    REQUESTS_OK.store(0, Ordering::Relaxed);
    REQUESTS_FAILED.store(0, Ordering::Relaxed);
    BYTES_IN.store(0, Ordering::Relaxed);
    BYTES_OUT.store(0, Ordering::Relaxed);
    TOKENS_IN.store(0, Ordering::Relaxed);
    TOKENS_OUT.store(0, Ordering::Relaxed);
    STARTED_AT.store(now_ms(), Ordering::Relaxed);
    window_in().clear();
    window_out().clear();
    lock(log_ring()).clear();
}

/// 服务器停止：只标记未运行（保留最后一次的累计值供面板展示）
pub fn mark_stopped() {
    STARTED_AT.store(0, Ordering::Relaxed);
}

/// 连接建立：活跃连接数 +1，累计连接数 +1。
pub fn conn_opened() {
    CONNS_ACTIVE.fetch_add(1, Ordering::Relaxed);
    CONNS_TOTAL.fetch_add(1, Ordering::Relaxed);
}

/// 连接关闭：活跃连接数 -1（累计值不回退）。
pub fn conn_closed() {
    CONNS_ACTIVE.fetch_sub(1, Ordering::Relaxed);
}

/// 请求开始：在途请求数 +1。
pub fn request_started() {
    REQUESTS_ACTIVE.fetch_add(1, Ordering::Relaxed);
}

/// 请求结束：在途请求数 -1，并按 `ok`（HTTP < 400）计入成功或失败。
pub fn request_finished(ok: bool) {
    REQUESTS_ACTIVE.fetch_sub(1, Ordering::Relaxed);
    if ok {
        REQUESTS_OK.fetch_add(1, Ordering::Relaxed);
    } else {
        REQUESTS_FAILED.fetch_add(1, Ordering::Relaxed);
    }
}

/// 累加入站字节并写入滑动窗口。
pub fn add_bytes_in(n: u64) {
    BYTES_IN.fetch_add(n, Ordering::Relaxed);
    window_in().add(n);
}

/// 累加出站字节并写入滑动窗口。
pub fn add_bytes_out(n: u64) {
    BYTES_OUT.fetch_add(n, Ordering::Relaxed);
    window_out().add(n);
}

/// 累加上游返回的输入/输出 token 数。
pub fn add_tokens(input: u32, output: u32) {
    TOKENS_IN.fetch_add(input as u64, Ordering::Relaxed);
    TOKENS_OUT.fetch_add(output as u64, Ordering::Relaxed);
}

/// 追加一条请求日志；超出 [`LOG_CAP`] 时丢弃最旧一条。
pub fn push_log(entry: LogEntry) {
    let mut g = lock(log_ring());
    if g.len() >= LOG_CAP {
        g.pop_front();
    }
    g.push_back(entry);
}

/// 最近 `n` 条请求日志（最新在前）
pub fn recent(n: usize) -> Vec<LogEntry> {
    let g = lock(log_ring());
    g.iter().rev().take(n).cloned().collect()
}

/// 采集当前统计快照（dock 每帧调用：计数器无锁读取，速率走滑动窗口）。
pub fn snapshot() -> Snapshot {
    let started = STARTED_AT.load(Ordering::Relaxed);
    Snapshot {
        running: started > 0,
        uptime_s: if started > 0 {
            now_ms().saturating_sub(started) / 1000
        } else {
            0
        },
        conns_active: CONNS_ACTIVE.load(Ordering::Relaxed),
        conns_total: CONNS_TOTAL.load(Ordering::Relaxed),
        requests_active: REQUESTS_ACTIVE.load(Ordering::Relaxed),
        requests_ok: REQUESTS_OK.load(Ordering::Relaxed),
        requests_failed: REQUESTS_FAILED.load(Ordering::Relaxed),
        bytes_in: BYTES_IN.load(Ordering::Relaxed),
        bytes_out: BYTES_OUT.load(Ordering::Relaxed),
        bytes_in_rate: window_in().rate(),
        bytes_out_rate: window_out().rate(),
        tokens_in: TOKENS_IN.load(Ordering::Relaxed),
        tokens_out: TOKENS_OUT.load(Ordering::Relaxed),
    }
}

/// 测试用串行锁：统计是进程内单例，触碰它的用例必须串行（生产代码从不取此锁）
#[cfg(test)]
pub fn serial() -> MutexGuard<'static, ()> {
    /// 测试串行化锁单例，避免并发用例互相干扰统计。
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    lock(L.get_or_init(|| Mutex::new(())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_and_snapshot_track_lifecycle() {
        let _l = serial();
        reset();
        assert!(snapshot().running);

        conn_opened();
        request_started();
        add_bytes_in(100);
        add_bytes_out(50);
        request_finished(true);
        add_tokens(10, 20);

        let s = snapshot();
        assert_eq!(s.conns_active, 1);
        assert_eq!(s.conns_total, 1);
        assert_eq!(s.requests_active, 0);
        assert_eq!(s.requests_ok, 1);
        assert_eq!(s.requests_failed, 0);
        assert_eq!(s.bytes_in, 100);
        assert_eq!(s.bytes_out, 50);
        assert_eq!(s.tokens_in, 10);
        assert_eq!(s.tokens_out, 20);
        assert!(s.bytes_in_rate > 0.0, "窗口内应有速率");

        conn_closed();
        request_started();
        request_finished(false);
        let s = snapshot();
        assert_eq!(s.conns_active, 0);
        assert_eq!(s.requests_failed, 1);

        mark_stopped();
        let s = snapshot();
        assert!(!s.running);
        assert_eq!(s.uptime_s, 0);
    }

    #[test]
    fn log_ring_is_bounded_and_newest_first() {
        let _l = serial();
        reset();
        for i in 0..(LOG_CAP + 7) {
            push_log(LogEntry {
                at_ms: 1000 + i as u64,
                model: format!("m{i}"),
                ms: 1,
                status: 200,
                tokens_out: 0,
            });
        }
        let recent5 = recent(5);
        assert_eq!(recent5.len(), 5);
        assert_eq!(recent5[0].model, format!("m{}", LOG_CAP + 6));
        assert_eq!(recent5[4].model, format!("m{}", LOG_CAP + 2));
        assert_eq!(recent(LOG_CAP + 100).len(), LOG_CAP, "环形缓冲有上限");
    }
}
