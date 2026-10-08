//! 扩展注册表：编译期静态注册 + 全局遍历 + 启用/禁用状态 + 扩展模式。
//!
//! 每个扩展（工具扩展与底栏扩展）带一个 `enabled` 标志：
//! - 注册时从 settings.json 的 `disabledExtensions` / `enabledExtensions`
//!   应用持久化状态，未显式配置则回落到扩展声明的 [`Extension::default_enabled`]
//!   （默认启用）；
//! - [`registered`] / [`registered_footers`] 只返回已启用且当前模式可用的扩展
//!   （hook 分发、工具分发、底栏渲染自动按状态过滤）；
//! - /extension 面板经 [`set_extension_enabled`] 切换（仅改内存），Ctrl+S 时经
//!   [`persist_extension_states`] 写盘；
//! - 扩展模式（[`current_extension_mode`]）经 [`set_extension_mode`] 切换，
//!   写盘持久化；模式是运行时过滤层，不触碰各扩展 enabled 状态。

use super::{
    Extension, ExtensionCommand, ExtensionMode, ExtensionSetting, ToolExposure,
    banner::BannerExtension, clear_deferred_activations, footer::FooterExtension, renderers,
};
use crate::{
    core::{provider::api_impls, settings_manager, virtual_models},
    error::Result,
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

/// 当前扩展模式（进程级单例，懒加载时从 settings.json 读取）
static EXTENSION_MODE: OnceLock<Mutex<ExtensionMode>> = OnceLock::new();
/// 普通（工具）扩展注册表，按注册顺序保存
static REGISTRY: OnceLock<Mutex<Vec<ExtensionEntry>>> = OnceLock::new();
/// 底栏扩展注册表，按注册顺序保存
static FOOTER_REGISTRY: OnceLock<Mutex<Vec<FooterEntry>>> = OnceLock::new();
/// 横幅扩展注册表，按注册顺序保存
static BANNER_REGISTRY: OnceLock<Mutex<Vec<BannerEntry>>> = OnceLock::new();
/// `--no-extensions`：本进程内所有扩展（含内置）一律默认禁用。
static ALL_EXTENSIONS_DISABLED: AtomicBool = AtomicBool::new(false);

/// 注册表中的一条普通扩展记录
struct ExtensionEntry {
    /// 是否启用（注册时结合 settings.json 与扩展默认值确定）
    enabled: bool,
    /// 扩展本体，Arc 便于快照时零拷贝共享
    ext: Arc<dyn Extension>,
}

/// 注册表中的一条底栏扩展记录
struct FooterEntry {
    /// 是否启用（同组互斥：至多一个为 true）
    enabled: bool,
    /// 底栏扩展本体
    ext: Arc<dyn FooterExtension>,
}

/// 注册表中的一条横幅扩展记录
struct BannerEntry {
    /// 是否启用（同组互斥：至多一个为 true）
    enabled: bool,
    /// 横幅扩展本体
    ext: Arc<dyn BannerExtension>,
}

/// `--no-extensions` 是否生效（进程级）。
fn all_extensions_disabled() -> bool {
    ALL_EXTENSIONS_DISABLED.load(Ordering::Relaxed)
}

/// `--no-extensions`：禁用本进程内**所有**扩展（含内置 footer / 横幅）。
///
/// `--no-extensions` 语义（同样覆盖内置扩展，不只是外部扩展）；
/// 无外部扩展加载器，因此这里就是「一个都不启用」。
/// 必须在 CLI 解析后调用（注册早于解析：动态 flag 注入依赖已注册的扩展声明），
/// 故本函数同时把已注册条目立即置为禁用；`/extension` 面板仍可在本次运行内逐个开启。
pub fn set_all_extensions_disabled(disabled: bool) {
    ALL_EXTENSIONS_DISABLED.store(disabled, Ordering::Relaxed);
    if !disabled {
        return;
    }

    let previously_enabled: Vec<Arc<dyn Extension>> = {
        let mut reg = registry().lock().unwrap();
        for e in reg.iter_mut() {
            e.enabled = false;
        }
        reg.iter().map(|e| e.ext.clone()).collect()
    };

    for e in footer_registry().lock().unwrap().iter_mut() {
        e.enabled = false;
    }

    for e in banner_registry().lock().unwrap().iter_mut() {
        e.enabled = false;
    }

    // 延迟工具激活集合也作废（工具表已不再包含这些扩展的工具）
    clear_deferred_activations();

    // `--no-extensions` 在注册之后才解析：注册时扩展可能已按初始状态启动资源，
    // 这里补一次禁用通知（锁外），否则标志位关了、后台资源仍在跑。
    for ext in previously_enabled {
        ext.on_enabled_changed(false);
    }
}

/// 全局扩展注册表（懒加载初始化）。
fn registry() -> &'static Mutex<Vec<ExtensionEntry>> {
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// footer 扩展注册表（懒加载初始化）。
fn footer_registry() -> &'static Mutex<Vec<FooterEntry>> {
    FOOTER_REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// banner 扩展注册表（懒加载初始化）。
fn banner_registry() -> &'static Mutex<Vec<BannerEntry>> {
    BANNER_REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// 扩展模式全局状态；首次访问时读 settings.json 初始化。
fn mode_state() -> &'static Mutex<ExtensionMode> {
    EXTENSION_MODE.get_or_init(|| {
        Mutex::new(ExtensionMode::parse(
            &settings_manager::read_extension_mode(),
        ))
    })
}

