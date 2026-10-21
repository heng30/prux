//! bash 命令的静态分析：切分语句、追踪 `cd`、提取路径目标、识别无法静态判定的包装。
//!
//! 这里**不追求** shell 语法的完整实现，目标只有一个：给出「这条命令可能碰到哪些路径、
//! 它匹配哪段命令文本」，并且**在拿不准时明确说拿不准**（`indirect` / `complete = false`），
//! 由上层按 fail-closed 处理。误判的方向必须是「多拦」，不是「放过」。
//!
//! 支持的形状：
//! - 引号（`'…'` 字面、`"…"` 插值）与反斜杠转义；
//! - 语句分隔 `;` `&&` `||` `|` `|&` `&` 与换行；
//! - 重定向 `>` `>>` `<` `<<<` `2>` 等（`<<` 的 body 由 [`strip_heredocs`] 单独摘掉）；
//! - 命令替换 `$(…)`、反引号，子 shell `( … )`——内部命令**也**参与求值；
//! - 前导环境变量赋值（`FOO=bar cmd`）在匹配前剥掉；
//! - 字面 `cd` 会移动后续命令的工作目录。
//!
//! 明确**不**下降的地方：`if`/`while`/`for`/`case` 与 `{ … }` 花括号组的内容按外层语句
//! 的文本一起匹配（不做结构解析）。

use super::policy::Direction;
use crate::utils::paths::{absolutize, expand_home};
use std::path::{Path, PathBuf};

/// 一条命令的来源；决定拦截文案里怎么解释「为什么这条也被检查了」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Origin {
    /// 顶层语句（`&&` / `;` / `|` 切分出来的那一段）。
    TopLevel,
    /// 命令替换 `$(…)` / 反引号里的命令。
    Substitution,
    /// 子 shell `( … )` 里的命令。
    Subshell,
    /// 未加引号的 heredoc body 里的命令替换。
    HeredocBody,
}

impl Origin {
    /// 拦截文案里用的来源说明（顶层为空串，其余给出嵌套语境）。
    pub(super) fn note(self) -> &'static str {
        match self {
            Origin::TopLevel => "",
            Origin::Substitution => "inside command substitution",
            Origin::Subshell => "inside subshell",
            Origin::HeredocBody => "inside heredoc body",
        }
    }
}

/// 一个命令行里的单词。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Word {
    /// 去掉引号后的文本（`"a b"` → `a b`，单个单词里仍可含空格）。
    pub text: String,
    /// 是否带过引号；带引号的单词里出现的 `*` / `?` 不是通配符。
    pub quoted: bool,
}

/// 一条被门控的命令。
#[derive(Debug, Clone)]
pub(super) struct Command {
    /// 去掉引号、剥掉环境变量前缀后按空格拼接的文本，用于 `bash` 表面的模式匹配。
    pub text: String,
    /// 单词序列（同样已剥掉环境变量前缀）。
    pub words: Vec<Word>,
    /// 来源语境。
    pub origin: Origin,
    /// 重定向目标：`(路径, 方向)`。
    pub redirects: Vec<(String, Direction)>,
    /// 执行这条命令时的工作目录（被前面的字面 `cd` 移动过）。
    pub cwd: PathBuf,
    /// 工作目录是否**不可知**（前面出现过非字面 `cd`）；为真时不再做绝对拼写匹配。
    pub cwd_unknown: bool,
}

impl Command {
    /// 这条命令是否触碰写了某个路径的参数（用于把路径 token 判成写方向）。
    fn writes_paths(&self) -> bool {
        self.words
            .first()
            .map(|w| write_effect_command(&w.text))
            .unwrap_or(false)
    }

    /// 参与 `path` / `external_directory` 层求值的路径目标：`(拼写, 方向)`。
    ///
    /// 重定向目标按自身方向；其余路径形参数按命令效果决定读或写。
    pub(super) fn path_targets(&self) -> Vec<(String, Direction)> {
        let mut out: Vec<(String, Direction)> = Vec::new();
        out.extend(self.redirects.iter().cloned());

        let write = self.writes_paths();
        for word in &self.words {
            if !path_shape_or_existing(&word.text, &self.cwd) {
                continue;
            }
            // 重定向目标已经单独记过，别重复计一次读。
            if self.redirects.iter().any(|(p, _)| p == &word.text) {
                continue;
            }
            let dir = if write {
                Direction::Write
            } else {
                Direction::Read
            };
            out.push((word.text.clone(), dir));
        }
        out
    }

