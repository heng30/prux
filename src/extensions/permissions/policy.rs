//! 策略模型与求值：三态动作、模式表（last-match-wins）、多表面与四层 most-restrictive-wins 组合。
//!
//! 动作只有三种：`allow` 静默放行、`ask` 需要确认（本扩展降级为 fail-closed 拒绝，
//! 见模块根的说明）、`deny` 直接拒绝。严格度 `deny > ask > allow`——任何一层给出
//! `deny` 就不再询问，任何一层给出 `ask` 就能把别层的 `allow` 收紧。
//!
//! 值可以写成字符串（`"allow"`）或带理由的对象（`{"action": "deny", "reason": "..."}`）。

use super::paths::wildcard_match;
use crate::utils::paths::expand_home;
use serde_json::Value;
use std::{collections::BTreeMap, fmt, str::FromStr};
use strum_macros::{EnumString, IntoStaticStr};

/// 权限动作三态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub(super) enum Action {
    /// 静默放行，不产生任何提示。
    Allow,
    /// 需要用户确认；本扩展在无阻塞对话框的前提下按 fail-closed 处理为拒绝。
    Ask,
    /// 直接拒绝，并把理由交回模型。
    Deny,
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(<&'static str>::from(*self))
    }
}

/// 一条规则的值：动作 + 可选理由（理由会原样出现在拦截消息与 review log 里）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RuleValue {
    /// 命中该规则时采用的动作。
    pub action: Action,
    /// 配置里显式写的理由；`None` 表示没有，调用方自己生成缺省文案。
    pub reason: Option<String>,
}

impl Default for RuleValue {
    /// 缺省规则值是 `ask`——最小权限：没配的东西要确认，而不是默默放行。
    fn default() -> Self {
        RuleValue::new(Action::Ask)
    }
}

impl RuleValue {
    /// 只有一个动作、没有理由的规则值。
    pub(super) fn new(action: Action) -> Self {
        RuleValue {
            action,
            reason: None,
        }
    }

    /// 解析配置里的一个规则值：字符串，或 `{action, reason}` 对象。
    ///
    /// 返回 `None` 表示值形状不认识（动作拼错、对象缺 `action`、类型不是字符串/对象）；
    /// 调用方应把它当作**配置错误**对待（fail-closed，不允许静默回落 allow）。
    pub(super) fn parse(value: &Value) -> Option<Self> {
        match value {
            Value::String(s) => Action::from_str(s.trim()).ok().map(RuleValue::new),
            Value::Object(map) => {
                let action = map
                    .get("action")?
                    .as_str()
                    .and_then(|s| Action::from_str(s.trim()).ok())?;
                let reason = map
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .map(str::to_string);
                Some(RuleValue { action, reason })
            }
            _ => None,
        }
    }
}

/// 保序的模式表：同一表面下的多条 `pattern → 规则`。
///
/// 求值语义是 **last-match-wins**——遍历顺序即书写顺序，最后一条匹配的模式生效。
/// 因此宽泛的兜底（`"*"`）要写在前面，具体例外写在后面；顺序写反了例外会被兜底吃掉。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct PatternMap {
    /// `(模式, 规则值)`，保持配置里的书写顺序。
    rules: Vec<(String, RuleValue)>,
    /// 规则值无法解析的模式名（动作拼错、形状不认识），供加载层告警。
    invalid: Vec<String>,
}

/// 一次模式命中的完整证据（用于拦截文案与 review log）。
#[derive(Debug, Clone)]
pub(super) struct Match {
    /// 命中的模式原文（配置里怎么写的）。
    pub pattern: String,
    /// 命中的规则值。
    pub rule: RuleValue,
    /// 实际匹配上的拼写；与 [`Match::pattern`] 不同时说明是「按绝对值/家目录展开后命中」。
    pub matched_as: String,
}

impl PatternMap {
    /// 从配置对象构造；跳过无法解析的条目并回报其模式名（调用方据此告警）。
    pub(super) fn parse(value: &Value) -> (Self, Vec<String>) {
        let mut rules = Vec::new();
        let mut bad = Vec::new();
        if let Value::Object(map) = value {
            for (pattern, raw) in map {
                match RuleValue::parse(raw) {
                    Some(rule) => rules.push((pattern.clone(), rule)),
                    None => bad.push(pattern.clone()),
                }
            }
        }
        (
            PatternMap {
                rules,
                invalid: bad.clone(),
            },
            bad,
        )
    }

    /// 表中的全部模式（按书写顺序）。
    pub(super) fn patterns(&self) -> impl Iterator<Item = &str> {
        self.rules.iter().map(|(p, _)| p.as_str())
    }

