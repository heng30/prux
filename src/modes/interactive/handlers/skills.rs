//! `/skills` 面板：打开/重扫、草稿切换、Ctrl+S 应用、二级详情。
//!
//! 状态的唯一真源是 `settings.json` 的 `skills` 过滤数组。面板只写精确 `-<name>` 项
//! （追加到末尾，压过前面的规则），不碰手写的 `+path` / `!glob` 规则；对那种情况
//! 行尾标 `[filtered]` 说明“面板改不动它”。扩展技能在开启时「检查并创建」到
//! `agent_dir()/skills/<name>/**`（见 [`crate::extensions::skills::materialize`]），
//! 但那份副本优先级最低：同名时生效与展示的都是用户自己目录里的副本。
//!
//! 草稿模型对齐 `/extension`：空格只改草稿（勾选即时可见），Ctrl+S 才写盘 + 重扫 +
//! 重建 skill 工具，关闭面板丢弃草稿。

use crate::{
    core::{
        settings_manager::{self, agent_dir},
        skills,
    },
    extensions::skills as bundled,
    modes::interactive::{
        app::{App, MsgLevel},
        panel::{PanelItem, PanelKind},
        skills_panel::SkillRow,
    },
    utils::paths::home_dir,
};
use std::collections::HashSet;

impl App {
    /// 打开 `/skills` 面板：先关闭其它模态选择器，丢弃旧草稿并重扫磁盘。
    pub(crate) fn open_skills_panel(&mut self) {
        self.panel.close();
        self.session_selector.close();
        self.ext_settings.close();
        self.settings_selector.close();
        self.skill_draft.clear();
        self.skill_panel_error = None;
        self.rebuild_skill_rows();
        let items = self.skill_panel_items();
        self.panel
            .open(PanelKind::Skill, "/skills".to_string(), items);
    }

    /// 重建行数据：重扫磁盘（无过滤 = 全部）与过滤后（实算启用态），再并入尚未落盘的扩展技能。
    ///
    /// 「实算」是刻意的：`load_skills` 返回的就是这套过滤规则的最终结果，
    /// 因此不会出现“勾选框说开着、实际没加载”的谎报。
    pub(super) fn rebuild_skill_rows(&mut self) {
        let agent_dir = agent_dir();
        // 走 [`home_dir`] 而不是直接读 `$HOME`：测试接缝（HomeGuard）才能隔离 `~/.agents/skills`
        let home_dir = home_dir();
        let rules = settings_manager::read_settings_skills();

        let catalog = bundled::catalog();
        let bundled_names: Vec<String> = catalog.iter().map(|c| c.name.clone()).collect();
        let load = |filter: &[String]| {
            skills::load_skills(
                &self.cwd,
                &agent_dir,
                &home_dir,
                &self.reload_ctx.skill_paths,
                &bundled_names,
                self.reload_ctx.include_default_skills,
                self.reload_ctx.trusted,
                filter,
            )
        };
        let all = load(&[]);
        let effective = load(&rules);
        let enabled: HashSet<&str> = effective.iter().map(|s| s.name.as_str()).collect();

        let mut rows: Vec<SkillRow> = all
            .iter()
            .map(|s| {
                SkillRow {
                    name: s.name.clone(),
                    description: s.description.clone(),
                    disable_model_invocation: s.disable_model_invocation,
                    source: s.source.clone(),
                    path: s.path.display().to_string(),
                    enabled: enabled.contains(s.name.as_str()),
                    has_exact_exclude: skills::filter_has_exact_exclude(&rules, &s.name),
                    // 能出现在发现结果里 = 磁盘上已有副本（可能来自 agent_dir/skills，
                    // 也可能来自 ~/.agents/skills 等项目/用户技能根目录），一律不算「未安装」。
                    installed: true,
                }
            })
            .collect();

        // 尚未落盘（因而不在发现结果里）的扩展技能也要列出来，否则用户无从开启
        for c in &catalog {
            if all.iter().any(|s| s.name == c.name) {
                continue;
            }
            rows.push(SkillRow {
                name: c.name.clone(),
                description: c.description.clone(),
                disable_model_invocation: c.disable_model_invocation,
                source: "builtin".to_string(),
                path: c.target_skill_md().display().to_string(),
                enabled: enabled.contains(c.name.as_str()),
                has_exact_exclude: skills::filter_has_exact_exclude(&rules, &c.name),
                installed: bundled::is_installed(&c.name),
            });
        }

        rows.sort_by(|a, b| a.name.cmp(&b.name));
        self.skill_rows = rows;
    }

