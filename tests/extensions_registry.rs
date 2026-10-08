//! 分布式切片自注册回归测试：验证 `linkme` 切片在最终产物中正确汇聚、
//! `register_all` 按 priority 降序注册、footer 互斥的默认启用者。

use prux::core::extensions::{
    extension_count, footer_extension_count, footer_names, is_extension_enabled, panel_entries,
    registered, registered_footers, unregister_extension,
};
use prux::core::settings_manager;
use prux::test_support::AgentDirGuard;
use std::sync::{Mutex, MutexGuard, OnceLock};

// 全局扩展注册表是“被测对象本身”（linkme 切片汇聚），跨测试共享且互相污染：
// 这就是一把**必须**存在的序列化锁（单把叶级、无嵌套 → 无锁序死锁）。
static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn guard() -> MutexGuard<'static, ()> {
    TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn setup_agent_dir() {
    // 每测试独立临时 agent_dir（线程本地 override），不触碰真实用户配置
    settings_manager::write_disabled_extensions(&[]).ok();
}

#[test]
fn register_all_collects_and_sorts_slices() {
    let _l = guard();
    let _ad = AgentDirGuard::temp();
    setup_agent_dir();

    // 调两次注册：全局表跨测试累积，先清空已知内置再注册，保证断言计数准确
    for n in [
        "plan-mode",
        "goal",
        "subagent",
        "tasks",
        "loop-detect",
        "hang-detect",
        "notify",
        "mcp",
        "web-access",
        "proxy",
        "rewind",
        "skills",
        "extra-themes",
        "downloader",
        "doc-helper",
        "debug-provider",
        "tool-search",
        "codemode",
        "jet",
        "update-check",
        "footer(normal)",
        "footer(minimal)",
        "footer(rich)",
    ] {
        unregister_extension(n);
    }

    prux::extensions::register_all();

    // 分布式切片确实汇聚：20 个工具扩展 + 3 个底栏扩展
    assert_eq!(extension_count(), 20, "普通扩展应全被切片收集");
    assert_eq!(footer_extension_count(), 3, "footer 扩展应全被切片收集");

    // plan-mode / goal / subagent / tasks / web-access / proxy / doc-helper / notify 是声明 default_enabled() == false 的默认可选扩展：
    // 注册后在注册表中但为禁用态，不进入 registered()（工具/命令/flag 均不生效）
    assert!(!is_extension_enabled("plan-mode"), "plan-mode 应默认不启用");
    assert!(!is_extension_enabled("goal"), "goal 应默认不启用");
    assert!(!is_extension_enabled("subagent"), "subagent 应默认不启用");
    assert!(!is_extension_enabled("tasks"), "tasks 应默认不启用");
    assert!(
        !is_extension_enabled("web-access"),
        "web-access 应默认不启用"
    );
    assert!(
        !is_extension_enabled("proxy"),
        "proxy 应默认不启用（占端口 + 暴露凭据）"
    );
    assert!(
        !is_extension_enabled("rewind"),
        "rewind 应默认不启用（回退会改变模型上下文）"
    );
    assert!(
        !is_extension_enabled("doc-helper"),
        "doc-helper 应默认不启用"
    );
    assert!(
        !is_extension_enabled("notify"),
        "notify 应默认不启用（外部命令需用户主动配置）"
    );
    assert!(
        !is_extension_enabled("hang-detect"),
        "hang-detect 应默认不启用（会自动中断并重开回合）"
    );
    assert!(
        !is_extension_enabled("mcp"),
        "mcp 应默认不启用（会向系统提示注入 MCP 工具）"
    );
    assert!(
        !is_extension_enabled("jet"),
        "jet 应默认不启用（需要用户先配路由目标与分类器）"
    );
    // downloader 默认启用（工具可用性属于开箱能力）：`/download` 无需先开扩展即可用；
    // 其 dock 段默认隐藏，靠 `/download` 显示
    assert!(
        is_extension_enabled("downloader"),
        "downloader 应默认启用（提供 /download 命令）"
    );
    // update-check 默认启用（版本提示属于开箱能力）
    assert!(
        is_extension_enabled("update-check"),
        "update-check 应默认启用（启动时检查新版本）"
    );

    // 显式启用后（/extension 面板或 settings.json enabledExtensions）参与分发。
    // 注意：loop-detect 声明 default_enabled() == false（与 plan-mode/goal 一致），
    // 因此这里也需显式启用才能进入 registered()。
    prux::core::extensions::set_extension_enabled("plan-mode", true);
    prux::core::extensions::set_extension_enabled("goal", true);
    prux::core::extensions::set_extension_enabled("subagent", true);
    prux::core::extensions::set_extension_enabled("tasks", true);
    prux::core::extensions::set_extension_enabled("loop-detect", true);
    prux::core::extensions::set_extension_enabled("hang-detect", true);
    prux::core::extensions::set_extension_enabled("notify", true);
    prux::core::extensions::set_extension_enabled("mcp", true);
    // skills 也声明 default_enabled() == false（需要显式开启才接管技能面板）
    prux::core::extensions::set_extension_enabled("skills", true);
    prux::core::extensions::set_extension_enabled("web-access", true);
    prux::core::extensions::set_extension_enabled("proxy", true);
    prux::core::extensions::set_extension_enabled("rewind", true);
    prux::core::extensions::set_extension_enabled("doc-helper", true);
    prux::core::extensions::set_extension_enabled("downloader", true);
    prux::core::extensions::set_extension_enabled("update-check", true);
    prux::core::extensions::set_extension_enabled("debug-provider", true);
    prux::core::extensions::set_extension_enabled("tool-search", true);
    prux::core::extensions::set_extension_enabled("jet", true);

    // 普通扩展全部注册且按 priority 降序
    // （extra-themes:100 → goal:90 → plan-mode:85 → tasks:80 → subagent:78 → loop-detect:75
    //   → notify:74 → hang-detect:73 → proxy:72 → rewind:71 → web-access:70 → mcp:69
    //   → skills:65 → downloader:60 → update-check:55 → doc-helper:50 → debug-provider:45
    //   → tool-search:40 → jet:38）
    let names: Vec<String> = registered().iter().map(|e| e.name().to_string()).collect();
    assert_eq!(
        names,
        vec![
            "extra-themes".to_string(),
            "goal".to_string(),
            "plan-mode".to_string(),
            "tasks".to_string(),
            "subagent".to_string(),
            "loop-detect".to_string(),
            "notify".to_string(),
            "hang-detect".to_string(),
            "proxy".to_string(),
            "rewind".to_string(),
            "web-access".to_string(),
            "mcp".to_string(),
            "skills".to_string(),
            "downloader".to_string(),
            "update-check".to_string(),
            "doc-helper".to_string(),
            "debug-provider".to_string(),
            "tool-search".to_string(),
            "jet".to_string()
        ],
        "注册顺序应按 priority 降序: {:?}",
        names
    );

    // footer 按 priority 降序注册（rich:20 → minimal:15 → normal:10），
    // 互斥"新注册启用者禁用已启用 footer"→ 最后注册的 footer(normal) 生效
    let fnames: Vec<String> = footer_names();
    assert_eq!(
        fnames,
        vec![
            "footer(rich)".to_string(),
            "footer(minimal)".to_string(),
            "footer(normal)".to_string()
        ],
        "footer 注册顺序: {:?}",
        fnames
    );
    assert!(!is_extension_enabled("footer(rich)"));
    assert!(!is_extension_enabled("footer(minimal)"));
    assert!(
        is_extension_enabled("footer(normal)"),
        "最后注册的 footer 应默认启用"
    );
    // 渲染快照：互斥后只剩最后注册者 footer(normal)
    let rendered: Vec<String> = registered_footers()
        .iter()
        .map(|f| f.name().to_string())
        .collect();
    assert_eq!(
        rendered,
        vec!["footer(normal)".to_string()],
        "底栏互斥应只剩最后注册者: {:?}",
        rendered
    );

    // 面板条目含全部（工具扩展在前、底栏在后）
    let entries: Vec<String> = panel_entries().iter().map(|(n, _, _)| n.clone()).collect();
    assert!(entries.contains(&"plan-mode".to_string()));
    assert!(entries.contains(&"footer(rich)".to_string()));
}

