//! journal：一次运行留下的记录，让**下一次**运行可以跳过已完成的工作。
//!
//! ## 重放到底买到什么
//!
//! 文档里写的迭代回路是"改落盘的脚本再跑一遍"。没有 journal，那次重跑会把每个 agent
//! 从头再付一遍——对一个 40 agent 的审计来说，这就是为改最后一段的一行代码付整次运行的钱。
//! 有了它，没变的前缀从磁盘回来，只有改动那部分真跑。
//!
//! ## 为什么是**前缀**，不是查找表
//!
//! 每条记录既按运行中的位置、也按"决定这个 agent 做什么的一切"的哈希作键。重放按位置顺序走，
//! 遇到第一条对不上的就**停止复用**，从那之后的每次调用都真跑。乱序复用后面的命中，等于复用
//! 在**不同上游条件**下产出的结果：同一个 prompt 在**另一次**运行的第 12 位做的不是同一件事，
//! 因为喂给它的东西变了。
//!
//! 失败的记录按失败记下、且永远不会被当成成功重放：一次在第 5 个 agent 上死掉的运行，
//! 恢复它的意义就是**重试第 5 个**，所以前缀在那里就结束了，第 5 个起真跑。
//!
//! ## 用了 `agent({ resume })` 的运行
//!
//! 整盘不重放。被重放的 agent 是文件里的一段文本、不是活的 child，所以这次运行里没有一段
//! 对话可供后续 `resume` 续；而能找回那段对话的 id 表属于真正 spawn 它的那次运行。
//! 与其重放一个前缀、然后在第一个 `resume` 处卡住，不如一开始就放弃整盘缓存、老实付全价。
//! 粗，但是故意的：另一种做法要额外记"每条记录是在哪个 label 下跑的"、并把前缀截到最早的
//! 那个再取回处——为一个只值一次运行的场景引入第二个键概念。
//!
//! ## 并发下的顺序
//!
//! 位置按调用到达顺序分配，而 `pipeline` 里这个顺序取决于谁先跑完。重放通常能复现它
//! （缓存调用按 journal 顺序作答），但不保证。这就是键要与位置一起校验的原因：
//! 一次交错方式不同的运行只会丢掉缓存命中，**永远不会**拿到另一个 agent 的答案。

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

/// 一条已结算的 agent 调用（可重放）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JournalEntry {
    /// 运行中的位置——与 `wf-agent-N` 用的是同一个计数器。
    pub index: u64,
    /// 调用载荷的哈希；对不上就结束可重放前缀。
    pub key: String,
    /// 该 agent 是否成功。失败会结束重放前缀。
    pub ok: bool,
    /// 它的答案（如果有）。
    pub text: Option<String>,
    /// 这次调用是否在续跑一个更早的 child（`agent({ resume })`）：**整盘缓存因此作废**。
    pub resumed: bool,
}

/// 决定一个 agent 做什么的字段。
///
/// 故意不是整个载荷：`phaseIndex`/`phaseTitle` 只把那一行在进度树里挪个位置，
/// 一个 token 都不改，所以重新分阶段不该丢掉一小时的成果。
#[derive(Debug, Default)]
pub(crate) struct JournalKeyInput {
    /// 任务正文。
    pub prompt: String,
    /// 标签。
    pub label: Option<String>,
    /// 型号。
    pub model: Option<String>,
    /// 类型名。
    pub agent_type: Option<String>,
    /// 思考档位。
    pub effort: Option<String>,
    /// 收尾命令。
    pub gate: Option<String>,
    /// 续跑目标 label。
    pub resume: Option<String>,
    /// 已序列化的 `agent({ schema })`（有这个调用才传）。
    pub schema: Option<String>,
}

/// 调用载荷的稳定哈希（字段顺序在这里定死，不由调用方决定）。
pub(crate) fn key(input: &JournalKeyInput) -> String {
    let opt = |v: &Option<String>| match v {
        Some(s) => Value::String(s.clone()),
        None => Value::Null,
    };

    // schema **只在存在时追加**：无条件追加会改掉每一条已有记录的正则形式、
    // 让磁盘上所有 journal 作废；条件追加则无 schema 的调用与从前逐字一致，
    // 而加了/改了 schema 仍然算出不同的键。
    let mut parts: Vec<Value> = vec![
        Value::String(input.prompt.clone()),
        opt(&input.label),
        opt(&input.model),
        opt(&input.agent_type),
        opt(&input.effort),
        opt(&input.gate),
        opt(&input.resume),
    ];

    if let Some(schema) = input.schema.as_ref() {
        parts.push(Value::String(schema.clone()));
    }

    let canonical = serde_json::to_string(&parts).unwrap_or_default();
    let digest = Sha256::digest(canonical.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex[..32].to_string()
}

/// 读一个 journal 文件，按位置排序。
///
/// **从不报错**：文件缺失、被截断、被人手改坏都只意味着"没有可重放的东西"，代价是 token；
/// 而拒绝运行代价是整次运行。末行不完整是常态——文件是在 agent 陆续结算时追加的。
pub(crate) fn read(path: &Path) -> Vec<JournalEntry> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };

    let mut out: Vec<JournalEntry> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|v| entry_from_json(&v))
        .collect();
    out.sort_by_key(|e| e.index);
    out
}