/// 当前扩展模式（懒加载：首次访问读 settings.json，重启保持上次选择）
pub fn current_extension_mode() -> ExtensionMode {
    *mode_state().lock().unwrap()
}

/// 切换扩展模式（/extension 面板 Tab）：只改模式，不触碰各扩展 enabled
/// 状态（切回全部模式即恢复原状），立即写盘持久化。
///
/// 模式变化会实时改变扩展的**有效启用状态**（[`effective_enabled`]）：可用性
/// 翻转的扩展同步收到 [`Extension::on_enabled_changed`]，否则切到 Minimal 后
/// 已启动的资源（如 proxy 的监听端口）会继续运行。
pub fn set_extension_mode(mode: ExtensionMode) {
    let previous = {
        let mut state = mode_state().lock().unwrap();
        std::mem::replace(&mut *state, mode)
    };

    _ = settings_manager::write_extension_mode(mode.as_str());

    if previous != mode {
        sync_effective_states(previous, mode);
    }
}

/// 插件在当前模式下是否可用（见 [`ExtensionMode::usable_in`]）：
/// `All` 模式兜底（所有插件可用）；其余模式要求存在声明的模式满足
/// “级别低于当前模式或恰好等于当前模式”（声明 `[Minimal]` 即 Minimal 及以上可用）。
fn usable_in_mode(modes: &[ExtensionMode], current: ExtensionMode) -> bool {
    current == ExtensionMode::All || modes.iter().any(|d| d.usable_in(current))
}

/// 扩展在当前模式下的**有效**启用状态：已启用且声明了当前模式（见 [`usable_in_mode`]）。
///
/// 生命周期回调 [`Extension::on_enabled_changed`] 按此求值，而非裸 `enabled`：
/// 否则声明 `[Dev, Creator]` 的扩展（如 proxy）只要在 settings.json 里开着，
/// Minimal 模式下启动仍会收到 `true` 并占用端口 / 拉起后台线程。
fn effective_enabled(enabled: bool, modes: &[ExtensionMode], mode: ExtensionMode) -> bool {
    enabled && usable_in_mode(modes, mode)
}

/// 模式切换后同步可用性翻转的扩展：新模式下可用则通知启用（重新启动资源），
/// 不可用则通知禁用（停掉资源）。只通知翻转者，避免每次切模式重跑无关扩展的初始化。
fn sync_effective_states(previous: ExtensionMode, current: ExtensionMode) {
    let notifications: Vec<(Arc<dyn Extension>, bool)> = {
        let reg = registry().lock().unwrap();
        reg.iter()
            .filter(|e| {
                effective_enabled(e.enabled, &e.ext.modes(), previous)
                    != effective_enabled(e.enabled, &e.ext.modes(), current)
            })
            .map(|e| {
                (
                    e.ext.clone(),
                    effective_enabled(e.enabled, &e.ext.modes(), current),
                )
            })
            .collect()
    };

    for (ext, enabled) in notifications {
        ext.on_enabled_changed(enabled);
    }
}

/// settings.json 中是否禁用了该扩展（注册时应用持久化状态）
fn persisted_disabled(name: &str) -> bool {
    settings_manager::read_disabled_extensions()
        .iter()
        .any(|n| n == name)
}

/// settings.json 中是否显式启用了该扩展（仅对 `default_enabled() == false`
/// 的扩展有意义：它们默认关闭，需在此列出或经 /extension 面板开启）。
fn persisted_enabled(name: &str) -> bool {
    settings_manager::read_enabled_extensions()
        .iter()
        .any(|n| n == name)
}

/// 扩展注册/重载时的初始启用状态：`disabledExtensions` 显式禁用优先，
/// 其次 `enabledExtensions` 显式启用，都未配置时回落到
/// [`Extension::default_enabled`]（默认 `true`）。
fn initial_enabled(ext: &dyn Extension) -> bool {
    if all_extensions_disabled() {
        return false;
    }
    if persisted_disabled(ext.name()) {
        return false;
    }
    ext.default_enabled() || persisted_enabled(ext.name())
}