    /// 命令的「绝对拼写」：把所有路径形参数替换成按工作目录归一化后的绝对路径。
    ///
    /// 工作目录不可知时返回 `None`（上游同样不做这一层匹配）。
    pub(super) fn absolute_text(&self) -> Option<String> {
        if self.cwd_unknown {
            return None;
        }
        let mut changed = false;
        let mut parts: Vec<String> = Vec::with_capacity(self.words.len());
        for (idx, word) in self.words.iter().enumerate() {
            // 上游把「所有路径参数」一次性替换成绝对拼写；这里放宽到所有非选项、非通配、
            // 非变量的词，让 `rm build` 也能匹配 `rm /repo/build`。多出来的候选串只会
            // 让判定更严格（fail-closed 方向），不会放宽。第 0 个词是命令名，不替换。
            let absolutizable = idx > 0
                && !word.text.starts_with('-')
                && !word.text.contains(['*', '?', '[', '$', '`'])
                && !word.text.is_empty();
            if absolutizable {
                let abs = absolutize(&word.text, &self.cwd);
                let abs = abs.to_string_lossy().to_string();
                if abs != word.text {
                    changed = true;
                    parts.push(abs);
                    continue;
                }
            }
            parts.push(word.text.clone());
        }
        changed.then(|| parts.join(" "))
    }
}

/// 一次扫描的完整结果。
#[derive(Debug, Clone, Default)]
pub(super) struct Scan {
    /// 顶层命令（按出现顺序，已应用 `cd` 追踪）。
    pub commands: Vec<Command>,
    /// 嵌套命令（命令替换 / 子 shell / heredoc body），同样参与求值。
    pub nested: Vec<Command>,
    /// 无法静态判定的包装证据，如 `bash -c`、`sudo`、`xargs`；非空时整条调用按 ask 处理。
    pub indirect: Vec<String>,
    /// 引号 / 括号是否平衡。为假表示解析没走完，上层按 fail-closed 处理。
    pub complete: bool,
}

impl Scan {
    /// 全部需要求值的命令（顶层 + 嵌套）。
    pub(super) fn all_commands(&self) -> impl Iterator<Item = &Command> {
        self.commands.iter().chain(self.nested.iter())
    }
}

/// 扫描一段 bash 命令文本。
pub(super) fn scan(input: &str) -> Scan {
    let (stripped, heredocs) = strip_heredocs(input);
    let mut scanner = Scanner::new(&stripped);
    let mut commands = scanner.parse_statements(0, Origin::TopLevel);
    let complete = scanner.complete;

    // heredoc：加了引号的分隔符不插值，body 是纯文本；未加引号的 body 里的命令替换要检查。
    let mut nested: Vec<Command> = std::mem::take(&mut scanner.nested);
    let mut scanned_body: Vec<Command> = Vec::new();
    for body in &heredocs {
        if !body.interpolating {
            continue;
        }
        let mut inner = Scanner::new(&body.text);
        let mut found = inner.parse_statements(0, Origin::HeredocBody);
        found.extend(inner.nested);
        scanned_body.extend(found);
    }

    // `cd` 追踪只对顶层语句有意义（嵌套命令沿用外层此刻的工作目录）。
    apply_cd_tracking(&mut commands, &mut nested);

    nested.extend(scanned_body);
    Scan {
        commands,
        nested,
        indirect: scanner.indirect,
        complete,
    }
}

/// 把每条命令的工作目录按字面 `cd` 推进（顶层顺序执行，`&&`/`;`/`|` 都按顺序看）。
///
/// 非字面 `cd`（`cd "$DIR"`、`cd -`、裸 `cd`）之后工作目录不可知：后续命令标记
/// `cwd_unknown`，不再做绝对拼写匹配（只按原文匹配），但**路径检查照旧**。
fn apply_cd_tracking(top: &mut [Command], nested: &mut [Command]) {
    let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut unknown = false;
    for cmd in top.iter_mut() {
        cmd.cwd = cwd.clone();
        cmd.cwd_unknown = unknown;
        if let Some(effect) = cd_effect(cmd) {
            match effect {
                Some(target) => {
                    let home = expand_home(&target);
                    cwd = absolutize(&home, &cwd);
                    unknown = false;
                }
                // 是 cd 但目标不是字面量：之后的工作目录不可知。
                None => unknown = true,
            }
        }
    }
    for cmd in nested.iter_mut() {
        cmd.cwd = cwd.clone();
        cmd.cwd_unknown = unknown;
    }
}