    /// 是否**每一条**规则都解析为 `deny`（用于把整个表面从模型可见工具里摘掉）。
    pub(super) fn all_deny(&self) -> bool {
        !self.rules.is_empty() && self.rules.iter().all(|(_, r)| r.action == Action::Deny)
    }

    /// 按 last-match-wins 求值：`texts` 是所有候选拼写，最后一条命中任一拼写的规则生效。
    ///
    /// 返回 `None` 表示没有任何规则命中（该层不参与组合）。
    pub(super) fn evaluate(&self, texts: &[String]) -> Option<Match> {
        let mut hit: Option<Match> = None;
        for (pattern, rule) in &self.rules {
            let pattern_expanded = expand_home(pattern);
            for text in texts {
                if wildcard_match(pattern, text) || wildcard_match(&pattern_expanded, text) {
                    hit = Some(Match {
                        pattern: pattern.clone(),
                        rule: rule.clone(),
                        matched_as: text.clone(),
                    });
                    break;
                }
            }
        }
        hit
    }

    /// 设置一条规则：同模式已在表里时替换其值，否则追加到末尾。
    pub(super) fn set(&mut self, pattern: &str, rule: RuleValue) {
        match self.rules.iter_mut().find(|(p, _)| p == pattern) {
            Some((_, slot)) => *slot = rule,
            None => self.rules.push((pattern.to_string(), rule)),
        }
    }
}

/// 一个工具表面的配置：要么是一条兜底规则（`"read": "allow"`），要么是一张模式表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Surface {
    /// 字符串简写：等价于 `{"*": <该值>}`。
    CatchAll(RuleValue),
    /// 对象写法：按路径或命令模式分别给值。
    Patterns(PatternMap),
}

impl Surface {
    /// 表面的全部模式（字符串简写视为只有一条 `*`）。
    pub(super) fn patterns(&self) -> Vec<&str> {
        match self {
            Surface::CatchAll(_) => vec!["*"],
            Surface::Patterns(map) => map.patterns().collect(),
        }
    }

    /// 该表面是否被**彻底**拒绝（模型根本看不到这个工具）。
    pub(super) fn all_deny(&self) -> bool {
        match self {
            Surface::CatchAll(rule) => rule.action == Action::Deny,
            Surface::Patterns(map) => map.all_deny(),
        }
    }

    /// 对给定候选拼写求值。
    pub(super) fn evaluate(&self, texts: &[String]) -> Option<Match> {
        match self {
            Surface::CatchAll(rule) => texts.first().map(|t| Match {
                pattern: "*".to_string(),
                rule: rule.clone(),
                matched_as: t.clone(),
            }),
            Surface::Patterns(map) => map.evaluate(texts),
        }
    }
}

/// 一次判定的结果：动作 + 是哪一层、哪条规则决定的。
#[derive(Debug, Clone)]
pub(super) struct Decision {
    /// 最终动作。
    pub action: Action,
    /// 做出决定的表面（`path` / `external_directory` / 工具名 / `bash`）。
    pub surface: String,
    /// 命中的模式原文。
    pub pattern: String,
    /// 该规则自带的理由（`None` = 由调用方生成缺省文案）。
    pub reason: Option<String>,
    /// 实际匹配上的拼写；用于向用户解释「按绝对路径命中」这种情况。
    pub matched_as: Option<String>,
}

impl Decision {
    /// 由一次模式命中与它所属的表面名构造决策。
    pub(super) fn from_match(surface: &str, hit: Match) -> Self {
        let rewritten = hit.matched_as != hit.pattern;
        Decision {
            action: hit.rule.action,
            surface: surface.to_string(),
            pattern: hit.pattern,
            reason: hit.rule.reason,
            matched_as: rewritten.then_some(hit.matched_as),
        }
    }

    /// 合并两次决策，取更严格者；严格度相同则保留先到的那条
    /// （层序为 path → external_directory → 工具 → bash，先到即更外层）。
    pub(super) fn tighten(self, other: Option<Decision>) -> Self {
        match other {
            None => self,
            Some(o) if o.action > self.action => o,
            Some(_) => self,
        }
    }
}

/// 访问方向：读还是写。`path_read` / `path_write` 与 `external_directory_read/write`
/// 靠它区分「允许读某个目录，但写入仍然拦」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Direction {
    /// 读取目标（`read` / `grep` / `find` / `ls`，bash 的普通参数与 `<` 重定向）。
    Read,
    /// 写入目标（`write` / `edit`，bash 的 `>` / `>>` / `tee` 等重定向目标）。
    Write,
}

