//! `skills` 扩展：统一的技能管理（`/skills` 面板）。
//!
//! 两件事：
//!
//! 1. **内置（扩展）技能目录**：`assets/skills/**` 由 `build.rs` 在编译期内嵌
//!    （[`embedded::EMBEDDED_SKILL_FILES`]），本模块按顶层目录分组为一个技能目录。
//!    这些技能**默认不落盘、不进系统提示**；只有用户在 `/skills` 面板里显式开启时，
//!    才「检查并创建」到 `agent_dir()/skills/<name>/**`（内容幂等：已存在的文件不覆盖）。
//!    落盘后的副本是**兜底**：同名时优先级最低，让位于项目/用户目录里的任何一份
//!    （名单经 [`bundled_skill_names`] 传给 [`crate::core::skills::load_skills`]）。
//! 2. **启停状态**：唯一真源是 `settings.json` 的 `skills` 过滤数组（`-<name>` 精确排除）。
//!    本扩展**不**自带配置文件；面板 Ctrl+S 时读-改-写该数组（见
//!    [`crate::core::settings_manager::write_settings_skills`]），随后重扫并重建 skill 工具。
//!
//! 同名技能按现有发现规则先到先得（`SkillCollector::push`），
//! 启停粒度是**技能名**，不区分来源；来源只作为详情页的展示信息。

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded_skills.rs"));
}

use crate::{
    core::{
        extensions::{Extension, ExtensionCommand, ExtensionMode, ExtensionTool},
        settings_manager::agent_dir,
        skills,
    },
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_SKILLS},
    modes::interactive::{app::App, handlers::register_slash_command},
};
use std::{path::PathBuf, sync::Arc};

/// 扩展名（也是 `/extension` 面板里的条目名与命令归属）。
pub const EXT: &str = "skills";
/// 斜杠命令名（不含前导 `/`）。
pub const CMD: &str = "skills";

/// 自声明工厂：linkme 分布式切片，priority 值大者先注册（见 [`crate::extensions`]）。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static SKILLS_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_SKILLS,
    make: || Arc::new(Skills::new()),
};

/// 一条随二进制分发的扩展技能（`assets/skills/<目录>` 的顶层目录）。
#[derive(Debug, Clone)]
pub struct CatalogSkill {
    /// `assets/skills` 下的目录名；frontmatter 缺 `name` 时作为技能名兜底。
    pub dir: String,
    /// 技能名（frontmatter `name`，缺失时取 `dir`）；落盘目录名也取它。
    pub name: String,
    /// frontmatter `description`（面板详情页展示）。
    pub description: String,
    /// frontmatter `disable-model-invocation`：为真时即使启用也不进 `<available_skills>`。
    pub disable_model_invocation: bool,
    /// 该技能目录下的全部文件：(技能目录内 POSIX 相对路径, 内容)。
    files: Vec<(String, &'static [u8])>,
}

impl CatalogSkill {
    /// 该技能落盘后的根目录：`agent_dir()/skills/<name>`。
    pub fn target_dir(&self) -> PathBuf {
        agent_dir().join("skills").join(&self.name)
    }

