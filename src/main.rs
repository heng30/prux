//! prux 入口：clap 解析 → 包子命令 / 快速路径 / 会话准备 → 模式分发。
//!
//! 各阶段逻辑按职责拆分到 cli/（参数、文件、会话选择）与 modes/（运行模式），本文件只做组装。

use prux::{
    APP_NAME,
    cli::{
        args::{Args, Command, ParsedArgs, VALID_THINKING_LEVELS},
        auth_command::run_auth_command,
        file_processor::{build_initial_messages, process_file_arguments},
        initial_message::compose_initial_parts,
        list_models::run_list_models,
        mcp_command::run_mcp_command,
        project_trust::resolve_trusted,
        session_picker::{find_session_by_id, pick_session, resolve_session_arg},
    },
    core::{
        self,
        agent_session::{Agent, ToolSelection},
        context::{
            discover_append_system_prompt, discover_system_prompt, load_project_context_files,
        },
        crash_log::install_panic_hook,
        doc_sync::ensure_docs,
        export_html::export_session,
        model_resolver::{find_model, provider_explicitly_configured, resolve_provider_model},
        model_scope::{self, ScopedModel},
        prompt_templates::load_prompt_templates,
        session_manager::{
            ContinueRecentResult, OpenSessionResult, Session, SessionRepairPlan,
            default_session_dir,
        },
        settings_manager::{
            DEFAULT_TOOL_NAMES, Settings, agent_dir, read_settings, read_settings_default_tools,
        },
        skills::load_skills,
        tools_manager,
    },
    extensions::{self, skills::bundled_skill_names},
    modes::interactive::{
        self,
        app::{RepairAction, SessionRepairRequest, ZombieOperationsRequest},
    },
    utils::paths::{ensure_home_env, expand_tilde_path, resolve_path, theme_name_from_arg},
};
use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
};