/// 判断一条命令是不是 `cd`。
///
/// - `None`：不是 `cd`，工作目录不变；
/// - `Some(Some(target))`：是 `cd` 且目标是字面量，可以推进工作目录；
/// - `Some(None)`：是 `cd` 但目标不可知（`cd "$DIR"`、`cd -`、裸 `cd`），
///   之后的命令工作目录未知，不能再做绝对拼写匹配。
fn cd_effect(cmd: &Command) -> Option<Option<String>> {
    let first = cmd.words.first()?;
    if first.text != "cd" && first.text != "pushd" {
        return None;
    }
    Some(match cmd.words.get(1) {
        Some(w) if !w.text.contains('$') && !w.text.contains('`') && w.text != "-" => {
            Some(w.text.clone())
        }
        _ => None,
    })
}

/// 一个 heredoc body。
#[derive(Debug, Clone)]
struct Heredoc {
    /// body 原文（不含结束分隔符那一行）。
    text: String,
    /// 分隔符是否未加引号（未加引用时 body 会做参数/命令替换）。
    interpolating: bool,
}

/// 把 heredoc body 从命令文本里摘掉，返回「只剩命令」的文本与全部 body。
///
/// `<<` / `<<-` 后面的分隔符若被引号包住（`<<'EOF'`），body 是纯文本，不参与求值；
/// 否则 body 里的 `$(…)` / 反引号会真的执行，需要单独扫描。
///
/// 同一行可以挂多个 heredoc（`cat <<A <<B`），按出现顺序各自吞掉自己的 body。
fn strip_heredocs(input: &str) -> (String, Vec<Heredoc>) {
    let mut out_lines: Vec<String> = Vec::new();
    let mut bodies: Vec<Heredoc> = Vec::new();
    let mut lines = input.lines().peekable();

    while let Some(line) = lines.next() {
        let pending = heredoc_delimiters(line);
        out_lines.push(line.to_string());
        if pending.is_empty() {
            continue;
        }

        for (delimiter, strip_tabs, quoted) in pending {
            let mut body = String::new();
            for next in lines.by_ref() {
                let probe = if strip_tabs {
                    next.trim_start_matches('\t')
                } else {
                    next
                };
                if probe.trim_end() == delimiter {
                    break;
                }
                body.push_str(next);
                body.push('\n');
            }
            if !quoted {
                bodies.push(Heredoc {
                    text: body,
                    interpolating: true,
                });
            }
        }
    }

    (out_lines.join("\n"), bodies)
}

/// 找出这一行里的 heredoc 分隔符：`(分隔符, 是否 `<<-`)`。
///
/// 只认同「引号之外」的 `<<`；`<<<`（here-string）不算 heredoc。
fn heredoc_delimiters(line: &str) -> Vec<(String, bool, bool)> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut quote: Option<char> = None;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
                i += 1;
                continue;
            }
            None => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                    i += 1;
                    continue;
                }
                if c == '\\' {
                    i += 2;
                    continue;
                }
            }
        }
        if c == '<' && chars.get(i + 1) == Some(&'<') {
            // `<<<` 是 here-string，不是 heredoc。
            if chars.get(i + 2) == Some(&'<') {
                i += 3;
                continue;
            }
            let strip_tabs = chars.get(i + 2) == Some(&'-');
            let mut j = if strip_tabs { i + 3 } else { i + 2 };
            while chars.get(j).is_some_and(|c| c.is_whitespace()) {
                j += 1;
            }
            // 分隔符是否带引号：决定 body 是否会做命令替换。
            let quoted = chars.get(j).is_some_and(|c| *c == '\'' || *c == '"');
            let (word, next) = read_shell_word(&chars, j);
            if !word.is_empty() {
                out.push((word, strip_tabs, quoted));
            }
            i = next.max(i + 2);
            continue;
        }
        i += 1;
    }
    out
}