/// 编译期静态注册一个扩展
///
/// 扩展作者实现 [`Extension`] trait 后，在 `main` 或应用组装处调用本函数，
/// 扩展即被纳入全局注册表，其工具与钩子在 Agent 循环中生效。
/// 若 settings.json 的 `disabledExtensions` 包含该扩展名，则注册为禁用状态；
/// 若扩展声明 [`Extension::default_enabled`] 为 `false`，则仅在 `enabledExtensions`
/// 列出该扩展时注册为启用状态（默认可选扩展按需开启）。
pub fn register_extension(ext: impl Extension + 'static) {
    register_extension_arc(Arc::new(ext));
}

/// 注册一个已装箱的扩展（[`Arc<dyn Extension>`] 入口）。
///
/// 与 [`register_extension`] 等价；供分布式切片 loader
/// （[`crate::extensions::register_all`]）按统一顺序注册用。
pub fn register_extension_arc(ext: Arc<dyn Extension>) {
    let enabled = initial_enabled(ext.as_ref());

    registry().lock().unwrap().push(ExtensionEntry {
        enabled,
        ext: ext.clone(),
    });

    // 虚拟模型：注册时收集进全局注册表（归属扩展名 → 可随扩展启停/模式动态过滤）。
    for definition in ext.virtual_models() {
        virtual_models::register(definition, ext.name());
    }

    // 协议实现（图片 / 分类器）：同样按归属扩展名登记，随扩展启停过滤。
    api_impls::register_image_apis(ext.name(), ext.image_apis());
    api_impls::register_classifier_apis(ext.name(), ext.classifier_apis());

    // 工具渲染器：同样按归属扩展名登记（同 owner 整体替换），随扩展启停过滤。
    renderers::register_tool_renderers(ext.name(), ext.tool_renderers());

    // 初始同步：按恢复的初始状态给扩展通知**有效**启用态（启用则写入、禁用则清理），
    // 处理上次未干净退出/文件缺失的情况。注意与当前模式取交集：声明 `[Dev, Creator]`
    // 的扩展在 Minimal 模式下即便持久化开启，也必须收到 false（不得占用端口/起线程）。
    // 锁外调用——on_enabled_changed 可能回调扩展系统
    // （如 plan-mode 广播事件走 dispatch_agent_event → registered()）重新获取 registry 锁。
    ext.on_enabled_changed(effective_enabled(
        enabled,
        &ext.modes(),
        current_extension_mode(),
    ));

    // 注册钩子：扩展在此接线需要 TUI 状态的执行入口（斜杠命令 handler、快捷键 handler 等）。
    // 同样锁外调用，避免重入 registry 锁。
    ext.on_registered();
}

/// 注销一个已注册扩展（移除注册表条目，钩子/工具分发立即停止）。
/// 返回是否实际移除了条目。
///
/// 主要用于测试自清理（全局注册表跨测试累积会污染并行的 agent 循环）；
/// 也为运行时工具注册 API（registerTool 等价物）提供删除原语。
pub fn unregister_extension(name: &str) -> bool {
    virtual_models::unregister_owner(name);
    api_impls::unregister_owner(name);
    renderers::unregister_tool_renderers(name);
    let mut reg = registry().lock().unwrap();
    let before = reg.len();
    reg.retain(|e| e.ext.name() != name);
    reg.len() != before
}

/// 当前已注册的扩展数量（诊断用，含禁用）
pub fn extension_count() -> usize {
    registry().lock().unwrap().len()
}

/// /reload：把扩展运行时状态回退到 settings.json 持久化配置
/// （disabledExtensions / enabledExtensions + extensionMode）。扩展代码是编译期静态注册、
/// 本身不变，但 enabled 状态与模式重新从磁盘应用，覆盖运行时临时切换。
pub fn reload_from_settings() {
    let mode = ExtensionMode::parse(&settings_manager::read_extension_mode());
    *mode_state().lock().unwrap() = mode;

    let disabled: Vec<String> = settings_manager::read_disabled_extensions();
    let is_disabled = |name: &str| all_extensions_disabled() || disabled.iter().any(|n| n == name);

    let notifications: Vec<(Arc<dyn Extension>, bool)> = {
        let mut reg = registry().lock().unwrap();
        for e in reg.iter_mut() {
            e.enabled = initial_enabled(e.ext.as_ref());
        }
        reg.iter()
            .map(|e| {
                (
                    e.ext.clone(),
                    effective_enabled(e.enabled, &e.ext.modes(), mode),
                )
            })
            .collect()
    };

    // 通知各扩展恢复后的最终状态（/reload 兜底同步文件；幂等）；同样与当前模式取交集
    for (ext, enabled) in notifications {
        ext.on_enabled_changed(enabled);
    }

    // footer 互斥恢复：与注册时一致，最后一个启用者生效
    let enabled_flags: Vec<bool> = footer_registry()
        .lock()
        .unwrap()
        .iter()
        .map(|e| !is_disabled(e.ext.name()))
        .collect();

    let mut footers = footer_registry().lock().unwrap();
    let mut enabled_idx: Vec<usize> = Vec::new();
    for (i, e) in footers.iter_mut().enumerate() {
        e.enabled = enabled_flags[i];
        if e.enabled {
            enabled_idx.push(i);
        }
    }
    if let Some(last) = enabled_idx.pop() {
        for i in enabled_idx {
            footers[i].enabled = false;
        }
        footers[last].enabled = true;
    }

    // banner 互斥恢复：与注册时一致，最后一个启用者生效
    let banner_flags: Vec<bool> = banner_registry()
        .lock()
        .unwrap()
        .iter()
        .map(|e| !is_disabled(e.ext.name()))
        .collect();

    let mut banners = banner_registry().lock().unwrap();
    let mut banner_enabled_idx: Vec<usize> = Vec::new();
    for (i, e) in banners.iter_mut().enumerate() {
        e.enabled = banner_flags[i];
        if e.enabled {
            banner_enabled_idx.push(i);
        }
    }
    if let Some(last) = banner_enabled_idx.pop() {
        for i in banner_enabled_idx {
            banners[i].enabled = false;
        }
        banners[last].enabled = true;
    }
}

