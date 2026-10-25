//! 检索文本原语：词元切分与命中摘录。
//!
//! 面向**词法检索**的一组纯函数：把查询切成词元（CJK 按相邻二字组合展开）、找出命中位置、
//! 围绕命中点取一段可读摘录。不含打分策略——词元上限、加权系数、停用词之外的过滤
//! 都由调用方决定。另带几个与检索无关的通用小工具：[`has_cjk`]（按语种换字体之类的判定）、
//! [`code_fence`]（按内容定代码围栏长度）与 [`escape_special`]（按字符集合做反斜杠转义）。

/// 停用词表：只由这些词构成的查询没有检索价值。
///
/// 前段是英文虚词（小写形式入表，与词元的小写化对应）；中文按字 / 二字组合切分，
/// 所以后段同时列中文单字虚词（`的` / `了`）与常见二字虚词。`问题` 这类有实义的词不列，
/// 免得把有意义的查询清空。
const STOP_WORDS: &[&str] = &[
    "a", "about", "after", "all", "also", "an", "and", "any", "are", "as", "at", "be", "because",
    "been", "before", "but", "by", "can", "could", "did", "do", "does", "for", "from", "had",
    "has", "have", "how", "i", "if", "in", "into", "is", "it", "its", "just", "me", "my", "no",
    "not", "of", "on", "or", "our", "out", "should", "so", "some", "than", "that", "the", "their",
    "them", "then", "there", "these", "they", "this", "to", "up", "was", "we", "were", "what",
    "when", "where", "which", "who", "why", "will", "with", "would", "you", "your", "的", "了",
    "是", "在", "和", "就", "也", "都", "而", "及", "与", "或", "但", "被", "把", "让", "给", "从",
    "对", "向", "于", "为", "以", "之", "其", "这", "那", "个", "我", "你", "他", "她", "它", "吗",
    "呢", "吧", "啊", "呀", "嘛", "哪", "谁", "我们", "你们", "他们", "这个", "那个", "这些",
    "那些", "这样", "那样", "什么", "怎么", "如何", "因为", "所以", "如果", "但是", "然后", "可以",
    "还是", "已经", "没有", "就是", "一个", "一些",
];

/// 是否为需要按字符组合切分的中日韩文字（表意文字、假名、谚文）。
///
/// 与 `display::char_width` 的「占 2 列」判定不是一回事：那是终端显示宽度（还包含全角标点、
/// 彝文等），这里只认**不靠空格断词**的文字，用于决定要不要展开成二字组合。
pub fn is_cjk_script(ch: char) -> bool {
    matches!(ch as u32,
        0x2E80..=0x2FFF      // CJK 部首补充 / 康熙部首
        | 0x3040..=0x30FF    // 平假名 / 片假名
        | 0x3400..=0x4DBF    // CJK 统一表意文字扩展 A
        | 0x4E00..=0x9FFF    // CJK 统一表意文字
        | 0xAC00..=0xD7AF    // 谚文音节
        | 0xF900..=0xFAFF    // CJK 兼容表意文字
        | 0x20000..=0x3FFFF  // CJK 扩展 B 及以后
    )
}

/// 文本里是否有中日韩文字：决定排版时要不需要加一个 CJK 字体族。
///
/// 范围沿用 [`is_cjk_script`]（表意文字 / 假名 / 谚文）；全角标点不算——它们几乎只与汉字
/// 同时出现，判「这段文字要不要中文字体」用这个就够。
pub fn has_cjk(text: &str) -> bool {
    text.chars().any(is_cjk_script)
}

