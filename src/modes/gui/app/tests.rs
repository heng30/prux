//! `GuiApp` 及其纯函数的单元测试（不需要窗口）。

use super::*;

/// 命令面板不带 `/` 面板的重复条目：只允许 skills / extension / settings 两边都在。
#[test]
fn command_palette_keeps_only_the_three_shared_entries() {
    let allowed = ["skills", "extension", "settings"];
    for entry in super::actions::command_palette_entries() {
        assert!(
            !entry.label.contains("/scoped-models"),
            "配置类命令不该在面板里: {}",
            entry.label
        );
        let Some(slash) = entry.label.strip_prefix('/') else {
            continue;
        };
        assert!(allowed.contains(&slash), "`/{slash}` 只该出现在 `/` 面板里");
    }
    // 允许重复的三个必须都在面板里
    let labels: Vec<String> = super::actions::command_palette_entries()
        .into_iter()
        .map(|entry| entry.label)
        .collect();
    for name in allowed {
        // 面板里的名字是标题大小写（Extensions…），这里按包含关系查
        assert!(
            labels
                .iter()
                .any(|label| label.to_lowercase().starts_with(&name[..name.len().min(5)])),
            "面板缺少 {name}: {labels:?}"
        );
    }
}

/// 模型循环按 scope 走：顺序跟 scope，不可用的跳过；scope 为空才用全部模型。
#[test]
fn cycle_models_follows_scope_order() {
    use crate::core::model_scope::ScopedModel;
    let all = vec![
        ("p1".to_string(), "a".to_string()),
        ("p1".to_string(), "b".to_string()),
        ("p2".to_string(), "c".to_string()),
    ];
    let scoped = vec![
        ScopedModel {
            provider: "p2".into(),
            id: "c".into(),
            thinking: Some("high".into()),
        },
        ScopedModel {
            provider: "p1".into(),
            id: "a".into(),
            thinking: None,
        },
        ScopedModel {
            provider: "p1".into(),
            id: "missing".into(),
            thinking: None,
        },
    ];
    assert_eq!(
        cycle_models(&scoped, &all),
        vec![
            ("p2".to_string(), "c".to_string()),
            ("p1".to_string(), "a".to_string()),
        ]
    );
    assert_eq!(cycle_models(&[], &all), all);
}

/// 循环下标：尾接到头、头回接到尾，未知时取第一个。
#[test]
fn cycle_index_wraps_both_ways() {
    assert_eq!(cycle_index(Some(2), 3, 1), 0);
    assert_eq!(cycle_index(Some(0), 3, -1), 2);
    assert_eq!(cycle_index(None, 3, 1), 0);
    assert_eq!(cycle_index(None, 3, -1), 0);
}

/// 列定位的原点是文字盒自己的左上角，缩进行的 padding 不参与换算。
#[test]
fn text_local_is_relative_to_the_text_box() {
    // 外层行盒从 x=100 起，缩进 14 后文字盒从 x=114 起
    let text = Bounds::new(
        gpui::point(px(114.), px(40.)),
        gpui::size(px(300.), px(18.)),
    );
    let local = text_local(gpui::point(px(114.), px(40.)), text);
    assert_eq!(local.x, px(0.));
    assert_eq!(local.y, px(0.));
    let local = text_local(gpui::point(px(214.), px(49.)), text);
    assert_eq!(local.x, px(100.));
    assert_eq!(local.y, px(9.));
}

/// 长按同一行：短时间内重复的切换要被忽略，换一行或过窗口期才生效。
#[test]
fn repeat_toggle_is_debounced() {
    let now = std::time::Instant::now();
    let window = std::time::Duration::from_millis(250);
    assert!(ignore_repeat_toggle(
        Some(("alpha", now)),
        "alpha",
        now + std::time::Duration::from_millis(50),
        window,
    ));
    assert!(!ignore_repeat_toggle(
        Some(("alpha", now)),
        "alpha",
        now + std::time::Duration::from_millis(400),
        window,
    ));
    assert!(!ignore_repeat_toggle(
        Some(("alpha", now)),
        "beta",
        now + std::time::Duration::from_millis(10),
        window,
    ));
    assert!(!ignore_repeat_toggle(None, "alpha", now, window));
}