/// 注册一个底栏扩展。
/// 注册后 TUI 底栏由扩展渲染；全部禁用后底栏不显示（不留空行）。
/// footer 扩展互斥：新扩展默认启用时自动禁用其他已启用的 footer
/// （保证同一时刻至多一个 footer 生效，渲染取首个启用项）。
pub fn register_footer_extension(ext: impl FooterExtension + 'static) {
    register_footer_extension_arc(Arc::new(ext));
}

/// 注册一个已装箱的底栏扩展（[`Arc<dyn FooterExtension>`] 入口）。
///
/// 与 [`register_footer_extension`] 等价；供分布式切片 loader
/// （[`crate::extensions::register_all`]）按统一顺序注册用。
pub fn register_footer_extension_arc(ext: Arc<dyn FooterExtension>) {
    let enabled = !persisted_disabled(ext.name());
    if enabled {
        for e in footer_registry().lock().unwrap().iter_mut() {
            e.enabled = false;
        }
    }

    footer_registry()
        .lock()
        .unwrap()
        .push(FooterEntry { enabled, ext });
}

/// 当前已注册的底栏扩展数量（诊断用，含禁用）
pub fn footer_extension_count() -> usize {
    footer_registry().lock().unwrap().len()
}

/// 注册一个横幅扩展。
/// 注册后 TUI 在干净启动时于窗口顶部渲染 banner；全部禁用后不渲染、不留空行。
/// banner 扩展互斥：新扩展默认启用时自动禁用其他已启用的 banner
/// （保证同一时刻至多一个 banner 生效，渲染取首个启用项）。
pub fn register_banner_extension(ext: impl BannerExtension + 'static) {
    register_banner_extension_arc(Arc::new(ext));
}

/// 注册一个已装箱的横幅扩展（[`Arc<dyn BannerExtension>`] 入口）。
///
/// 与 [`register_banner_extension`] 等价；供分布式切片 loader
/// （[`crate::extensions::register_all`]）按统一顺序注册用。
pub fn register_banner_extension_arc(ext: Arc<dyn BannerExtension>) {
    let enabled = !persisted_disabled(ext.name());
    if enabled {
        for e in banner_registry().lock().unwrap().iter_mut() {
            e.enabled = false;
        }
    }

    banner_registry()
        .lock()
        .unwrap()
        .push(BannerEntry { enabled, ext });
}

/// 当前已注册的横幅扩展数量（诊断用，含禁用）
pub fn banner_extension_count() -> usize {
    banner_registry().lock().unwrap().len()
}

/// 已启用扩展的快照（克隆 Arc 列表，避免跨 await 持锁）。
/// 按当前模式过滤（[`usable_in_mode`]：All 兜底，其余模式按声明级别匹配）。
/// 当前已启用且模式可用的普通扩展列表（事件分发 / 键盘分发 / flag 注入遍历用）
pub fn registered() -> Vec<Arc<dyn Extension>> {
    let mode = current_extension_mode();
    registry()
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.enabled && usable_in_mode(&e.ext.modes(), mode))
        .map(|e| e.ext.clone())
        .collect()
}

/// 是否有已启用（且当前模式可用）的扩展请求从默认系统提示词中移除`程序文档段`。
///
/// 汇总各扩展的 [`Extension::suppress_documentation`]：任一为 `true` 即为真。
/// 供 [`crate::core::system_prompt::build_system_prompt`] 决定是否拼入文档描述。
pub fn documentation_suppressed() -> bool {
    registered().iter().any(|e| e.suppress_documentation())
}