/// 从一行 JSON 还原一条记录；非 JSON 对象或缺 `index`/`key`/`ok` 时返回 `None`（该行按损坏丢弃）。
fn entry_from_json(v: &Value) -> Option<JournalEntry> {
    let obj = v.as_object()?;
    let index = obj.get("index").and_then(Value::as_u64)?;
    let key = obj.get("key").and_then(Value::as_str)?.to_string();
    let ok = obj.get("ok").and_then(Value::as_bool)?;
    let text = obj.get("text").and_then(Value::as_str).map(str::to_string);
    let resumed = obj.get("resumed").and_then(Value::as_bool).unwrap_or(false);

    Some(JournalEntry {
        index,
        key,
        ok,
        text,
        resumed,
    })
}

/// 把一条记录序列化为紧凑 JSON；`text` 为 `None`、`resumed` 为 `false` 时省略对应字段。
fn entry_to_json(entry: &JournalEntry) -> Value {
    let mut v = serde_json::json!({
        "index": entry.index,
        "key": entry.key,
        "ok": entry.ok,
    });

    if let Some(text) = entry.text.as_ref() {
        v["text"] = Value::String(text.clone());
    }

    if entry.resumed {
        v["resumed"] = Value::Bool(true);
    }
    v
}

/// 追加一条已结算的调用。**写不进去不算运行失败**（代价是将来少一次恢复）。
pub(crate) fn append(path: &Path, entry: &JournalEntry) {
    if let Some(parent) = path.parent() {
        _ = std::fs::create_dir_all(parent);
    }

    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };

    _ = writeln!(file, "{}", entry_to_json(entry));
}

/// 本次运行要用的 journal（读进来的旧记录 + 往哪写）。
pub(crate) struct Journal {
    /// 上一次运行的已结算调用，按位置序。空 = 不重放。
    pub entries: Vec<JournalEntry>,
    /// 本次运行的文件（`None` = 不记录，将来也就无从恢复）。
    pub path: Option<PathBuf>,
    /// 这次是恢复哪一次运行（消息与统计用）。
    pub resumed_from: Option<String>,
}

impl Journal {
    /// 带 `resumed` 标记的 journal **整盘不重放**（见模块文档）。
    pub(crate) fn replayable(&mut self) -> bool {
        if self.entries.iter().any(|e| e.resumed) {
            self.entries.clear();
            return false;
        }

        !self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(prompt: &str) -> JournalKeyInput {
        JournalKeyInput {
            prompt: prompt.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn key_is_stable_and_field_sensitive() {
        let a = key(&input("hello"));
        assert_eq!(a.len(), 32);
        assert_eq!(a, key(&input("hello")), "同样的载荷 → 同样的键");
        assert_ne!(a, key(&input("hello!")));
        let mut with_label = input("hello");
        with_label.label = Some("l".into());
        assert_ne!(a, key(&with_label), "label 参与键");
        // 无 schema 的调用与"从前"逐字一致（条件追加的意义）
        let mut with_schema = input("hello");
        with_schema.schema = Some("{\"type\":\"object\"}".into());
        assert_ne!(a, key(&with_schema));
    }

    #[test]
    fn entries_round_trip_and_tolerate_damage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wf.jsonl");
        append(
            &path,
            &JournalEntry {
                index: 1,
                key: "k1".into(),
                ok: true,
                text: Some("answer".into()),
                resumed: false,
            },
        );
        append(
            &path,
            &JournalEntry {
                index: 0,
                key: "k0".into(),
                ok: false,
                text: None,
                resumed: false,
            },
        );
        // 手改坏的一行 + 半截行
        std::fs::write(
            &path,
            format!("{}\n{{ not json\n", std::fs::read_to_string(&path).unwrap()),
        )
        .unwrap();

        let entries = read(&path);
        assert_eq!(entries.len(), 2, "坏行跳过、其余保留: {entries:?}");
        assert_eq!(entries[0].index, 0, "按位置排序");
        assert!(!entries[0].ok);
        assert_eq!(entries[1].text.as_deref(), Some("answer"));
        // 不存在的文件 → 空（不 panic）
        assert!(read(&dir.path().join("nope.jsonl")).is_empty());
    }

    #[test]
    fn resumed_marker_disables_replay_entirely() {
        let mut journal = Journal {
            entries: vec![
                JournalEntry {
                    index: 0,
                    key: "k".into(),
                    ok: true,
                    text: Some("t".into()),
                    resumed: false,
                },
                JournalEntry {
                    index: 1,
                    key: "k".into(),
                    ok: true,
                    text: Some("t".into()),
                    resumed: true,
                },
            ],
            path: None,
            resumed_from: Some("wf_x".into()),
        };
        assert!(!journal.replayable(), "带 resume 的 journal 整盘不重放");
        assert!(journal.entries.is_empty());
    }
}
