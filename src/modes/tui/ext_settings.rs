//! 扩展设置面板：占据输入框区域的设置视图（**一次只展示一个扩展**，对齐 `/settings`）。
//!
//! 由扩展自己的命令（如 `/loop-detect`）或
//! [`crate::core::extensions::ExtensionUiRequest::ShowSettings`] 指定扩展名打开：
//! 显示期间隐藏普通输入框并**接管键盘**（type to search、↑/↓ 移动、
//! Space/Enter 在预定义选项里循环、Esc/Ctrl+C 隐藏）。
//!
//! 设置项来自该扩展的 [`crate::core::extensions::Extension::settings`]；
//! 应用经 [`crate::core::extensions::apply_extension_setting`]。

use super::line_input::InputBox;
use crate::{
    core::extensions::{ExtensionSetting, apply_extension_setting, settings_for},
    utils::fuzzy::fuzzy_filter,
};

/// 面板最多同时展示的设置行数（超出窗口滚动）
pub const MAX_VISIBLE: usize = 10;

/// 扩展设置面板状态（单扩展）
#[derive(Debug, Default)]
pub struct ExtSettingsPanel {
    /// 面板已打开：渲染可见、接管键盘、行首显示 `→`
    pub visible: bool,
    /// 当前展示的扩展名（空 = 未指定）
    pub ext: String,
    /// 该扩展声明的设置项（顺序固定）
    pub items: Vec<ExtensionSetting>,
    /// 搜索输入框（type to search）
    pub filter: InputBox,
    /// 过滤后的 items 下标
    pub filtered: Vec<usize>,
    /// 当前高亮项在 `filtered` 中的下标（越界时收敛到末项）
    pub selected: usize,
    /// 列表滚动偏移（首项在 `filtered` 中的下标）
    pub offset: usize,
    /// 最近一次应用失败的原因（渲染在提示行）
    pub error: Option<String>,
}

impl ExtSettingsPanel {
    /// 构造空面板（默认隐藏、无扩展、无过滤）
    pub fn new() -> Self {
        Self::default()
    }

    /// 打开面板并拉取该扩展的设置项（清空搜索、回到首项）。
    pub fn open(&mut self, ext: impl Into<String>) {
        self.ext = ext.into();
        self.filter.clear();
        self.selected = 0;
        self.offset = 0;
        self.error = None;
        self.rebuild();
        self.visible = true;
    }

    /// 关闭面板（隐藏 + 释放键盘）。
    pub fn close(&mut self) {
        self.visible = false;
        self.ext.clear();
        self.items.clear();
        self.filter.clear();
        self.filtered.clear();
        self.selected = 0;
        self.offset = 0;
        self.error = None;
    }

    /// 是否已打开（可见）
    pub fn is_open(&self) -> bool {
        self.visible
    }

    /// 是否正展示某个扩展
    pub fn is_open_for(&self, ext: &str) -> bool {
        self.visible && self.ext == ext
    }

    /// 重新拉取该扩展的设置并重算过滤（打开时与每次应用后调用）。
    pub fn rebuild(&mut self) {
        self.items = settings_for(&self.ext);
        self.recompute();
        self.clamp_offset();
    }

    /// 按搜索框内容重算过滤列表
    pub fn recompute(&mut self) {
        let query = self.filter.value.clone();
        let matched = fuzzy_filter(&self.items, &query, |i| i.label.as_str());
        self.filtered = matched
            .into_iter()
            .map(|item| {
                self.items
                    .iter()
                    .position(|i| std::ptr::eq(i, item))
                    .unwrap_or(0)
            })
            .collect();
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    /// 当前选中项
    pub fn current(&self) -> Option<&ExtensionSetting> {
        let idx = *self.filtered.get(self.selected)?;
        self.items.get(idx)
    }

    /// 移动选中（在过滤结果内循环）。
    pub fn move_selection(&mut self, delta: i32) {
        let n = self.filtered.len() as i32;
        if n == 0 {
            return;
        }
        self.selected = (self.selected as i32 + delta).rem_euclid(n) as usize;
        self.clamp_offset();
    }

    /// Space/Enter：把当前项循环到下一个选项并应用。
    /// 返回是否发生变更。
    pub fn cycle_selected(&mut self) -> bool {
        let Some(item) = self.current() else {
            return false;
        };
        let Some(next) = item.next_value().map(str::to_string) else {
            return false; // 只读项
        };
        let key = item.key.clone();

        if let Err(err) = apply_extension_setting(&self.ext, &key, &next) {
            self.error = Some(err);
            return false;
        }
        self.error = None;
        // 以扩展回读的值为准重建（复合设置可能联动多个内部键）
        self.rebuild();
        true
    }

    /// 把 `offset` 约束到让选中项可见
    fn clamp_offset(&mut self) {
        let total = self.filtered.len();
        if total <= MAX_VISIBLE {
            self.offset = 0;
            return;
        }
        self.offset = self.offset.min(total - MAX_VISIBLE);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + MAX_VISIBLE {
            self.offset = self.selected + 1 - MAX_VISIBLE;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::arc_with_non_send_sync)]
    use super::*;
    use crate::core::extensions::{Extension, ExtensionTool};
    use std::sync::Mutex;

    static VALUE: Mutex<Option<String>> = Mutex::new(None);

    struct FakeExt;

    impl Extension for FakeExt {
        fn name(&self) -> &str {
            "fake-settings"
        }
        fn tools(&self) -> Vec<ExtensionTool> {
            Vec::new()
        }
        fn settings(&self) -> Vec<ExtensionSetting> {
            let value = VALUE
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "b".to_string());
            vec![
                ExtensionSetting::cycle("mode", "Mode", "cycle a/b/c", value, ["a", "b", "c"]),
                ExtensionSetting::cycle("other", "Other knob", "unrelated", "x", ["x", "y"]),
            ]
        }
        fn apply_setting(&self, key: &str, value: &str) -> Result<(), String> {
            if key != "mode" {
                return Err(format!("unknown setting: {key}"));
            }
            *VALUE.lock().unwrap() = Some(value.to_string());
            Ok(())
        }
    }

