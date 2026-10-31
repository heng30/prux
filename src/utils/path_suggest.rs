//! `@` 文件候选：`ignore` 递归扫描 + 路径打分排序，TUI 与 GUI 共用同一套算法。
//!
//! 除候选本身，还提供补全分词用的判定：[`is_autocomplete_separator`]（空白或 CJK 标点）
//! 与 [`is_at_token_start`]（`@` 是否位于 token 起始，`user@example.com` 不算）。

use crate::utils::fuzzy::path_score;
use ignore::WalkState;
use regex::Regex;
use std::{
    cmp::Ordering,
    collections::BinaryHeap,
    path::Path,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering},
    },
};

/// 单次扫描的遍历上限：`~` 下有 `Code -> /data/Code` 这类指向巨树的符号链接。
const SCAN_MAX_VISITED: usize = 50_000;

/// 遍历线程数：固定小值，避免每次按键扫描都拉起 num_cpus 个线程。
const SCAN_THREADS: usize = 4;

/// 路径 token 前可能出现的开括号及其配对闭括号。
/// `(~/Dev` / `` `src/ma `` 这类散文包装符不是分隔符，但补全前必须剥掉才能当合法路径用。
const PATH_WRAPPERS: [(char, char); 5] =
    [('(', ')'), ('[', ']'), ('{', '}'), ('<', '>'), ('`', '`')];

/// 一条 `@` 文件候选。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// 展示名：文件名（目录带尾随 `/`）
    pub name: String,
    /// 展示路径：含用户已输入的目录前缀（`sub/ma` → `sub/main.rs`）
    pub display: String,
    /// 写回输入框的补全文本（目录带尾随 `/`；含空白或 CJK 标点时加引号）
    pub insert: String,
}

/// 扫描 `cwd` 下的文件与目录，按 [`path_score`] 打分取前 `limit` 条。
///
/// 同步阻塞实现：调用方必须放进后台线程；`cancel` 置位后尽快退出。
/// 查询里出现 `/` 时最后一段是文件名查询、前面是目录（`sub/ma` 只扫 `cwd/sub`）；
/// 目录不存在返回空列表。
pub fn candidates(query: &str, cwd: &str, limit: usize, cancel: &AtomicBool) -> Vec<Candidate> {
    let normalized = query.replace('\\', "/");

    let Some((base_dir, query, display_base)) = split_base_and_query(&normalized, cwd) else {
        return Vec::new();
    };

    if base_dir.is_empty() {
        return Vec::new();
    }

    let mut builder = ignore::WalkBuilder::new(&base_dir);
    builder
        .hidden(false)
        .follow_links(true)
        .parents(true)
        .add_custom_ignore_filename(".fdignore")
        .threads(SCAN_THREADS)
        .filter_entry(|e| e.file_name() != ".git");

    let visited = AtomicUsize::new(0);
    let top = Mutex::new(BinaryHeap::<RankedCandidate>::new());
    builder.build_parallel().run(|| {
        Box::new(|entry| {
            if cancel.load(AtomicOrdering::Relaxed) {
                return WalkState::Quit;
            }
            if visited.fetch_add(1, AtomicOrdering::Relaxed) >= SCAN_MAX_VISITED {
                return WalkState::Quit;
            }
            let Ok(entry) = entry else {
                return WalkState::Continue;
            };
            if entry.depth() == 0 {
                return WalkState::Continue; // 根自身不参与匹配
            }
            let Ok(rel) = entry.path().strip_prefix(&base_dir) else {
                return WalkState::Continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            if rel.is_empty() {
                return WalkState::Continue;
            }

            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            let score = if query.is_empty() {
                1
            } else {
                path_score(&rel, &query, is_dir)
            };
            if score <= 0 {
                return WalkState::Continue;
            }

            let mut top = top.lock().unwrap();
            top.push(RankedCandidate {
                score,
                depth: entry.depth(),
                path: rel,
                is_dir,
            });
            if top.len() > limit {
                top.pop(); // 堆顶是当前最差候选
            }
            WalkState::Continue
        })
    });

    // into_sorted_vec：按 Ord 升序 = 排名从优到劣
    top.into_inner()
        .unwrap()
        .into_sorted_vec()
        .into_iter()
        .map(|c| candidate_item(&c.path, c.is_dir, &display_base))
        .collect()
}

/// 取路径最后一段（已归一化为 / 分隔）
fn basename(path: &str) -> &str {
    path.rfind('/').map(|i| &path[i + 1..]).unwrap_or(path)
}

/// 候选排序键：分数高优先，其次浅层、短路径、字典序。
/// `Ord` 把「更优」排在前面，故 `BinaryHeap::into_sorted_vec()` 直接得到排名顺序；
/// 路径各不相同，排序是全序，故边遍历边保留 top-N 与「全量收集后再排」结果一致，且结果与遍历顺序（并行调度）无关。
#[derive(PartialEq, Eq)]
struct RankedCandidate {
    /// 匹配得分，越高越靠前。
    score: i32,
    /// 相对扫描根的目录深度，越浅越靠前。
    depth: usize,
    /// 候选文件或目录的路径。
    path: String,
    /// true = 该项是目录（同分时略有加成）。
    is_dir: bool,
}

impl Ord for RankedCandidate {
    /// 按「更优在前」排序：分数降序，再按目录深度浅、路径短、字典序。
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .score
            .cmp(&self.score)
            .then_with(|| self.depth.cmp(&other.depth))
            .then_with(|| self.path.len().cmp(&other.path.len()))
            .then_with(|| self.path.cmp(&other.path))
    }
}