/// `--no-extensions`：立即禁用**已注册**的扩展（含刚才默认启用的内置扩展），
/// 且此后注册的扩展也保持禁用；运行中经 /extension 面板显式开启仍可生效。
///
/// 刻意**不**调用 `register_all()`：全局底栏注册表无法注销，重复注册会让
/// 同一文件里断言 footer 计数的测试互相污染（两测试共享注册表）。
#[test]
fn no_extensions_disables_registered_extensions() {
    use prux::core::extensions::{
        Extension, ExtensionTool, register_extension, reload_from_settings,
        set_all_extensions_disabled,
    };

    let _l = guard();
    let _ad = AgentDirGuard::temp();
    setup_agent_dir();

    struct ProbeExt;
    impl Extension for ProbeExt {
        fn name(&self) -> &str {
            "no-extensions-probe"
        }
        fn tools(&self) -> Vec<ExtensionTool> {
            Vec::new()
        }
    }

    register_extension(ProbeExt);
    assert!(is_extension_enabled("no-extensions-probe"), "默认应启用");

    set_all_extensions_disabled(true);
    assert!(
        !is_extension_enabled("no-extensions-probe"),
        "--no-extensions 应立即禁用已注册扩展"
    );
    assert!(
        !registered()
            .iter()
            .any(|e| e.name() == "no-extensions-probe"),
        "被禁用的扩展不进入 registered()（工具/命令/flag 均不生效）"
    );

    // 面板显式开启仍然生效（对齐 pi `-e builtin:<name>` 的恢复语义）
    prux::core::extensions::set_extension_enabled("no-extensions-probe", true);
    assert!(is_extension_enabled("no-extensions-probe"));
    prux::core::extensions::set_extension_enabled("no-extensions-probe", false);

    // 复位，避免影响同文件其它测试
    set_all_extensions_disabled(false);
    reload_from_settings();
    assert!(unregister_extension("no-extensions-probe"));
}