/// 进程入口：注册扩展、解析（含扩展 flag 的）CLI，处理 auth/mcp/export 等子命令与
/// 模型选择，再按 TUI / 非交互模式分派运行。
#[tokio::main]
async fn main() {
    // Windows 上把 `HOME` 统一成 `dirs::home_dir()`：PowerShell 不导出它、git bash 导出的是
    // POSIX 形式，不统一就会「换个 shell 换一份配置」。必须在 spawn 线程之前（见 [`ensure_home_env`]）。
    ensure_home_env();

    // 进程标记（子进程继承）
    unsafe {
        std::env::set_var("AI_AGENT", "prux");
        std::env::set_var("PRUX_CODING_AGENT", "true");
    }

    // 显式忽略 SIGPIPE：与 Rust 运行时默认一致，这里重申以免被其它库改动。
    //
    // 不能改成 `SigDfl`（默认动作 = 终止进程）：会向子进程 stdin 写数据
    // （MCP / subagent / wl-copy 等），管道对端一旦先退出，写入进程就被 SIGPIPE
    // 直接杀死——不产生 core、不写崩溃日志、会话也不留 interrupted 记录，
    // 表现为「跑到一半无端退出」。写入失败应作为 `io::Error`（EPIPE）在调用侧处理。
    #[cfg(unix)]
    unsafe {
        _ = nix::sys::signal::signal(
            nix::sys::signal::Signal::SIGPIPE,
            nix::sys::signal::SigHandler::SigIgn,
        );
    }

    extensions::register_all(); // 注册扩展（提前：动态 CLI flag 注入需要已启用扩展声明）

    // 保证 `agent_dir/bin` 在 PATH 末尾，让子进程（bash 工具 / MCP / 扩展起的外部命令）
    // 也能按命令名找到安装的 fd、rg；启动时先做一次，之后下载工具无需再改 PATH。
    tools_manager::ensure_bin_in_path();

    // 未捕获 panic → 写入 agent_dir()/crashes.json（下次启动提示 /bug，/bug 自动附带）
    install_panic_hook();

    let ParsedArgs {
        args: parsed,
        extension_flags,
        extension_flag_values,
    } = match Args::parse_with_extension_flags() {
        Ok(a) => a,
        Err(e) => {
            // 非 -h/-v 错误时 e.print() 已输出到 stderr，退出码 1
            let code = if e.use_stderr() { 1 } else { 0 };
            _ = e.print();
            std::process::exit(code);
        }
    };

    // `--no-extensions`：本进程所有扩展（含内置）一律禁用。必须在解析后调用
    // （注册早于解析，因为动态 CLI flag 注入需要已注册扩展的声明），
    // 且要早于下面的扩展 flag 分发（被禁用的扩展不应再收到 flag）。
    if parsed.no_extensions {
        core::extensions::set_all_extensions_disabled(true);
    }

    // `--no-mcp`：关闭内置 MCP 支持（不连接服务器、不注入 `mcp` 工具）。
    // 与 `--no-extensions` 同理，注册早于解析，所以这里直接禁用已注册的 mcp 扩展；
    // 只改本次运行的内存状态，不写回 settings 的 `disabledExtensions`。
    if parsed.no_mcp {
        core::extensions::set_extension_enabled(extensions::mcp::EXT, false);
    }

    // `--tools` 的条目校验：纯名字/通配与 `+name`/`-name` 不可混用，后者只接受精确名。
    if let Some(err) = core::settings_manager::get_tool_list_error(&parsed.tools) {
        eprintln!("Invalid tools option: {err}");
        std::process::exit(1);
    }

    // 把解析出的扩展 flag 值分发给声明它的扩展（--plan → plan-mode 开启）。
    // 未启用扩展的 flag 不注入解析，此处分发天然只覆盖已启用扩展。
    for (name, value) in &extension_flags {
        for ext in core::extensions::registered() {
            ext.apply_cli_flag(name, *value);
        }
    }

    for (name, value) in &extension_flag_values {
        for ext in core::extensions::registered() {
            ext.apply_cli_flag_value(name, value);
        }
    }

    // 初始化全局键位绑定（读取 agent_dir/keybindings.json）。
    // 重复解析命令也先加载一次，打印/交互模式共用同一键位表。
    let kb = core::keybindings::init_global(&agent_dir());
    if !kb.conflicts().is_empty() {
        eprintln!("keybindings.json conflicts:");
        for c in kb.conflicts() {
            eprintln!(
                "  {} is bound to multiple actions: {}",
                c.key,
                c.keybindings.join(", ")
            );
        }
    }

    // auth 子命令：打印凭据 / 检查 provider 认证就绪状态
    if let Some(Command::Auth { args }) = &parsed.command {
        std::process::exit(run_auth_command(args));
    }

    // mcp 子命令：MCP 服务器配置管理 / OAuth 登录 / 连通性诊断
    if let Some(Command::Mcp { args }) = &parsed.command {
        std::process::exit(run_mcp_command(args).await);
    }

    // --export <session> [out]：导出 HTML
    if let Some(session_path) = parsed.export_in() {
        let settings = read_settings();
        let theme = parsed
            .use_theme
            .clone()
            .or(settings.theme)
            .or_else(|| parsed.theme.first().cloned());
        let theme = theme.as_deref().map(theme_name_from_arg);
        match Session::open(session_path) {
            Ok(sess) => match export_session(&sess, parsed.export_out(), theme.as_deref()) {
                Ok(p) => {
                    println!("Exported session to {}", p);
                    return;
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            },
            Err(e) => {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
    }

    // 解析`settings.json`文件
    let settings = read_settings();

    // scope 名单（--models CLI 优先，否则 settings.enabledModels）+ 初始模型选择
    let scope_models = resolve_startup_scope(&parsed, &settings);
    let (provider, model_id, thinking_from_model, model_unconfigured) =
        select_startup_model(&parsed, &settings, &scope_models);

    let mut thinking = parsed
        .thinking
        .clone()
        .or(thinking_from_model)
        .or(settings.default_thinking_level.clone());

    // --thinking 非法值仅产生 warning 诊断并继续运行（非硬错误退出）
    if let Some(t) = &thinking
        && !VALID_THINKING_LEVELS.contains(&t.as_str())
    {
        eprintln!(
            "Warning: Invalid thinking level \"{}\". Valid values are: {}. Ignoring.",
            t,
            VALID_THINKING_LEVELS.join(", ")
        );
        thinking = None;
    }

    if let Some(search) = &parsed.list_models {
        // 无显式 provider 时 resolve 出的 deepseek 是兜底，不算可用模型，不列出目录
        if !provider_explicitly_configured(
            parsed.provider.as_deref(),
            parsed.model.as_deref(),
            &settings,
        ) {
            eprintln!(
                "No provider configured; specify --provider <name> (or PRUX_PROVIDER / settings default_provider) to list models, e.g. `prux --provider deepseek --list-models`"
            );
            return;
        }
        run_list_models(&provider, search);
        return;
    }

    // 模型兜底链：配置的模型，不存在或 API 不支持时不退出，警告后回退默认模型（deepseek/deepseek-flash）
    let mut provider = provider;
    let mut model_id = model_id;
    let model_entry = match find_model(&provider, &model_id) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "Warning: model {}/{} unavailable ({}); using default model",
                provider, model_id, e
            );
            let (fp, fm) = fallback_model();
            provider = fp;
            model_id = fm;
            match find_model(&provider, &model_id) {
                Ok(m) => m,
                Err(e2) => {
                    eprintln!("Error: {}", e2);
                    std::process::exit(1);
                }
            }
        }
    };

    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let cwd_path = PathBuf::from(&cwd);
    let agent_dir = agent_dir();

    // 必须早于任何项目资源读取——项目 `<PROJECT_SCOPE_NAME>/settings.json` 的 `defaultTools` 只在受信任时生效，
    // 且 `-a/--approve`、`--no-approve` 要能被它看见（resolve_trusted 同时把结论登记为本次运行的会话信任，
    // 供 worker 线程上的子代理资源读取）。
    let trusted = resolve_trusted(&cwd_path, &agent_dir, parsed.approve_option());

    // 工具选择（默认 = settings defaultTools ?? 标准默认 read/bash/edit/write；
    // --no-tools 全关（含扩展）；--no-builtin-tools 仅关内置保留扩展；
    // --tools 为全工具 allowlist（条目支持 `*` 通配；未点名的 MCP 工具仍注册但不对模型声明），
    // 全部为 `+name`/`-name` 时改为在默认选择上增减；--exclude-tools 过滤结果）。
    // `settings_default_tools` 记下启动基线（已套用 `--tools` 修饰符），
    // 供 `/reload` 重读 settings 时只增不减地并入新工具；
    // 命令行显式指定过 allowlist / --no-tools / --no-builtin-tools 时为 None（`/reload` 不重读）。
    let mut default_tool_modifiers: Vec<String> = Vec::new();
    let (selection, settings_default_tools) = if parsed.no_tools {
        (ToolSelection::NoTools, None)
    } else if parsed.no_builtin_tools {
        (ToolSelection::NoBuiltinTools, None)
    } else if !parsed.tools.is_empty() {
        if parsed
            .tools
            .iter()
            .all(|t| core::settings_manager::is_tool_modifier(t))
        {
            // 纯 `+name`/`-name`：等价于 settings 的 defaultTools 语法，在默认选择上增减。
            // 基线与 `settings_default_tools` 都记「已套修饰符的结果」，使 `/reload` 能按同一组修饰符重算。
            default_tool_modifiers = parsed.tools.clone();
            let base = read_settings_default_tools()
                .unwrap_or_else(|| DEFAULT_TOOL_NAMES.iter().map(|s| s.to_string()).collect());
            let names =
                core::settings_manager::apply_tool_modifiers(&base, &default_tool_modifiers);
            (ToolSelection::Default(names.clone()), Some(names))
        } else {
            (ToolSelection::Allowlist(parsed.tools.clone()), None)
        }
    } else {
        let names = read_settings_default_tools()
            .unwrap_or_else(|| DEFAULT_TOOL_NAMES.iter().map(|s| s.to_string()).collect());
        (ToolSelection::Default(names.clone()), Some(names))
    };

    // 离线是进程级判定：`/download` 扩展（手动安装 fd/rg）要读到同一份，
    // 而 `--offline` 不写 env，故存进 runtime。
    let offline = core::runtime::init_offline(parsed.offline);

    // 启动时同步内嵌文档到 agent_dir/docs，保证系统提示词指向的官方文档目录最新（首个 agent turn 前就绪）。
    ensure_docs();

    // 三级回退：--session-dir > PRUX_CODING_AGENT_SESSION_DIR env > 默认派生
    let default_dir = std::env::var("PRUX_CODING_AGENT_SESSION_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(expand_tilde_path)
        .unwrap_or_else(|| default_session_dir(&cwd, &agent_dir));

    // 会话选择
    let mut session = select_session(&parsed, &cwd, &default_dir);

    // --name：设置会话显示名
    if let Some(name) = &parsed.name {
        let name = name.trim();
        if name.is_empty() {
            eprintln!("Error: --name requires a non-empty value");
            std::process::exit(1);
        }
        if let Some(s) = &mut session {
            s.append_session_info(name);
        }
    }

    let context_files = if parsed.no_context_files {
        Vec::new()
    } else {
        let mut files = load_project_context_files(&cwd, &agent_dir);
        if !trusted {
            // 只保留全局上下文文件（agentDir 下），项目级文件需要信任后才加载
            files.retain(|(p, _)| Path::new(p).starts_with(&agent_dir));
        }
        files
    };

    // 技能：--no-skills 关闭默认发现与 settings 过滤；显式 --skill 仍生效
    for raw in &parsed.skill {
        if !resolve_path(raw, &cwd).exists() {
            eprintln!("Warning: skill path does not exist: {}", raw);
        }
    }
    let home_dir = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"));
    let skills = load_skills(
        &cwd,
        &agent_dir,
        &home_dir,
        &parsed.skill,
        &bundled_skill_names(),
        !parsed.no_skills,
        trusted,
        &settings.skills,
    );

    // Prompt templates / themes
    for raw in &parsed.prompt_template {
        if !resolve_path(raw, &cwd).exists() {
            eprintln!("Warning: prompt template path does not exist: {}", raw);
        }
    }
    let prompt_templates = load_prompt_templates(
        &cwd,
        &agent_dir,
        &parsed.prompt_template,
        !parsed.no_prompt_templates,
    );
    let theme_override = parsed
        .use_theme
        .clone()
        .or_else(|| settings.theme.clone())
        .or_else(|| parsed.theme.first().cloned());

    // --api-key 校验：必须通过 --model/--provider+--model/--models 指定模型，
    // 否则报 error diagnostics 且不设置 runtime key（避免 key 被静默忽略）
    let api_key_override = if parsed.api_key.is_some()
        && parsed.model.is_none()
        && parsed.provider.is_none()
        && parsed.models.is_empty()
    {
        eprintln!(
            "Error: --api-key requires a model to be specified via --model, --provider+--model, or --models."
        );
        None
    } else {
        parsed.api_key.clone()
    };

    // 追加系统提示：CLI --append-system-prompt 显式优先；
    // 否则自动发现 `PROJECT_SCOPE_NAME/APPEND_SYSTEM.md`（需项目信任）或全局 agentDir/APPEND_SYSTEM.md
    let append_system_prompt = if parsed.append_system_prompt.is_empty() {
        discover_append_system_prompt(&cwd, &agent_dir, trusted)
    } else {
        Some(parsed.append_system_prompt.join("\n\n"))
    };

    // SYSTEM.md 替换系统提示：命中时优先于 --system-prompt
    let system_override = discover_system_prompt(&cwd, &agent_dir, trusted)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| parsed.system_prompt.clone().unwrap_or_default());
    let system_prompt_cli = if system_override.is_empty() {
        None
    } else {
        Some(system_override)
    };

    let mut agent = {
        // CLI 只跑交互式 TUI：提前标记 UI 已接入，使启动阶段的自动写盘
        // （thinking 级别 / 默认模型回写）在 settings.json 损坏时也能弹覆写确认窗。
        core::settings_manager::set_interactive_settings_ui(true);

        let build = |entry: core::model_resolver::ModelEntry| {
            Agent::new(
                entry,
                cwd.clone(),
                selection.clone(),
                parsed.exclude_tools.clone(),
                thinking.clone(),
                system_prompt_cli.clone(),
                append_system_prompt.clone(),
                context_files.clone(),
                session.clone(),
                api_key_override.clone(),
                offline,
                skills.clone(),
            )
        };

        match build(model_entry) {
            Ok(a) => a,
            Err(e) => {
                // restoreModelFromSession：模型 API 不支持等错误时，警告并回退默认模型重试，而不是退出
                eprintln!("Warning: {}; using default model", e);
                let (fp, fm) = fallback_model();
                let entry = match find_model(&fp, &fm) {
                    Ok(m) => m,
                    Err(e2) => {
                        eprintln!("Error: {}", e2);
                        std::process::exit(1);
                    }
                };
                match build(entry) {
                    Ok(a) => a,
                    Err(e2) => {
                        eprintln!("Error: {}", e2);
                        std::process::exit(1);
                    }
                }
            }
        }
    };

    agent.model_cycle = scope_models;
    agent.model_is_fallback = model_unconfigured;
    agent.set_settings_default_tools(settings_default_tools);
    agent.set_default_tool_modifiers(default_tool_modifiers);

    // @文件 → text/image；stdin + fileText + 第一条 CLI 消息合成 initialMessage
    let (file_text, file_images) =
        match process_file_arguments(&parsed.file_args(), &cwd, agent.model.image_resize) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        };

    let mut cli_messages = parsed.messages();
    let initial_text = compose_initial_parts(None, &file_text, &mut cli_messages);
    let initial_messages = build_initial_messages(&initial_text, &file_images, &cli_messages);

    // TUI 模式需要交互终端：stdin/stdout 非 TTY 时无法渲染
    if !stdin_is_tty() || !stdout_is_tty() {
        eprintln!(
            "Error: {APP_NAME} only supports the interactive TUI mode, which requires stdin and stdout to be a TTY.",
        );
        std::process::exit(1);
    }

    // 扩展会话钩子：恢复持久化状态（如 plan-mode 的 cwd 校验 + resume 重扫 [DONE:n]；
    // goal 读取会话内自定义条目并执行重载保护），并按可能变化的状态重建工具列表
    // （filter_tools / filter_extension_tools 立即生效）。
    for ext in core::extensions::registered() {
        ext.on_session_start(&agent.cwd, &agent.messages, agent.session.as_ref());
    }

    // footer 扩展的会话历史：启动即恢复的会话（`-c` / `--resume`）也要按历史重建计数，
    // 否则底栏计数从 0 开始、与同一帧从消息算出的用量对不上。
    let session_path = agent
        .session
        .as_ref()
        .and_then(|s| s.get_session_file())
        .map(|p| p.to_string_lossy().to_string());
    core::extensions::dispatch_footer_session(session_path.as_deref(), &agent.messages);

    agent.rebuild_tools();

    // 会话执行上下文：让扩展在本会话首个回合之前（如首条消息即 `@mention`）
    // 也能拿到 make_sub_agent（否则要等首次工具执行才会经工具 ctx 暴露）。
    core::extensions::dispatch_exec_ctx(&agent.exec_ctx());

    let reload_ctx = interactive::app::ReloadCtx {
        skill_paths: parsed.skill.clone(),
        prompt_paths: parsed.prompt_template.clone(),
        include_default_skills: !parsed.no_skills,
        include_default_prompt_templates: !parsed.no_prompt_templates,
        no_context_files: parsed.no_context_files,
        trusted,
        trust_override: parsed.approve_option(),
        cli_system_prompt: parsed.system_prompt.clone(),
        cli_append_system_prompt: if parsed.append_system_prompt.is_empty() {
            None
        } else {
            Some(parsed.append_system_prompt.join("\n\n"))
        },
    };

    match interactive::run_interactive(
        agent,
        initial_messages,
        prompt_templates,
        theme_override.as_deref(),
        parsed.no_themes,
        reload_ctx,
        parsed.verbose,
    )
    .await
    {
        Ok(_) => return,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

/// 默认回退模型：首个支持 provider 的默认模型（deepseek/deepseek-v4-pro）
fn fallback_model() -> (String, String) {
    core::model_resolver::default_model_for(core::model_resolver::SUPPORTED_PROVIDERS[0])
        .unwrap_or_else(|| ("deepseek".to_string(), "deepseek-v4-pro".to_string()))
}

/// 中间行损坏的会话：非交互（stdin/stdout 非 TTY）直接失败退出；
/// 交互时把修复请求 stage 到全局队列，TUI 事件循环首帧弹确认面板，
/// 并返回 None 让主流程正常启动（面板确认后经 worker ResumeSession 重开）。
fn stage_session_repair_for_tui(path: PathBuf, plan: SessionRepairPlan) -> Option<Session> {
    stage_session_repair_for_tui_req(SessionRepairRequest {
        path,
        plan,
        action: RepairAction::Resume,
    })
}

/// 打开成功的会话存在 ≥2 个崩溃残留的未完成 operation（僵尸）：**不加载**，
/// stage 恢复方式选择面板（Continue / Rewrite / Cancel），用户确认后才经
/// worker ResumeZombie 决定是否装配；无僵尸或非交互时直接返回会话。
fn maybe_open_with_zombie_panel(s: Session) -> Option<Session> {
    let count = s.open_operations_count();
    if count < 2 || !stdin_is_tty() || !stdout_is_tty() {
        return Some(s);
    }

    if let Some(path) = s.get_session_file() {
        interactive::handlers::stage_zombie_operations_global(ZombieOperationsRequest {
            path: path.to_path_buf(),
            count,
        });
    }
    None
}

/// 按请求（含修复后动作）中间行损坏的会话：非交互（stdin/stdout 非 TTY）直接失败退出；
/// 交互时把修复请求 stage 到全局队列，TUI 事件循环首帧弹确认面板，
/// 并返回 None 让主流程正常启动（面板确认后经 worker 按动作重开/fork）。
fn stage_session_repair_for_tui_req(req: SessionRepairRequest) -> Option<Session> {
    if !stdin_is_tty() || !stdout_is_tty() {
        eprintln!("Error: failed to open session: {}", req.plan.error);
        std::process::exit(1);
    }

    eprintln!(
        "Warning: session corrupted ({}); will ask to repair in the TUI.",
        req.plan.error
    );

    interactive::handlers::stage_session_repair_global(req);
    None
}

/// 解析启动 scope 名单：--models CLI 优先，否则 settings.enabledModels
/// （对齐 pi `parsed.models ?? getEnabledModels`）。无匹配的 pattern 打印 warning 诊断。
/// 返回有序 ScopedModel（provider/id + 可选 thinking）。
fn resolve_startup_scope(parsed: &Args, settings: &Settings) -> Vec<ScopedModel> {
    let scope_patterns: Vec<String> = if !parsed.models.is_empty() {
        parsed.models.clone()
    } else {
        settings.enabled_models.clone()
    };

    if scope_patterns.is_empty() {
        return Vec::new();
    }

    let available: Vec<model_scope::AvailableModel> = core::auth::list_configured_providers()
        .iter()
        .flat_map(|p| {
            core::model_resolver::list_models(p)
                .into_iter()
                .map(|(id, name)| model_scope::AvailableModel {
                    provider: p.clone(),
                    id,
                    name,
                })
        })
        .collect();

    let resolved = model_scope::resolve_model_scope(&scope_patterns, &available);
    for d in &resolved.diagnostics {
        match d {
            model_scope::ScopeDiagnostic::NoMatch { pattern } => {
                eprintln!("Warning: No models match pattern \"{pattern}\"");
            }
            model_scope::ScopeDiagnostic::InvalidThinkingLevel { pattern } => {
                eprintln!(
                    "Warning: Invalid thinking level in pattern \"{pattern}\". Using default instead."
                );
            }
        }
    }
    resolved.scoped
}

/// 初始模型选择：--model > PRUX_MODEL > settings > scope[0]
/// 返回 (provider, model_id, thinking, model_unconfigured)。
fn select_startup_model(
    parsed: &Args,
    settings: &Settings,
    scope_models: &[ScopedModel],
) -> (String, String, Option<String>, bool) {
    let model_unconfigured = parsed.model.is_none()
        && std::env::var("PRUX_MODEL").is_err()
        && settings.default_model.is_none();

    // 显式恢复会话时不用 scope[0] 兜底
    let resuming_session = parsed.resume
        || parsed.continue_session
        || parsed.session.is_some()
        || parsed.session_id.is_some()
        || parsed.fork.is_some();

    if model_unconfigured && !scope_models.is_empty() && !resuming_session {
        let first = &scope_models[0];
        return (
            first.provider.clone(),
            first.id.clone(),
            first.thinking.clone(),
            model_unconfigured,
        );
    }
    match resolve_provider_model(
        parsed.provider.as_deref(),
        parsed.model.as_deref(),
        settings,
    ) {
        Ok(v) => (v.0, v.1, v.2, model_unconfigured),
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

/// 会话选择：--no-session / --fork / --session / --session-id / --resume / --session-dir / -c
fn select_session(parsed: &Args, cwd: &str, default_dir: &Path) -> Option<Session> {
    if parsed.fork.is_some()
        && (parsed.no_session
            || parsed.session.is_some()
            || parsed.resume
            || parsed.continue_session)
    {
        eprintln!(
            "Error: --fork cannot be used with --no-session, --session, --resume, or --continue."
        );
        std::process::exit(1);
    }

    if parsed.session_id.is_some()
        && (parsed.session.is_some() || parsed.resume || parsed.continue_session)
    {
        eprintln!("Error: --session-id cannot be used with --session, --resume, or --continue.");
        std::process::exit(1);
    }

    if parsed.no_session {
        return Session::create(cwd, None, false).ok();
    }

    if let Some(fork_path) = &parsed.fork {
        let fork_dir = parsed
            .session_dir
            .clone()
            .map(expand_tilde_path)
            .or_else(|| Some(default_dir.to_path_buf()));
        let resolved = resolve_session_arg(fork_path, default_dir, fork_dir.as_deref());

        // 源会话完整性检查：中间行损坏可修复 → 弹面板，确认后重新 fork
        if resolved.is_file() {
            return match Session::open_checked(&resolved.to_string_lossy()) {
                OpenSessionResult::Ok(_) => {
                    match Session::fork_from(&resolved.to_string_lossy(), cwd, fork_dir) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            eprintln!("Error: cannot fork session {}: {}", fork_path, e);
                            std::process::exit(1);
                        }
                    }
                }
                OpenSessionResult::Invalid(e) => {
                    eprintln!("Error: cannot fork session {}: {}", fork_path, e);
                    std::process::exit(1);
                }
                OpenSessionResult::Repairable { path, plan } => {
                    stage_session_repair_for_tui_req(SessionRepairRequest {
                        path,
                        plan,
                        action: RepairAction::Fork { dir: fork_dir },
                    })
                }
            };
        }

        return match Session::fork_from(&resolved.to_string_lossy(), cwd, fork_dir) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("Error: cannot fork session {}: {}", fork_path, e);
                std::process::exit(1);
            }
        };
    }

    if let Some(path) = &parsed.session {
        let resolved = resolve_session_arg(path, default_dir, None);
        if resolved.is_file() {
            return match Session::open_checked(&resolved.to_string_lossy()) {
                OpenSessionResult::Ok(s) => maybe_open_with_zombie_panel(*s),
                OpenSessionResult::Invalid(e) => {
                    eprintln!("Error: failed to open session: {}", e);
                    std::process::exit(1);
                }
                OpenSessionResult::Repairable { path, plan } => {
                    stage_session_repair_for_tui(path, plan)
                }
            };
        }

        // 当前项目目录找不到时，全局搜索其他项目，
        // 命中则提示并 fork 进当前目录（避免打开别的项目的会话破坏 cwd 语义）。
        if let Some(found) = find_session_globally(path, default_dir) {
            if !confirm_fork_into_current(&found, cwd) {
                return Session::create(cwd, None, false).ok();
            }

            let dir = parsed
                .session_dir
                .clone()
                .map(expand_tilde_path)
                .unwrap_or_else(|| default_dir.to_path_buf());

            return match Session::fork_from(&found.to_string_lossy(), cwd, Some(dir)) {
                Ok(s) => Some(s),
                Err(e) => {
                    eprintln!("Error: failed to fork session {}: {}", found.display(), e);
                    std::process::exit(1);
                }
            };
        }

        eprintln!("Error: failed to open session: {}", path);
        std::process::exit(1);
    }

    if let Some(id) = &parsed.session_id {
        let dir = parsed
            .session_dir
            .clone()
            .map(expand_tilde_path)
            .unwrap_or_else(|| default_dir.to_path_buf());
        let found = find_session_by_id(&dir, id).or_else(|| find_session_by_id(default_dir, id));

        return match found {
            Some(path) => match Session::open_checked(&path.to_string_lossy()) {
                OpenSessionResult::Ok(s) => maybe_open_with_zombie_panel(*s),
                OpenSessionResult::Invalid(e) => {
                    eprintln!("Error: failed to open session: {}", e);
                    std::process::exit(1);
                }
                OpenSessionResult::Repairable { path, plan } => {
                    stage_session_repair_for_tui(path, plan)
                }
            },
            None => match Session::create_with_id(cwd, Some(dir), true, id) {
                Ok(s) => Some(s),
                Err(e) => {
                    eprintln!("Error: failed to create session: {}", e);
                    std::process::exit(1);
                }
            },
        };
    }

    if parsed.resume {
        let dir = parsed
            .session_dir
            .clone()
            .map(expand_tilde_path)
            .unwrap_or_else(|| default_dir.to_path_buf());
        return pick_session(&dir).and_then(|p| {
            match Session::open_checked(p.to_string_lossy().as_ref()) {
                OpenSessionResult::Ok(s) => maybe_open_with_zombie_panel(*s),
                OpenSessionResult::Invalid(_) => None,
                OpenSessionResult::Repairable { path, plan } => {
                    stage_session_repair_for_tui(path, plan)
                }
            }
        });
    }

    if let Some(dir) = &parsed.session_dir {
        let dir = expand_tilde_path(dir.clone());
        if parsed.continue_session {
            return match Session::continue_recent_checked(cwd, Some(dir), true) {
                ContinueRecentResult::Ok(s) | ContinueRecentResult::Created(s) => {
                    maybe_open_with_zombie_panel(s)
                }
                ContinueRecentResult::Failed => None,
                ContinueRecentResult::Repairable { path, plan } => {
                    stage_session_repair_for_tui(path, plan)
                }
            };
        }
        return Session::create(cwd, Some(dir), true).ok();
    }

    if parsed.continue_session {
        return match Session::continue_recent_checked(cwd, None, true) {
            ContinueRecentResult::Ok(s) | ContinueRecentResult::Created(s) => {
                maybe_open_with_zombie_panel(s)
            }
            ContinueRecentResult::Failed => None,
            ContinueRecentResult::Repairable { path, plan } => {
                stage_session_repair_for_tui(path, plan)
            }
        };
    }

    _ = std::fs::create_dir_all(default_dir);
    Session::create(cwd, Some(default_dir.to_path_buf()), true).ok()
}

