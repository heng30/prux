//! settings.json 持久化回归：
//! 1) 持久化的 `disabledExtensions` 决定 footer 选择（重启后不恢复默认值）；
//! 2) 多个 prux 进程并发写 settings.json 时，互不覆盖对方的键（跨进程写锁）。
//!
//! 背景：设置写盘是「读整个文件 → 改一个键 → 覆盖写回」。此前只在进程内串行，
//! 两个进程并发写不同键时，后写者会用自己读到的旧快照覆盖前写者刚写的键
//! （read-modify-write 丢失更新），表现为 `disabledExtensions` 被静默改回旧值、
//! 底栏 footer 恢复默认值。

use prux::core::settings_manager;

/// 子进程写入模式的环境变量（父测试经 current_exe 重新执行自身时为子进程）。
const CHILD_MODE: &str = "PRUX_TEST_SETTINGS_WRITER";

const SEED: &str = r#"{
  "autoCompact": true,
  "autocompleteMaxVisible": 0,
  "defaultProvider": "opencode-go",
  "disabledExtensions": ["footer(minimal)", "footer(normal)"],
  "historyMaxEntries": 0,
  "theme": "synthwave"
}"#;

/// 持久化的 disabledExtensions 必须让 footer(rich) 生效（而不是回落到默认的 normal）。
///
/// 设置写盘/读取只要有一处把 disabledExtensions 丢掉，注册期 `register_footer_extension`
/// 就会把三个 footer 都当启用，互斥后最后一个（footer(normal)）生效 = 「恢复默认值」。
#[test]
fn persisted_disabled_extensions_select_rich_footer() {
    use prux::core::extensions::{is_extension_enabled, registered_footers};
    use prux::test_support::AgentDirGuard;

    let _ad = AgentDirGuard::temp();
    let dir = settings_manager::agent_dir();
    std::fs::write(dir.join("settings.json"), SEED).unwrap();

    prux::extensions::register_all();

    assert!(
        is_extension_enabled("footer(rich)"),
        "rich 应被持久化状态启用"
    );
    assert!(!is_extension_enabled("footer(minimal)"));
    assert!(
        !is_extension_enabled("footer(normal)"),
        "normal 不应回落到默认"
    );

    let rendered: Vec<String> = registered_footers()
        .iter()
        .map(|f| f.name().to_string())
        .collect();
    assert_eq!(
        rendered,
        vec!["footer(rich)"],
        "底栏应是 rich 而非默认 normal"
    );
}

/// 子进程写循环：`a` 单调写 autocompleteMaxVisible，`b` 单调写 historyMaxEntries。
/// 两个键互不相同——任一丢失更新都会让最终值 < N。
fn child_write_loop(which: &str, n: usize) {
    for i in 0..n {
        match which {
            "a" => settings_manager::write_settings_autocomplete_max_visible(i + 1).unwrap(),
            "b" => settings_manager::write_settings_history_max_entries(i + 1).unwrap(),
            _ => {}
        }
    }
}

#[test]
fn concurrent_settings_writes_do_not_lose_keys() {
    let n: usize = 2500;

    // 子进程分支：由父测试以 `--exact` 重新执行本测试二进制，只跑写循环后退出。
    if let Ok(which) = std::env::var(CHILD_MODE) {
        child_write_loop(&which, n);
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("settings.json"), SEED).unwrap();

    let exe = std::env::current_exe().unwrap();
    let spawn = |which: &str| {
        std::process::Command::new(&exe)
            .args([
                "--exact",
                "concurrent_settings_writes_do_not_lose_keys",
                "--nocapture",
            ])
            .env("PRUX_AGENT_DIR", dir.path())
            .env(CHILD_MODE, which)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn writer child")
    };

    let a = spawn("a");
    let b = spawn("b");
    for child in [a, b] {
        let out = child.wait_with_output().expect("wait writer child");
        assert!(
            out.status.success(),
            "writer child failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // 有跨进程写锁时：最后写者读到的是对方的最新值，两个键都应到 N。
    // 无锁时后写者用陈旧快照覆盖，会出现 < N（丢更新）。
    let text = std::fs::read_to_string(dir.path().join("settings.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        v.get("autocompleteMaxVisible").and_then(|v| v.as_u64()),
        Some(n as u64),
        "autocompleteMaxVisible 丢了更新: {text}"
    );
    assert_eq!(
        v.get("historyMaxEntries").and_then(|v| v.as_u64()),
        Some(n as u64),
        "historyMaxEntries 丢了更新: {text}"
    );
    // 未被并发修改的键也必须保留。
    assert_eq!(v.get("theme").and_then(|v| v.as_str()), Some("synthwave"));
    assert_eq!(
        v.get("disabledExtensions"),
        Some(&serde_json::json!(["footer(minimal)", "footer(normal)"])),
        "并发写不应影响 disabledExtensions: {text}"
    );
}