/// 已启用底栏扩展的快照（TUI 渲染时遍历；禁用后不渲染）。
/// 按当前模式过滤（[`usable_in_mode`]）。
/// 当前已启用且模式可用的 footer 扩展列表
pub fn registered_footers() -> Vec<Arc<dyn FooterExtension>> {
    let mode = current_extension_mode();
    footer_registry()
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.enabled && usable_in_mode(&e.ext.modes(), mode))
        .map(|e| e.ext.clone())
        .collect()
}

/// 已启用横幅扩展的快照（TUI 渲染时遍历；禁用后不渲染）。
/// 按当前模式过滤（[`usable_in_mode`]）。
/// 当前已启用且模式可用的 banner 扩展列表
pub fn registered_banners() -> Vec<Arc<dyn BannerExtension>> {
    let mode = current_extension_mode();
    banner_registry()
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.enabled && usable_in_mode(&e.ext.modes(), mode))
        .map(|e| e.ext.clone())
        .collect()
}

/// 取指定扩展声明的设置项（按注册顺序查找）；扩展不存在 / 已禁用 / 当前模式
/// 不可用 / 未声明设置时返回空。TUI 扩展设置面板一次只展示一个扩展。
pub fn settings_for(ext_name: &str) -> Vec<ExtensionSetting> {
    registered()
        .into_iter()
        .find(|e| e.name() == ext_name)
        .map(|e| e.settings())
        .unwrap_or_default()
}

/// 按扩展名应用一项设置（扩展设置面板用）。
/// 扩展不存在 / 已禁用 / 当前模式不可用时返回 `Err`。
pub fn apply_extension_setting(
    ext_name: &str,
    key: &str,
    value: &str,
) -> std::result::Result<(), String> {
    match registered().into_iter().find(|e| e.name() == ext_name) {
        Some(ext) => ext.apply_setting(key, value),
        None => Err(format!("extension not available: {ext_name}")),
    }
}

/// 扩展声明名的合法性：非空、不含空白字符与 `/`。
///
/// 空名（或含 `/`、空白的名）会让 `/` 候选列表出现空条目并命中任何前缀匹配的输入，
/// 因此这类声明整条丢弃（扩展加载即报错，而不是等用户敲 `/` 才炸）。
fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.chars().any(|c| c.is_whitespace() || c == '/')
}

/// 扩展声明的静态校验与冲突检查：非法命令名一律告警，多个已启用扩展声明同名工具 /
/// 斜杠命令 / CLI flag 时，先注册者生效、后注册者被静默丢弃——这里把丢弃显式变成告警。
/// 扩展均为编译期内置（无外部包加载器），同一机制退化为内置扩展之间的重名检测。
pub fn registration_issues() -> Vec<String> {
    /// 登记一个扩展声明的名字；若去重键（`kind:key_name`）已被占用，
    /// 则往 `warnings` 追加「后注册者被遮蔽」告警。`shown` 是展示给用户的名字。
    fn claim(
        taken: &mut HashMap<String, String>,
        warnings: &mut Vec<String>,
        kind: &str,
        key_name: &str,
        shown: &str,
        owner: &str,
    ) {
        let key = format!("{kind}:{key_name}");
        match taken.get(&key) {
            Some(prev) => warnings.push(format!(
                "extension `{owner}` registers {kind} `{shown}`, shadowing `{prev}`; only the first registration takes effect"
            )),
            None => {
                taken.insert(key, owner.to_string());
            }
        }
    }

    let mut taken: HashMap<String, String> = HashMap::new();
    let mut warnings: Vec<String> = Vec::new();

    for ext in registered() {
        let owner = ext.name().to_string();
        for tool in ext.tools() {
            claim(
                &mut taken,
                &mut warnings,
                "tool",
                &tool.name,
                &tool.name,
                &owner,
            );
        }
        for cmd in ext.commands() {
            if !valid_name(&cmd.name) {
                warnings.push(format!(
                    "extension `{owner}` declares a command with an invalid name `{}`; the declaration is ignored",
                    cmd.name
                ));
                continue;
            }

            for sub in &cmd.subcommands {
                if !valid_name(sub.name) {
                    warnings.push(format!(
                        "extension `{owner}` declares subcommand `{}` of `/{cmd_name}` with an invalid name; the declaration is ignored",
                        sub.name,
                        cmd_name = cmd.name
                    ));
                }
            }

            claim(
                &mut taken,
                &mut warnings,
                "command",
                &cmd.name,
                &format!("/{}", cmd.name),
                &owner,
            );
        }
        for flag in ext.cli_flags() {
            claim(
                &mut taken,
                &mut warnings,
                "flag",
                flag.name,
                &format!("--{}", flag.name),
                &owner,
            );
        }
    }

    warnings
}

