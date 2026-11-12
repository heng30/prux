//! 共用的 syntect 语法集：默认集 + 内嵌补充语法。
//!
//! syntect 的默认集（约 75 种）不含 Zig / Dart / Nix，这三个用 `assets/syntaxes/` 里的
//! Sublime 语法补上；Gradle 没有独立语法，按 Groovy 高亮（语义等价，且默认集自带 Groovy）。
//! TUI 与桌面 GUI 共用同一份集合，避免两个界面认得不同的语言。

use crate::embedded;
use std::{path::Path, sync::OnceLock};
use syntect::parsing::{SyntaxDefinition, SyntaxReference, SyntaxSet};

/// 没有独立语法、但按等价语言高亮的语言名（fence token 与文件扩展名同表）。
const SYNTAX_ALIASES: &[(&str, &str)] = &[("gradle", "groovy")];

/// 内嵌的补充语法源码（默认集里没有的语言，编译期内嵌）。
const EXTRA_SYNTAXES: &[&str] = &[
    embedded!("syntaxes/Zig.sublime-syntax"),
    embedded!("syntaxes/Dart.sublime-syntax"),
    embedded!("syntaxes/Nix.sublime-syntax"),
];

/// 进程内共用的语法集（首次调用时构建并常驻）。
///
/// 渲染按行喂入（无尾部 `\n`），必须用 nonewlines 变体：newlines 变体里以「行尾 `\n`」
/// 为退栈条件的上下文（如 C++ 的 `meta.preprocessor.include`）永远不弹栈，
/// 会导致 include 上下文泄漏、整块代码高亮塌陷。
pub fn syntax_set() -> &'static SyntaxSet {
    /// 常驻语法集单例。
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(|| {
        let mut builder = SyntaxSet::load_defaults_nonewlines().into_builder();
        for source in EXTRA_SYNTAXES {
            if let Ok(syntax) = SyntaxDefinition::load_from_str(source, false, None) {
                builder.add(syntax);
            }
        }
        builder.build()
    })
}

/// 把外部传来的语言标记（fence token / 扩展名）归一为可查的名字。
///
/// GitHub 风格的 fence token 可能带修饰（`rust,ignore`、`{rust,linenos}`、`python hl_lines=1`）：
/// 取首个空白分隔 token，再截断到首个 `,` `.` `;` `:` `{` `}`，避免后缀让查找落空。空串返回 `None`。
pub fn normalize_language(lang: &str) -> Option<&str> {
    let token = lang
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_start_matches('{')
        .split([',', '.', ';', ':', '{', '}'])
        .next()
        .unwrap_or("")
        .trim();
    (!token.is_empty()).then_some(token)
}

/// 把别名解析成实际语法名（未知名字原样返回）。
fn resolve_alias(lang: &str) -> &str {
    SYNTAX_ALIASES
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(lang))
        .map(|(_, real)| *real)
        .unwrap_or(lang)
}

/// 按 fence token 找语法（含别名回退）；找不到返回 `None`（调用方回退纯文本）。
pub fn find_syntax_by_token(lang: &str) -> Option<&'static SyntaxReference> {
    let set = syntax_set();
    let token = normalize_language(lang)?;
    set.find_syntax_by_token(resolve_alias(token))
}

/// 按文件名找语法（扩展名 / 首行 shebang 由语法集自行判定，别名同样生效）。
pub fn find_syntax_for_filename(path: &str) -> Option<&'static SyntaxReference> {
    let set = syntax_set();
    if let Ok(Some(syntax)) = set.find_syntax_for_file(path) {
        return Some(syntax);
    }
    let extension = Path::new(path).extension().and_then(|ext| ext.to_str())?;
    set.find_syntax_by_token(resolve_alias(extension))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 补充语法都能解析出来，并按各自的 markdown 扩展名/token 查到。
    #[test]
    fn extra_syntaxes_are_available() {
        for (lang, scope) in [
            ("zig", "source.zig"),
            ("dart", "source.dart"),
            ("nix", "source.nix"),
        ] {
            let syntax = find_syntax_by_token(lang).unwrap_or_else(|| panic!("缺少 {lang} 语法"));
            assert_eq!(syntax.scope.to_string(), scope, "{lang} scope 不符");
        }
    }

    /// Gradle 没有独立语法：按 Groovy 高亮。
    #[test]
    fn gradle_falls_back_to_groovy() {
        let gradle = find_syntax_by_token("gradle").expect("gradle 应能查到语法");
        let groovy = find_syntax_by_token("groovy").expect("默认集应带 groovy");
        assert_eq!(gradle.scope, groovy.scope);
        assert!(find_syntax_for_filename("build.gradle").is_some());
    }

    /// fence token 的修饰后缀不影响查找（`rust,ignore` / `{rust,linenos}` 等）。
    #[test]
    fn fence_modifiers_are_stripped() {
        assert_eq!(normalize_language("rust,ignore"), Some("rust"));
        assert_eq!(normalize_language("{rust,linenos}"), Some("rust"));
        assert_eq!(normalize_language("python hl_lines=1"), Some("python"));
        assert_eq!(normalize_language("  "), None);
        assert!(find_syntax_by_token("rust,ignore").is_some());
    }
}