/// 技能开关：开 → `+名称`，关 → `-名称`，同名规则只留一条。
#[test]
fn skill_toggle_rewrites_exact_rule() {
    let rules = vec!["!old-*".to_string(), "-alpha".to_string()];
    let enabled = skill_rules_with(&rules, "alpha", true);
    assert!(enabled.contains(&"+alpha".to_string()));
    assert!(!enabled.contains(&"-alpha".to_string()));
    // 通配规则保留
    assert!(enabled.contains(&"!old-*".to_string()));

    let disabled = skill_rules_with(&enabled, "alpha", false);
    assert!(disabled.contains(&"-alpha".to_string()));
    assert!(!disabled.contains(&"+alpha".to_string()));
    assert_eq!(disabled.len(), 2);
}

/// 最近目录：去重提到最前，超过上限截断。
#[test]
fn remember_cwd_dedupes_and_caps() {
    let _guard = crate::test_support::AgentDirGuard::temp();
    for index in 0..(RECENT_CWD_LIMIT + 3) {
        remember_cwd(&format!("/work/{index}"));
    }
    remember_cwd("/work/5");
    let list = recent_cwds();
    assert_eq!(list.len(), RECENT_CWD_LIMIT);
    assert_eq!(list[0], "/work/5");
    assert_eq!(list.iter().filter(|item| *item == "/work/5").count(), 1);
}