/// 当前已启用扩展声明的全部斜杠命令（已按模式过滤）。
///
/// 命令随扩展生命周期动态增删：扩展禁用 / 当前模式不可用时其命令不再返回。
/// 扩展之间同名命令先到先得（保留先注册者的声明）。
pub fn registered_commands() -> Vec<ExtensionCommand> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();

    for ext in registered() {
        for mut cmd in ext.commands() {
            if !valid_name(&cmd.name) {
                continue;
            }
            cmd.subcommands.retain(|s| valid_name(s.name));
            if !seen.iter().any(|s| s == &cmd.name) {
                seen.push(cmd.name.clone());
                out.push(cmd);
            }
        }
    }
    out
}

/// 命令名由哪个当前已启用扩展提供（已按模式过滤），先到先得。
/// 找不到（未注册 / 扩展禁用 / 模式不可用）返回 `None`。
pub fn command_provider(name: &str) -> Option<String> {
    if !valid_name(name) {
        return None;
    }
    registered()
        .iter()
        .find(|e| e.commands().iter().any(|c| c.name == name))
        .map(|e| e.name().to_string())
}

/// 指定扩展命令是否忙碌（流式输出）时可安全执行（handler 不锁 agent）。
///
/// 查询当前已启用扩展的声明（已按模式过滤），先到先得；未注册 / 扩展禁用 /
/// 模式不可用 / 非扩展命令一律返回 `false`——忙碌时保守地按普通文本排队，
/// 避免与持有 agent 锁的 prompt future 互等。
pub fn command_busy_safe(name: &str) -> bool {
    if !valid_name(name) {
        return false;
    }
    registered()
        .iter()
        .flat_map(|e| e.commands())
        .find(|c| c.name == name)
        .map(|c| c.busy_safe)
        .unwrap_or(false)
}

/// /extension 面板条目：(扩展名, 是否启用, 声明的模式列表)。
/// 工具扩展在前、底栏扩展在后、横幅扩展最后（统一展示）。
pub fn panel_entries() -> Vec<(String, bool, Vec<ExtensionMode>)> {
    let mut out: Vec<(String, bool, Vec<ExtensionMode>)> = registry()
        .lock()
        .unwrap()
        .iter()
        .map(|e| (e.ext.name().to_string(), e.enabled, e.ext.modes()))
        .collect();
    out.extend(
        footer_registry()
            .lock()
            .unwrap()
            .iter()
            .map(|e| (e.ext.name().to_string(), e.enabled, e.ext.modes())),
    );
    out.extend(
        banner_registry()
            .lock()
            .unwrap()
            .iter()
            .map(|e| (e.ext.name().to_string(), e.enabled, e.ext.modes())),
    );
    out
}

/// 扩展声明的模式列表（工具扩展优先匹配，其次底栏扩展、横幅扩展；找不到返回空）。
/// 注意 `All` 是兜底：返回列表可能为空，但插件仍隐式属于全部模式。
pub fn extension_modes(name: &str) -> Vec<ExtensionMode> {
    if let Some(e) = registry()
        .lock()
        .unwrap()
        .iter()
        .find(|e| e.ext.name() == name)
    {
        return e.ext.modes();
    }

    if let Some(e) = footer_registry()
        .lock()
        .unwrap()
        .iter()
        .find(|e| e.ext.name() == name)
    {
        return e.ext.modes();
    }

    banner_registry()
        .lock()
        .unwrap()
        .iter()
        .find(|e| e.ext.name() == name)
        .map(|e| e.ext.modes())
        .unwrap_or_default()
}

/// 扩展是否启用（工具扩展优先匹配，其次底栏扩展、横幅扩展）
///
/// 返回的是**持久化开关状态**（与 /extension 面板 checkbox、写盘一致），
/// **不包含**当前模式的可用性；判断“此刻是否真的生效”用 [`is_extension_active`]。
pub fn is_extension_enabled(name: &str) -> bool {
    match registry()
        .lock()
        .unwrap()
        .iter()
        .find(|e| e.ext.name() == name)
    {
        Some(e) => e.enabled,
        _ => footer_registry()
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.ext.name() == name)
            .map(|e| e.enabled)
            .unwrap_or_else(|| {
                banner_registry()
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|e| e.ext.name() == name)
                    .map(|e| e.enabled)
                    .unwrap_or(false)
            }),
    }
}

/// 扩展在当前模式下是否**生效**（已启用且声明了当前模式，见 [`usable_in_mode`]）。
///
/// 与 [`is_extension_enabled`]（纯持久化开关状态，供 /extension 面板与写盘用）不同：
/// 启动期判断“是否该拉起资源 / 发一次启动提示”（如 `on_registered` 里起后台线程）
/// 必须用本函数——否则声明 `[Dev, Creator]` 的扩展在 Minimal 模式下也会被启动。
/// 查找顺序同 [`is_extension_enabled`]：工具扩展 → 底栏 → 横幅。
pub fn is_extension_active(name: &str) -> bool {
    let mode = current_extension_mode();
    let active = |enabled: bool, modes: Vec<ExtensionMode>| enabled && usable_in_mode(&modes, mode);

    if let Some(e) = registry()
        .lock()
        .unwrap()
        .iter()
        .find(|e| e.ext.name() == name)
    {
        return active(e.enabled, e.ext.modes());
    }

    if let Some(e) = footer_registry()
        .lock()
        .unwrap()
        .iter()
        .find(|e| e.ext.name() == name)
    {
        return active(e.enabled, e.ext.modes());
    }

    banner_registry()
        .lock()
        .unwrap()
        .iter()
        .find(|e| e.ext.name() == name)
        .is_some_and(|e| active(e.enabled, e.ext.modes()))
}

