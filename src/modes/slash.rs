//! 斜杠命令在两个界面之间的共用部分：`/` 补全候选与提交前的展开。
//!
//! 内置命令表各自维护（终端有 `dock` 这类纯终端命令、桌面有自己的面板命令），
//! 但「候选怎么拼、重名谁优先」和「`/skill:名`、`/模板名 参数` 怎么展开成提示词」
//! 必须两边一致——这些规则只写在这里。

use crate::core::{
    extensions,
    prompt_templates::{PromptTemplate, expand_prompt_template},
    skills::{Skill, expand_skill_command},
};

/// 提交前的文本展开：`/skill:名称` 换成技能正文、`/模板名 参数` 换成模板正文。
///
/// 只有斜杠开头的文本会被展开；两者都不命中时原样返回（未知斜杠命令按普通输入发送）。
/// 顺序是技能在前、模板在后：模板正文里写的 `/skill:名` 不会被二次展开。
pub fn expand_user_text(text: &str, skills: &[Skill], templates: &[PromptTemplate]) -> String {
    let expanded = expand_skill_command(text, skills);
    expand_prompt_template(&expanded, templates)
}

/// `/` 补全候选：内置命令 → 扩展命令 → 技能（`skill:名`）→ 提示词模板。
///
/// 重名按这个顺序优先：内置命令压扩展命令（分发也是内置优先），命令压同名模板
/// （分发先命中命令，模板永远轮不到，列出来只会误导）。
/// `exclude` 是扩展命令里要跳过的名字（界面自己已有面板入口的那些）。
/// `skills` / `templates` 是 `(名称, 说明)`，由调用方按自己的数据结构摊平。
pub fn candidates(
    builtin: &[(&str, &str)],
    exclude: &[&str],
    skills: &[(String, String)],
    templates: &[(String, String)],
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = builtin
        .iter()
        .map(|(name, description)| (name.to_string(), description.to_string()))
        .collect();

    for command in extensions::registered_commands() {
        if exclude.contains(&command.name.as_str())
            || builtin.iter().any(|(name, _)| *name == command.name)
        {
            continue;
        }
        out.push((command.name, command.description));
    }

    // 技能挂 `skill:` 前缀：不会与命令名撞，裸名参与模糊匹配由调用方决定
    for (name, description) in skills {
        out.push((format!("skill:{name}"), description.clone()));
    }

    for (name, description) in templates {
        // 每次重新查一遍：模板之间也可能重名。`exclude` 里的名字同样是命令
        // （只是界面不在补全里列），同名模板照样轮不到。
        if exclude.contains(&name.as_str()) || out.iter().any(|(taken, _)| taken == name) {
            continue;
        }
        out.push((name.clone(), description.clone()));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template(name: &str, content: &str) -> PromptTemplate {
        PromptTemplate {
            name: name.to_string(),
            description: format!("{name} template"),
            argument_hint: None,
            content: content.to_string(),
            file_path: format!("/tmp/{name}.md"),
        }
    }

    /// 候选顺序：内置 → 技能 → 模板；与内置/技能重名的模板被剔除。
    #[test]
    fn candidates_keep_builtin_first_and_drop_shadowed_templates() {
        let builtin = [("model", "Select model")];
        let skills = vec![("research".to_string(), "Deep research".to_string())];
        let templates = vec![
            ("review".to_string(), "Review code".to_string()),
            ("model".to_string(), "Shadowed".to_string()),
            (
                "skills".to_string(),
                "Shadowed by an unlisted command".to_string(),
            ),
        ];

        let candidates = candidates(&builtin, &["skills"], &skills, &templates);
        let names: Vec<&str> = candidates.iter().map(|(name, _)| name.as_str()).collect();

        assert_eq!(names[0], "model");
        assert!(names.contains(&"skill:research"));
        assert!(names.contains(&"review"));
        // 模板与内置命令重名：命令优先，模板不列（永远轮不到）
        assert_eq!(names.iter().filter(|name| **name == "model").count(), 1);
        // 与「不在补全里列的界面命令」重名的模板同样不列
        assert!(!names.contains(&"skills"), "{names:?}");
    }

    /// `exclude` 里的扩展命令不出现在候选里（界面自己已有面板入口）。
    #[test]
    fn candidates_skip_excluded_extension_commands() {
        let registered = extensions::registered_commands();
        let Some(command) = registered.first() else {
            return; // 没有已启用的扩展命令：无从验证，跳过
        };
        let excluded = [command.name.as_str()];
        let names: Vec<String> = candidates(&[], &excluded, &[], &[])
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(!names.contains(&command.name), "{names:?}");
    }

    /// `/skill:名` 与 `/模板名 参数` 展开成提示词文本；不命中的原样返回。
    #[test]
    fn expansion_replaces_skills_and_templates() {
        let templates = vec![template("review", "Please review $1 carefully.")];
        assert_eq!(
            expand_user_text("/review src/main.rs", &[], &templates),
            "Please review src/main.rs carefully."
        );
        // 不带参数：模板正文原样
        assert_eq!(
            expand_user_text("/review", &[], &templates),
            "Please review $1 carefully."
        );
        assert_eq!(expand_user_text("hello", &[], &templates), "hello");
        assert_eq!(expand_user_text("/nope x", &[], &templates), "/nope x");
    }
}
