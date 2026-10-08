//! `store()` / `load()` 的跨调用持久化。
//!
//! 写入落成会话自定义条目（`customType = "codemode-store"`，形状 `{set, delete}`），
//! 因此 `/resume` 后仍在；`load()` 读的是「**当前分支**上所有该类型条目按顺序应用」的快照
//! ——快照在每次脚本执行前由 [`seed_from_entries`] 用宿主给的当前分支条目重建
//! （分支切走的数据不进快照），本进程内尚未落盘的写入经 [`apply_writes`] 叠在其上。

use crate::core::extensions::{ExtensionUiRequest, request_ui};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Mutex, OnceLock},
};

/// 会话自定义条目类型（导出页按 customType 显示为 `[codemode-store]`）。
pub const ENTRY_TYPE: &str = "codemode-store";

/// 当前会话的 `store()` 快照（脚本执行前按当前分支重建）。
fn store() -> &'static Mutex<BTreeMap<String, Value>> {
    static STORE: OnceLock<Mutex<BTreeMap<String, Value>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// 本进程内已写入、但尚未确认出现在分支快照里的写入（`None` = 删除）。
///
/// 存在的理由：落盘是异步的（`PersistSessionEntry` 交给 UI 侧追加条目），
/// 且无 UI（headless/SDK）时不会落盘。若只按分支重建快照，这些写入会在下次脚本执行时消失。
fn pending_writes() -> &'static Mutex<BTreeMap<String, Option<Value>>> {
    static PENDING: OnceLock<Mutex<BTreeMap<String, Option<Value>>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// `load()` 用的快照副本。
pub fn snapshot() -> BTreeMap<String, Value> {
    store().lock().unwrap().clone()
}

/// 清空快照与未落盘写入（切换会话时调用）。
pub fn reset() {
    store().lock().unwrap().clear();
    pending_writes().lock().unwrap().clear();
}

/// 按**当前分支**的会话条目重建快照：条目按落盘顺序应用（`set` 覆盖、`delete` 删除），
/// 整体替换内存快照（多个会话共享进程时不会串值）；之后叠上尚未落盘的写入。
pub fn seed_from_entries(entries: &[Value]) {
    let mut next: BTreeMap<String, Value> = BTreeMap::new();
    for entry in entries {
        if entry.get("customType").and_then(|v| v.as_str()) != Some(ENTRY_TYPE) {
            continue;
        }

        let Some(data) = entry.get("data") else {
            continue;
        };

        if let Some(delete) = data.get("delete").and_then(|v| v.as_array()) {
            for key in delete.iter().filter_map(|v| v.as_str()) {
                next.remove(key);
            }
        }

        if let Some(set) = data.get("set").and_then(|v| v.as_object()) {
            for (key, value) in set {
                next.insert(key.clone(), value.clone());
            }
        }
    }

    // 未落盘的写入叠在分支快照之上；已经出现在分支里的（值相同 / 删除已生效）从待写表里剔除，
    // 避免它长期驻留、在分支回退后又被重新叠回去。
    let mut pending = pending_writes().lock().unwrap();
    pending.retain(|key, value| match value {
        Some(v) => {
            if next.get(key) == Some(v) {
                false
            } else {
                next.insert(key.clone(), v.clone());
                true
            }
        }
        None => {
            if next.contains_key(key) {
                next.remove(key);
                true
            } else {
                false
            }
        }
    });

    *store().lock().unwrap() = next;
}