    /// 该技能落盘后的 `SKILL.md` 路径。
    pub fn target_skill_md(&self) -> PathBuf {
        self.target_dir().join("SKILL.md")
    }
}

/// 扩展技能目录：把内嵌文件表按顶层目录分组，逐个解析 `SKILL.md` 取元信息。
///
/// 没有 `SKILL.md`（或 frontmatter 不合规、缺 description）的目录会被静默跳过，
/// 与运行期扫描的口径一致；返回结果按技能名排序，保证面板顺序稳定。
pub fn catalog() -> Vec<CatalogSkill> {
    let mut dirs: Vec<String> = Vec::new();
    for (rel, _) in embedded::EMBEDDED_SKILL_FILES {
        if let Some((dir, _)) = rel.split_once('/')
            && !dirs.iter().any(|d| d == dir)
        {
            dirs.push(dir.to_string());
        }
    }

    let mut out: Vec<CatalogSkill> = Vec::new();
    for dir in dirs {
        let files: Vec<(String, &'static [u8])> = embedded::EMBEDDED_SKILL_FILES
            .iter()
            .filter_map(|(rel, bytes)| {
                rel.split_once('/')
                    .filter(|(d, _)| *d == dir)
                    .map(|(_, rest)| (rest.to_string(), *bytes))
            })
            .collect();

        let Some((_, skill_md)) = files.iter().find(|(rel, _)| rel == "SKILL.md") else {
            continue;
        };

        let Ok(content) = std::str::from_utf8(skill_md) else {
            continue;
        };

        let name = skills::skill_name_from_content(content, &dir);

        // 目标路径用技能名（落盘目录名 = 技能名），据此解析出 description 等元信息
        let path = agent_dir().join("skills").join(&name).join("SKILL.md");
        let Some(parsed) = skills::load_skill_from_content(&path, "builtin", content) else {
            continue;
        };

        out.push(CatalogSkill {
            dir,
            name: parsed.name,
            description: parsed.description,
            disable_model_invocation: parsed.disable_model_invocation,
            files,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// 按技能名取目录条目。
pub fn catalog_skill(name: &str) -> Option<CatalogSkill> {
    catalog().into_iter().find(|c| c.name == name)
}

/// 内置（随二进制分发）技能的名单，供 [`crate::core::skills::load_skills`] 传 `bundled_names`：
/// 这些名字的落盘副本会被垫到最低优先级（同名时让位于用户自己写的技能）。
pub fn bundled_skill_names() -> Vec<String> {
    catalog().into_iter().map(|c| c.name).collect()
}

/// 该扩展技能是否已落盘（`agent_dir()/skills/<name>/SKILL.md` 是文件）。
pub fn is_installed(name: &str) -> bool {
    agent_dir()
        .join("skills")
        .join(name)
        .join("SKILL.md")
        .is_file()
}

/// 「检查并创建」：把该扩展技能缺失的文件写入 `agent_dir()/skills/<name>/**`。
///
/// 已存在的文件**一律不覆盖**（对齐 `extra-themes` 的内容幂等口径，保护外部定制）；
/// 返回本次新写入的文件数；技能名不在目录中或写盘失败时返回 `Err`。
pub fn materialize(name: &str) -> Result<usize, String> {
    let Some(skill) = catalog_skill(name) else {
        return Err(format!("not a bundled skill: {name}"));
    };

    let root = skill.target_dir();
    let mut written = 0usize;
    for (rel, bytes) in &skill.files {
        let target = root.join(rel);
        if target.is_file() {
            continue;
        }

        if let Some(parent) = target.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return Err(format!("create {}: {e}", parent.display()));
        }

        if let Err(e) = std::fs::write(&target, bytes) {
            return Err(format!("write {}: {e}", target.display()));
        }

        written += 1;
    }
    Ok(written)
}

/// `skills` 扩展：只有一条 `/skills` 命令，无工具、无钩子。
#[derive(Default)]
pub struct Skills;

impl Skills {
    /// 构造扩展实例（无内部状态）。
    pub fn new() -> Self {
        Self
    }
}

/// `/skills` 命令入口：打开技能管理面板。
fn command_skills(st: &mut App, _raw: &str) -> bool {
    st.open_skills_panel();
    false
}

impl Extension for Skills {
    /// 返回固定扩展名 `skills`。
    fn name(&self) -> &str {
        EXT
    }

    /// 返回 `/extension` 面板展示的一句话描述。
    fn description(&self) -> &str {
        "Manage skills: enable/disable per name and install bundled skills into agent_dir/skills"
    }

    /// 声明在 Dev 与 Creator 两种模式下可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 默认关闭
    fn default_enabled(&self) -> bool {
        false
    }

    /// 不提供任何工具。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/skills` 命令（存在性随扩展启用状态动态增删）。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description: "Manage skills (enable/disable, install bundled)".to_string(),
            busy_safe: true,
            subcommands: Vec::new(),
        }]
    }

    /// 注册 `/skills` 的 TUI 执行入口。
    fn on_registered(&self) {
        register_slash_command(EXT, CMD, command_skills);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_groups_embedded_files_by_directory() {
        let cat = catalog();
        assert!(!cat.is_empty(), "内嵌目录不应为空");
        // 排序稳定且不重名
        let names: Vec<&str> = cat.iter().map(|c| c.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "目录应按技能名排序");
        assert_eq!(
            names.iter().collect::<std::collections::HashSet<_>>().len(),
            names.len(),
            "技能名不应重复: {names:?}"
        );

        // grill-me 是单文件技能；diagnosing-bugs 带 scripts/ 子文件
        let grill = cat.iter().find(|c| c.name == "grill-me").expect("grill-me");
        assert!(grill.description.to_lowercase().contains("grill"));
        assert!(grill.disable_model_invocation);
        assert_eq!(grill.files.len(), 1);

        let diag = cat
            .iter()
            .find(|c| c.name == "diagnosing-bugs")
            .expect("diagnosing-bugs");
        assert!(
            diag.files
                .iter()
                .any(|(rel, _)| rel == "scripts/hitl-loop.template.sh"),
            "子目录文件必须一并内嵌: {:?}",
            diag.files.iter().map(|(r, _)| r).collect::<Vec<_>>()
        );
    }
}