/// 读取一个 shell 单词（处理引号与转义），返回 `(去掉引号后的文本, 结束后下标)`。
fn read_shell_word(chars: &[char], start: usize) -> (String, usize) {
    let mut text = String::new();
    let mut i = start;
    let mut quote: Option<char> = None;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if c == '\\' && q == '"' {
                    if let Some(next) = chars.get(i + 1) {
                        text.push(*next);
                        i += 2;
                        continue;
                    }
                } else {
                    text.push(c);
                }
                i += 1;
            }
            None => {
                if c.is_whitespace() || ";&|<>()".contains(c) {
                    break;
                }
                if c == '\'' || c == '"' {
                    quote = Some(c);
                    i += 1;
                    continue;
                }
                if c == '\\'
                    && let Some(next) = chars.get(i + 1)
                {
                    text.push(*next);
                    i += 2;
                    continue;
                }
                text.push(c);
                i += 1;
            }
        }
    }
    (text, i)
}

/// 逐字符扫描器：把命令文本切成语句与单词，并递归处理替换与子 shell。
struct Scanner {
    /// 输入字符序列。
    chars: Vec<char>,
    /// 当前扫描位置。
    i: usize,
    /// 嵌套命令（命令替换 / 子 shell）。
    nested: Vec<Command>,
    /// 无法静态判定的包装证据。
    indirect: Vec<String>,
    /// 引号与括号是否平衡。
    complete: bool,
}

impl Scanner {
    /// 用给定文本构造扫描器。
    fn new(input: &str) -> Self {
        Scanner {
            chars: input.chars().collect(),
            i: 0,
            nested: Vec::new(),
            indirect: Vec::new(),
            complete: true,
        }
    }

    /// 解析语句序列；`depth > 0` 表示身处子 shell，遇到 `)` 返回。
    ///
    /// 宏在每个 `flush_command!()` 里重置 `cur_quoted` / `pending_redirect`，收尾那次
    /// 赋值之后不再被读取，故允许未使用赋值。
    #[allow(unused_assignments)]
    fn parse_statements(&mut self, depth: usize, origin: Origin) -> Vec<Command> {
        let mut out: Vec<Command> = Vec::new();
        let mut words: Vec<Word> = Vec::new();
        let mut redirects: Vec<(String, Direction)> = Vec::new();
        let mut cur = String::new();
        let mut cur_quoted = false;
        let mut quote: Option<char> = None;
        // 上一个 `>` / `>>` / `<` 期望的下一个单词是重定向目标。
        let mut pending_redirect: Option<Direction> = None;

        macro_rules! flush_word {
            () => {
                if !cur.is_empty() || cur_quoted {
                    let text = std::mem::take(&mut cur);
                    let word = Word {
                        text,
                        quoted: cur_quoted,
                    };
                    cur_quoted = false;
                    if let Some(dir) = pending_redirect.take() {
                        redirects.push((word.text.clone(), dir));
                    }
                    words.push(word);
                }
            };
        }

        macro_rules! flush_command {
            () => {
                flush_word!();
                pending_redirect = None;
                if !words.is_empty() {
                    let words = std::mem::take(&mut words);
                    if let Some(evidence) = detect_wrapper(&words) {
                        self.indirect.push(evidence);
                    }
                    let redirects = std::mem::take(&mut redirects);
                    out.push(build_command(words, redirects, origin));
                }
            };
        }

        while self.i < self.chars.len() {
            let c = self.chars[self.i];

            if let Some(q) = quote {
                match c {
                    '\\' if q == '"' => {
                        if let Some(next) = self.chars.get(self.i + 1) {
                            cur.push(*next);
                            self.i += 2;
                            continue;
                        }
                        self.i += 1;
                    }
                    _ if c == q => {
                        quote = None;
                        cur_quoted = true;
                        self.i += 1;
                    }
                    '$' if q == '"' && self.chars.get(self.i + 1) == Some(&'(') => {
                        self.read_substitution();
                    }
                    '`' if q == '"' => self.read_backtick(),
                    _ => {
                        cur.push(c);
                        self.i += 1;
                    }
                }
                continue;
            }

            match c {
                '\'' | '"' => {
                    quote = Some(c);
                    cur_quoted = true;
                    self.i += 1;
                }
                '\\' => {
                    if let Some(next) = self.chars.get(self.i + 1) {
                        cur.push(*next);
                        self.i += 2;
                    } else {
                        self.i += 1;
                    }
                }
                ' ' | '\t' | '\r' => {
                    flush_word!();
                    self.i += 1;
                }
                '\n' => {
                    flush_command!();
                    self.i += 1;
                }
                ';' | '&' | '|' => {
                    // `;;`、`&&`、`||`、`|&` 都按一次分隔处理。
                    flush_command!();
                    self.i += 1;
                    while self
                        .chars
                        .get(self.i)
                        .is_some_and(|c| matches!(c, ';' | '&' | '|'))
                    {
                        self.i += 1;
                    }
                }
                '(' => {
                    // 语句起点的 `(` 视为子 shell；否则当作子 shell 一样递归（保守）。
                    flush_word!();
                    self.i += 1;
                    let inner = self.parse_statements(depth + 1, Origin::Subshell);
                    out.extend(inner);
                }
                ')' => {
                    flush_command!();
                    self.i += 1;
                    if depth > 0 {
                        self.complete = true;
                        return out;
                    }
                    // 顶层多出来的 `)`：语法错误，标记不完整交给上层 fail-closed。
                    self.complete = false;
                }
                '>' | '<' => {
                    // `2>` 里的 fd 前缀不是单词内容。
                    if !cur.is_empty() && cur.chars().all(|c| c.is_ascii_digit()) {
                        cur.clear();
                    }
                    flush_word!();
                    let dir = if c == '>' {
                        Direction::Write
                    } else {
                        Direction::Read
                    };
                    self.i += 1;
                    // 吃掉 `>>`、`>&`、`<&`、`<<<`、`<<` 的第二三个字符（操作符本身不需要保留）。
                    while self
                        .chars
                        .get(self.i)
                        .is_some_and(|c| matches!(c, '>' | '<' | '&'))
                    {
                        self.i += 1;
                    }
                    pending_redirect = Some(dir);
                }
                '$' if self.chars.get(self.i + 1) == Some(&'(') => {
                    self.read_substitution();
                    // 命令替换本身作为一个单词的片段参与外层文本。
                    cur.push_str("$(...)");
                }
                '`' => {
                    self.read_backtick();
                    cur.push_str("`...`");
                }
                _ => {
                    cur.push(c);
                    self.i += 1;
                }
            }
        }

        flush_command!();
        self.complete = self.complete && quote.is_none();
        out
    }