/// 切换扩展启用状态（/extension 面板空格）。返回是否找到该扩展。
/// 工具扩展优先匹配，其次底栏扩展、横幅扩展。调用方（/extension 面板 Ctrl+S）负责
/// [`persist_extension_states`] 写盘。
/// footer/banner 扩展互斥：启用一个时自动禁用同组其他扩展，保证组内至多一个生效。
/// 工具扩展的 on_enabled_changed 在锁外调用（回调可能重入 registry 锁，如
/// plan-mode 广播 plan-mode:changed → dispatch_agent_event → registered()）。
pub fn set_extension_enabled(name: &str, enabled: bool) -> bool {
    let target: Option<Arc<dyn Extension>> = {
        let mut reg = registry().lock().unwrap();
        match reg.iter_mut().find(|e| e.ext.name() == name) {
            Some(e) => {
                e.enabled = enabled;
                Some(e.ext.clone())
            }
            _ => None,
        }
    };

    if let Some(ext) = target {
        let effective = effective_enabled(enabled, &ext.modes(), current_extension_mode());
        ext.on_enabled_changed(effective);
        return true;
    }

    // 底栏扩展：仅当目标确实是 footer 时才做同组互斥。
    // 关键：互斥禁用「同组其他」必须在确认目标属于本组之后——
    // 否则切换 banner（或未知名字）会顺带禁用当前启用的 footer，
    // 使 footer 的 enabled 状态被写进 disabledExtensions，重启后底栏丢失。
    {
        let mut reg = footer_registry().lock().unwrap();
        if reg.iter().any(|e| e.ext.name() == name) {
            for e in reg.iter_mut() {
                if e.ext.name() == name {
                    e.enabled = enabled;
                } else if enabled && e.enabled {
                    e.enabled = false;
                }
            }
            return true;
        }
    }

    // 横幅扩展：同组互斥，同上。
    {
        let mut reg = banner_registry().lock().unwrap();
        if reg.iter().any(|e| e.ext.name() == name) {
            for e in reg.iter_mut() {
                if e.ext.name() == name {
                    e.enabled = enabled;
                } else if enabled && e.enabled {
                    e.enabled = false;
                }
            }
            return true;
        }
    }

    false
}

/// 已注册 footer 扩展名列表（含禁用；面板互斥时用于同步刷新 checkbox）
pub fn footer_names() -> Vec<String> {
    footer_registry()
        .lock()
        .unwrap()
        .iter()
        .map(|e| e.ext.name().to_string())
        .collect()
}

/// 已注册 banner 扩展名列表（含禁用；面板互斥时用于同步刷新 checkbox）
pub fn banner_names() -> Vec<String> {
    banner_registry()
        .lock()
        .unwrap()
        .iter()
        .map(|e| e.ext.name().to_string())
        .collect()
}

/// 收集启用/禁用状态写回 settings.json（read-modify-write 保留其他键）。
///
/// - `disabledExtensions`：默认启用（`default_enabled() == true`）但当前被关闭的扩展；
/// - `enabledExtensions`：默认关闭（`default_enabled() == false`）但当前被开启的扩展。
///
/// 默认关闭且当前仍关闭的扩展不写任何列表（= 回到默认态）。footer/banner 扩展
/// 恒为默认启用，只会进 `disabledExtensions`。
pub fn persist_extension_states() -> Result<()> {
    let mut disabled: Vec<String> = Vec::new();
    let mut enabled: Vec<String> = Vec::new();

    let tools: Vec<(String, bool, bool)> = registry()
        .lock()
        .unwrap()
        .iter()
        .map(|e| (e.ext.name().to_string(), e.enabled, e.ext.default_enabled()))
        .collect();

    for (name, is_enabled, default_enabled) in tools {
        if is_enabled {
            if !default_enabled {
                enabled.push(name);
            }
        } else if default_enabled {
            disabled.push(name);
        }
    }

    disabled.extend(
        footer_registry()
            .lock()
            .unwrap()
            .iter()
            .filter(|e| !e.enabled)
            .map(|e| e.ext.name().to_string()),
    );
    disabled.extend(
        banner_registry()
            .lock()
            .unwrap()
            .iter()
            .filter(|e| !e.enabled)
            .map(|e| e.ext.name().to_string()),
    );
    disabled.sort();
    disabled.dedup();
    enabled.sort();
    enabled.dedup();
    settings_manager::write_extension_states(&disabled, &enabled)
}