/// 代码围栏：比内容里最长的反引号连串再多一个，至少 3 个。
///
/// 围栏必须比内容里任何反引号连串都长，否则内容会提前闭合围栏。返回的只是围栏本身，
/// 调用方在代码前后各用一次（Typst 与 Markdown 的围栏规则相同）。
pub fn code_fence(content: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for ch in content.chars() {
        if ch == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat((longest + 1).max(3))
}

/// 在 `specials` 里出现的字符前加一个反斜杠，其余字符原样保留。
///
/// 给「特殊字符按前缀转义」的语法共用（Markdown / Typst / 正则都长这样）：
/// 特殊字符集合由调用方提供，本函数不认任何具体语法。
pub fn escape_special(text: &str, specials: &[char]) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if specials.contains(&ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// 把文本切成「连续 CJK」与「连续非 CJK」的交替片段，返回 `(片段, 是否为 CJK)`。
///
/// 中英混写（`缓存bbc`）只有拆开才能各自按规则处理；空片段不会出现在结果里。
pub fn split_cjk_runs(text: &str) -> Vec<(&str, bool)> {
    let mut runs: Vec<(&str, bool)> = Vec::new();
    let mut start = 0usize;
    let mut current: Option<bool> = None;
    for (index, ch) in text.char_indices() {
        let cjk = is_cjk_script(ch);
        match current {
            Some(prev) if prev == cjk => {}
            Some(prev) => {
                runs.push((&text[start..index], prev));
                start = index;
                current = Some(cjk);
            }
            None => current = Some(cjk),
        }
    }
    if let Some(cjk) = current {
        runs.push((&text[start..], cjk));
    }
    runs
}

/// 一段连续 CJK 文本的词元：单字保留单字，多字按相邻二字组合（bigram）拆开。
///
/// 例：`缓存失效` → `缓存` / `存失` / `失效`。跨词边界的组合（`存失`）是噪声，但
/// 不匹配时不计分，代价远小于「整段连写导致一个词元都命中不了」。
pub fn cjk_bigrams(run: &str) -> Vec<String> {
    let chars: Vec<char> = run.chars().collect();
    match chars.len() {
        0 => Vec::new(),
        1 => vec![run.to_string()],
        _ => chars
            .windows(2)
            .map(|pair| pair.iter().collect::<String>())
            .collect(),
    }
}

/// 把查询切成词元：按非字母数字分割、小写化、去停用词与单字符 ASCII 词、去重。
///
/// 最多返回 `max_terms` 个词元（上限由调用方决定，防止超长查询把打分拖慢）。
/// 连续 CJK 串（中日韩文字没有空格）先按相邻二字组合拆开，见 [`cjk_bigrams`]；
/// 返回空向量表示查询里没有可用的关键词。
pub fn terms(query: &str, max_terms: usize) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for raw in query.split(|c: char| !c.is_alphanumeric()) {
        for (run, cjk) in split_cjk_runs(raw) {
            let candidates = if cjk {
                cjk_bigrams(run)
            } else {
                vec![run.to_lowercase()]
            };
            for term in candidates {
                push_term(term, &mut terms);
                if terms.len() >= max_terms {
                    return terms;
                }
            }
        }
    }
    terms
}

/// 往词元表里放一个词元：去空串、去停用词、去单字符 ASCII 词、去重。
fn push_term(term: String, terms: &mut Vec<String>) {
    if term.is_empty() || is_stop_word(&term) || is_stop_pair(&term) {
        return;
    }
    // 单个 ASCII 字符（`a` / `c`，通常来自 `C++` 这类写法）检索价值太低
    if term.is_ascii() && term.chars().count() < 2 {
        return;
    }
    if terms.iter().any(|seen| seen == &term) {
        return;
    }
    terms.push(term);
}

/// 词元是否在停用词表里（中文词元就是它自己）。
fn is_stop_word(term: &str) -> bool {
    STOP_WORDS.contains(&term)
}

/// 单个字符是否为中文单字虚词。
fn is_stop_char(ch: char) -> bool {
    let mut buf = [0u8; 4];
    is_stop_word(ch.encode_utf8(&mut buf))
}

/// 二字 CJK 词元是否由两个单字虚词组成（`的了` / `对于` 这类没有独立检索价值）。
///
/// 整段虚词连写（`的了吗呢`）只会切出这种组合，识别不出就只能当成有效查询。
fn is_stop_pair(term: &str) -> bool {
    let mut chars = term.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(first), Some(second), None) => {
            is_cjk_script(first)
                && is_cjk_script(second)
                && is_stop_char(first)
                && is_stop_char(second)
        }
        _ => false,
    }
}

/// 找出 `needles` 里最早出现的一个在 `text` 中的**字符**下标；一个都没命中返回 `None`。
///
/// 按下标化（不区分大小写）匹配，返回的是**原文**的字符下标。
pub fn first_match_index(text: &str, needles: &[String]) -> Option<usize> {
    if needles.is_empty() {
        return None;
    }
    let folded = Folded::new(text);
    let byte = needles
        .iter()
        .filter_map(|needle| folded.lower.find(needle.as_str()))
        .min()?;
    // 小写下标 → 原文字节 → 原文字符下标（窗口按原文字符数算）
    let lower_char = folded.lower[..byte].chars().count();
    let original_byte = folded.origin[lower_char].0;
    Some(text[..original_byte].chars().count())
}