    /// 读取 `$(…)` 内部并递归扫描；位置停在右括号之后。
    fn read_substitution(&mut self) {
        // 跳过 `$`
        self.i += 1;
        debug_assert_eq!(self.chars.get(self.i), Some(&'('));
        let start = self.i + 1;
        let mut depth = 1usize;
        let mut j = start;
        let mut quote: Option<char> = None;
        while j < self.chars.len() {
            let c = self.chars[j];
            match quote {
                Some(q) => {
                    if c == q {
                        quote = None;
                    }
                }
                None => match c {
                    '\'' | '"' => quote = Some(c),
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                },
            }
            j += 1;
        }
        if depth != 0 {
            self.complete = false;
            self.i = self.chars.len();
            return;
        }
        let inner: String = self.chars[start..j].iter().collect();
        let mut sub = Scanner::new(&inner);
        let mut found = sub.parse_statements(0, Origin::Substitution);
        found.extend(sub.nested);
        self.nested.extend(found);
        self.indirect.extend(sub.indirect);
        self.i = j + 1;
    }

    /// 读取反引号内部并递归扫描。
    fn read_backtick(&mut self) {
        let start = self.i + 1;
        let mut j = start;
        while j < self.chars.len() && self.chars[j] != '`' {
            if self.chars[j] == '\\' {
                j += 1;
            }
            j += 1;
        }
        if j >= self.chars.len() {
            self.complete = false;
            self.i = self.chars.len();
            return;
        }
        let inner: String = self.chars[start..j].iter().collect();
        let mut sub = Scanner::new(&inner);
        let mut found = sub.parse_statements(0, Origin::Substitution);
        found.extend(sub.nested);
        self.nested.extend(found);
        self.indirect.extend(sub.indirect);
        self.i = j + 1;
    }
}