/// 扩展详情行（/extension 二级菜单展示）：描述 + 空行 + fork 来源 + 工具/hooks 摘要 + 类别。
/// 描述后固定跟一行空行，与其余信息分隔。
/// 找不到返回空（面板不会展示空详情）。
pub fn extension_detail_lines(name: &str) -> Vec<String> {
    // 先在锁内取出扩展（Arc 克隆），**释放注册表锁后**再调用扩展方法：
    // 扩展回调可能重入注册表（`tool-search` 的 `tools()` 会调 [`registered`]），
    // 而 `registry()` 是不可重入的 `Mutex`，持锁回调会直接死锁。
    let found: Option<Arc<dyn Extension>> = registry()
        .lock()
        .unwrap()
        .iter()
        .find(|e| e.ext.name() == name)
        .map(|e| e.ext.clone());

    if let Some(ext) = found {
        let mut lines = Vec::new();
        let desc = ext.description();
        lines.push(if desc.is_empty() {
            "  (No Description)".to_string()
        } else {
            format!("  {}", desc)
        });
        lines.push(String::new());

        let modes: Vec<&str> = ext.modes().iter().map(|m| m.as_str()).collect();
        lines.push(if modes.is_empty() {
            "  mode: all".to_string()
        } else {
            format!("  mode: {}", modes.join(", "))
        });

        // 工具名带上 namespace，并标注非 direct 的暴露方式与已声明的注解
        let tools: Vec<String> = ext
            .tools()
            .iter()
            .map(|t| {
                let mut marks: Vec<String> = Vec::new();
                if t.exposure != ToolExposure::Direct {
                    marks.push(t.exposure.as_str().to_string());
                }

                marks.extend(t.annotations.labels().iter().map(|s| s.to_string()));
                let name = match &t.namespace {
                    Some(ns) => format!("{ns}/{}", t.name),
                    None => t.name.clone(),
                };

                if marks.is_empty() {
                    name
                } else {
                    format!("{} ({})", name, marks.join(", "))
                }
            })
            .collect();

        if !tools.is_empty() {
            lines.push(format!("  tools: {}", tools.join(", ")));
        }

        let hooks: Vec<String> = ext.hooks().iter().map(|h| format!("{:?}", h)).collect();
        if !hooks.is_empty() {
            lines.push(format!("  hooks: {}", hooks.join(", ")));
        }

        let commands: Vec<String> = ext.commands().iter().map(|c| c.name.clone()).collect();
        if !commands.is_empty() {
            lines.push(format!("  commands: {}", commands.join(", ")));
        }
        lines.push(String::new());

        if let Some(fork) = ext.fork_project() {
            lines.push(format!(
                "  forked from: {} v{}",
                fork.plugin_name, fork.plugin_version
            ));

            if !fork.url.is_empty() {
                lines.push(format!("  fork url: {}", fork.url));
            }
        }

        return lines;
    }

    // 底栏 / 横幅扩展：同样先取出 Arc，再在锁外调用扩展方法。
    let footer: Option<Arc<dyn FooterExtension>> = footer_registry()
        .lock()
        .unwrap()
        .iter()
        .find(|e| e.ext.name() == name)
        .map(|e| e.ext.clone());

    if let Some(ext) = footer {
        let mut lines = Vec::new();
        let desc = ext.description();
        lines.push(if desc.is_empty() {
            "  (No Description)".to_string()
        } else {
            format!("  {}", desc)
        });
        lines.push(String::new());
        let modes: Vec<&str> = ext.modes().iter().map(|m| m.as_str()).collect();
        lines.push(if modes.is_empty() {
            "  mode: all".to_string()
        } else {
            format!("  mode: {}", modes.join(", "))
        });
        lines.push(
            "  type: footer extension (renders TUI footer bar, receives agent events)".to_string(),
        );
        return lines;
    }

    let banner: Option<Arc<dyn BannerExtension>> = banner_registry()
        .lock()
        .unwrap()
        .iter()
        .find(|e| e.ext.name() == name)
        .map(|e| e.ext.clone());

    if let Some(ext) = banner {
        let mut lines = Vec::new();
        let desc = ext.description();
        lines.push(if desc.is_empty() {
            "  (No Description)".to_string()
        } else {
            format!("  {}", desc)
        });
        lines.push(String::new());
        let modes: Vec<&str> = ext.modes().iter().map(|m| m.as_str()).collect();
        lines.push(if modes.is_empty() {
            "  mode: all".to_string()
        } else {
            format!("  mode: {}", modes.join(", "))
        });
        lines.push(
            "  type: banner extension (renders startup banner at the top of the TUI)".to_string(),
        );
        return lines;
    }

    Vec::new()
}