/// 生效的权限策略（全局 + 项目配置合并后的结果）。
///
/// 路径类表面用 `Option` 表达「配置里有没有这个键」：没配 = 该层不参与组合，
/// 这与上游「Configs without a `path` key behave identically to before」一致。
#[derive(Debug, Clone, Default)]
pub(super) struct Policy {
    /// `permission["*"]`：任何表面都没有命中时的兜底，缺省 `ask`（最小权限）。
    pub universal: RuleValue,
    /// 工具表面：工具名 → 规则（`read` / `write` / `bash` / `mcp` / 任意扩展工具名）。
    pub surfaces: BTreeMap<String, Surface>,
    /// 横切路径表面：对所有工具与 bash 的路径 token 生效。
    pub path: Option<PatternMap>,
    /// 只对读取生效的路径表面。
    pub path_read: Option<PatternMap>,
    /// 只对写入生效的路径表面。
    pub path_write: Option<PatternMap>,
    /// 工作目录之外访问的边界表面（不分方向）。
    pub external_directory: Option<PatternMap>,
    /// 只对工作目录之外的读取生效的边界表面。
    pub external_directory_read: Option<PatternMap>,
    /// 只对工作目录之外的写入生效的边界表面。
    pub external_directory_write: Option<PatternMap>,
}

impl Policy {
    /// `path` 层求值：先方向专用表，再裸 `path` 表，取最严格者。
    ///
    /// `forms` 是这条路径的全部拼写（原文 / 家目录展开 / 绝对 / canonical），
    /// 任一拼写命中即算命中。
    pub(super) fn path_decision(&self, forms: &[String], dir: Direction) -> Option<Decision> {
        let directional = match dir {
            Direction::Read => self.path_read.as_ref(),
            Direction::Write => self.path_write.as_ref(),
        };
        let mut out = directional
            .and_then(|m| m.evaluate(forms))
            .map(|hit| Decision::from_match("path", hit));
        if let Some(hit) = self.path.as_ref().and_then(|m| m.evaluate(forms)) {
            let decision = Decision::from_match("path", hit);
            out = Some(match out {
                Some(prev) => prev.tighten(Some(decision)),
                None => decision,
            });
        }
        out
    }

    /// `external_directory` 层求值：同样先方向专用表，再裸表，取最严格者。
    pub(super) fn external_decision(&self, forms: &[String], dir: Direction) -> Option<Decision> {
        let directional = match dir {
            Direction::Read => self.external_directory_read.as_ref(),
            Direction::Write => self.external_directory_write.as_ref(),
        };
        let mut out = directional
            .and_then(|m| m.evaluate(forms))
            .map(|hit| Decision::from_match("external_directory", hit));
        if let Some(hit) = self
            .external_directory
            .as_ref()
            .and_then(|m| m.evaluate(forms))
        {
            let decision = Decision::from_match("external_directory", hit);
            out = Some(match out {
                Some(prev) => prev.tighten(Some(decision)),
                None => decision,
            });
        }
        out
    }

    /// 工具表面求值：`tool` 是工具名，`texts` 是候选拼写（路径类工具传全部路径拼写，
    /// 其它工具传一个占位串让它命中的是兜底规则）。
    pub(super) fn tool_decision(&self, tool: &str, texts: &[String]) -> Option<Decision> {
        let surface = self.surfaces.get(tool)?;
        surface
            .evaluate(texts)
            .map(|hit| Decision::from_match(tool, hit))
    }

    /// `bash` 表面求值：对一条顶层命令的「按原文」与「按绝对拼写」两种文本求值。
    pub(super) fn bash_decision(&self, texts: &[String]) -> Option<Decision> {
        let surface = self.surfaces.get("bash")?;
        surface
            .evaluate(texts)
            .map(|hit| Decision::from_match("bash", hit))
    }

    /// 「表面是否被彻底拒绝」——用于在组装工具表时把该工具整个摘掉。
    ///
    /// 只有**所有**模式都解析为 `deny` 才算：`"bash": "deny"` 摘掉 bash，
    /// 而 `"bash": {"*": "deny", "git *": "ask"}` 仍然保留（agent 可以试 `git`，
    /// 别的命令在执行时被拦）。
    pub(super) fn surface_all_deny(&self, tool: &str) -> bool {
        self.surfaces.get(tool).is_some_and(Surface::all_deny)
    }

    /// 该表面是否出现在配置里（`/permissions show` 用来区分「未配置」与「配成全 deny」）。
    pub(super) fn has_surface(&self, tool: &str) -> bool {
        self.surfaces.contains_key(tool)
    }

    /// 工作目录边界是否配置了任何规则（未配置时外部目录检查整层跳过）。
    pub(super) fn has_external(&self) -> bool {
        self.external_directory.is_some()
            || self.external_directory_read.is_some()
            || self.external_directory_write.is_some()
    }

