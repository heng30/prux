//! `/skills` 面板的行数据与展示辅助（面板本体复用通用 [`crate::modes::interactive::panel::Panel`]）。
//!
//! 一行 = 一个技能名。它可能来自运行期发现（[`crate::core::skills::load_skills`]），
//! 也可能是随二进制分发、**尚未落盘**的扩展技能（[`crate::extensions::skills::catalog`]）。
//! 同名技能按发现规则先到先得，面板不列被遮蔽的副本；启停粒度是技能名。

/// `/skills` 面板的一行。
#[derive(Debug, Clone)]
pub struct SkillRow {
    /// 技能名（frontmatter `name`），也是启停规则（`-<name>`）使用的键。
    pub name: String,
    /// frontmatter `description`（详情页展示）。
    pub description: String,
    /// frontmatter `disable-model-invocation`：为真时即使启用也不进 `<available_skills>`。
    pub disable_model_invocation: bool,
    /// 来源标签：发现根目录（`$HOME` 缩写为 `~`），未落盘的扩展技能为 `builtin`。
    pub source: String,
    /// 该技能 `SKILL.md` 的路径（未落盘时为待创建的目标路径）。
    pub path: String,
    /// 打开/刷新面板时实算出的启用态（该名字是否出现在过滤后的技能列表里）。
    pub enabled: bool,
    /// 规则数组里存在可被面板删除的精确 `-<name>` 项；
    /// 为 `false` 而 [`SkillRow::enabled`] 也为假时，说明是别的规则（`!glob` / `-path`）排除的。
    pub has_exact_exclude: bool,
    /// 磁盘上是否已有该技能（发现结果里有它，或已落盘到 `agent_dir()/skills/<name>`）。
    ///
    /// 为 `false` 只出现在随二进制分发、任何技能根目录里都扫不到的扩展技能上（`source` 为 `builtin`）；
    /// 此时开启它会先「检查并创建」到 `agent_dir()/skills`。
    pub installed: bool,
}

impl SkillRow {
    /// 面板列表标签：`[x] 名称` / `[ ] 名称`。
    pub fn label(&self, on: bool) -> String {
        format!("[{}] {}", if on { "x" } else { " " }, self.name)
    }

    /// 行尾注记（空格分隔，无注记时为空串）：
    /// `[not installed]` 未落盘的扩展技能、`[model-off]` 禁止模型主动调用、
    /// `[filtered]` 被手写规则排除（面板开关改不动它）。
    pub fn notes(&self, on: bool) -> String {
        let mut notes: Vec<&str> = Vec::new();
        if !self.installed {
            notes.push("[not installed]");
        }
        if self.disable_model_invocation {
            notes.push("[model-off]");
        }
        if !on && !self.has_exact_exclude {
            notes.push("[filtered]");
        }
        notes.join(" ")
    }

    /// 详情页行：SKILL.md 头信息 + 来源 + 路径（渲染侧按宽度折行）。
    ///
    /// `description` 放最后：它最长，折行可达多行，放中间会把来源/路径挤到下面不好找。
    pub fn detail_lines(&self) -> Vec<String> {
        vec![
            format!("  name: {}", self.name),
            format!(
                "  disable-model-invocation: {}",
                self.disable_model_invocation
            ),
            format!("  source: {}", self.source),
            format!("  path: {}", self.path),
            format!("  description: {}", self.description),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> SkillRow {
        SkillRow {
            name: "grill-me".to_string(),
            description: "grill the user".to_string(),
            disable_model_invocation: true,
            source: "~/.agents/skills".to_string(),
            path: "/home/u/.agents/skills/grill-me/SKILL.md".to_string(),
            enabled: true,
            has_exact_exclude: false,
            installed: true,
        }
    }

    #[test]
    fn label_reflects_state() {
        let r = row();
        assert_eq!(r.label(true), "[x] grill-me");
        assert_eq!(r.label(false), "[ ] grill-me");
    }

    #[test]
    fn notes_cover_installed_model_off_and_filtered() {
        let r = row();
        assert_eq!(r.notes(true), "[model-off]");

        let mut not_installed = row();
        not_installed.installed = false;
        assert_eq!(not_installed.notes(true), "[not installed] [model-off]");

        // 关闭且没有可删的精确 `-name`（规则排除）→ [filtered]
        assert_eq!(r.notes(false), "[model-off] [filtered]");

        // 关闭但有精确 `-name` → 面板能改，不标 [filtered]
        let mut excluded = row();
        excluded.has_exact_exclude = true;
        assert_eq!(excluded.notes(false), "[model-off]");
    }

    #[test]
    fn detail_lines_show_frontmatter_and_source() {
        let lines = row().detail_lines().join("\n");
        assert!(lines.contains("name: grill-me"), "{lines}");
        assert!(lines.contains("description: grill the user"), "{lines}");
        assert!(lines.contains("disable-model-invocation: true"), "{lines}");
        assert!(lines.contains("source: ~/.agents/skills"), "{lines}");
        assert!(lines.contains("path: /home/u/.agents/skills/grill-me/SKILL.md"));
    }

    /// description 排最后（最长、可折行，放中间会把来源/路径挤走）
    #[test]
    fn detail_lines_put_description_last() {
        let r = row();
        let lines = r.detail_lines();
        assert!(lines.last().unwrap().contains("description: "), "{lines:?}");
        assert!(
            lines.iter().position(|l| l.contains("path: ")) < Some(lines.len() - 1),
            "{lines:?}"
        );
    }
}