/// 应用一次成功脚本的写入：更新进程内快照与未落盘表，并请求把同一份写入落成会话条目。
///
/// `writes` 的 `None` 值表示删除（`store(key, undefined)`）。没有任何写入时什么都不做。
pub fn apply_writes(writes: &BTreeMap<String, Option<Value>>) {
    if writes.is_empty() {
        return;
    }

    let mut set = serde_json::Map::new();
    let mut delete: Vec<String> = Vec::new();

    {
        let mut current = store().lock().unwrap();
        for (key, value) in writes {
            match value {
                Some(value) => {
                    current.insert(key.clone(), value.clone());
                    set.insert(key.clone(), value.clone());
                }
                None => {
                    current.remove(key);
                    delete.push(key.clone());
                }
            }
        }
    }

    {
        let mut pending = pending_writes().lock().unwrap();
        for (key, value) in writes {
            pending.insert(key.clone(), value.clone());
        }
    }

    request_ui(ExtensionUiRequest::PersistSessionEntry {
        custom_type: ENTRY_TYPE.to_string(),
        data: json!({ "set": Value::Object(set), "delete": delete }),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `store()` / 待写表都是进程级全局，测试并行跑会互相清值；用一把锁串起来。
    static STORE_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// 取测试锁（毒化后继续用内部值，与其它全局状态测试一致）。
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        STORE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 分支条目形状的载荷：`{customType, data:{set, delete}}`。
    fn entry(set: Value, delete: &[&str]) -> Value {
        json!({
            "customType": ENTRY_TYPE,
            "data": { "set": set, "delete": delete },
        })
    }

    /// 分支快照：按顺序应用 set / delete，非本类型的条目忽略。
    #[test]
    fn seed_applies_branch_entries_in_order() {
        let _guard = lock();
        reset();
        seed_from_entries(&[
            json!({ "customType": "other", "data": { "set": { "x": 1 } } }),
            entry(json!({ "a": 1, "b": 2 }), &[]),
            entry(json!({ "a": 3 }), &["b"]),
        ]);
        let snap = snapshot();
        assert_eq!(snap.get("a"), Some(&json!(3)), "后写覆盖先写");
        assert_eq!(snap.get("b"), None, "delete 生效");
        assert_eq!(snap.get("x"), None, "非本扩展条目不进快照");
        reset();
    }

    /// 只看到当前分支：换一份（更短/不同）分支条目后，旧分支的值不再出现在快照里。
    #[test]
    fn seed_replaces_snapshot_with_given_branch() {
        let _guard = lock();
        reset();
        seed_from_entries(&[entry(json!({ "main": 1 }), &[])]);
        assert_eq!(snapshot().get("main"), Some(&json!(1)));

        // 切到另一条分支（没有 codemode-store 条目）→ 快照必须清空
        seed_from_entries(&[json!({ "customType": "user", "data": {} })]);
        assert!(snapshot().is_empty(), "分支上没有写入时快照为空");
        reset();
    }

    /// 未落盘的写入在分支重放后仍在（headless/SDK 场景），落盘后不再重复叠加。
    #[test]
    fn unpersisted_writes_survive_reseed_and_are_pruned_once_persisted() {
        let _guard = lock();
        reset();
        let mut writes: BTreeMap<String, Option<Value>> = BTreeMap::new();
        writes.insert("k".to_string(), Some(json!("v")));
        apply_writes(&writes);
        assert_eq!(snapshot().get("k"), Some(&json!("v")));

        // 尚未落盘（分支上没有该条）→ 重放后仍在
        seed_from_entries(&[json!({ "customType": "user", "data": {} })]);
        assert_eq!(snapshot().get("k"), Some(&json!("v")));

        // 落盘后分支里已有同值 → 重放仍对，且待写表被剔除（值不同时不得覆盖分支）
        seed_from_entries(&[entry(json!({ "k": "v" }), &[])]);
        assert_eq!(snapshot().get("k"), Some(&json!("v")));
        seed_from_entries(&[entry(json!({ "k": "other" }), &[])]);
        assert_eq!(
            snapshot().get("k"),
            Some(&json!("other")),
            "分支里的值以分支为准"
        );
        reset();
    }

    /// 未落盘的删除同样生效，并在分支落盘后停止影响快照。
    #[test]
    fn unpersisted_delete_applies_until_persisted() {
        let _guard = lock();
        reset();
        seed_from_entries(&[entry(json!({ "gone": 1 }), &[])]);
        assert_eq!(snapshot().get("gone"), Some(&json!(1)));

        let mut writes: BTreeMap<String, Option<Value>> = BTreeMap::new();
        writes.insert("gone".to_string(), None);
        apply_writes(&writes);
        seed_from_entries(&[entry(json!({ "gone": 1 }), &[])]);
        assert_eq!(snapshot().get("gone"), None, "未落盘的删除仍是删除");

        // 落盘形态：删除是 `{set:{}, delete:[key]}`（不重置该键的值）
        seed_from_entries(&[entry(json!({}), &["gone"])]);
        assert_eq!(snapshot().get("gone"), None);
        // 分支已不含该键 → 待写表剔除，此后再出现于分支的值不得被删除掉
        seed_from_entries(&[entry(json!({ "gone": 2 }), &[])]);
        assert_eq!(snapshot().get("gone"), Some(&json!(2)));
        reset();
    }

    /// 空写入不产生任何条目（也不改快照）。
    #[test]
    fn empty_writes_are_noops() {
        let _guard = lock();
        reset();
        apply_writes(&BTreeMap::new());
        assert!(snapshot().is_empty());
    }
}