    /// 横切路径层是否配置了任何规则。
    pub(super) fn has_path(&self) -> bool {
        self.path.is_some() || self.path_read.is_some() || self.path_write.is_some()
    }

    /// 当前策略覆盖到的表面名（工具名 + 三个固定表面），按字典序。
    pub(super) fn surface_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.surfaces.keys().cloned().collect();
        for fixed in ["path", "external_directory"] {
            if !names.iter().any(|n| n == fixed) {
                names.push(fixed.to_string());
            }
        }
        names.sort();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(value: Value) -> PatternMap {
        PatternMap::parse(&value).0
    }

    #[test]
    fn action_parsing() {
        assert_eq!(Action::from_str("allow"), Ok(Action::Allow));
        assert_eq!(Action::from_str("ask"), Ok(Action::Ask));
        assert_eq!(Action::from_str("deny"), Ok(Action::Deny));
        assert!(Action::from_str("nope").is_err());
        // 严格度序：deny > ask > allow（`Decision::tighten` 依赖它）。
        assert!(Action::Deny > Action::Ask);
        assert!(Action::Ask > Action::Allow);
    }

    #[test]
    fn rule_value_object_form() {
        let rule = RuleValue::parse(&json!({"action": "deny", "reason": "use pnpm"}))
            .expect("object form");
        assert_eq!(rule.action, Action::Deny);
        assert_eq!(rule.reason.as_deref(), Some("use pnpm"));
        assert!(RuleValue::parse(&json!({"reason": "x"})).is_none());
        assert!(RuleValue::parse(&json!(3)).is_none());
    }

    #[test]
    fn last_match_wins() {
        let m = map(json!({"*": "allow", "*.env": "deny"}));
        let hit = m.evaluate(&["src/.env".to_string()]).expect("hit");
        assert_eq!(hit.rule.action, Action::Deny);
        assert_eq!(hit.pattern, "*.env");

        // 兜底写在后面会把前面的例外吃掉——这正是 last-match-wins 的含义。
        let m = map(json!({"*.env": "deny", "*": "allow"}));
        let hit = m.evaluate(&["src/.env".to_string()]).expect("hit");
        assert_eq!(hit.rule.action, Action::Allow);
        assert_eq!(hit.pattern, "*");
    }

    #[test]
    fn no_match_is_none() {
        let m = map(json!({"*.env": "deny"}));
        assert!(m.evaluate(&["src/a.rs".to_string()]).is_none());
    }

    #[test]
    fn any_spelling_matches() {
        let m = map(json!({"/work/proj/*": "deny"}));
        let forms = vec!["src/a.rs".to_string(), "/work/proj/src/a.rs".to_string()];
        let hit = m.evaluate(&forms).expect("hit on absolute spelling");
        assert_eq!(hit.matched_as, "/work/proj/src/a.rs");
    }

    #[test]
    fn surface_catch_all_and_all_deny() {
        let s = Surface::CatchAll(RuleValue::new(Action::Deny));
        assert!(s.all_deny());
        let s = Surface::Patterns(map(json!({"*": "deny", "git *": "ask"})));
        assert!(!s.all_deny());
    }

    #[test]
    fn directional_tables_compose() {
        let policy = Policy {
            universal: RuleValue::new(Action::Ask),
            path: Some(map(json!({"*": "allow"}))),
            path_write: Some(map(json!({"*": "deny"}))),
            ..Policy::default()
        };
        let forms = vec!["src/a.rs".to_string()];
        let read = policy.path_decision(&forms, Direction::Read).expect("read");
        assert_eq!(read.action, Action::Allow);
        let write = policy
            .path_decision(&forms, Direction::Write)
            .expect("write");
        assert_eq!(write.action, Action::Deny);
    }

    #[test]
    fn decision_tighten_prefers_stricter() {
        let deny = Decision {
            action: Action::Deny,
            surface: "path".into(),
            pattern: "*.env".into(),
            reason: None,
            matched_as: None,
        };
        let allow = Decision {
            action: Action::Allow,
            ..deny.clone()
        };
        assert_eq!(
            allow.clone().tighten(Some(deny.clone())).action,
            Action::Deny
        );
        assert_eq!(deny.clone().tighten(Some(allow)).action, Action::Deny);
        assert_eq!(deny.action, Action::Deny);
    }

    #[test]
    fn set_replaces_same_pattern() {
        let mut m = map(json!({"rm *": "ask"}));
        m.set("rm *", RuleValue::new(Action::Allow));
        let hit = m.evaluate(&["rm -rf x".to_string()]).expect("hit");
        assert_eq!(hit.rule.action, Action::Allow);
        assert_eq!(m.patterns().count(), 1);
    }
}