impl PartialOrd for RankedCandidate {
    /// 委托给 [`Ord::cmp`]，与全序保持一致。
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// 拆分 @ scoped 查询：含 / 时最后一段为文件名查询、前面为目录，返回 (base_dir, query, display_base)
fn split_base_and_query(normalized: &str, cwd: &str) -> Option<(String, String, String)> {
    match normalized.rfind('/') {
        Some(slash) => {
            let display_base = normalized[..=slash].to_string();
            let query = normalized[slash + 1..].to_string();
            let base_dir = if display_base.starts_with("~/") {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                format!(
                    "{}/{}",
                    home,
                    display_base.trim_start_matches("~/").trim_end_matches('/')
                )
            } else if display_base.starts_with('/') {
                // `@/x`：display_base 就是 "/"，根目录不能 trim 成空串
                let trimmed = display_base.trim_end_matches('/');
                if trimmed.is_empty() {
                    "/".to_string()
                } else {
                    trimmed.to_string()
                }
            } else {
                format!("{}/{}", cwd, display_base.trim_end_matches('/'))
            };

            if !Path::new(&base_dir).is_dir() {
                return None;
            }
            Some((base_dir, query, display_base))
        }
        None => Some((cwd.to_string(), normalized.to_string(), String::new())),
    }
}

/// 由相对路径构造候选条目（目录名/补全带尾随 `/`；含空白/CJK 标点的路径加引号，
/// 否则补全后 token 被截断）
fn candidate_item(rel: &str, is_dir: bool, display_base: &str) -> Candidate {
    let display_path = if display_base.is_empty() {
        rel.to_string()
    } else if display_base == "/" {
        format!("/{rel}")
    } else {
        format!("{display_base}{rel}")
    };

    let entry_name = basename(rel);
    let completion = if is_dir {
        format!("{display_path}/")
    } else {
        display_path.clone()
    };
    let completion = if completion.chars().any(is_autocomplete_separator) {
        format!("\"{completion}\"")
    } else {
        completion
    };

    Candidate {
        name: if is_dir {
            format!("{entry_name}/")
        } else {
            entry_name.to_string()
        },
        display: display_path,
        insert: completion,
    }
}

/// CJK 标点：仅标点分隔词与补全（CJK 文字仍是词/路径的一部分）。CJK 脚本内的标点 + 显式清单。
fn is_cjk_punctuation(c: char) -> bool {
    /// CJK 标点判定用的正则缓存：匹配 Unicode 标点类别。
    static PUNCT: OnceLock<Regex> = OnceLock::new();
    /// CJK 文字脚本判定用的正则缓存：Han / 假名 / 谚文 / 注音等。
    static CJK_SCRIPT: OnceLock<Regex> = OnceLock::new();

    // CJK 脚本内的标点（regex crate 无 lookahead，故交集分两次判）
    let punct = PUNCT.get_or_init(|| Regex::new(r"\p{Punctuation}").expect("punct"));
    let script = CJK_SCRIPT.get_or_init(|| {
        regex::Regex::new(
            r"[\p{Script_Extensions=Han}\p{Script_Extensions=Hiragana}\p{Script_Extensions=Katakana}\p{Script_Extensions=Hangul}\p{Script_Extensions=Bopomofo}]",
        )
        .expect("cjk script")
    });

    let s = c.to_string();
    (script.is_match(&s) && punct.is_match(&s))
        || matches!(
            c,
            '，' | '．'
                | '：'
                | '；'
                | '！'
                | '？'
                | '（'
                | '）'
                | '［'
                | '］'
                | '｛'
                | '｝'
                | '“'
                | '”'
                | '‘'
                | '’'
                | '…'
                | '—'
        )
}

/// 补全分隔符：空白或 CJK 标点。
pub fn is_autocomplete_separator(c: char) -> bool {
    c.is_whitespace() || is_cjk_punctuation(c)
}

/// 返回与开包装符 `c` 配对的闭括号；`c` 不是已知开括号时返回 `None`。
fn wrapper_closer(c: char) -> Option<char> {
    PATH_WRAPPERS
        .iter()
        .find(|(open, _)| *open == c)
        .map(|(_, close)| *close)
}

/// 剥掉 token 开头的连续开括号：`(~/Dev` → `~/Dev`。
/// token 自身含对应闭括号（`app/[slug]/pa`、`(group)/pa`）时保留，避免误伤真实路径。
pub fn strip_leading_wrappers(token: &str) -> &str {
    let mut rest = token;
    while let Some(first) = rest.chars().next() {
        let Some(closer) = wrapper_closer(first) else {
            break;
        };

        let tail = &rest[first.len_utf8()..];

        if tail.contains(closer) {
            break;
        }
        rest = tail;
    }
    rest
}

/// `index`（字节偏移）是否位于 token 起始：跳过紧邻的开括号后，前一字符是分隔符或行首。
/// 用于 `(@src` / `` `@src `` 这类被包装符包住的 `@`。
pub fn is_at_token_start(text: &str, index: usize) -> bool {
    let mut start = index;
    while start > 0 {
        let Some(prev) = text[..start].chars().next_back() else {
            break;
        };
        if wrapper_closer(prev).is_none() {
            break;
        }
        start -= prev.len_utf8();
    }

    start == 0
        || text[..start]
            .chars()
            .next_back()
            .is_some_and(is_autocomplete_separator)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// 建临时目录：`.git` 目录存在即被 ignore 视为 git 仓（require_git 语义）
    fn write_file(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    /// 扫描结果按打分排序、与并行遍历顺序无关，且 scoped 查询只扫子目录。
    #[test]
    fn scan_and_score_without_external_fd() {
        // 不依赖外部 fd：任何环境都能跑
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_file(root, "sub/main.rs", "x");
        write_file(root, "src/clone.txt", "x");
        write_file(root, "login.txt", "x");
        let cwd = root.to_str().unwrap();
        let never = AtomicBool::new(false);
        let inserts = |items: &[Candidate]| -> Vec<String> {
            items.iter().map(|i| i.insert.clone()).collect()
        };

        // @lo：文件名匹配优先（login 开头命中 > clone 子串命中）
        let items = candidates("lo", cwd, 20, &never);
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names[0], "login.txt", "got: {names:?}");
        assert!(names.contains(&"clone.txt"), "got: {names:?}");

        // 结果与遍历顺序（并行调度）无关：重复扫描得到同一序列
        assert_eq!(inserts(&candidates("lo", cwd, 20, &never)), inserts(&items));

        // @sub/ma：scoped 到 sub 目录，插入相对路径
        let items = candidates("sub/ma", cwd, 20, &never);
        let got = inserts(&items);
        assert!(got.contains(&"sub/main.rs".to_string()), "got: {got:?}");
    }

    /// gitignore / .fdignore / 隐藏文件 / .git 目录各自的取舍。
    #[test]
    fn honors_gitignore_fdignore_hidden_and_skips_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap(); // 仅需 .git 存在
        std::fs::write(root.join(".gitignore"), "secret.txt\n").unwrap();
        std::fs::write(root.join(".fdignore"), "fdignored.txt\n").unwrap();
        write_file(root, "secret.txt", "x");
        write_file(root, "fdignored.txt", "x");
        write_file(root, "kept.txt", "x");
        write_file(root, ".hidden.txt", "x");
        write_file(root, ".git/config", "x");

        let items = candidates("", root.to_str().unwrap(), 20, &AtomicBool::new(false));
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"kept.txt"), "got: {names:?}");
        assert!(
            names.contains(&".hidden.txt"),
            "--hidden：隐藏文件可见: {names:?}"
        );
        assert!(!names.contains(&"secret.txt"), ".gitignore 生效: {names:?}");
        assert!(
            !names.contains(&"fdignored.txt"),
            ".fdignore 生效: {names:?}"
        );
        assert!(!names.contains(&".git/"), ".git 目录不参与候选: {names:?}");
    }

    /// 非 git 仓不应用 .gitignore，但 .ignore 仍生效（对齐 fd 10 默认的 require_git）。
    #[test]
    fn ignores_gitignore_outside_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".gitignore"), "secret.txt\n").unwrap();
        std::fs::write(root.join(".ignore"), "plainignored.txt\n").unwrap();
        write_file(root, "secret.txt", "x");
        write_file(root, "plainignored.txt", "x");

        let items = candidates("", root.to_str().unwrap(), 20, &AtomicBool::new(false));
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert!(
            names.contains(&"secret.txt"),
            "非 git 仓：.gitignore 不生效: {names:?}"
        );
        assert!(
            !names.contains(&"plainignored.txt"),
            ".ignore 生效: {names:?}"
        );
    }

    /// 取消标志置位后不再产出候选。
    #[test]
    fn cancel_stops_scan() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "main.rs", "x");
        let cancel = AtomicBool::new(true); // 一进去就作废
        let items = candidates("", dir.path().to_str().unwrap(), 20, &cancel);
        assert!(items.is_empty(), "取消后不应产出候选: {items:?}");
    }

    /// `/` 结尾的 scoped 查询（`@/etc`）不能把根目录 trim 成空串。
    #[test]
    fn split_base_and_query_handles_root_and_scoped() {
        let (base, query, display) = split_base_and_query("/etc", "/tmp").unwrap();
        assert_eq!(base, "/");
        assert_eq!(query, "etc");
        assert_eq!(display, "/");

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();

        let (base, query, display) = split_base_and_query("sub/ma", root).unwrap();
        assert_eq!(base, format!("{root}/sub"));
        assert_eq!(query, "ma");
        assert_eq!(display, "sub/");

        // 目录不存在 → 无候选
        assert!(split_base_and_query("nope/x", root).is_none());
    }

    /// 含空白/CJK 标点的路径要加引号，目录补全带尾随 `/`。
    #[test]
    fn candidate_item_quotes_paths_with_separators() {
        let f = candidate_item("my dir/file.txt", false, "");
        assert_eq!(f.name, "file.txt");
        assert_eq!(f.display, "my dir/file.txt");
        assert_eq!(f.insert, "\"my dir/file.txt\"");

        let plain = candidate_item("plain.txt", false, "");
        assert_eq!(plain.insert, "plain.txt");

        // 目录：名称与补全都带尾随 /（便于继续补全）
        let dir = candidate_item("src", true, "");
        assert_eq!(dir.name, "src/");
        assert_eq!(dir.insert, "src/");

        // scoped 显示前缀（`@sub/ma`）
        let scoped = candidate_item("main.rs", false, "sub/");
        assert_eq!(scoped.display, "sub/main.rs");
        assert_eq!(scoped.insert, "sub/main.rs");
    }

    /// 排序 = 分数降序 → 深度升序 → 路径长度升序 → 字典序。
    #[test]
    fn ranked_candidates_order_by_score_then_depth_length_name() {
        let mk = |score, depth, path: &str| RankedCandidate {
            score,
            depth,
            path: path.to_string(),
            is_dir: false,
        };
        let mut heap = BinaryHeap::from(vec![
            mk(80, 2, "a/bbbb.rs"),
            mk(30, 1, "a.rs"),
            mk(80, 1, "longer.rs"),
            mk(80, 1, "a.rs"),
        ]);
        let got: Vec<i32> = std::iter::from_fn(|| heap.pop().map(|c| c.score)).collect();
        // 堆顶是「最差」，逐个 pop 得到从劣到优
        assert_eq!(got, vec![30, 80, 80, 80]);
        let sorted: Vec<String> = BinaryHeap::from(vec![
            mk(80, 2, "a/bbbb.rs"),
            mk(80, 1, "longer.rs"),
            mk(80, 1, "a.rs"),
        ])
        .into_sorted_vec()
        .into_iter()
        .map(|c| c.path)
        .collect();
        assert_eq!(sorted, vec!["a.rs", "longer.rs", "a/bbbb.rs"]);
    }

    /// CJK 标点算补全分隔符，CJK 文字与 ASCII 不算。
    #[test]
    fn cjk_punctuation_is_autocomplete_separator() {
        assert!(is_autocomplete_separator('，'));
        assert!(is_autocomplete_separator('。'));
        assert!(is_autocomplete_separator(' '));
        assert!(!is_autocomplete_separator('文'));
        assert!(!is_autocomplete_separator('a'));
    }

    /// 包装符后的 `@` 视为 token 起始，邮箱/词中的不算。
    #[test]
    fn at_after_wrapper_is_token_start() {
        // 包装符后：视为 token 起始（`(@src`、`` `@src ``）
        assert!(is_at_token_start("(@src", 1));
        assert!(is_at_token_start("`@src", 1));
        assert!(is_at_token_start("见 (@src", 5));
        // 行首 `@`
        assert!(is_at_token_start("@src", 0));
        // 邮箱/词中 `@`：不是 token 起始
        assert!(!is_at_token_start("user@example.com", 4));
        assert!(!is_at_token_start("app/(a)@x", 7));
    }
}