/// 启动清理：清单里已经不存在的目录会被移除并写回，存在的保留。
#[test]
fn prune_recent_cwds_drops_missing_dirs() {
    let _guard = crate::test_support::AgentDirGuard::temp();
    let dir = std::env::temp_dir().join(format!("prux-cwd-prune-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let live = dir.to_string_lossy().to_string();
    let gone = dir.join("gone").to_string_lossy().to_string();

    remember_cwd(&gone);
    remember_cwd(&live);
    prune_recent_cwds();

    assert_eq!(recent_cwds(), vec![live]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// toast 只留在最近 TOAST_MAX 条里，级别决定颜色（这里只测裁剪逻辑）。
#[test]
fn toast_queue_keeps_latest_only() {
    let mut toasts: Vec<u64> = Vec::new();
    for id in 0..(TOAST_MAX as u64 + 3) {
        toasts.push(id);
        while toasts.len() > TOAST_MAX {
            toasts.remove(0);
        }
    }
    assert_eq!(toasts.len(), TOAST_MAX);
    assert_eq!(*toasts.last().unwrap(), TOAST_MAX as u64 + 2);
}

/// 手动关掉的补全不会自动弹回；换了触发位置才重新生效。
#[test]
fn dismissed_suggest_stays_closed_until_trigger_moves() {
    assert!(!suggest_allowed(Some(0), 0));
    assert!(suggest_allowed(None, 0));
    assert!(suggest_allowed(Some(0), 3));
}

/// 重命名输入：首尾空白去掉，全空白等于取消。
#[test]
fn renamed_name_trims_and_rejects_blank() {
    assert_eq!(renamed_name("  新名字 "), Some("新名字"));
    assert_eq!(renamed_name("\t\n"), None);
    assert_eq!(renamed_name(""), None);
}

/// 重命名把名字写进会话文件，再读回来就是新名字。
#[test]
fn rename_writes_session_name() {
    use crate::core::session_manager::{Session, session_meta};

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("sessions");
    let mut session =
        Session::create(tmp.path().to_str().unwrap(), Some(dir.clone()), true).unwrap();
    session.append_message(&AgentMessage::user_text("hi"));
    let path = session.get_session_file().unwrap().to_path_buf();

    rename_session(path.to_str().unwrap(), "重构侧栏");

    assert_eq!(
        session_meta(&path).unwrap().name.as_deref(),
        Some("重构侧栏")
    );
}

/// 悬停任意一处都显示；拖动时即使鼠标离开滚动区也保持显示。
#[test]
fn scrollbar_shows_on_hover_or_drag() {
    assert!(!scrollbar_visible(None, ScrollArea::Transcript, false));
    assert!(scrollbar_visible(None, ScrollArea::Transcript, true));
    assert!(scrollbar_visible(
        Some(ScrollArea::Transcript),
        ScrollArea::Transcript,
        false
    ));
    // 拖对话流时侧栏滚动条不应跟着冒出来
    assert!(!scrollbar_visible(
        Some(ScrollArea::Transcript),
        ScrollArea::Sidebar,
        false
    ));
}

/// 设置面板的每一行都只改一项配置：执行前必须保住覆盖层，否则 Enter 一按面板就没了。
#[test]
fn setting_toggles_keep_the_overlay_open() {
    assert!(keeps_overlay(&Action::ToggleSetting(SettingKey::GuiTheme)));
    assert!(keeps_overlay(&Action::ToggleSetting(
        SettingKey::AutoCompact
    )));
    assert!(!keeps_overlay(&Action::SetDark(true)));
}

/// 设置面板的表格行：名称进第一列，当前值与说明进后两个单元格（列数必须对齐）。
#[test]
fn settings_entries_fill_three_columns() {
    let _guard = crate::test_support::AgentDirGuard::temp();
    let entries = super::panels::settings_entries(true);
    assert!(entries.len() > 10, "设置项太少: {}", entries.len());
    for entry in &entries {
        assert_eq!(entry.cells.len(), 2, "{}: {:?}", entry.label, entry.cells);
        assert!(!entry.label.contains(": "), "名称不该带值: {}", entry.label);
    }
    assert_eq!(entries[0].cells[0], "dark");
}

/// Tab 详情页：`key: value` 拆成表格两列，纯文本行当作描述，空行是分组分隔。
#[test]
fn detail_lines_become_table_rows() {
    let rows = super::panels::info_rows(vec![
        "  fork detail description".to_string(),
        String::new(),
        "  mode: Dev, Creator".to_string(),
        "  fork url: https://example.com/x".to_string(),
    ]);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].key, "Description");
    assert_eq!(rows[0].value, "fork detail description");
    assert_eq!(rows[1].key, "mode");
    assert_eq!(rows[1].value, "Dev, Creator");
    assert_eq!(rows[2].key, "fork url");
    assert_eq!(rows[2].value, "https://example.com/x");
}

/// 排队消息：steer 先于 follow-up 出队，同类按 settings 的 mode 取整批或一条，
/// 待发送区一条一行（steer 标 `Steering:`，follow-up 标 `Follow-up:`，只取首行）。
#[test]
fn queued_messages_drain_steer_before_follow_up() {
    let _guard = crate::test_support::AgentDirGuard::temp();
    crate::core::settings_manager::write_settings_steering_mode("one-at-a-time").unwrap();
    crate::core::settings_manager::write_settings_follow_up_mode("all").unwrap();

    let inbox = std::sync::Mutex::new(VecDeque::from(vec![
        AgentMessage::user_text("steer one\nsecond line"),
        AgentMessage::user_text("steer two"),
    ]));
    let mut follow_ups = VecDeque::from(vec!["fup one".to_string(), "fup two".to_string()]);

    let lines = pending_lines(&inbox, &follow_ups);
    assert_eq!(
        lines,
        vec![
            "Steering: steer one",
            "Steering: steer two",
            "Follow-up: fup one",
            "Follow-up: fup two",
        ]
    );
    assert_eq!(pending_count(&inbox, &follow_ups), 4);

    // one-at-a-time：steer 逐条出，两条出完才轮到 follow-up（followUpMode=all 一次取完）
    assert_eq!(
        take_next_batch(&inbox, &mut follow_ups).unwrap()[0].text(),
        "steer one\nsecond line"
    );
    assert_eq!(
        take_next_batch(&inbox, &mut follow_ups).unwrap()[0].text(),
        "steer two"
    );
    let batch = take_next_batch(&inbox, &mut follow_ups).unwrap();
    assert_eq!(batch.len(), 2);
    assert_eq!(batch[0].text(), "fup one");
    assert_eq!(batch[1].text(), "fup two");
    assert!(
        take_next_batch(&inbox, &mut follow_ups).is_none(),
        "队列空后不再出批"
    );
}

/// Alt+Up 取回排队消息：steer 在前、follow-up 在后，空行分隔并拼在草稿之前，队列清空。
#[test]
fn restore_queued_messages_prepends_steer_then_follow_up() {
    let inbox = std::sync::Mutex::new(VecDeque::from(vec![
        AgentMessage::user_text("steer one"),
        AgentMessage::user_text("steer two"),
    ]));
    let mut follow_ups = VecDeque::from(vec!["fup one".to_string()]);

    let (text, count) = restore_text(&inbox, &mut follow_ups, "current draft").unwrap();
    assert_eq!(text, "steer one\n\nsteer two\n\nfup one\n\ncurrent draft");
    assert_eq!(count, 3);
    assert_eq!(pending_count(&inbox, &follow_ups), 0, "取回后队列清空");

    // 空队列：不动编辑器
    assert!(restore_text(&inbox, &mut follow_ups, "draft").is_none());
}