/// stdin 是否连接到终端（决定能否交互式提问）。
fn stdin_is_tty() -> bool {
    std::io::stdin().is_terminal()
}

/// stdout 是否连接到终端（决定是否启用 TUI 渲染）。
fn stdout_is_tty() -> bool {
    std::io::stdout().is_terminal()
}

/// 全局搜索会话：agent_dir/sessions 下所有项目目录按 id 匹配
fn find_session_globally(id: &str, current_dir: &Path) -> Option<PathBuf> {
    let root = agent_dir().join("sessions");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return None;
    };

    for e in entries.flatten() {
        let dir = e.path();
        if dir == current_dir {
            continue;
        }

        if let Some(found) = find_session_by_id(&dir, id) {
            return Some(found);
        }
    }

    None
}

/// 跨项目会话命中时询问是否 fork 进当前目录
fn confirm_fork_into_current(_found: &Path, _cwd: &str) -> bool {
    // 非交互（stdin 非 TTY）时默认拒绝；交互时读一行 y/n
    if !std::io::stdin().is_terminal() {
        return false;
    }

    eprint!("Found session in another project. Fork this session into current directory? [y/N] ");
    let mut line = String::new();
    _ = std::io::stdin().read_line(&mut line);
    line.trim().eq_ignore_ascii_case("y")
}