/// 在 `text` 中按**小写化**（不区分大小写）查找 `needle` 的全部出现位置。
///
/// `needle` 需已是小写形式；返回的区间是**原文**的字节区间（起含止不含），可直接拿去切原文。
/// 小写化可能改变字节/字符数（`İ` → `i̇`），所以偏移必须经 [`Folded`] 的换算表映射回原文，
/// 不能直接拿小写串上的偏移去切原文。`needle` 为空时返回空。
pub fn case_insensitive_matches(text: &str, needle: &str) -> Vec<(usize, usize)> {
    let folded = Folded::new(text);
    let mut matches = Vec::new();
    if needle.is_empty() {
        return matches;
    }

    // 逐次向右推进：`chars_before` 是 `lower[..search_from]` 的字符数，摊还 O(n)
    let mut search_from = 0usize;
    let mut chars_before = 0usize;
    while let Some(found) = folded.lower[search_from..].find(needle) {
        let start = search_from + found;
        let end = start + needle.len();
        let start_char = chars_before + folded.lower[search_from..start].chars().count();
        let end_char = start_char + needle.chars().count();
        // 匹配区间覆盖的小写字符都在同一个原文区间里：取首字符的起、末字符的止
        matches.push((folded.origin[start_char].0, folded.origin[end_char - 1].1));
        chars_before = end_char;
        search_from = end;
    }
    matches
}

/// 小写化后的文本 + 「小写字符 → 原文字节区间」的换算表。
///
/// 一个原文字符可能小写成多个字符（`İ` → `i̇`），因此表按**字符**不是字节存，
/// 多个小写字符会共享同一个原文区间。
struct Folded {
    /// 小写化（`to_lowercase`）后的文本，与 `text` 逐字符对应但长度可能不同
    lower: String,
    /// 下标 = `lower` 的字符下标，值 = 产生该字符的原文字节的 `(起, 止)`
    origin: Vec<(usize, usize)>,
}

impl Folded {
    /// 对 `text` 做一次小写化并记下每个小写字符的来源区间。
    fn new(text: &str) -> Self {
        let mut lower = String::with_capacity(text.len());
        let mut origin: Vec<(usize, usize)> = Vec::with_capacity(text.len() + 1);
        for (byte, ch) in text.char_indices() {
            for lowered in ch.to_lowercase() {
                origin.push((byte, byte + ch.len_utf8()));
                lower.push(lowered);
            }
        }
        Folded { lower, origin }
    }
}

