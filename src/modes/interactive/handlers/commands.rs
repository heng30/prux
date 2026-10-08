//! /斜杠命令分发

use super::expand_user_text;
use crate::{
    PROJECT_SCOPE_NAME,
    cli::args::VALID_THINKING_LEVELS,
    core::{
        self,
        changelog::CHANGELOG,
        extensions::{SubcommandDef, dispatch_agent_event},
        keybindings,
        keybindings::KEYBINDINGS,
        prompt_templates::{self, PromptTemplate},
        provider::AgentMessage,
        session_manager, settings_manager,
        skills::{self, Skill},
    },
    extensions::skills::bundled_skill_names,
    modes::interactive::{
        agent_actor::AgentCommand,
        app::{App, MsgLevel, SysSpan, WorkingKind},
        theme::{Theme, available_theme_names},
    },
    utils::{glyphs::DEF_DONE, paths::resolve_path, wheel::WheelScrollAccelerator},
};
use std::{
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

/// 扩展注册的键位回调：用户按下绑定键后直接操作 App 状态。
pub type ExtensionKeybindingHandler = fn(&mut App);
/// 扩展注册的斜杠命令回调：接收命令名后的参数字符串，返回是否已处理。
type SlashCommandHandler = fn(&mut App, &str) -> bool;

/// 忙碌时可安全执行的内置命令（handler 不锁 agent、纯 UI 操作）。
/// 其余内置命令忙碌时仍入 steer 队列，避免与可能锁 agent 的 handler 死锁。
const BUILTIN_BUSY_SAFE: &[&str] = &["dock"];

/// 内置斜杠命令候选（名称, 描述, 子命令；不显示短命令/别名）。
/// 内置命令不属于任何扩展、无生命周期，保持静态注册；
/// 扩展命令见 [`slash_commands`]（随扩展启用状态动态增删）。
const BUILTIN_COMMANDS: &[(&str, &str, &[SubcommandDef])] = &[
    ("quit", "Quit the TUI", &[]),
    ("compact", "Compact the context", &[]),
    ("copy", "Copy last agent message to clipboard", &[]),
    ("new", "Start a new session", &[]),
    ("hotkeys", "Show keybindings", &[]),
    ("thinking", "Set thinking level (<level>)", &[]),
    ("session", "Show current session info", &[]),
    ("changelog", "Show changelog entries", &[]),
    ("resume", "List/switch sessions (<index>)", &[]),
    (
        "history",
        "Search prompt history (live filter; @clear / @clear-all to delete)",
        &[],
    ),
    ("trust", "Trust the current project", &[]),
    (
        "bug",
        "Report a bug (collects diagnostics, exports a zip)",
        &[],
    ),
    ("tree", "Show session tree / navigate (<entryId> [-s])", &[]),
    ("label", "Set/clear session label (<entryId> [label])", &[]),
    ("fork", "Fork current session", &[]),
    (
        "clone",
        "Duplicate the current session at the current position",
        &[],
    ),
    ("share", "Share session", &[]),
    ("export", "Export session (<out> [.html|.jsonl])", &[]),
    ("theme", "Show/switch theme (<name>)", &[]),
    (
        "reload",
        "Reload keybindings, extensions, skills, prompts, themes, and context files",
        &[],
    ),
    ("settings", "Open settings", &[]),
    ("model", "Select model (<id>)", &[]),
    (
        "scoped-models",
        "Enable/disable models for Ctrl+P cycling",
        &[],
    ),
    ("name", "Rename session (<name>)", &[]),
    ("import", "Import session (<path>)", &[]),
    ("login", "Store an API key for a provider", &[]),
    ("logout", "Remove stored API key for a provider", &[]),
    ("extension", "Enable/disable extensions", &[]),
    ("dock", "Toggle the dock panel", &[]),
];

/// 扩展命令：执行入口注册表
///
/// 命令的"存在性"（是否显示、是否可执行）由已启用扩展的
/// [`crate::core::extensions::Extension::commands`] 声明动态决定；
/// 执行需要 TUI 状态（&mut App），不能下沉到 core，故入口在 TUI 层注册。
/// 扩展命令执行入口：返回 true 表示退出 TUI（与 `handle_slash_command` 一致）。
static EXT_COMMAND_HANDLERS: OnceLock<Mutex<Vec<(String, String, SlashCommandHandler)>>> =
    OnceLock::new();

/// 扩展快捷键执行入口类型：需要 TUI 状态 &mut App + agent
static EXT_KEYBINDING_HANDLERS: OnceLock<Mutex<Vec<(String, String, ExtensionKeybindingHandler)>>> =
    OnceLock::new();

/// 重载后的内容资源快照（context files / skills / prompts / system prompt）
struct ReloadedAssets {
    /// 重载到的上下文文件（路径, 内容）对，按加载顺序。
    context_files: Vec<(String, String)>,
    /// 重载到的技能列表（已按 settings.skills 过滤）。
    skills: Vec<Skill>,
    /// 重载到的提示词模板列表。
    prompt_templates: Vec<PromptTemplate>,
    /// 发现到的系统提示；None = 未找到 SYSTEM.md 或内容为空。
    system_prompt: Option<String>,
    /// 发现到的追加系统提示；None = 未找到 APPEND_SYSTEM.md。
    append_system_prompt: Option<String>,
}

impl App {
    /// 命令候选集中的处理函数（handle_slash_command）：返回 true 表示退出 TUI
    pub(super) fn handle_slash_command(&mut self, cmd: &str) -> bool {
        let raw = cmd;
        let cmd = cmd.trim().to_lowercase();
        let mut quit = false;
        match cmd.as_str() {
            "quit" | "q" => quit = self.cmd_quit(),
            "compact" => self.cmd_compact(raw),
            "reload" => self.cmd_reload(),
            "copy" => self.cmd_copy(),
            "new" => self.cmd_new(),
            "session" => self.cmd_session(),
            "changelog" => self.cmd_changelog(),
            "resume" => self.cmd_resume(),
            "history" => self.cmd_history(raw),
            "trust" => self.cmd_trust(),
            "tree" => self.cmd_tree(""),
            "share" => self.cmd_share(),
            "theme" => self.cmd_theme(),
            "settings" => self.cmd_settings(),
            "model" => self.cmd_model(),
            "extension" => self.cmd_extension(),
            "login" => self.cmd_login(),
            "logout" => self.cmd_logout(),
            "export" => self.cmd_export(None),
            "thinking" => self.cmd_thinking(&cmd),
            "scoped-models" => self.cmd_scoped_models(),
            "hotkeys" => self.cmd_hotkeys(),
            "debug" => self.cmd_debug(),
            "dock" => self.cmd_dock(),
            "bug" => self.cmd_bug(raw),
            "fork" => self.cmd_fork(),
            "clone" => self.cmd_clone(),
            _ if cmd.starts_with("compact ") => self.cmd_compact(raw),
            _ if cmd.starts_with("thinking ") => self.cmd_thinking(raw),
            _ if cmd.starts_with("export ") => self.cmd_export(Some(raw)),
            _ if cmd.starts_with("theme ") => self.cmd_theme_named(raw),
            _ if cmd.starts_with("tree ") => self.cmd_tree(raw),
            _ if cmd.starts_with("label ") => self.cmd_label(raw),
            _ if cmd.starts_with("model ") => self.cmd_model_named(raw),
            _ if cmd.starts_with("name ") => self.cmd_name(raw),
            _ if cmd.starts_with("session ") => self.cmd_session_switch(raw),
            _ if cmd.starts_with("history ") => self.cmd_history(raw),
            _ if cmd.starts_with("bug ") => self.cmd_bug(raw),
            _ if cmd.starts_with("import ") => self.cmd_import(raw),
            // 扩展命令：存在性由已启用扩展声明动态决定（禁用/模式过滤后不进入此分支）；
            // 执行入口查 TUI 注册表。命令名取首个空白前的部分（`/plan foo` 命中 `plan`），
            // 原文（含参数，保留大小写）传给 handler。未命中内置与扩展命令 → 未知命令
            // 作为普通用户输入（与 /skill、模板展开同一路径）。
            _ => match extension_command_handler(command_name(&cmd)) {
                Some(handler) => quit = handler(self, raw.trim()),
                None => self.cmd_unknown(&cmd),
            },
        }
        self.dirty = true;
        quit
    }

    /// 处理 `/quit`：置退出标志，返回 `true` 让主循环结束本轮并退出。
    fn cmd_quit(&mut self) -> bool {
        self.quit_requested = true;
        true
    }

    /// 处理 `/compact [自定义摘要指令]`：置待压缩标志并记录指令（缺省则用内置摘要提示词）；
    /// 忙碌态与结果提示由状态栏/worker 回执呈现，此处不往文本区推送提示。
    // /compact [instructions]：解析自定义摘要指令
    // 不向文本区推送提示：压缩中的忙碌态由状态栏呈现（start_next_action 设置
    // status + WorkingKind::Compaction），文本区只在完成后输出压缩结果提示。
    fn cmd_compact(&mut self, raw: &str) {
        let instructions = raw.trim_start_matches("compact").trim();
        self.pending_compact = true;
        self.compact_instructions = if instructions.is_empty() {
            None
        } else {
            Some(instructions.to_string())
        };
    }

    /// 处理 `/reload`：重载 keybindings/扩展/skills/prompts/themes/context 并重建系统提示，
    /// 成功后输出一行提示。
    fn cmd_reload(&mut self) {
        self.reload_all_resources();
        self.push_msg(
            format!(
                "{DEF_DONE} Reloaded keybindings, extensions, skills, prompts, themes, and context files"
            ),
            MsgLevel::Success,
        );
    }

    /// 项目信任面板确认后重载项目资源（context/skills/prompts/system prompt + 主题等）；
    /// 与 /reload 共用同一实现，但不额外输出「Reloaded」提示（信任回执已单独提示）。
    pub(super) fn reload_project_resources(&mut self) {
        self.reload_all_resources();
    }

    /// 重载 keybindings、extensions、skills、prompts、themes、context files，
    /// 并重建系统提示/工具列表（应用扩展启用状态）。/reload 与项目信任确认共用。
    fn reload_all_resources(&mut self) {
        // 重载 keybindings、extensions、skills、prompts、themes、context files，
        // 并重建系统提示/工具列表（应用扩展启用状态）
        let agent_dir = settings_manager::agent_dir();

        // 1) settings → 同步内存设置（showThinking / autocompleteMaxVisible / historyMaxEntries）
        self.set_show_thinking(!settings_manager::read_settings_hide_thinking());
        self.autocomplete_max_visible = settings_manager::read_settings_autocomplete_max_visible();
        self.wheel_accel = WheelScrollAccelerator::for_terminal(
            settings_manager::read_settings_fullscreen_wheel_scroll_lines(),
        );
        // 历史档位同步（可被外部改 settings.json 后 `/reload` 生效）+ 按新档位截断内存列表。
        // 刻意**不从磁盘重新载入列表**：档位 0 期间提交的 prompt 只存在于内存，
        // 用磁盘内容覆盖会把它们静默丢掉；而档位 > 0 时每次提交都已追写磁盘，
        // 内存列表本就是磁盘的超集，重载拿不到额外东西。
        self.history_max_entries = settings_manager::read_settings_history_max_entries();
        self.editor.cap_history(self.history_max_entries);

        // 2) keybindings：重新读取 agent_dir/keybindings.json 并替换全局表
        self.reload_keybindings_with_conflict_warning(&agent_dir);

        // 3) extensions：重新应用 settings.json 的 disabledExtensions / enabledExtensions + extensionMode
        core::extensions::reload_from_settings();

        // 重载后声明问题可能变化（启用/禁用、extensionMode 切换）→ 重新告警
        for warning in core::extensions::registration_issues() {
            self.push_msg(warning, MsgLevel::Warning);
        }

        // 4) 从 agent 取 reload 所需参数并重载 context files / skills / prompts / system prompt
        let home_dir = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/"));
        let assets = self.reload_content_assets(&agent_dir, &home_dir);

        // frontmatter 不合法的 prompt 模板：重载后同样提示
        for w in prompt_templates::take_load_warnings() {
            self.push_msg(w, MsgLevel::Warning);
        }

        let ReloadedAssets {
            context_files,
            skills,
            prompt_templates,
            system_prompt,
            append_system_prompt,
        } = assets;

        // 8) actor：skills/context/system prompt 缓存传入 worker 由其重建系统提示/工具
        // （compose_tools 消费 worker 侧 agent.skills + rebuild_ctx，UI 侧副本仅用于 /skill、模板展开）
        self.skills = skills.clone();
        self.prompt_templates = prompt_templates;
        self.worker.send(AgentCommand::Reload {
            context_files,
            skills,
            system_prompt,
            append_system_prompt,
        });

        // 9) 主题：按当前主题名重新读取磁盘文件（默认/CLI 显式主题名保留）
        self.reload_theme();
    }

    /// 只重载「内容资源」里的 skills 部分并下发 worker（`/skills` 面板 Ctrl+S 用）。
    ///
    /// 走与 `/reload` 同一条 `reload_content_assets`（重扫 context files / skills /
    /// prompts / system prompt），但**不**动 keybindings、扩展启用状态与主题
    /// （面板只对技能负责）。重建 skill 工具只能靠 `AgentCommand::Reload`：
    /// `RebuildTools` 读的是 worker 侧旧 `agent.skills` 缓存，不会带上新技能。
    pub(super) fn reload_skills(&mut self) {
        let agent_dir = settings_manager::agent_dir();
        let home_dir = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/"));
        let ReloadedAssets {
            context_files,
            skills,
            prompt_templates,
            system_prompt,
            append_system_prompt,
        } = self.reload_content_assets(&agent_dir, &home_dir);

        self.skills = skills.clone();
        self.prompt_templates = prompt_templates;
        self.worker.send(AgentCommand::Reload {
            context_files,
            skills,
            system_prompt,
            append_system_prompt,
        });
    }

    /// 重载 keybindings.json 到全局表，冲突时给出警告消息
    fn reload_keybindings_with_conflict_warning(&mut self, agent_dir: &Path) {
        let kb = keybindings::set_global(keybindings::KeybindingsManager::load(agent_dir));
        if !kb.conflicts().is_empty() {
            let conflicts: Vec<String> = kb
                .conflicts()
                .iter()
                .map(|c| {
                    format!(
                        "{} is bound to multiple actions: {}",
                        c.key,
                        c.keybindings.join(", ")
                    )
                })
                .collect();
            self.push_msg(
                format!("keybindings.json conflicts: {}", conflicts.join("; ")),
                MsgLevel::Warning,
            );
        }
    }

    /// 按启动参数重载 context files / skills / prompts / system prompt。
    fn reload_content_assets(&self, agent_dir: &Path, home_dir: &Path) -> ReloadedAssets {
        // 4) 从 agent 取 reload 所需参数
        let (
            cwd,
            trusted,
            no_context_files,
            skill_paths,
            prompt_paths,
            include_default_skills,
            include_default_prompt_templates,
        ) = (
            self.cwd.clone(),
            self.reload_ctx.trusted,
            self.reload_ctx.no_context_files,
            self.reload_ctx.skill_paths.clone(),
            self.reload_ctx.prompt_paths.clone(),
            self.reload_ctx.include_default_skills,
            self.reload_ctx.include_default_prompt_templates,
        );

        // 5) context files（信任门控与启动一致；--no-context-files 保持空）
        let context_files = if no_context_files {
            Vec::new()
        } else {
            let mut files = core::context::load_project_context_files(&cwd, agent_dir);
            if !trusted {
                files.retain(|(p, _)| Path::new(p).starts_with(agent_dir));
            }
            files
        };

        // 6) skills：重读 settings.skills 过滤 + 沿用 CLI 显式路径/默认发现开关
        let settings = settings_manager::read_settings();
        let skills = skills::load_skills(
            &cwd,
            agent_dir,
            home_dir,
            &skill_paths,
            &bundled_skill_names(),
            include_default_skills,
            trusted,
            &settings.skills,
        );

        // 7) prompts
        let prompt_templates = prompt_templates::load_prompt_templates(
            &cwd,
            agent_dir,
            &prompt_paths,
            include_default_prompt_templates,
        );

        // 8) system prompt：CLI 显式覆盖优先，否则按信任状态重新发现项目/全局 SYSTEM.md、
        // APPEND_SYSTEM.md（信任授予后项目文件需立即生效）
        let append_system_prompt = if self.reload_ctx.cli_append_system_prompt.is_some() {
            self.reload_ctx.cli_append_system_prompt.clone()
        } else {
            core::context::discover_append_system_prompt(&cwd, agent_dir, trusted)
        };
        let system_prompt = core::context::discover_system_prompt(&cwd, agent_dir, trusted)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| self.reload_ctx.cli_system_prompt.clone());

        ReloadedAssets {
            context_files,
            skills,
            prompt_templates,
            system_prompt,
            append_system_prompt,
        }
    }

    /// 重载主题：按当前主题名重新读取磁盘文件（默认/CLI 显式主题名保留）
    fn reload_theme(&mut self) {
        let theme_name = self.theme.name.clone();
        let mut theme = Theme::load(&theme_name);
        theme.syntax_highlight = settings_manager::read_settings_syntax_highlight();
        theme.mermaid = settings_manager::read_settings_mermaid();
        theme.latex = settings_manager::read_settings_latex();
        self.theme = theme;
    }

    /// 处理 `/copy`：把最后一条助手消息写入系统剪贴板。
    //复制最后一条助手消息到剪贴板
    fn cmd_copy(&mut self) {
        self.copy_last_assistant_message();
    }

    /// 处理 `/new`：清空当前 transcript、流式与滚动状态，通知扩展会话级状态重置，
    /// 并由 worker 开启一个全新会话。
    fn cmd_new(&mut self) {
        self.messages.clear();
        self.system_messages.clear();
        self.expand_overrides.clear(); // 会话切换：丢弃上一条 transcript 的逐块展开态
        self.clear_streaming();
        self.context_tokens_override = None;
        self.current_session_path = None;
        self.scroll = 0;
        self.max_scroll = 0;
        self.last_render_total = 0;
        self.refresh_context_percent();
        self.invalidate_render();

        // 通知扩展（如 footer(rich)）重置会话级状态
        dispatch_agent_event(&serde_json::json!({ "type": "session:new" }));
        self.worker.send(AgentCommand::NewSession);
        self.push_msg(format!("{DEF_DONE} New session started"), MsgLevel::Success);
    }

    /// 处理 `/session`：向 worker 请求当前会话统计，回执由主循环渲染成提示。
    // 显示当前会话信息：发 SessionStats 命令，回执 session_stats 由主循环渲染
    fn cmd_session(&mut self) {
        self.worker.send(AgentCommand::SessionStats);
    }

    /// 处理 `/resume`：打开会话选择器以恢复历史会话。
    // /resume 打开选择器
    fn cmd_resume(&mut self) {
        self.open_session_panel();
    }

    /// 处理 `/trust`：弹出项目信任选择面板（Trust / Trust parent / Do not trust），
    /// 保存后提示重启生效。
    fn cmd_trust(&mut self) {
        // 弹出选择面板（Trust / Trust parent / Do not trust），保存后提示重启生效
        self.open_project_trust_dialog();
    }

    /// 处理 `/tree`：请求 worker 输出会话树（参数未使用）。
    fn cmd_tree(&mut self, _cmd: &str) {
        self.worker.send(AgentCommand::Tree);
    }

    /// 处理 `/label <entryId> [标签]`：给指定树节点打标签，标签缺省时清除该节点的标签。
    fn cmd_label(&mut self, cmd: &str) {
        let rest = cmd_arg(cmd, "label");
        let (id, label) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let label = label.trim();
        self.worker.send(AgentCommand::Label {
            entry_id: id.to_string(),
            label: if label.is_empty() {
                None
            } else {
                Some(label.to_string())
            },
        });
    }

    /// 处理 `/fork`：整树复制当前会话为新分支；无当前会话时只提示错误、不发起复制。
    // fork_from 整树复制走 ForkFrom（与修复面板同一条路径）；无会话时 UI 直接拦下
    fn cmd_fork(&mut self) {
        let Some(path) = self.current_session_path.clone() else {
            self.push_msg("no current session".to_string(), MsgLevel::Error);
            return;
        };
        self.worker.send(AgentCommand::ForkFrom { path, dir: None });
    }

    /// /clone：按当前 leaf 复制活动分支，worker 内完成
    fn cmd_clone(&mut self) {
        self.worker.send(AgentCommand::ForkClone);
    }

    /// /share：会话导出 + gist 上传由 worker 执行（回执 command_notice）
    /// 设置忙碌态展示 spinner，真正的分享在 worker 完成（on_share_done 复位）。
    fn cmd_share(&mut self) {
        self.busy = true;
        self.working_kind = WorkingKind::Sharing;
        self.clear_status();
        self.worker.send(AgentCommand::Share {
            theme: Some(self.theme.name.clone()),
        });
    }

    /// /export：会话导出由 worker 执行（回执 command_notice）
    fn cmd_export(&mut self, out: Option<&str>) {
        let out = out.and_then(parse_path_arg);
        let theme = Some(self.theme.name.clone());
        self.worker.send(AgentCommand::Export { out, theme });
    }

    /// 处理 `/theme`：打开主题选择面板。
    fn cmd_theme(&mut self) {
        self.open_theme_panel();
    }

    /// 处理 `/theme <名称>`：名称可用时切换主题并持久化到 settings.json，
    /// 未知名称则提示可用主题列表、不切换。
    fn cmd_theme_named(&mut self, cmd: &str) {
        let name = cmd_arg(cmd, "theme").to_string();
        let cwd = self.cwd.clone();
        let available =
            available_theme_names(&cwd, &settings_manager::agent_dir(), !self.no_themes);

        if available.iter().any(|n| n == &name) {
            self.theme = Theme::load(&name);
            self.invalidate_render(); // 主题已变：清消息渲染缓存，避免旧主题颜色的块残留
            self.push_msg_dedup(
                format!("switched theme: {}", self.theme.name),
                "switched theme: ",
                MsgLevel::Info,
            );

            //成功切换后持久化到 settings.json（重启恢复）
            if let Err(e) = settings_manager::write_theme(&name) {
                self.push_msg(format!("failed to persist theme: {}", e), MsgLevel::Error);
            }
        } else {
            self.push_msg(
                format!(
                    "unknown theme: {} (available: {})",
                    name,
                    available.join(", ")
                ),
                MsgLevel::Warning,
            );
        }
    }

    /// 处理 `/settings`：先关闭其他模态选择器，再打开设置面板。
    // 打开设置选择器（与 panel / session selector 互斥，先关闭其他模态选择器）
    fn cmd_settings(&mut self) {
        self.panel.close();
        self.session_selector.close();
        self.ext_settings.close();
        self.settings_selector.open(
            self.show_thinking,
            &self.theme.name,
            self.autocomplete_max_visible,
            self.thinking_level.as_deref().unwrap_or("off"),
        );
    }

    /// 处理 `/model`：打开模型选择面板。
    fn cmd_model(&mut self) {
        self.open_model_panel();
    }

    /// 处理 `/extension`：打开扩展管理面板。
    fn cmd_extension(&mut self) {
        self.open_extension_panel();
    }

    /// 处理 `/login`：打开登录认证面板。
    fn cmd_login(&mut self) {
        self.open_login_auth_panel();
    }

    /// 处理 `/logout`：打开登出面板。
    fn cmd_logout(&mut self) {
        self.open_logout_panel();
    }

    /// 处理 `/model <名称>`：按 canonical 名或 id 匹配切换模型；存在 scope 时只在 scope 内查找，
    /// 未命中则提示并放弃切换。切换不写回 settings.json。
    fn cmd_model_named(&mut self, cmd: &str) {
        let arg = cmd_arg(cmd, "model").to_string();

        // 有 scope 时只在 scope 内匹配
        if !self.model_cycle.is_empty() {
            let arg_l = arg.to_lowercase();
            let matched = self.model_cycle.iter().find_map(|m| {
                if m.canonical().to_lowercase() == arg_l || m.id.to_lowercase() == arg_l {
                    Some((m.provider.clone(), m.id.clone()))
                } else {
                    None
                }
            });
            let Some((provider, model_id)) = matched else {
                self.push_msg(
                    format!("model not found in scope: {arg}"),
                    MsgLevel::Warning,
                );
                return;
            };

            self.maybe_append_to_scope(&provider, &model_id);
            self.worker.send(AgentCommand::SwitchModel {
                provider,
                model_id,
                persist: false,
            });
            return;
        }

        let provider = self.current_provider.clone().unwrap_or_default();
        self.worker.send(AgentCommand::SwitchModel {
            provider,
            model_id: arg,
            persist: false,
        });
    }

    /// 处理 `/name <名称>`：设置当前会话的显示名称。
    fn cmd_name(&mut self, cmd: &str) {
        let name = cmd_arg(cmd, "name").to_string();
        self.worker.send(AgentCommand::SetSessionName { name });
    }

    /// 处理 `/session <序号>`：按 1 起的序号恢复 `list_sessions` 中的会话；
    /// 序号越界或非数字时只提示可用范围，不切换。
    fn cmd_session_switch(&mut self, cmd: &str) {
        let arg = cmd_arg(cmd, "session").to_string();
        let cwd = self.cwd.clone();
        let agent_dir = settings_manager::agent_dir();
        let dir = session_manager::default_session_dir(&cwd, &agent_dir);
        let sessions = session_manager::list_sessions(&dir);

        match arg.parse::<usize>() {
            Ok(idx) if idx >= 1 && idx <= sessions.len() => {
                let path = sessions[idx - 1].to_string_lossy().to_string();
                self.worker.send(AgentCommand::ResumeSession { path });
            }
            Ok(0) => {
                self.push_msg(
                    "invalid index: 0 (available 1-{})".replace("{}", &sessions.len().to_string()),
                    MsgLevel::Warning,
                );
            }
            _ => {
                self.push_msg(
                    format!("invalid index: {} (available 1-{})", arg, sessions.len()),
                    MsgLevel::Warning,
                );
            }
        }
    }

    /// 处理 `/import <path.jsonl>`：去掉可选的 `@` 前缀并按当前目录解析相对路径，
    /// 文件存在时弹确认面板（确认后替换当前会话，文件损坏再走修复面板）；
    /// 参数为空或文件不存在则提示错误。
    fn cmd_import(&mut self, cmd: &str) {
        let raw = cmd_arg(cmd, "import");
        if raw.is_empty() {
            self.push_msg("Usage: /import <path.jsonl>".to_string(), MsgLevel::Error);
            return;
        }

        // 参数以 @ 开头时，移除 @ 后的内容为实际路径。
        let path_arg = raw.strip_prefix('@').unwrap_or(raw);
        if path_arg.is_empty() {
            self.push_msg("Usage: /import <path.jsonl>".to_string(), MsgLevel::Error);
            return;
        }

        // 参数是路径：~/绝对路径/相对路径（相对当前目录；纯文件名=当前目录下的 jsonl）。
        let source = resolve_path(path_arg, &self.cwd);
        if !source.is_file() {
            self.push_msg(
                format!(
                    "import failed: Failed to read session file: {}: No such file or directory",
                    source.display()
                ),
                MsgLevel::Error,
            );
            return;
        }

        // 替换当前会话前先弹确认面板。Yes 后执行导入；
        // 文件损坏时 open_checked 返回 Repairable → 再弹修复面板。
        self.stage_import_confirm(source);
    }

    /// `/history [关键词]`：历史搜索面板的**兜底入口**，绝不把内容发给模型。
    ///
    /// 正常路径下 `/history` 一打完就被 `refresh_suggestions` 的历史分支接管
    /// （面板出现、实时过滤、Enter 发送选中项），根本走不到派发层。可达路径只有两条：
    /// - 面板被 Esc 关掉后再按 Enter；
    /// - 零匹配时面板虽在，但 Enter 已在 `handle_input_submit_key` 里被吞掉（不到这里）。
    ///
    /// 所以这里做的是「把 `/history <原关键词>` 写回编辑器」，让随后的
    /// `refresh_suggestions` 重新接管面板——相当于 Esc 后再 Enter 就是重新打开搜索。
    ///
    /// **绝不能落到 `cmd_unknown`**：否则 `/history foo` 会被当成普通 prompt 发给模型，
    /// 「搜历史」变成「向模型发垃圾」。
    fn cmd_history(&mut self, raw: &str) {
        let arg = cmd_arg(raw, "history").trim().to_string();

        // 控制词：直接执行（弹确认面板），与面板里选中控制词那一行同一条路。
        // 这条路径用于面板未激活的情形（Esc 关掉后按 Enter）：`/history @clear`
        // 无论面板开没开都应该是「清历史」，而不是「重新打开搜索」。
        if arg == "@clear" || arg == "@clear-all" {
            self.open_history_clear_confirm(arg == "@clear-all");
            return;
        }

        let line = if arg.is_empty() {
            "/history".to_string()
        } else {
            format!("/history {}", arg)
        };
        self.editor.replace_all(&line);
        self.dirty = true;
    }

    /// 未识别的斜杠命令：不报错、无系统提示，按普通用户输入做 skill/模板展开后
    /// 作为一条用户消息发出（`/foo` 原样保留在文本里）。
    // 未知斜杠命令不拦截、不报错、无系统提示，当作普通用户输入
    // （skill/模板展开结果直接作为消息内容，TUI 折叠渲染 skill 块）
    fn cmd_unknown(&mut self, cmd: &str) {
        let text = format!("/{}", cmd);
        let expanded = expand_user_text(self, &text);

        self.messages.push(AgentMessage::user_text(&expanded));
        self.status_start = Some(expanded);
    }

    /// /thinking [level]：查看/设置思考级别。无参数时打开选择面板；带参数时直接校验并下发。
    fn cmd_thinking(&mut self, cmd: &str) {
        let arg = cmd_arg(cmd, "thinking").to_string();
        if arg.is_empty() {
            self.open_thinking_panel();
            return;
        }

        if !VALID_THINKING_LEVELS.contains(&arg.as_str()) {
            self.push_msg(
                format!(
                    "Invalid thinking level \"{}\". Valid values: {}",
                    arg,
                    VALID_THINKING_LEVELS.join(", ")
                ),
                MsgLevel::Error,
            );
            return;
        }
        self.worker.send(AgentCommand::SetThinking {
            level: arg,
            persist: false,
        });
    }

    /// /hotkeys：打印键位绑定摘要
    fn cmd_hotkeys(&mut self) {
        let bindings = KEYBINDINGS;
        let mut lines = vec![format!(
            "Keybindings (edit ~/{PROJECT_SCOPE_NAME}/keybindings.json to change):"
        )];
        for kb in bindings {
            if kb.default_keys.is_empty() {
                continue;
            }
            lines.push(format!(
                "  {}: {}",
                kb.default_keys.join(", "),
                kb.description
            ));
        }
        lines.push("  ctrl+x: copy last assistant message".to_string());
        self.push_msg(lines.join("\n"), MsgLevel::Info);
    }

    /// /changelog：显示当前版本的更新日志。
    ///
    /// 内容为编译期内嵌的 Markdown（`assets/changelog/v.<version>.md`，见
    /// [`crate::core::changelog`]），不进入 LLM 上下文，因此按系统消息展示
    /// （与 /hotkeys 一致）；标题段用 accent 加粗，正文为默认系统色。
    fn cmd_changelog(&mut self) {
        self.push_msg_rich(
            vec![
                SysSpan {
                    fg: Some("accent".to_string()),
                    bold: true,
                    text: format!("What's New in v{}?\n\n", env!("CARGO_PKG_VERSION")),
                },
                SysSpan::plain(CHANGELOG),
            ],
            MsgLevel::Info,
        );
    }

    /// /debug：运行状态由 worker 汇总（回执 command_notice）
    fn cmd_debug(&mut self) {
        self.worker.send(AgentCommand::DebugInfo);
    }

    /// /dock：切换停靠面板显示。打开时若没有任何扩展提供内容则提示；
    /// 关闭时复位偏移与区域。面板可见还需任一扩展提供非空内容（渲染时判空自动隐藏）。
    fn cmd_dock(&mut self) {
        if !self.dock_visible {
            if core::extensions::dock_sections().is_empty() {
                self.push_msg(
                    "No dock content. Enable an extension that provides it (e.g. /plan)."
                        .to_string(),
                    MsgLevel::Info,
                );
            } else {
                self.dock_visible = true;
                self.dock_offset = 0;
                self.dirty = true;
            }
        } else {
            self.dock_visible = false;
            self.dock_offset = 0;
            self.dock_area = None;
            self.dirty = true;
        }
    }

    /// 处理 `/scoped-models`：打开模型 scope 管理面板。
    fn cmd_scoped_models(&mut self) {
        self.open_scoped_models_panel();
    }
}