/// 由单词与重定向构造命令：剥掉环境变量前缀，生成用于匹配的命令文本。
///
/// 包装识别不在这里做——证据汇总在 [`Scanner::indirect`]（见 `flush_command` 宏），
/// 因为同一层命令可能有多条包装。
fn build_command(words: Vec<Word>, redirects: Vec<(String, Direction)>, origin: Origin) -> Command {
    let words = strip_env_prefix(words);
    let text = words
        .iter()
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    Command {
        text,
        words,
        origin,
        redirects,
        cwd: PathBuf::from("."),
        cwd_unknown: false,
    }
}

/// 剥掉前导的环境变量赋值（`FOO=bar BAZ=1 cmd …` → `cmd …`）。
fn strip_env_prefix(words: Vec<Word>) -> Vec<Word> {
    let mut idx = 0usize;
    for word in &words {
        if is_env_assignment(&word.text) {
            idx += 1;
        } else {
            break;
        }
    }
    if idx == 0 {
        words
    } else {
        words[idx..].to_vec()
    }
}

/// `NAME=value` 形式的环境变量赋值（NAME 是合法标识符）。
fn is_env_assignment(text: &str) -> bool {
    let Some((name, _)) = text.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// 识别「隐藏了被门控命令」的包装；返回证据文案。
///
/// 透明包装（`env` / `nohup` / `timeout` / `nice` / `command` / `exec` 等）由
/// [`unwrap_transparent`] 剥掉后继续分析，这里只报**不透明**的那些：它们的参数是
/// 代码或转交给别的解释器，静态看不出来。
fn detect_wrapper(words: &[Word]) -> Option<String> {
    let first = words.first()?.text.as_str();
    let second = words.get(1).map(|w| w.text.as_str()).unwrap_or("");

    match first {
        // shell 以 `-c` 执行字符串；不带 `-c` 从 stdin 读脚本，同样看不出来。
        "bash" | "sh" | "zsh" | "dash" | "ksh" | "fish" | "csh" | "tcsh" | "ash" | "mksh"
        | "pwsh" | "powershell" => Some(format!("{first} (shell wrapper)")),
        "eval" => Some("eval".to_string()),
        "sudo" | "doas" | "su" | "runuser" => Some(format!("{first} (privilege wrapper)")),
        "xargs" | "parallel" => Some(format!("{first} (argument fan-out)")),
        "ssh" | "nsenter" | "unshare" | "chroot" | "systemd-run" | "script" | "watch" => {
            Some(format!("{first} (remote/wrapped execution)"))
        }
        "docker" | "podman" | "kubectl" => Some(format!("{first} (container exec)")),
        // 解释器：`-c` / `-e` 之后的字符串是代码。
        "python" | "python3" | "node" | "ruby" | "perl" | "php" | "lua" | "deno" => {
            Some(format!("{first} (interpreter)"))
        }
        // `sed`/`awk` 的脚本体可能执行命令。
        "awk" | "gawk" | "sed" => Some(format!("{first} (script body)")),
        "find"
            if words
                .iter()
                .any(|w| matches!(w.text.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir")) =>
        {
            Some("find -exec".to_string())
        }
        "git" if second == "alias" => Some("git alias".to_string()),
        _ => None,
    }
}

/// 剥掉透明包装，返回剩余的命令文本（空表示剥完什么都不剩）。
///
/// 这些包装只是「把后面的命令跑起来」，剥掉后照常求值比整条 ask 更精确：
/// `timeout 10 npm test` 应当按 `npm test` 的规则判。
pub(super) fn unwrap_transparent(text: &str) -> Option<String> {
    let mut rest = text.trim().to_string();
    loop {
        let words: Vec<String> = rest.split_whitespace().map(str::to_string).collect();
        let first = words.first()?;
        let skip = match first.as_str() {
            "env" | "nohup" | "command" | "builtin" | "exec" | "time" | "setsid" | "stdbuf"
            | "unbuffer" => skip_transparent_prefix(&words),
            "nice" | "ionice" => skip_leading_options(&words, &["-n", "-c", "-p", "--adjustment"]),
            "timeout" => {
                // `timeout [opts] DURATION cmd …`：跳过选项与时长。
                let after = skip_leading_options(&words, &["-s", "-k", "--signal", "--kill-after"]);
                let drop = usize::from(after < words.len());
                (after + drop).min(words.len())
            }
            "flock" => {
                // `flock [-x] FILE cmd …`：跳过选项与锁文件。
                let after = skip_leading_options(&words, &["-w", "-E", "--timeout"]);
                (after + 1).min(words.len())
            }
            _ => return Some(rest),
        };
        if skip == 0 || skip >= words.len() {
            return None;
        }
        rest = words[skip..].join(" ");
    }
}

/// `env` 之类的透明包装：跳过自身、选项、以及可能夹在中间的环境变量赋值。
fn skip_transparent_prefix(words: &[String]) -> usize {
    let mut idx = 1usize;
    while idx < words.len() {
        let w = &words[idx];
        if w.starts_with('-') || is_env_assignment(w) {
            idx += 1;
            continue;
        }
        break;
    }
    idx
}

/// 跳过「选项 + 选项的值」；返回第一条非选项参数的下标。
fn skip_leading_options(words: &[String], valued: &[&str]) -> usize {
    let mut idx = 1usize;
    while idx < words.len() {
        let w = words[idx].as_str();
        if !w.starts_with('-') {
            break;
        }
        idx += 1;
        if valued.contains(&w) && idx < words.len() {
            idx += 1;
        }
    }
    idx
}

/// 该单词按形状是否像路径（不看文件系统）。
///
/// 判据：POSIX 绝对路径、`./`/`../` 相对路径、含 `/` 的段、以 `.` 开头的隐藏文件、
/// 以 `~` / `$HOME` / `${HOME}` 开头的家目录路径。裸文件名（`id_rsa`、`.some.secret` 之外的
/// `config.json`）不在此列——它们要靠 [`path_shape_or_existing`] 的磁盘探测来兜。
fn looks_like_path(text: &str) -> bool {
    if text.is_empty() || text.starts_with('-') {
        return false;
    }
    if text.starts_with('/')
        || text.starts_with("./")
        || text.starts_with("../")
        || text.contains('/')
        || text.contains('\\')
        || text.starts_with('~')
        || text.starts_with("$HOME")
        || text.starts_with("${HOME}")
    {
        return true;
    }
    // 隐藏文件（`.env`、`.ssh`），但排除 `.` / `..` 本身。
    text.starts_with('.') && text != "." && text != ".."
}

/// 路径形状判定 + 「裸文件名但磁盘上真的存在」的兜底（`cat id_rsa` 也要能拦）。
///
/// 存在性只在 `cwd` 下探一次（成本可接受：每条命令的路径候选通常只有几个）。
pub(super) fn path_shape_or_existing(text: &str, cwd: &Path) -> bool {
    if looks_like_path(text) {
        return true;
    }
    if text.is_empty() || text.starts_with('-') || text.contains(['$', '*', '?', '`']) {
        return false;
    }
    cwd.join(text).exists()
}

/// 会写文件的命令名集合（用于把路径参数判成写方向）。
fn write_effect_command(name: &str) -> bool {
    matches!(
        name,
        "rm" | "mv"
            | "cp"
            | "ln"
            | "install"
            | "touch"
            | "mkdir"
            | "rmdir"
            | "tee"
            | "truncate"
            | "chmod"
            | "chown"
            | "chgrp"
            | "dd"
            | "shred"
            | "unlink"
            | "rsync"
            | "sed"
            | "perl"
            | "git"
            | "npm"
            | "pnpm"
            | "yarn"
            | "cargo"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(scan: &Scan) -> Vec<String> {
        scan.all_commands().map(|c| c.text.clone()).collect()
    }

    #[test]
    fn splits_on_operators() {
        let scan = scan("cd /repo && npm install x; rm -rf y | tee z");
        let all = texts(&scan);
        assert!(all.contains(&"cd /repo".to_string()));
        assert!(all.contains(&"npm install x".to_string()));
        assert!(all.contains(&"rm -rf y".to_string()));
        assert!(all.contains(&"tee z".to_string()));
        assert!(scan.complete);
    }

    #[test]
    fn quotes_do_not_split() {
        let scan = scan("echo 'a && b' \"c | d\"");
        let all = texts(&scan);
        assert_eq!(all, vec!["echo a && b c | d".to_string()]);
    }

    #[test]
    fn command_substitution_is_evaluated() {
        let scan = scan("echo $(rm -rf foo)");
        assert!(texts(&scan).contains(&"rm -rf foo".to_string()));
        let nested = scan
            .nested
            .iter()
            .find(|c| c.text == "rm -rf foo")
            .expect("nested command");
        assert_eq!(nested.origin, Origin::Substitution);
    }

    #[test]
    fn backtick_is_evaluated() {
        let scan = scan("echo `rm -rf foo`");
        assert!(texts(&scan).contains(&"rm -rf foo".to_string()));
    }

    #[test]
    fn subshell_is_evaluated() {
        let scan = scan("(cd /tmp && rm x)");
        assert!(texts(&scan).contains(&"rm x".to_string()));
    }

    #[test]
    fn redirects_are_write_targets() {
        let scan = scan("echo hi > .env");
        let cmd = scan.commands.first().expect("command");
        assert_eq!(cmd.redirects, vec![(".env".to_string(), Direction::Write)]);
        let targets = cmd.path_targets();
        assert!(targets.contains(&(".env".to_string(), Direction::Write)));
    }

    #[test]
    fn env_prefix_is_stripped() {
        let scan = scan("FOO=1 BAR=2 rm -rf x");
        assert_eq!(texts(&scan), vec!["rm -rf x".to_string()]);
    }

    #[test]
    fn cd_moves_working_directory() {
        let scan = scan("cd /tmp && rm a.txt");
        let rm = scan
            .commands
            .iter()
            .find(|c| c.text.starts_with("rm"))
            .expect("rm");
        assert_eq!(rm.cwd, PathBuf::from("/tmp"));
        assert!(!rm.cwd_unknown);
        assert_eq!(rm.absolute_text().as_deref(), Some("rm /tmp/a.txt"));
    }

    #[test]
    fn nonliteral_cd_marks_unknown() {
        let scan = scan("cd \"$DIR\" && rm a.txt");
        let rm = scan
            .commands
            .iter()
            .find(|c| c.text.starts_with("rm"))
            .expect("rm");
        assert!(rm.cwd_unknown);
        assert!(rm.absolute_text().is_none());
    }

    #[test]
    fn indirect_wrappers_are_reported() {
        for input in [
            "bash -c 'rm -rf /'",
            "sudo rm -rf /",
            "eval \"rm -rf /\"",
            "xargs rm",
            "find . -exec rm {} ;",
            "python -c 'import os'",
        ] {
            let scan = scan(input);
            assert!(!scan.indirect.is_empty(), "expected indirect for {input}");
        }
    }

    #[test]
    fn transparent_wrappers_unwrap() {
        assert_eq!(
            unwrap_transparent("timeout 10 npm test").as_deref(),
            Some("npm test")
        );
        assert_eq!(
            unwrap_transparent("env FOO=1 npm test").as_deref(),
            Some("npm test")
        );
        assert_eq!(
            unwrap_transparent("nohup cargo build").as_deref(),
            Some("cargo build")
        );
        assert_eq!(
            unwrap_transparent("sudo rm -rf /").as_deref(),
            Some("sudo rm -rf /")
        );
    }

    #[test]
    fn heredoc_body_is_split_out() {
        let scan = scan("cat <<EOF\nhello $(rm -rf x)\nEOF\nnpm test");
        assert!(texts(&scan).contains(&"rm -rf x".to_string()));
        assert!(texts(&scan).contains(&"npm test".to_string()));
    }

    #[test]
    fn quoted_heredoc_body_is_literal() {
        let scan = scan("cat <<'EOF'\n$(rm -rf x)\nEOF\n");
        assert!(!texts(&scan).contains(&"rm -rf x".to_string()));
    }

    #[test]
    fn unterminated_quote_marks_incomplete() {
        let scan = scan("echo 'oops");
        assert!(!scan.complete);
    }

    #[test]
    fn path_shapes() {
        assert!(looks_like_path("src/a.rs"));
        assert!(looks_like_path(".env"));
        assert!(looks_like_path("~/.ssh/config"));
        assert!(looks_like_path("/etc/passwd"));
        assert!(!looks_like_path("origin"));
        assert!(!looks_like_path("-rf"));
        assert!(!looks_like_path("status"));
    }
}