/// 从 `text` 里截出包含首个命中位置的一段，两端按需补省略号。
///
/// `needles` 为空（或都没命中）时退化为取文本开头，供「只看相邻内容」的场景使用；
/// `text` 应是已压成单行的文本。命中点落在窗口左侧 1/3 处，前后都能看到一点上下文。
pub fn excerpt_containing(text: &str, needles: &[String], max_chars: usize) -> String {
    let total = text.chars().count();
    if total == 0 {
        return String::new();
    }

    let start_char = match first_match_index(text, needles) {
        Some(found) => found.saturating_sub(max_chars / 3),
        None => 0,
    };
    let end_char = (start_char + max_chars).min(total);

    let mut out: String = text
        .chars()
        .skip(start_char)
        .take(end_char - start_char)
        .collect();
    if start_char > 0 {
        out.insert(0, '…');
    }
    if end_char < total {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用词元上限（与 `session_search` 的默认值一致）。
    const MAX_TERMS: usize = 16;

    /// 围栏要比内容里最长的反引号连串长，最短 3 个。
    #[test]
    fn code_fence_outgrows_inner_backtick_runs() {
        assert_eq!(code_fence("plain"), "```");
        assert_eq!(code_fence("a ` b"), "```");
        assert_eq!(code_fence("```"), "````");
        assert_eq!(code_fence("x ```` y"), "`````");
    }

    /// 只有列在 `specials` 里的字符才加反斜杠。
    #[test]
    fn escape_special_prefixes_listed_chars_only() {
        assert_eq!(escape_special("a#b_c", &['#', '_']), "a\\#b\\_c");
        assert_eq!(escape_special("no specials", &['#']), "no specials");
        assert_eq!(escape_special("q\"uote\\", &['"', '\\']), "q\\\"uote\\\\");
    }

    /// CJK 判定：中文 / 假名 / 谚文命中，纯西文不命中。
    #[test]
    fn has_cjk_detects_east_asian_scripts() {
        assert!(has_cjk("中文"));
        assert!(has_cjk("カタカナ"));
        assert!(has_cjk("한국어"));
        assert!(has_cjk("mixed 中文 text"));
        assert!(!has_cjk("plain ascii only"));
        assert!(!has_cjk(""));
    }

    #[test]
    fn terms_drop_stop_words_single_chars_and_duplicates() {
        assert_eq!(
            terms("How do I fix the Cache cache?", MAX_TERMS),
            vec!["fix".to_string(), "cache".to_string()]
        );
        // 单个 ASCII 字符（C++ 会切成 c）没有检索价值，中文单字保留
        assert_eq!(terms("C++ 改", MAX_TERMS), vec!["改".to_string()]);
        assert!(terms("the and of", MAX_TERMS).is_empty());
    }

    #[test]
    fn terms_split_cjk_into_bigrams() {
        assert_eq!(
            terms("缓存失效", MAX_TERMS),
            vec!["缓存".to_string(), "存失".to_string(), "失效".to_string()]
        );
        // 单字 CJK 保留（过滤只针对单字符 ASCII）
        assert_eq!(terms("改", MAX_TERMS), vec!["改".to_string()]);
        // 中英混写在同一片段里也能各自切好
        assert_eq!(
            terms("缓存BBC", MAX_TERMS),
            vec!["缓存".to_string(), "bbc".to_string()]
        );
    }

    #[test]
    fn chinese_terms_keep_distinctive_words() {
        let tokens = terms("如何修复缓存失效的问题", MAX_TERMS);
        assert!(tokens.contains(&"缓存".to_string()), "{tokens:?}");
        assert!(tokens.contains(&"失效".to_string()), "{tokens:?}");
        assert!(tokens.contains(&"修复".to_string()), "{tokens:?}");
        assert!(!tokens.contains(&"如何".to_string()), "中文虚词应被过滤");
        // 只剩虚词的查询必须被清空（调用方据此报错）
        assert!(terms("的了吗呢", MAX_TERMS).is_empty());
    }

    #[test]
    fn terms_are_capped() {
        let query = (0..40)
            .map(|i| format!("term{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(terms(&query, MAX_TERMS).len(), MAX_TERMS);
    }

    #[test]
    fn cjk_runs_split_mixed_scripts() {
        assert_eq!(
            split_cjk_runs("缓存bbc失效"),
            vec![("缓存", true), ("bbc", false), ("失效", true)]
        );
        assert!(split_cjk_runs("").is_empty());
        assert_eq!(split_cjk_runs("plain"), vec![("plain", false)]);
    }

    #[test]
    fn excerpt_windows_around_the_match() {
        let needles = terms("needle", MAX_TERMS);
        let text = format!("{} needle {}", "x".repeat(500), "y".repeat(500));
        let out = excerpt_containing(&text, &needles, 60);
        assert!(out.contains("needle"), "{out}");
        assert!(out.starts_with('…') && out.ends_with('…'), "{out}");
        assert!(out.chars().count() <= 62, "{}", out.chars().count());
    }

    #[test]
    fn excerpt_without_needles_takes_the_head() {
        let out = excerpt_containing("short text", &[], 40);
        assert_eq!(out, "short text");
        let long = "z".repeat(100);
        let out = excerpt_containing(&long, &[], 10);
        assert_eq!(out.chars().count(), 11, "截断后补一个省略号：{out}");
        assert!(out.ends_with('…'));
    }

    #[test]
    fn excerpt_locates_matches_even_if_lowercasing_changes_length() {
        // İ 小写化成两个字符：小写串的下标不能直接拿去切原文
        let text = format!("{}缓存失效{}", "İ".repeat(30), "x".repeat(30));
        let out = excerpt_containing(&text, &terms("缓存失效", MAX_TERMS), 20);
        assert!(out.contains("缓存失效"), "{out}");
    }

    #[test]
    fn first_match_index_picks_the_earliest_needle() {
        let needles = vec!["later".to_string(), "early".to_string()];
        assert_eq!(first_match_index("early then later", &needles), Some(0));
        assert_eq!(first_match_index("nothing here", &needles), None);
        assert_eq!(first_match_index("anything", &[]), None);
    }

    #[test]
    fn case_insensitive_matches_report_original_byte_ranges() {
        let text = "İİİNEEDLE tail";
        let ranges = case_insensitive_matches(text, "needle");
        assert_eq!(ranges.len(), 1);
        assert_eq!(&text[ranges[0].0..ranges[0].1], "NEEDLE", "{ranges:?}");

        // 多次出现：每个区间都能切回原文，且升序
        let text = "Cache cache CACHE";
        let ranges = case_insensitive_matches(text, "cache");
        assert_eq!(ranges.len(), 3, "{ranges:?}");
        for (start, end) in &ranges {
            assert_eq!(text[*start..*end].to_lowercase(), "cache");
        }
        assert!(ranges.windows(2).all(|w| w[0].0 < w[1].0));

        assert!(case_insensitive_matches("anything", "").is_empty());
        assert!(case_insensitive_matches("anything", "absent").is_empty());
    }
}