/// 解析 "/export <path>" 等带路径命令的参数
fn parse_path_arg(cmd_with_prefix: &str) -> Option<String> {
    let space = cmd_with_prefix.find(' ')?;
    let args = cmd_with_prefix[space + 1..].trim_start();
    if args.is_empty() {
        return None;
    }
    let first = args.chars().next()?;
    if first == '"' || first == '\'' {
        let rest = &args[first.len_utf8()..];
        return rest
            .find(first)
            .map(|i| rest[..i].to_string())
            .filter(|s| !s.is_empty());
    }
    Some(args.split_whitespace().next().unwrap_or("").to_string())
}

/// 动态命令候选（名称, 描述）：内置命令 + 已启用扩展声明的命令。
/// 扩展命令随扩展生命周期增删（/extension 禁用、模式过滤）；
/// 与内置命令重名的扩展命令被排除（内置优先）。
pub(super) fn slash_commands() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = BUILTIN_COMMANDS
        .iter()
        .map(|(n, d, _)| (n.to_string(), d.to_string()))
        .collect();
    let builtin: Vec<&str> = BUILTIN_COMMANDS.iter().map(|(n, _, _)| *n).collect();
    for cmd in core::extensions::registered_commands() {
        if builtin.contains(&cmd.name.as_str()) {
            continue;
        }
        out.push((cmd.name, cmd.description));
    }
    out
}