    fn with_fake<T>(f: impl FnOnce() -> T) -> T {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::core::extensions::register_extension(FakeExt);
        *VALUE.lock().unwrap() = None;
        let out = f();
        crate::core::extensions::unregister_extension("fake-settings");
        out
    }

    #[test]
    fn cycles_through_choices_and_applies() {
        with_fake(|| {
            let mut p = ExtSettingsPanel::new();
            p.open("fake-settings");
            assert!(p.visible && p.is_open());
            assert!(p.is_open_for("fake-settings"));
            assert_eq!(p.filtered.len(), 2);
            assert_eq!(p.current().unwrap().value, "b");

            assert!(p.cycle_selected(), "应切换到下一档");
            assert_eq!(VALUE.lock().unwrap().as_deref(), Some("c"));
            assert_eq!(p.current().unwrap().value, "c", "面板应回读新值");

            assert!(p.cycle_selected());
            assert_eq!(p.current().unwrap().value, "a", "循环回卷到首项");

            assert!(p.error.is_none());
        });
    }

    #[test]
    fn search_filters_and_bounds_selection() {
        with_fake(|| {
            let mut p = ExtSettingsPanel::new();
            p.open("fake-settings");
            assert_eq!(p.filtered.len(), 2);

            p.filter.set_value("other");
            p.recompute();
            assert_eq!(p.filtered.len(), 1, "搜索应过滤到 Other knob");
            assert_eq!(p.current().unwrap().label, "Other knob");

            p.filter.set_value("zzz-nope");
            p.recompute();
            assert!(p.filtered.is_empty(), "无匹配应为空");
            assert!(!p.cycle_selected(), "无匹配时不可切换");

            p.filter.clear();
            p.recompute();
            assert_eq!(p.filtered.len(), 2, "清空搜索恢复全部");
        });
    }

    #[test]
    fn opens_only_the_requested_extension() {
        with_fake(|| {
            let mut p = ExtSettingsPanel::new();
            p.open("fake-settings");
            assert_eq!(p.ext, "fake-settings");
            assert_eq!(p.items.len(), 2);

            // 换成未注册扩展名：只显示该扩展（空），不混入其他扩展的设置
            p.open("not-registered");
            assert_eq!(p.ext, "not-registered");
            assert!(p.items.is_empty(), "不应显示其他扩展的设置");
            assert!(!p.cycle_selected());

            p.close();
            assert!(!p.visible && !p.is_open());
            assert!(p.ext.is_empty());
            assert!(p.filter.is_empty());
        });
    }

    #[test]
    fn selection_wraps_and_offset_stays_in_range() {
        let mut p = ExtSettingsPanel::new();
        p.ext = "x".to_string();
        p.items = (0..20)
            .map(|i| ExtensionSetting::cycle(format!("k{i}"), "L", "", "v", ["v", "w"]))
            .collect();
        p.recompute();
        p.move_selection(-1);
        assert_eq!(p.selected, 19, "从首项向上应回卷到末项");
        assert!(p.offset + MAX_VISIBLE >= p.filtered.len());
        p.move_selection(1);
        assert_eq!(p.selected, 0);
        assert_eq!(p.offset, 0);
    }
}
