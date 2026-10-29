//! `GuiApp` 及其纯函数的单元测试（不需要窗口）。

use super::*;

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