/// 命令的子命令候选（`/<命令> ` 后的二级候选，按声明顺序）。
///
/// 优先内置表（分发也是内置优先）：内置命令命中即返回，不再看扩展，
/// 避免被同名扩展命令的声明“借用”。
pub(super) fn subcommands_for(name: &str) -> Vec<SubcommandDef> {
    if let Some((_, _, subs)) = BUILTIN_COMMANDS.iter().find(|(n, _, _)| *n == name) {
        return subs.to_vec();
    }
    core::extensions::registered_commands()
        .into_iter()
        .find(|c| c.name == name)
        .map(|c| c.subcommands)
        .unwrap_or_default()
}

/// 从斜杠命令原文中取出关键字后的参数（关键字匹配大小写不敏感，参数保留原大小写）。
/// 例如 cmd_arg("/Import /home/a/b.jsonl", "import") == "/home/a/b.jsonl"
fn cmd_arg<'a>(raw: &'a str, keyword: &str) -> &'a str {
    let trimmed = raw.trim_start();
    match trimmed.to_lowercase().strip_prefix(&keyword.to_lowercase()) {
        Some(_) => trimmed[keyword.len()..].trim_start(),
        None => trimmed,
    }
}

/// 动态注册命令
fn ext_command_handlers() -> &'static Mutex<Vec<(String, String, SlashCommandHandler)>> {
    EXT_COMMAND_HANDLERS.get_or_init(|| Mutex::new(Vec::new()))
}