    /// 按当前草稿优先的状态构造面板条目。
    fn skill_panel_items(&self) -> Vec<PanelItem> {
        self.skill_rows
            .iter()
            .map(|r| {
                let on = self.skill_state(&r.name);
                PanelItem {
                    label: r.label(on),
                    value: r.name.clone(),
                    desc: r.notes(on),
                    ..Default::default()
                }
            })
            .collect()
    }

    /// 某个技能的显示状态：草稿优先，否则用行上的实算值。
    pub(crate) fn skill_state(&self, name: &str) -> bool {
        self.skill_draft
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, on)| *on)
            .unwrap_or_else(|| {
                self.skill_rows
                    .iter()
                    .find(|r| r.name == name)
                    .map(|r| r.enabled)
                    .unwrap_or(false)
            })
    }

    /// 写入草稿；与实算状态一致时不留草稿条目（草稿非空 ⇔ 有未保存改动）。
    fn stage_skill_state(&mut self, name: &str, enabled: bool) {
        self.skill_draft.retain(|(n, _)| n != name);
        let base = self
            .skill_rows
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.enabled)
            .unwrap_or(false);
        if base != enabled {
            self.skill_draft.push((name.to_string(), enabled));
        }
    }

    /// 空格/回车：切换选中技能的草稿状态（只改草稿，面板勾选即时可见）。
    pub(super) fn toggle_selected_skill(&mut self) {
        let Some(layer) = self.panel.top() else {
            return;
        };
        let filtered = self.panel.filtered_items();
        let Some(item) = filtered.get(layer.selected).cloned() else {
            return;
        };
        let name = item.value.clone();
        let next = !self.skill_state(&name);
        self.stage_skill_state(&name, next);
        self.refresh_skill_labels();
        self.dirty = true;
    }

    /// 按草稿优先的状态重建面板内全部 `[x]/[ ] 名称` 标签与注记。
    pub(super) fn refresh_skill_labels(&mut self) {
        let labels: Vec<(String, String)> = match self.panel.top() {
            Some(layer) => layer
                .items
                .iter()
                .map(|it| {
                    let on = self.skill_state(&it.value);
                    let notes = self
                        .skill_rows
                        .iter()
                        .find(|r| r.name == it.value)
                        .map(|r| r.notes(on))
                        .unwrap_or_default();
                    (
                        format!("[{}] {}", if on { "x" } else { " " }, it.value),
                        notes,
                    )
                })
                .collect(),
            None => return,
        };

        if let Some(layer) = self.panel.top_mut() {
            for (it, (label, notes)) in layer.items.iter_mut().zip(labels) {
                it.label = label;
                it.desc = notes;
            }
        }
    }

    /// 回车：进入二级详情（SKILL.md 头信息 + 来源 + 路径）。
    pub(super) fn open_skill_detail(&mut self) {
        let Some(layer) = self.panel.top() else {
            return;
        };
        let filtered = self.panel.filtered_items();
        let Some(item) = filtered.get(layer.selected) else {
            return;
        };
        let name = item.value.clone();
        let Some(row) = self.skill_rows.iter().find(|r| r.name == name).cloned() else {
            return;
        };
        self.skill_detail = row.detail_lines();
        self.panel.push(PanelKind::SkillDetail, name, Vec::new());
    }

    /// 关闭面板：丢弃未保存草稿（磁盘与 `settings.json` 全程未被改动）。
    pub(super) fn discard_skill_draft(&mut self) {
        self.skill_draft.clear();
        self.skill_panel_error = None;
    }

    /// Ctrl+S：把草稿落到 `settings.json` 的 `skills` 数组、落盘开启的扩展技能，
    /// 然后重扫并重建 skill 工具。
    ///
    /// 写盘失败时保留草稿并只显示错误（不发 `Reload`）；单个扩展技能落盘失败只累积错误行，
    /// 其余照常应用（不整体回滚）。
    pub(super) fn save_skill_states(&mut self) {
        if self.skill_draft.is_empty() {
            return;
        }

        let draft = std::mem::take(&mut self.skill_draft);
        let mut rules = settings_manager::read_settings_skills();
        let mut errors: Vec<String> = Vec::new();

        for (name, desired) in &draft {
            if *desired {
                if bundled::catalog_skill(name).is_some()
                    && !bundled::is_installed(name)
                    && let Err(e) = bundled::materialize(name)
                {
                    errors.push(format!("install {name}: {e}"));
                    continue;
                }
                skills::filter_remove_exact_exclude(&mut rules, name);
            } else {
                skills::filter_add_exact_exclude(&mut rules, name);
            }
        }

        if let Err(e) = settings_manager::write_settings_skills(&rules) {
            self.skill_draft = draft; // 保留草稿，用户可以改完再存
            self.skill_panel_error = Some(format!("failed to save skills: {e}"));
            self.dirty = true;
            return;
        }

        self.reload_skills();
        self.rebuild_skill_rows();
        self.refresh_skill_labels();
        self.suggestion.deselect();
        self.dirty = true;

        if errors.is_empty() {
            self.skill_panel_error = None;
            self.push_msg(
                "skills updated: tools rebuilt".to_string(),
                MsgLevel::Success,
            );
        } else {
            let err = errors.join("; ");
            self.skill_panel_error = Some(err.clone());
            self.push_msg(
                format!("skills updated with errors: {err}"),
                MsgLevel::Warning,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::AgentDirGuard;

    /// 测试：书写一个最小技能目录。
    fn write_skill(root: &std::path::Path, name: &str, body: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: d-{name}\n---\n{body}"),
        )
        .unwrap();
    }

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// 面板只列“无过滤扫描 + 扩展技能目录”的并集，且未落盘的扩展技能带 `[not installed]`。
    #[test]
    fn rows_include_uninstalled_bundled_skills() {
        let _g = lock();
        let _ad = AgentDirGuard::temp();
        let _home = crate::test_support::HomeGuard::temp();
        let agent = crate::core::settings_manager::agent_dir();
        write_skill(&agent.join("skills"), "local-only", "x");

        let mut st = App::new();
        st.cwd = agent.display().to_string();
        st.reload_ctx.include_default_skills = true;
        st.rebuild_skill_rows();

        let local = st.skill_rows.iter().find(|r| r.name == "local-only");
        assert!(local.is_some(), "本地技能应出现在面板里");
        assert!(local.unwrap().installed);

        let bundled_row = st
            .skill_rows
            .iter()
            .find(|r| r.name == "grill-me")
            .expect("未落盘的扩展技能也应列出");
        assert!(!bundled_row.installed, "磁盘上没有副本 → 未安装");
        assert_eq!(bundled_row.source, "builtin");
        assert!(bundled_row.notes(false).contains("[not installed]"));
    }

    /// 已从别的技能根目录（如 `~/.agents/skills`）扫到的扩展技能不算「未安装」：
    /// 它本来就在磁盘上、也真的处于启用态，再标 `[not installed]` 会让「勾选 + 未安装」自相矛盾。
    #[test]
    fn discovered_bundled_skill_is_not_marked_uninstalled() {
        let _g = lock();
        let _ad = AgentDirGuard::temp();
        let home = tempfile::tempdir().unwrap();
        let _home = crate::test_support::HomeGuard::set(home.path());
        write_skill(&home.path().join(".agents").join("skills"), "grill-me", "x");

        let mut st = App::new();
        st.cwd = crate::core::settings_manager::agent_dir()
            .display()
            .to_string();
        st.reload_ctx.include_default_skills = true;
        st.rebuild_skill_rows();

        let row = st
            .skill_rows
            .iter()
            .find(|r| r.name == "grill-me")
            .expect("应列出 grill-me");
        assert!(row.installed, "扫到的技能必须算已安装: {row:?}");
        // source 是发现根目录（`~` 缩写读的是真实 $HOME，测试里断言后缀即可）
        assert!(row.source.ends_with(".agents/skills"), "{row:?}");
        assert!(!row.notes(true).contains("[not installed]"), "{row:?}");
    }

    /// 内置技能的落盘副本优先级最低：agent 目录里那份同名时让位于用户自己的副本。
    #[test]
    fn bundled_copy_yields_to_user_copy() {
        let _g = lock();
        let _ad = AgentDirGuard::temp();
        let home = tempfile::tempdir().unwrap();
        let _home = crate::test_support::HomeGuard::set(home.path());
        let agent = crate::core::settings_manager::agent_dir();

        write_skill(&agent.join("skills"), "grill-me", "bundled");
        write_skill(
            &home.path().join(".agents").join("skills"),
            "grill-me",
            "user",
        );

        let mut st = App::new();
        st.cwd = agent.display().to_string();
        st.reload_ctx.include_default_skills = true;
        st.rebuild_skill_rows();

        let row = st
            .skill_rows
            .iter()
            .find(|r| r.name == "grill-me")
            .expect("应列出 grill-me");
        assert!(
            row.path.starts_with(&home.path().display().to_string()),
            "应选用户副本而不是内置副本: {row:?}"
        );
        assert!(
            row.path.ends_with(".agents/skills/grill-me/SKILL.md"),
            "{row:?}"
        );
    }

    /// 草稿 → Ctrl+S：写 `settings.json` 的 `-name`、落盘扩展技能、重扫后勾选态回读一致。
    #[test]
    fn ctrl_s_persists_disable_and_installs_bundled() {
        let _g = lock();
        let _ad = AgentDirGuard::temp();
        let _home = crate::test_support::HomeGuard::temp();
        let agent = crate::core::settings_manager::agent_dir();
        crate::core::settings_manager::write_settings_skills(&[]).ok();

        let mut st = App::new();
        st.cwd = agent.display().to_string();
        st.reload_ctx.include_default_skills = true;
        st.open_skills_panel();

        // 初始：未落盘 → 未启用（默认不启用，只有显式开启才落盘）
        let row = st.skill_rows.iter().find(|r| r.name == "grill-me").unwrap();
        assert!(!row.enabled && !row.installed, "{row:?}");

        // 开启 grill-me：只写草稿，磁盘与 settings.json 均不动
        st.stage_skill_state("grill-me", true);
        assert!(!st.skill_draft.is_empty());
        assert!(crate::core::settings_manager::read_settings_skills().is_empty());
        assert!(!bundled::is_installed("grill-me"));

        // Ctrl+S：落盘扩展技能 + 重扫；勾选态回读一致
        st.save_skill_states();
        assert!(st.skill_draft.is_empty(), "保存后草稿应清空");
        assert!(bundled::is_installed("grill-me"), "开启应「检查并创建」");
        assert!(st.skill_state("grill-me"), "开启态应回读一致");
        assert!(
            st.skills.iter().any(|s| s.name == "grill-me"),
            "应生效在技能列表里"
        );

        // 关闭：写精确 `-name`，不删磁盘副本
        st.stage_skill_state("grill-me", false);
        st.save_skill_states();
        let rules = crate::core::settings_manager::read_settings_skills();
        assert!(rules.iter().any(|r| r == "-grill-me"), "{rules:?}");
        assert!(!st.skill_state("grill-me"), "关闭态应回读一致");
        assert!(bundled::is_installed("grill-me"), "关闭不删磁盘副本");

        // 清理：恢复规则与磁盘
        crate::core::settings_manager::write_settings_skills(&[]).ok();
        let _ = std::fs::remove_dir_all(agent.join("skills").join("grill-me"));
    }

    /// 手写的 `!glob` 排除 → 行标 `[filtered]`，且面板删 `-name` 也开不回来。
    #[test]
    fn glob_exclusion_is_reported_as_filtered() {
        let _g = lock();
        let _ad = AgentDirGuard::temp();
        let _home = crate::test_support::HomeGuard::temp();
        let agent = crate::core::settings_manager::agent_dir();
        write_skill(&agent.join("skills"), "keep-me", "x");
        crate::core::settings_manager::write_settings_skills(&["!keep-*".to_string()]).ok();

        let mut st = App::new();
        st.cwd = agent.display().to_string();
        st.reload_ctx.include_default_skills = true;
        st.rebuild_skill_rows();

        let row = st.skill_rows.iter().find(|r| r.name == "keep-me").unwrap();
        assert!(!row.enabled, "glob 排除应使技能不生效");
        assert!(!row.has_exact_exclude, "glob 不是可删的精确项");
        assert!(row.notes(false).contains("[filtered]"));

        crate::core::settings_manager::write_settings_skills(&[]).ok();
        let _ = std::fs::remove_dir_all(agent.join("skills"));
    }

    #[test]
    fn detail_shows_frontmatter_and_source() {
        let _g = lock();
        let _ad = AgentDirGuard::temp();
        let _home = crate::test_support::HomeGuard::temp();
        let agent = crate::core::settings_manager::agent_dir();
        write_skill(&agent.join("skills"), "demo", "x");

        let mut st = App::new();
        st.cwd = agent.display().to_string();
        st.reload_ctx.include_default_skills = true;
        st.open_skills_panel();
        st.skill_detail = st
            .skill_rows
            .iter()
            .find(|r| r.name == "demo")
            .unwrap()
            .detail_lines();
        let joined = st.skill_detail.join("\n");
        assert!(joined.contains("name: demo"), "{joined}");
        assert!(joined.contains("description: d-demo"), "{joined}");
        assert!(joined.contains("source: "), "{joined}");

        let _ = std::fs::remove_dir_all(agent.join("skills"));
    }
}
