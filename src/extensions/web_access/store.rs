//! 搜索结果存储（移植自 pi-web-access `storage.ts`）。
//!
//! 为 `web_search` → `get_search_content` 提供同一会话内的取回能力；
//! 内容检索（`findText`）见 [`crate::utils::find`]。
//!
//! 与上游差异：上游把结果写进会话自定义条目并落盘 `web-search-cache` 目录；
//! 本阶段先实现进程内 LRU 存储（128 条），足以支撑同一会话内
//! `web_search` → `get_search_content` 的取回；跨进程恢复留待后续阶段。
//! 淘汰策略完全交给 `lru::LruCache`（容量上限，超限丢最久未使用项），
//! 不做 TTL：条目只在被挤出缓存、或会话切换（[`clear`]）时才失效。

use super::{super::util::generate_id, fetch::FetchedContent, search::SearchResult};
use lru::LruCache;
use std::{
    num::NonZeroUsize,
    sync::{Mutex, OnceLock},
};

/// 存储条目上限
const MAX_ENTRIES: NonZeroUsize = NonZeroUsize::new(128).unwrap();

/// 单条查询结果
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct QueryResultData {
    /// 原始查询字符串
    pub query: String,
    /// 搜索服务返回的聚合答案（可为空）
    #[serde(default)]
    pub answer: String,
    /// 该查询命中的搜索结果列表
    #[serde(default)]
    pub results: Vec<SearchResult>,
    /// 该查询失败时的错误信息；成功时为 `None`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 产生该结果的 provider 名称（如 `brave`）；未知时为 `None`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// 存储数据
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum StoredData {
    /// 一次 `web_search` 调用的结果
    Search {
        /// 取回该条目用的存储 id（`get` / `get_search_content` 的 key）
        id: String,
        /// 本次调用涉及的各个查询及其结果
        queries: Vec<QueryResultData>,
    },
    /// 一次 `web_fetch` 调用的结果
    Fetch {
        /// 取回该条目用的存储 id
        id: String,
        /// 本次调用抓取的各 URL 内容
        urls: Vec<FetchedContent>,
    },
}

impl StoredData {
    /// 该条目用于取回的存储 id。
    pub fn id(&self) -> &str {
        match self {
            StoredData::Search { id, .. } | StoredData::Fetch { id, .. } => id,
        }
    }
}

/// 进程内 LRU 缓存，按存储 id 保存 `web_search` / `web_fetch` 的结果。
struct Store {
    /// 以存储 id 为 key 的 LRU 缓存，超容量时淘汰最久未使用项
    cache: LruCache<String, StoredData>,
}

/// 进程级单例存储（首次调用时惰性初始化）。
fn store() -> &'static Mutex<Store> {
    /// 进程级单例存储，首次访问时惰性初始化
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE.get_or_init(|| {
        Mutex::new(Store {
            cache: LruCache::new(MAX_ENTRIES),
        })
    })
}

/// 存入搜索数据
pub fn store_search(queries: Vec<QueryResultData>) -> String {
    let id = generate_id();
    insert(StoredData::Search {
        id: id.clone(),
        queries,
    });
    id
}

/// 存入抓取数据
pub fn store_fetch(urls: Vec<FetchedContent>) -> String {
    let id = generate_id();
    insert(StoredData::Fetch {
        id: id.clone(),
        urls,
    });
    id
}

/// 以条目的 id 为 key 写入缓存；超出容量时 LRU 自动淘汰最久未使用项。
fn insert(data: StoredData) {
    let id = data.id().to_string();
    let mut store = store().lock().unwrap_or_else(|e| e.into_inner());
    store.cache.put(id, data);
}

/// 读取存储数据（命中时刷新 LRU）
pub fn get(id: &str) -> Option<StoredData> {
    let mut store = store().lock().unwrap_or_else(|e| e.into_inner());
    store.cache.get(id).cloned()
}

/// 清空缓存（会话切换时调用；测试亦复用）
pub fn clear() {
    let mut store = store().lock().unwrap_or_else(|e| e.into_inner());
    store.cache.clear();
}

/// 缓存是进程级单例，触碰它的测试必须串行（同 `plan_mode::TEST_LOCK` 约定）：
/// 否则一个测试里的 `clear()`（含会话切换路径）会抽掉另一个测试刚存入的条目。
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_roundtrip_and_eviction() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let id = store_search(vec![QueryResultData {
            query: "hello".to_string(),
            answer: "world".to_string(),
            results: vec![],
            error: None,
            provider: Some("brave".to_string()),
        }]);
        let data = get(&id).expect("stored");
        match data {
            StoredData::Search { queries, .. } => {
                assert_eq!(queries[0].query, "hello");
                assert_eq!(queries[0].provider.as_deref(), Some("brave"));
            }
            _ => panic!("expected search data"),
        }
        assert!(get("missing").is_none());
    }

    #[test]
    fn store_evicts_least_recently_used() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let ids: Vec<String> = (0..MAX_ENTRIES.get())
            .map(|i| {
                store_fetch(vec![FetchedContent::error(
                    &format!("https://example.com/{i}"),
                    "test",
                )])
            })
            .collect();
        // 触碰第二个条目，使其成为最近使用
        assert!(get(&ids[1]).is_some());
        // 超容量插入会淘汰最久未使用的第一个条目
        let extra = store_fetch(vec![]);
        assert!(get(&ids[0]).is_none());
        assert!(get(&ids[1]).is_some());
        assert!(get(&extra).is_some());
    }
}