/// 注册扩展命令的"执行入口"（TUI 层，供扩展接线）。
///
/// 命令的"存在性"由已启用扩展的 [`crate::core::extensions::Extension::commands`]
/// 声明动态决定，本函数只绑定"谁来执行"——执行需要 TUI 状态（&mut App），
/// 不能下沉到 core，故入口在 TUI 层注册：扩展模块在自身初始化处（如 plan-mode
/// 的 `ensure_registered`）调用本函数即可接入，无需改动 TUI 代码。
///
/// 语义：按 `(ext, cmd)` 键注册——同一扩展重复注册同一命令时替换旧接线
/// （幂等，允许扩展重新初始化）；不同扩展声明同名命令互不覆盖，分发权由
/// [`crate::core::extensions::command_provider`] 的先到先得决定。
pub fn register_slash_command(ext: &str, cmd: &str, handler: SlashCommandHandler) {
    let mut table = ext_command_handlers().lock().unwrap();
    table.retain(|(e, c, _)| e != ext || c != cmd);
    table.push((ext.to_string(), cmd.to_string(), handler));
}

/// 斜杠命令是否可在忙碌（流式输出）时安全执行。
///
/// 忙碌时 prompt future 持续持有 agent 锁，只有 [`BUILTIN_BUSY_SAFE`] 中的内置命令、
/// 以及声明了 `busy_safe`（handler 不锁 agent）的扩展命令才允许立即分发；
/// 其余命令忙碌时仍作为普通文本入 steer 队列（与历史行为一致，避免死锁）。
pub(super) fn is_busy_safe_command(cmd: &str) -> bool {
    let name = command_name(cmd).to_lowercase();
    BUILTIN_BUSY_SAFE.contains(&name.as_str()) || core::extensions::command_busy_safe(&name)
}

/// 斜杠命令名：原文首个空白前的部分（`plan foo` → `plan`）。
/// 扩展命令带参数时按名查表，参数由 handler 自行解析。
fn command_name(cmd: &str) -> &str {
    cmd.trim()
        .split_once(char::is_whitespace)
        .map_or_else(|| cmd.trim(), |(name, _)| name)
}

/// 查询扩展命令执行入口：仅当命令由当前已启用扩展声明时返回。
/// 扩展禁用 / 模式不可用时 `command_provider` 返回 None，命令按未知处理。
fn extension_command_handler(cmd: &str) -> Option<SlashCommandHandler> {
    let provider = core::extensions::command_provider(cmd)?;
    let table = ext_command_handlers().lock().unwrap();
    table
        .iter()
        .find(|(ext, name, _)| ext == &provider && name == cmd)
        .map(|(_, _, h)| *h)
}

/// 动态注册keybinding
fn ext_keybinding_handlers() -> &'static Mutex<Vec<(String, String, ExtensionKeybindingHandler)>> {
    EXT_KEYBINDING_HANDLERS.get_or_init(|| Mutex::new(Vec::new()))
}

/// 注册扩展快捷键的"执行入口"（TUI 层，供扩展接线）。
///
/// 按键"存在性"由已启用扩展的 [`crate::core::extensions::Extension::keybindings`]
/// 声明动态决定，键盘分发遍历声明、命中后查本表定位执行函数——执行需要TUI 状态（&mut App），不能下沉到 core。
/// 扩展在 `on_registered` 调用本函数即可 接入，无需改动 TUI 代码。
/// 语义与 [`register_slash_command`] 一致：按 `(ext, id)` 键注册，重复注册替换旧接线（幂等）。
pub fn register_extension_keybinding(ext: &str, id: &str, handler: ExtensionKeybindingHandler) {
    let mut table = ext_keybinding_handlers().lock().unwrap();
    table.retain(|(e, i, _)| e != ext || i != id);
    table.push((ext.to_string(), id.to_string(), handler));
}

/// 查询扩展快捷键执行入口（按扩展名 + 键 id）。分发遍历 [`crate::core::extensions::registered`]
/// 时已保证扩展启用，这里仅查表定位。
pub(super) fn extension_keybinding_handler(
    ext: &str,
    id: &str,
) -> Option<ExtensionKeybindingHandler> {
    let table = ext_keybinding_handlers().lock().unwrap();
    table
        .iter()
        .find(|(e, i, _)| e == ext && i == id)
        .map(|(_, _, h)| *h)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::arc_with_non_send_sync)]
    use super::*;
    use crate::modes::interactive::agent_actor::WorkerHandle;
    use crate::modes::interactive::app::{App, SuggestionKind};
    use crate::modes::interactive::panel::PanelKind;
    use crate::utils::fuzzy::fuzzy_filter;

    /// channels 版 worker：可断言 handler 发出的命令（分层测试 C 的 handler 层）
    fn test_agent_cmd() -> (
        WorkerHandle,
        crate::modes::interactive::agent_actor::CommandRx,
    ) {
        crate::test_support::pin_test_agent_dir();
        let (cmd_tx, cmd_rx) = crate::modes::interactive::agent_actor::channels();
        (WorkerHandle::new(cmd_tx), cmd_rx)
    }

    #[test]
    fn model_named_does_not_block_while_agent_locked() {
        // 回归：/model <id> 经命令通道下发（actor），UI 不持锁不阻塞
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (agent, mut cmd_rx) = test_agent_cmd();
        crate::core::auth::write_auth_key("deepseek", "sk-test").unwrap();
        let mut st = App::new();
        st.worker = agent;
        st.handle_slash_command("model deepseek-v4-pro");
        let cmd = cmd_rx.try_recv().expect("应发出 SwitchModel 命令");
        assert!(
            matches!(cmd, crate::modes::interactive::agent_actor::AgentCommand::SwitchModel { ref model_id, .. } if model_id == "deepseek-v4-pro"),
            "应发 SwitchModel: {cmd:?}"
        );
        crate::core::auth::remove_auth("deepseek").ok();
    }

    #[test]
    fn model_named_only_matches_within_scope() {
        // 有 scope 时：/model <id> 只在 scope 内匹配（对齐 pi），未命中提示不发命令
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (agent, mut cmd_rx) = test_agent_cmd();
        crate::core::auth::write_auth_key("deepseek", "sk-test").unwrap();
        let mut st = App::new();
        st.worker = agent;
        st.model_cycle = vec![crate::core::model_scope::ScopedModel {
            provider: "deepseek".to_string(),
            id: "deepseek-flash".to_string(),
            thinking: None,
        }];

        // scope 外：提示 + 不发送
        st.handle_slash_command("model deepseek-v4-pro");
        assert!(
            cmd_rx.try_recv().is_err(),
            "scope 外模型不应下发 SwitchModel"
        );
        assert!(
            st.system_messages.iter().any(|m| {
                crate::modes::interactive::app::sys_spans_text(&m.1.spans)
                    .contains("not found in scope")
            }),
            "应提示 not found in scope"
        );

        // scope 内：下发 SwitchModel；已在 scope → 不重复并入
        st.handle_slash_command("model deepseek-flash");
        let cmd = cmd_rx.try_recv().expect("应发 SwitchModel");
        assert!(
            matches!(
                &cmd,
                crate::modes::interactive::agent_actor::AgentCommand::SwitchModel { provider, model_id, .. }
                    if provider == "deepseek" && model_id == "deepseek-flash"
            ),
            "应发 SwitchModel: {cmd:?}"
        );
        assert_eq!(st.model_cycle.len(), 1, "已在 scope 不应重复追加");

        crate::core::auth::remove_auth("deepseek").ok();
    }

    #[test]
    fn thinking_command_opens_panel_or_sets_directly() {
        // /thinking（无参）打开选择面板；/thinking <level> 直接校验并下发 SetThinking
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (agent, mut cmd_rx) = test_agent_cmd();
        let mut st = App::new();
        st.worker = agent;

        // / 候选列表应包含 thinking（内置命令）
        assert!(
            slash_commands().iter().any(|(n, _)| n == "thinking"),
            "/ 候选列表应包含 thinking"
        );

        // 无参：打开面板（不发送命令）
        st.handle_slash_command("thinking");
        assert!(
            st.panel.active && st.panel.top().map(|l| l.kind) == Some(PanelKind::Thinking),
            "/thinking 无参应打开选择面板"
        );
        st.panel.close();

        // 带参：直接设置并下发 SetThinking
        st.handle_slash_command("thinking high");
        let cmd = cmd_rx.try_recv().expect("应发出 SetThinking 命令");
        assert!(
            matches!(
                cmd,
                crate::modes::interactive::agent_actor::AgentCommand::SetThinking { ref level, .. }
                    if level == "high"
            ),
            "应发 SetThinking high: {cmd:?}"
        );

        // 非法级别：不发送命令，仅提示
        let mut drained = 0;
        while cmd_rx.try_recv().is_ok() {
            drained += 1;
        }
        st.handle_slash_command("thinking bogus");
        let extra = cmd_rx.try_recv().is_ok();
        assert!(!extra, "非法级别不应下发命令");
        assert!(drained == 0, "drained: {drained}");
    }

    #[test]
    fn model_named_still_switches_when_idle() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (agent, mut cmd_rx) = test_agent_cmd();
        crate::core::auth::write_auth_key("deepseek", "sk-test").unwrap();
        let mut st = App::new();
        st.worker = agent;
        st.handle_slash_command("model deepseek-v4-pro");
        let cmd = cmd_rx.try_recv().expect("应发出 SwitchModel 命令");
        assert!(
            matches!(cmd, crate::modes::interactive::agent_actor::AgentCommand::SwitchModel { ref model_id, .. } if model_id == "deepseek-v4-pro"),
            "应发 SwitchModel: {cmd:?}"
        );
        crate::core::auth::remove_auth("deepseek").ok();
    }

    fn cleanup_plan_state() {
        crate::extensions::plan_mode::set_enabled(false);
        let dir = crate::core::settings_manager::agent_dir();
        let _ = std::fs::remove_file(dir.join("plan-mode.json"));
    }

    /// 测试进程未跑 main::register_extensions：注册并启用 plan-mode 扩展，使
    /// filter_tools 在 rebuild_tools 时生效（plan-mode 默认关闭，测试显式开启）。
    fn ensure_plan_registered() {
        crate::extensions::plan_mode::ensure_registered();
        crate::core::extensions::set_extension_enabled("plan-mode", true);
    }

    fn plan_agent() -> WorkerHandle {
        WorkerHandle::null()
    }

    #[test]
    fn new_command_clears_mirror_and_sends_new_session() {
        // 回归：/new 立即清空 UI 消息镜像并发 NewSession 命令（而非打开不存在的会话文件）
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (agent, mut cmd_rx) = test_agent_cmd();
        let mut st = App::new();
        st.worker = agent;
        st.messages
            .push(crate::core::provider::AgentMessage::user_text("旧内容"));
        // 残留旧会话的 context 使用率余额（应随 /new 立即刷新，而非等首条消息）
        st.context_window = 1000;
        st.context_percent = 88.0;
        st.system_messages.push((
            0,
            crate::modes::interactive::app::SysMsg::new(
                MsgLevel::Info,
                vec![crate::modes::interactive::app::SysSpan::plain("stale")],
            ),
        ));
        let quit = st.handle_slash_command("new");
        assert!(!quit);
        assert!(st.messages.is_empty(), "旧消息应被立即清空");
        assert!(st.current_session_path.is_none(), "新会话未落盘不应有路径");
        assert!(
            st.context_percent < 88.0,
            "/new 后 context 使用率应即时刷新: {}",
            st.context_percent
        );
        let cmd = cmd_rx.try_recv().expect("应发出 NewSession 命令");
        assert!(
            matches!(
                cmd,
                crate::modes::interactive::agent_actor::AgentCommand::NewSession
            ),
            "应发 NewSession: {cmd:?}"
        );
        let last_txt = st
            .system_messages
            .last()
            .map(|(_, s)| crate::modes::interactive::app::sys_spans_text(&s.spans));
        assert!(
            last_txt
                .as_deref()
                .unwrap_or("")
                .contains("New session started"),
            "got: {last_txt:?}"
        );
    }

    #[test]
    fn plan_command_toggles_tools_and_restores() {
        // 对齐 pi /plan：切换计划模式，内建写工具从模型可见工具移除/恢复
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // plan mode 内存态是进程级单例：与 plan_mode.rs 测试串行（不共享文件，只串行内存态）
        let _plan = crate::extensions::plan_mode::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _agent = plan_agent(); // 先设置 PRUX_AGENT_DIR，再动全局状态
        ensure_plan_registered();
        cleanup_plan_state();
        let mut st = App::new();
        let quit = st.handle_slash_command("plan");
        assert!(!quit);
        assert!(crate::extensions::plan_mode::is_enabled());
        // 工具可见性变化由 worker 维护（actor）；此断言由场景测试覆盖
        let last_txt = st
            .system_messages
            .last()
            .map(|(_, s)| crate::modes::interactive::app::sys_spans_text(&s.spans));
        assert!(
            last_txt
                .as_deref()
                .unwrap_or("")
                .contains("Plan mode enabled"),
            "got: {last_txt:?}"
        );

        let quit = st.handle_slash_command("plan");
        assert!(!quit);
        assert!(!crate::extensions::plan_mode::is_enabled());
        // 工具可见性变化由 worker 维护（actor）；此断言由场景测试覆盖
        cleanup_plan_state();
    }

    #[test]
    fn plan_command_with_arg_enters_mode_and_sends_prompt() {
        // /plan <text>：先进入 plan mode（只读工具集），再把参数作为输入发送
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // plan mode 内存态是进程级单例：与 plan_mode.rs 测试串行（不共享文件，只串行内存态）
        let _plan = crate::extensions::plan_mode::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _agent = plan_agent();
        ensure_plan_registered();
        crate::extensions::plan_mode::reset_state_for_tests();
        cleanup_plan_state();
        let mut st = App::new();
        assert!(!crate::extensions::plan_mode::is_enabled());

        let quit = st.handle_slash_command("plan 分析 login 流程");
        assert!(!quit);
        assert!(
            crate::extensions::plan_mode::is_enabled(),
            "带参数应先进入 plan mode"
        );
        assert_eq!(
            st.status_start.as_deref(),
            Some("分析 login 流程"),
            "参数应作为输入触发下一轮"
        );
        assert_eq!(
            st.messages.last().map(|m| m.text()).as_deref(),
            Some("分析 login 流程"),
            "参数应入消息流"
        );

        // 已开启时再带参数：不重复提示，仍发送输入；参数保留原大小写
        let msg_count = st.system_messages.len();
        let quit = st.handle_slash_command("PLAN Fix the Bug");
        assert!(!quit);
        assert_eq!(st.system_messages.len(), msg_count, "已开启不应重复提示");
        assert_eq!(st.status_start.as_deref(), Some("Fix the Bug"));

        // 仅空白参数等同无参数：切回关闭
        st.status_start = None;
        let quit = st.handle_slash_command("plan   ");
        assert!(!quit);
        assert!(
            !crate::extensions::plan_mode::is_enabled(),
            "空白参数按无参数处理（切换关闭）"
        );
        cleanup_plan_state();
    }

    #[test]
    fn todos_command_toggles_dock_panel() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _agent = plan_agent();
        // plan mode 内存态是进程级单例：与 plan_mode.rs 测试串行（不共享文件，只串行内存态）
        let _plan = crate::extensions::plan_mode::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        ensure_plan_registered();
        cleanup_plan_state();
        // 复位进程级内存 plan 状态：set_enabled(false) 有 guard（已 disabled 直接 return），
        // 可能残留非空 todos（其它 plan 测试注入后未清）污染 /todos 断言。
        crate::extensions::plan_mode::reset_state_for_tests();
        let mut st = App::new();
        // 无计划：/todos 未注册 → 按未知命令作为普通用户输入发送，不打开面板
        let quit = st.handle_slash_command("todos");
        assert!(!quit);
        assert!(
            !crate::extensions::plan_mode::is_dock_shown(),
            "空列表不应打开面板"
        );
        let last = st.messages.last().unwrap().text().to_string();
        assert_eq!(last, "/todos", "无 todos 时 /todos 按未知命令发送");
        assert!(
            !st.system_messages.iter().any(|(_, s)| {
                crate::modes::interactive::app::sys_spans_text(&s.spans).contains("No todos")
            }),
            "无 todos 时不再提示 No todos（命令未注册）"
        );

        // 注入计划步骤后 /todos 恢复为已注册命令（候选可见），打开面板
        // （不再往聊天区输出）
        crate::extensions::plan_mode::set_enabled(true);
        {
            use crate::core::extensions::Extension;
            let ext = crate::extensions::plan_mode::PlanMode::new();
            ext.on_agent_event(&serde_json::json!({
                "type": "agent_end",
                "messages": [
                    { "role": "assistant", "content": [{ "type": "text", "text": "Plan:\n1. Inspect code\n2. Verify result" }] }
                ]
            }));
        }
        // 有 todos：/todos 重新出现在命令候选
        let names: Vec<String> = slash_commands().into_iter().map(|(n, _)| n).collect();
        assert!(names.contains(&"todos".to_string()), "got: {names:?}");
        let msg_count = st.system_messages.len();
        let quit = st.handle_slash_command("todos");
        assert!(!quit);
        assert!(
            crate::extensions::plan_mode::is_dock_shown(),
            "面板未显示：/todos 应显示 plan 段"
        );
        assert!(st.dock_visible, "面板未显示：/todos 应打开面板");
        assert_eq!(
            st.system_messages.len(),
            msg_count,
            "/todos 打开面板不得往聊天区输出"
        );

        // 面板显示中再次 /todos：切换隐藏 plan 段（面板本身保持打开）
        let quit = st.handle_slash_command("todos");
        assert!(!quit);
        assert!(
            !crate::extensions::plan_mode::is_dock_shown(),
            "面板显示中 /todos 应隐藏 plan 段"
        );
        assert!(st.dock_visible, "toggle 段显隐不动面板开关");

        // 面板显示中 plan 段已隐藏，再 /todos：重新显示 plan 段
        let quit = st.handle_slash_command("todos");
        assert!(!quit);
        assert!(
            crate::extensions::plan_mode::is_dock_shown(),
            "面板显示中 /todos 应重新显示 plan 段"
        );

        // 消费遗留的 Select 请求（core UI 总线）
        let _ = crate::core::extensions::take_pending_ui();
        cleanup_plan_state();
    }

    #[test]
    fn dock_panel_renders_above_status_and_auto_hides_on_empty() {
        // 整帧渲染：dock 面板（标题 + 列表 + 右侧滚动条）位于状态栏上方；
        // 内容清空（plan 关闭）后下一帧自动隐藏并清 dock_area。
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // plan mode 内存态是进程级单例：与 plan_mode.rs 测试串行（不共享文件，只串行内存态）
        let _plan = crate::extensions::plan_mode::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // 停靠内容会遍历所有扩展（含 subagent 的 widget_lines）：与触碰 subagent 全局态的测试串行，
        // 否则并发 subagent 测试的存活/已完成记录会让"内容为空自动隐藏"断言失败。
        let _sub = crate::extensions::subagent::test_lock();
        let _agent = plan_agent();
        ensure_plan_registered();
        cleanup_plan_state();
        crate::extensions::plan_mode::reset_state_for_tests();
        crate::extensions::plan_mode::set_enabled(true);
        {
            use crate::core::extensions::Extension;
            let ext = crate::extensions::plan_mode::PlanMode::new();
            let steps: Vec<String> = (1..=20).map(|i| format!("{i}. Step number {i}")).collect();
            ext.on_agent_event(&serde_json::json!({
                "type": "agent_end",
                "messages": [
                    { "role": "assistant", "content": [{ "type": "text", "text": format!("Plan:\n{}", steps.join("\n")) }] }
                ]
            }));
        }

        use ratatui::backend::TestBackend;
        let h = 24u16;
        let backend = TestBackend::new(60, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut st = App::new();
        // 面板可见性 = 全局 dock_visible 且 plan 段内容可见
        crate::extensions::plan_mode::show_dock_section();
        st.dock_visible = true;
        terminal
            .draw(|f| crate::modes::interactive::render::render_frame(f, &mut st))
            .unwrap();
        let buf = terminal.backend().buffer();
        let row = |y: u16| -> String {
            (0..60)
                .map(|c| buf.cell((c, y)).unwrap().symbol().to_string())
                .collect()
        };

        let (status_h, pending_h, input_h) =
            crate::modes::interactive::render::layout_heights(&st, 60);
        assert_eq!(pending_h, 0);
        assert_eq!(status_h, 1);
        assert_eq!(input_h, 3);
        // 20 条 todo + 标题 = 21 行平铺，面板高 = min(21, 14) = 14 行
        let dock_start = h - (14 + status_h + input_h);
        // 标题行 + 前 13 条可见，其余在视口外（整块滚动）
        assert!(
            row(dock_start).contains("Plan Progress (0/20)"),
            "标题行: {:?}",
            row(dock_start)
        );
        assert!(
            row(dock_start + 13).contains("13. ☐ Step number 13"),
            "最后可见行: {:?}",
            row(dock_start + 13)
        );
        // 状态栏紧贴面板之下
        assert!(
            row(dock_start + 14).trim().is_empty() || row(dock_start + 14).contains("Working"),
            "面板之下是状态栏: {:?}",
            row(dock_start + 14)
        );
        // 右侧滚动条（整块：total21/view14 → thumb 顶部对齐）
        assert_eq!(buf.cell((59, dock_start + 2)).unwrap().symbol(), "█");
        assert!(st.dock_area.is_some(), "可见时记录 dock_area");

        // 内容清空（plan mode 关闭）→ 下一帧自动隐藏
        crate::extensions::plan_mode::set_enabled(false);
        terminal
            .draw(|f| crate::modes::interactive::render::render_frame(f, &mut st))
            .unwrap();
        assert!(!st.dock_visible, "内容为空应自动隐藏");
        assert!(st.dock_area.is_none(), "隐藏后应清 dock_area");

        let _ = crate::core::extensions::take_pending_ui();
        cleanup_plan_state();
    }

    #[test]
    fn dock_command_toggles_panel_with_content_guard() {
        // /dock：有内容时打开/关闭面板；无内容时仅提示、不开面板
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // plan mode 内存态是进程级单例：与 plan_mode.rs 测试串行（不共享文件，只串行内存态）
        let _plan = crate::extensions::plan_mode::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // 停靠内容会遍历所有扩展（含 subagent 的 widget_lines）：与触碰 subagent 全局态的测试串行，
        // 否则并发 subagent 测试的存活代理会让“无内容”断言失败。
        let _sub = crate::extensions::subagent::test_lock();
        let _agent = plan_agent();
        ensure_plan_registered();
        cleanup_plan_state();
        crate::extensions::plan_mode::reset_state_for_tests();

        let mut st = App::new();
        // 无内容：不开面板，提示
        let quit = st.handle_slash_command("dock");
        assert!(!quit);
        assert!(!st.dock_visible, "无内容不应打开面板");
        let last_txt = st
            .system_messages
            .last()
            .map(|(_, s)| crate::modes::interactive::app::sys_spans_text(&s.spans));
        assert!(
            last_txt
                .as_deref()
                .unwrap_or("")
                .contains("No dock content"),
            "got: {last_txt:?}"
        );

        // 注入计划（plan 段可见）后 /dock 打开；再 /dock 关闭
        crate::extensions::plan_mode::set_enabled(true);
        crate::extensions::plan_mode::show_dock_section();
        {
            use crate::core::extensions::Extension;
            let ext = crate::extensions::plan_mode::PlanMode::new();
            ext.on_agent_event(&serde_json::json!({
                "type": "agent_end",
                "messages": [
                    { "role": "assistant", "content": [{ "type": "text", "text": "Plan:\n1. Do it\n2. Verify" }] }
                ]
            }));
        }
        let quit = st.handle_slash_command("dock");
        assert!(!quit);
        assert!(st.dock_visible, "有内容应打开面板");
        let quit = st.handle_slash_command("dock");
        assert!(!quit);
        assert!(!st.dock_visible, "再次 /dock 应关闭面板");
        assert_eq!(st.dock_offset, 0, "关闭时复位偏移");

        let _ = crate::core::extensions::take_pending_ui();
        cleanup_plan_state();
    }

    #[test]
    fn other_model_command_switches_without_deadlock() {
        // 回归：/model <id> 命令切换后不得死锁（旧实现二次 lock agent），
        // 消息对齐 pi 格式 Switched to <name> (<provider>)
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (agent, mut cmd_rx) = test_agent_cmd(); // 先设置 PRUX_AGENT_DIR，再动 auth.json
        crate::core::auth::write_auth_key("deepseek", "sk-test").unwrap();
        let mut st = App::new();
        st.worker = agent;
        st.handle_slash_command("model deepseek-v4-pro");
        let cmd = cmd_rx.try_recv().expect("应发出 SwitchModel 命令");
        assert!(
            matches!(cmd, crate::modes::interactive::agent_actor::AgentCommand::SwitchModel { ref model_id, .. } if model_id == "deepseek-v4-pro"),
            "应发 SwitchModel: {cmd:?}"
        );
        crate::core::auth::remove_auth("deepseek").ok();
    }

    #[test]
    fn slash_query_fuzzy_matches_command_names() {
        // 对齐 pi fuzzyFilter：查询字符按顺序出现即可命中（不要求连续），
        // 如 /odel 命中 /model；完全不相关的命令不命中。
        // 候选取动态集合（内置 + 当前已启用扩展命令）。
        let q = "odel";
        let commands = slash_commands();
        let names: Vec<&str> = commands
            .iter()
            .filter(|(n, _)| {
                !fuzzy_filter(std::slice::from_ref(&(n.as_str(), "")), q, |c| c.0).is_empty()
            })
            .map(|(n, _)| n.as_str())
            .collect();
        assert!(names.contains(&"model"));
        assert!(!names.contains(&"quit"));
    }

    #[test]
    fn unknown_slash_command_sends_as_user_message() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // 对齐 pi onSubmit：未知命令不报错，作为普通用户输入发送
        let mut st = App::new();
        let quit = st.handle_slash_command("fji");
        assert!(!quit);
        let last = st.messages.last().unwrap();
        assert_eq!(last.role, "user", "未知命令进入用户消息流");
        assert_eq!(last.text(), "/fji");
        assert_eq!(st.status_start.as_deref(), Some("/fji"), "触发 prompt 启动");
        assert!(
            !st.messages
                .iter()
                .any(|m| m.text().contains("unknown command")),
            "不再提示 unknown command"
        );
    }

    #[test]
    fn reload_command_reloads_resources() {
        use std::ffi::OsString;
        // 与读写 auth.json 的测试串行（本测试替换全局 PRUX_AGENT_DIR）
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        struct EnvGuard {
            prev: Option<OsString>,
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match &self.prev {
                    Some(v) => unsafe { std::env::set_var("PRUX_AGENT_DIR", v) },
                    None => unsafe { std::env::remove_var("PRUX_AGENT_DIR") },
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(agent_dir.join("skills").join("demo")).unwrap();
        std::fs::create_dir_all(agent_dir.join("prompts")).unwrap();
        std::fs::write(
            agent_dir.join("skills").join("demo").join("SKILL.md"),
            "---\nname: demo\ndescription: reload demo\n---\n# Demo Body\n",
        )
        .unwrap();
        std::fs::write(
            agent_dir.join("prompts").join("review.md"),
            "---\ndescription: review prompt\n---\nReview this code.\n",
        )
        .unwrap();
        // settings 同步：hideThinkingBlock / autocompleteMaxVisible
        std::fs::write(
            agent_dir.join("settings.json"),
            r#"{"hideThinkingBlock": true, "autocompleteMaxVisible": 9}"#,
        )
        .unwrap();
        // keybindings.json：两个动作绑同一键 → 冲突（证明重新读取磁盘）
        std::fs::write(
            agent_dir.join("keybindings.json"),
            r#"{"tui.editor.cursorUp": "ctrl+p", "tui.editor.cursorDown": "ctrl+p"}"#,
        )
        .unwrap();

        let guard = EnvGuard {
            prev: std::env::var_os("PRUX_AGENT_DIR"),
        };
        unsafe { std::env::set_var("PRUX_AGENT_DIR", agent_dir.as_os_str()) };

        {
            let mut st = App::new();
            st.reload_ctx = crate::modes::interactive::app::ReloadCtx {
                skill_paths: Vec::new(),
                prompt_paths: Vec::new(),
                include_default_skills: true,
                include_default_prompt_templates: true,
                no_context_files: false,
                trusted: true,
                trust_override: Some(true),
                cli_system_prompt: None,
                cli_append_system_prompt: None,
            };
            assert!(st.skills.is_empty() && st.prompt_templates.is_empty());

            let quit = st.handle_slash_command("reload");
            assert!(!quit);

            // skills / prompts 从磁盘重载
            assert!(
                st.skills.iter().any(|s| s.name == "demo"),
                "got: {:?}",
                st.skills.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
            assert!(
                st.prompt_templates.iter().any(|t| t.name == "review"),
                "got: {:?}",
                st.prompt_templates
                    .iter()
                    .map(|t| &t.name)
                    .collect::<Vec<_>>()
            );
            // settings 同步到 UI 内存
            assert!(!st.show_thinking, "hideThinkingBlock=true 应同步");
            assert_eq!(st.autocomplete_max_visible, 9);
            // keybindings 全局表已被磁盘配置替换（出现冲突）
            assert!(
                !crate::core::keybindings::get_global()
                    .conflicts()
                    .is_empty(),
                "keybindings.json 冲突应被检测到"
            );
            // 提示消息
            let last_txt = st
                .system_messages
                .last()
                .map(|(_, s)| crate::modes::interactive::app::sys_spans_text(&s.spans));
            assert!(
                last_txt
                    .as_deref()
                    .map(|t| t.contains("Reloaded"))
                    .unwrap_or(false),
                "got: {:?}",
                last_txt
            );
        }

        // 恢复全局 keybindings（避免污染其他测试的 get_global()）
        crate::core::keybindings::set_global(
            crate::core::keybindings::KeybindingsManager::new_default(),
        );
        drop(guard);
    }

    #[test]
    fn reload_command_passes_context_files_and_skills_to_worker() {
        // 回归：/reload 必须把重载后的 context files + skills 经 Reload 命令传入 worker——
        // compose_tools 消费的是 worker 侧 agent.skills + rebuild_ctx.context_files（启动快照），
        // 只更新 UI 副本会导致系统提示停留在旧快照。
        use std::ffi::OsString;
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        struct EnvGuard {
            prev: Option<OsString>,
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match &self.prev {
                    Some(v) => unsafe { std::env::set_var("PRUX_AGENT_DIR", v) },
                    None => unsafe { std::env::remove_var("PRUX_AGENT_DIR") },
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(agent_dir.join("skills").join("demo")).unwrap();
        std::fs::write(
            agent_dir.join("skills").join("demo").join("SKILL.md"),
            "---\nname: demo\ndescription: reload demo\n---\n# Demo Body\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# Project context\n").unwrap();
        // 全局上下文文件也走 agentDir（trusted 时保留）
        std::fs::write(agent_dir.join("AGENTS.md"), "# Global context\n").unwrap();

        let guard = EnvGuard {
            prev: std::env::var_os("PRUX_AGENT_DIR"),
        };
        unsafe { std::env::set_var("PRUX_AGENT_DIR", agent_dir.as_os_str()) };

        let (agent, mut cmd_rx) = crate::modes::interactive::agent_actor::channels();
        let mut st = App::new();
        st.cwd = dir.path().to_string_lossy().to_string();
        st.reload_ctx = crate::modes::interactive::app::ReloadCtx {
            skill_paths: Vec::new(),
            prompt_paths: Vec::new(),
            include_default_skills: true,
            include_default_prompt_templates: true,
            no_context_files: false,
            trusted: true,
            trust_override: Some(true),
            cli_system_prompt: None,
            cli_append_system_prompt: None,
        };
        st.worker = crate::modes::interactive::agent_actor::WorkerHandle::new(agent);

        let quit = st.handle_slash_command("reload");
        assert!(!quit);

        let cmd = cmd_rx.try_recv().expect("应发出 Reload 命令");
        match cmd {
            crate::modes::interactive::agent_actor::AgentCommand::Reload {
                context_files,
                skills,
                ..
            } => {
                assert!(
                    context_files.iter().any(|(p, _)| p.ends_with("AGENTS.md")),
                    "worker 侧应收到上下文文件: {context_files:?}"
                );
                assert!(
                    skills.iter().any(|s| s.name == "demo"),
                    "worker 侧应收到重载 skills: {:?}",
                    skills.iter().map(|s| &s.name).collect::<Vec<_>>()
                );
            }
            other => panic!("应发 Reload: {other:?}"),
        }
        drop(guard);
    }

    #[test]
    fn slash_list_excludes_short_and_exit() {
        let names: Vec<&str> = BUILTIN_COMMANDS.iter().map(|(n, _, _)| *n).collect();
        assert!(!names.contains(&"q"));
        assert!(!names.contains(&"h"));
        assert!(!names.contains(&"exit"));
        assert!(names.contains(&"quit"));
    }

    #[test]
    fn slash_list_covers_handled_builtins() {
        // 每个有处理分支的内置命令都必须出现在 / 补全候选中
        // （否则输入 / 看不到；debug 保持隐藏与 pi 原版一致）
        let names: Vec<&str> = BUILTIN_COMMANDS.iter().map(|(n, _, _)| *n).collect();
        for cmd in [
            "dock",
            "scoped-models",
            "label",
            "theme",
            "extension",
            "import",
            "fork",
            "clone",
        ] {
            assert!(names.contains(&cmd), "{cmd} 应有补全候选");
        }
        assert!(names.contains(&"changelog"), "changelog 应有补全候选");
    }

    #[test]
    fn changelog_command_shows_embedded_current_version() {
        // /changelog：内嵌当前版本 changelog 按系统消息展示，不进 LLM 上下文
        let _ad = crate::test_support::AgentDirGuard::temp();
        let mut st = App::new();
        let quit = st.handle_slash_command("changelog");
        assert!(!quit);
        let text = st
            .system_messages
            .last()
            .map(|(_, m)| crate::modes::interactive::app::sys_spans_text(&m.spans))
            .unwrap_or_default();
        assert!(text.contains("What's New"), "got: {text}");
        assert!(
            text.contains(crate::core::changelog::VERSION),
            "got: {text}"
        );
        assert!(
            text.contains(crate::core::changelog::CHANGELOG.trim()),
            "got: {text}"
        );
        assert!(st.messages.is_empty(), "changelog 不应进入 LLM 上下文");
    }

    // ------------------------------------------------------------------
    // 扩展命令动态增删：禁用/模式过滤后命令从候选与分发中移除
    // ------------------------------------------------------------------

    #[test]
    fn plan_todos_commands_follow_extension_lifecycle() {
        // 禁用 plan-mode：/plan /todos 从候选消失、分发按未知命令（user 消息）；
        // 重新启用后候选恢复、命令可执行
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // plan mode 内存态是进程级单例：与 plan_mode.rs 测试串行（不共享文件，只串行内存态）
        let _plan = crate::extensions::plan_mode::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _agent = plan_agent();
        ensure_plan_registered();
        cleanup_plan_state();

        // 禁用扩展
        assert!(crate::core::extensions::set_extension_enabled(
            "plan-mode",
            false
        ));
        crate::core::settings_manager::write_disabled_extensions(&["plan-mode".to_string()]).ok();

        let names: Vec<String> = slash_commands().into_iter().map(|(n, _)| n).collect();
        assert!(!names.contains(&"plan".to_string()), "got: {names:?}");
        assert!(!names.contains(&"todos".to_string()), "got: {names:?}");

        let mut st = App::new();
        let quit = st.handle_slash_command("plan");
        assert!(!quit);
        assert!(!crate::extensions::plan_mode::is_enabled());
        let last = st.messages.last().unwrap().text().to_string();
        assert_eq!(last, "/plan", "禁用后 /plan 按未知命令发送");

        let quit = st.handle_slash_command("todos");
        assert!(!quit);
        let last = st.messages.last().unwrap().text().to_string();
        assert_eq!(last, "/todos", "禁用后 /todos 按未知命令发送");

        // 重新启用：候选恢复 /plan；无 todo 任务时 /todos 不作为命令暴露
        // （与 filter_tools 同款按状态动态决定），避免空计划时误触发
        assert!(crate::core::extensions::set_extension_enabled(
            "plan-mode",
            true
        ));
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
        let names: Vec<String> = slash_commands().into_iter().map(|(n, _)| n).collect();
        assert!(names.contains(&"plan".to_string()), "got: {names:?}");
        assert!(
            !names.contains(&"todos".to_string()),
            "无 todos 时不应暴露 /todos: {names:?}"
        );

        let mut st = App::new();
        let quit = st.handle_slash_command("plan");
        assert!(!quit);
        assert!(
            crate::extensions::plan_mode::is_enabled(),
            "启用后 /plan 生效"
        );
        let _ = st.handle_slash_command("plan");
        assert!(
            !crate::extensions::plan_mode::is_enabled(),
            "再次 /plan 关闭"
        );

        let quit = st.handle_slash_command("todos");
        assert!(!quit);
        let last = st.messages.last().unwrap().text().to_string();
        assert_eq!(last, "/todos", "无 todos 时 /todos 按未知命令发送");
        cleanup_plan_state();
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
    }

    #[test]
    fn extension_commands_removed_in_minimal_mode() {
        // 极简模式下非 Minimal 扩展不可用，其命令同时从候选移除（与工具语义一致）
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        ensure_plan_registered();
        crate::core::extensions::set_extension_enabled("plan-mode", true);
        crate::core::settings_manager::write_extension_mode("all").ok();
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();

        crate::core::extensions::set_extension_mode(
            crate::core::extensions::ExtensionMode::Minimal,
        );
        let names: Vec<String> = slash_commands().into_iter().map(|(n, _)| n).collect();
        assert!(!names.contains(&"plan".to_string()), "got: {names:?}");
        assert!(!names.contains(&"todos".to_string()), "got: {names:?}");

        crate::core::extensions::set_extension_mode(crate::core::extensions::ExtensionMode::All);
        let names: Vec<String> = slash_commands().into_iter().map(|(n, _)| n).collect();
        assert!(names.contains(&"plan".to_string()), "got: {names:?}");

        crate::core::settings_manager::write_extension_mode("all").ok();
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
    }

    #[test]
    fn builtin_commands_override_extension_commands() {
        // 扩展声明与内置重名的命令：候选去重（仅内置一条）、分发走内置
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        struct ModelOverrideExt;
        impl crate::core::extensions::Extension for ModelOverrideExt {
            fn name(&self) -> &str {
                "model-override-ext"
            }
            fn tools(&self) -> Vec<crate::core::extensions::ExtensionTool> {
                Vec::new()
            }
            fn commands(&self) -> Vec<crate::core::extensions::ExtensionCommand> {
                vec![crate::core::extensions::ExtensionCommand {
                    name: "model".to_string(),
                    description: "shadow builtin model".to_string(),
                    busy_safe: false,
                    subcommands: Vec::new(),
                }]
            }
        }
        crate::core::extensions::register_extension(ModelOverrideExt);
        crate::core::extensions::set_extension_enabled("model-override-ext", true);
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();

        let commands = slash_commands();
        let model_count = commands.iter().filter(|(n, _)| n == "model").count();
        assert_eq!(model_count, 1, "重名扩展命令应被去重：{commands:?}");

        // 分发走内置：/model 打开模型面板
        let mut st = App::new();
        let quit = st.handle_slash_command("model");
        assert!(!quit);
        assert!(st.panel.active, "内置 /model 打开面板");
        st.panel.close();

        crate::core::extensions::set_extension_enabled("model-override-ext", false);
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
    }

    #[test]
    fn register_slash_command_adds_handler_dynamically() {
        // 注册 API：扩展声明命令 + TUI 注册执行入口 → 可执行；扩展禁用后取消
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        struct EchoCmdExt;
        impl crate::core::extensions::Extension for EchoCmdExt {
            fn name(&self) -> &str {
                "echo-cmd-ext"
            }
            fn tools(&self) -> Vec<crate::core::extensions::ExtensionTool> {
                Vec::new()
            }
            fn commands(&self) -> Vec<crate::core::extensions::ExtensionCommand> {
                vec![crate::core::extensions::ExtensionCommand {
                    name: "echo-cmd".to_string(),
                    description: "echo".to_string(),
                    busy_safe: false,
                    subcommands: Vec::new(),
                }]
            }
        }
        crate::core::extensions::register_extension(EchoCmdExt);
        crate::core::extensions::set_extension_enabled("echo-cmd-ext", true);
        register_slash_command("echo-cmd-ext", "echo-cmd", |st, _| {
            st.push_msg("echo-cmd executed".to_string(), MsgLevel::Info);
            false
        });
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();

        let mut st = App::new();
        let quit = st.handle_slash_command("echo-cmd");
        assert!(!quit);
        let last_txt = st
            .system_messages
            .last()
            .map(|(_, s)| crate::modes::interactive::app::sys_spans_text(&s.spans));
        assert!(
            last_txt
                .as_deref()
                .unwrap_or("")
                .contains("echo-cmd executed"),
            "got: {last_txt:?}"
        );

        // 禁用扩展：命令从候选移除、分发取消（unknown）
        crate::core::extensions::set_extension_enabled("echo-cmd-ext", false);
        let names: Vec<String> = slash_commands().into_iter().map(|(n, _)| n).collect();
        assert!(!names.contains(&"echo-cmd".to_string()), "got: {names:?}");
        let mut st = App::new();
        let quit = st.handle_slash_command("echo-cmd");
        assert!(!quit);
        let last = st.messages.last().unwrap().text().to_string();
        assert_eq!(last, "/echo-cmd", "禁用后按未知命令");
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
    }

    #[test]
    fn extension_detail_lines_display_commands() {
        // /extension 详情面板展示扩展声明的命令；无 todos 时仅声明 /plan，
        // 注入计划步骤后 /todos 随状态动态出现（与 filter_tools 同款按状态显隐）
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // plan mode 内存态是进程级单例：与 plan_mode.rs 测试串行（不共享文件，只串行内存态）
        let _plan = crate::extensions::plan_mode::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _agent = plan_agent();
        ensure_plan_registered();
        cleanup_plan_state();
        let lines = crate::core::extensions::extension_detail_lines("plan-mode");
        assert!(
            lines.iter().any(|l| l.contains("commands: plan")),
            "got: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("todos")),
            "无 todos 不应列出 /todos: {lines:?}"
        );

        // 有 todos：/todos 出现在声明的命令列表
        crate::extensions::plan_mode::set_enabled(true);
        {
            use crate::core::extensions::Extension;
            let ext = crate::extensions::plan_mode::PlanMode::new();
            ext.on_agent_event(&serde_json::json!({
                "type": "agent_end",
                "messages": [
                    { "role": "assistant", "content": [{ "type": "text", "text": "Plan:\n1. Inspect code\n2. Verify result" }] }
                ]
            }));
        }
        let _ = crate::core::extensions::take_pending_ui();
        let lines = crate::core::extensions::extension_detail_lines("plan-mode");
        assert!(
            lines.iter().any(|l| l.contains("commands: plan, todos")),
            "got: {lines:?}"
        );
        crate::extensions::plan_mode::reset_state_for_tests();
        let dir = crate::core::settings_manager::agent_dir();
        let _ = std::fs::remove_file(dir.join("plan-mode.json"));
    }

    #[test]
    fn settings_command_opens_selector() {
        // 对齐 pi handleSettingsCommand：/settings 打开设置选择器（其他模态关闭）
        let mut st = App::new();
        st.panel.active = true;
        let quit = st.handle_slash_command("settings");
        assert!(!quit);
        assert!(st.settings_selector.active);
        assert!(!st.panel.active, "打开 settings 应关闭 panel");
        assert!(!st.settings_selector.items.is_empty());
        assert!(st.settings_selector.filtered.len() >= 8);
        st.settings_selector.close();
    }

    #[test]
    fn skills_extension_declares_and_registers_slash_command() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::core::extensions::register_extension(crate::extensions::skills::Skills::new());
        // skills 扩展默认关闭（`default_enabled` = false）→ 注册后需显式启用才会声明 /skills
        crate::core::extensions::set_extension_enabled(crate::extensions::skills::EXT, true);
        assert!(
            slash_commands().iter().any(|(n, _)| n == "skills"),
            "扩展应声明 /skills 候选: {:?}",
            slash_commands()
        );
        assert!(
            extension_command_handler("skills").is_some(),
            "on_registered 应装好 /skills 的 TUI handler"
        );
    }

    #[test]
    fn slash_skill_command_expands_in_tui() {
        use crate::core::skills::load_skills;
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills").join("demo");
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::fs::write(
            skills_dir.join("SKILL.md"),
            "---\nname: demo\ndescription: d\n---\n# Body\n",
        )
        .unwrap();
        let skills = load_skills(
            dir.path().to_str().unwrap(),
            dir.path(),
            dir.path(),
            &[],
            &[],
            true,
            false,
            &[],
        );
        assert!(!skills.is_empty(), "skills should be discovered");

        let mut st = App::new();
        // actor：skills 走 App 镜像（启动时从 agent 复制）
        st.skills = skills;
        let quit = st.handle_slash_command("skill:demo do it");
        assert!(!quit);
        let last = st.messages.last().unwrap().text();
        assert!(last.contains("<skill name=\"demo\""), "got: {}", last);
        assert!(last.contains("# Body"), "got: {}", last);
        assert!(last.ends_with("do it"), "got: {}", last);
    }

    #[test]
    fn skill_commands_appear_in_slash_suggestions() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        use crate::core::skills::Skill;
        let mut st = App::new();
        st.skills = vec![
            Skill {
                name: "code-review".to_string(),
                description: "review code".to_string(),
                path: "/tmp/skills/code-review/SKILL.md".into(),
                base_dir: "/tmp/skills/code-review".into(),
                disable_model_invocation: false,
                source: "/tmp/skills".to_string(),
            },
            Skill {
                name: "demo".to_string(),
                description: "test skill".to_string(),
                path: "/tmp/skills/demo/SKILL.md".into(),
                base_dir: "/tmp/skills/demo".into(),
                disable_model_invocation: true,
                source: "/tmp/skills".to_string(),
            },
        ];
        // 用显式 `skill:` 前缀列出全部技能（对齐 pi #9120：无冒号查询按裸名匹配，
        // `/skill` 不再无条件列出技能）
        st.editor.insert_text("/skill:");
        st.refresh_suggestions();
        assert!(st.suggestion.active);
        assert_eq!(st.suggestion.kind, SuggestionKind::Commands);
        let names: Vec<&str> = st
            .suggestion
            .items
            .iter()
            .map(|i| i.name.as_str())
            .collect();
        // disable-model-invocation 的技能仍可显式调用（对齐 pi：仅系统提示隐藏）
        assert!(names.contains(&"skill:code-review"), "got: {:?}", names);
        assert!(names.contains(&"skill:demo"), "got: {:?}", names);
        assert!(
            st.suggestion
                .items
                .iter()
                .any(|i| i.name == "skill:code-review" && i.description == "review code")
        );

        // 按裸名查询：`/review` 命中 skill:code-review
        st.editor.clear();
        st.editor.insert_text("/review");
        st.refresh_suggestions();
        assert!(
            st.suggestion
                .items
                .iter()
                .any(|i| i.name == "skill:code-review")
        );
    }

    #[test]
    fn session_switch_clears_old_system_prompts() {
        // 回归：/session <n> 切换后旧会话的 TUI 提示（系统消息）必须清除，
        // 且 current_session_path 更新为新会话文件（对齐 resume_session_from_selector）
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (agent, mut cmd_rx) = test_agent_cmd();
        let agent_dir = crate::core::settings_manager::agent_dir();
        let dir = crate::core::session_manager::default_session_dir("/tmp", &agent_dir);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let mut s1 =
            crate::core::session_manager::Session::create("/tmp", Some(dir.clone()), true).unwrap();
        s1.append_message(&crate::core::provider::AgentMessage {
            role: "assistant".into(),
            thinking_level: None,
            content: vec![],
            ..crate::core::provider::AgentMessage::user_text("")
        });
        // 间隔确保 mtime 可分辨（文件系统时钟可能只有 ms 精度）
        std::thread::sleep(std::time::Duration::from_millis(10));
        let mut s2 =
            crate::core::session_manager::Session::create("/tmp", Some(dir.clone()), true).unwrap();
        s2.append_message(&crate::core::provider::AgentMessage {
            role: "assistant".into(),
            thinking_level: None,
            content: vec![],
            ..crate::core::provider::AgentMessage::user_text("")
        });
        let s2_file = s2
            .get_session_file()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap();

        let mut st = App::new();
        st.worker = agent;
        st.cwd = "/tmp".to_string(); // 会话目录定位用 App 缓存（actor）
        // 已存在一个旧会话的系统提示（模拟切换前的提示残留）
        st.system_messages.push((
            1,
            crate::modes::interactive::app::SysMsg::new(
                MsgLevel::Info,
                vec![crate::modes::interactive::app::SysSpan::plain(
                    "stale prompt",
                )],
            ),
        ));
        let s1_file = s1
            .get_session_file()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap();

        // list_sessions 按修改时间倒序（最新在前）：session 1 == s2，session 2 == s1
        // actor：/session <n> 下发 ResumeSession 命令，写回/清提示由回执处理（场景测试覆盖）
        st.handle_slash_command("session 1");
        let cmd1 = cmd_rx.try_recv().expect("第一次应发 ResumeSession");
        assert!(
            matches!(cmd1, crate::modes::interactive::agent_actor::AgentCommand::ResumeSession { ref path, .. } if *path == s2_file),
            "应切到最新会话: {cmd1:?}"
        );
        st.handle_slash_command("session 2");
        let cmd2 = cmd_rx.try_recv().expect("第二次应发 ResumeSession");
        assert!(
            matches!(cmd2, crate::modes::interactive::agent_actor::AgentCommand::ResumeSession { ref path, .. } if *path == s1_file),
            "应切到较旧会话: {cmd2:?}"
        );
    }

    #[test]
    fn cmd_arg_preserves_argument_case() {
        // 回归：/import 等带参数命令的整行被 to_lowercase 后路径变全小写（T/Z→t/z），
        // 导致文件查找失败。关键字匹配大小写不敏感，但参数必须保留原大小写。
        assert_eq!(
            cmd_arg(
                "import /home/blue/2026-09-04T13-38-45-057Z_a.jsonl",
                "import"
            ),
            "/home/blue/2026-09-04T13-38-45-057Z_a.jsonl"
        );
        assert_eq!(
            cmd_arg("Import /Home/Case.jsonl", "import"),
            "/Home/Case.jsonl"
        );
        assert_eq!(cmd_arg("name mySession", "name"), "mySession");
        assert_eq!(cmd_arg("thinking high", "thinking"), "high");
        assert_eq!(cmd_arg("model  gpt-4.1", "model"), "gpt-4.1");
        assert_eq!(cmd_arg("import", "import"), "", "无参数应为空");
    }

    #[test]
    fn export_path_arg_parses_prefix_and_quotes() {
        // 对齐 pi getPathCommandArgument：/export ~/out.html 只取路径参数，
        // 支持引号包裹
        assert_eq!(
            parse_path_arg("export ~/out.html"),
            Some("~/out.html".to_string())
        );
        assert_eq!(
            parse_path_arg("export \"/tmp/a b.html\""),
            Some("/tmp/a b.html".to_string())
        );
        assert_eq!(parse_path_arg("export"), None, "无空格无参数");
        assert_eq!(parse_path_arg("export "), None, "仅前缀无参数");
        assert_eq!(parse_path_arg("export foo bar"), Some("foo".to_string()));
    }

    #[test]
    fn compact_defers_without_text_area_prompt() {
        // /compact 只置 pending_compact（压缩由轮结束后执行），不向文本区推送
        // “Compacting...” 提示——压缩中的忙碌态由状态栏呈现，文本区只留完成结果。
        let mut st = App::new();
        st.handle_slash_command("compact");
        assert!(st.pending_compact, "应待执行压缩");
        assert!(st.compact_instructions.is_none(), "无参数不留自定义指令");
        assert!(st.system_messages.is_empty(), "压缩中不应在文本区推送提示");

        // 带自定义指令透传
        let mut st = App::new();
        st.handle_slash_command("compact focus on API changes");
        assert!(st.pending_compact);
        assert_eq!(
            st.compact_instructions.as_deref(),
            Some("focus on API changes")
        );
        assert!(st.system_messages.is_empty());
    }
}
