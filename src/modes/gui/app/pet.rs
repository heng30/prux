//! 主窗口里的桌宠层：挂载/卸载、推帧任务与拖动。
//!
//! 视图本身在 [`crate::modes::gui::pet`]，扩展的姿态与配置在 [`crate::extensions::pet`]；
//! 这里只把它接进 [`GuiApp`] 的渲染与鼠标通路。

use super::*;
use crate::{extensions::pet as pet_ext, modes::gui::pet as view};

impl GuiApp {
    /// 桌宠层：扩展启用且装好动画就画在主窗口里（对话流之上、覆盖层之下）。
    pub(super) fn render_pet(
        &mut self,
        root: gpui::Div,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        if !extensions::is_extension_enabled(pet_ext::NAME) {
            // 关掉扩展就丢掉解码好的帧，再开时重头来
            self.pet = None;
            return root;
        }
        if self.pet.is_none() {
            if !pet_ext::assets_ready() {
                return root;
            }
            self.pet = Some(view::Pet::new(window));
        }
        self.start_pet_tick(cx);

        let Some(pet) = self.pet.as_mut() else {
            return root;
        };
        let element = pet.element(window).on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                if let Some(pet) = this.pet.as_mut() {
                    pet.down(event.position);
                }
                // 别让这次按下冒泡到对话流去起文本选区
                cx.stop_propagation();
                cx.notify();
            }),
        );
        root.child(element)
    }

    /// 拖动中：按鼠标位置挪宠物（挂在根节点上，光标跑到别的元素上也不断）。
    pub(super) fn pet_drag(&mut self, at: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        if self.pet.as_mut().is_some_and(|pet| pet.drag_to(at)) {
            cx.notify();
        }
    }

    /// 松手：真的拖动过就把位置写进配置。
    pub(super) fn pet_release(&mut self, cx: &mut Context<Self>) {
        if let Some(pet) = self.pet.as_mut() {
            pet.release();
            cx.notify();
        }
    }

    /// 桌宠的推帧任务：12fps 换帧；宠物一消失就自己退出，下次再出现时重启。
    fn start_pet_tick(&mut self, cx: &mut Context<Self>) {
        if self.pet_ticking {
            return;
        }
        self.pet_ticking = true;

        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(1000 / view::FPS))
                    .await;

                let alive = this
                    .update(cx, |app, cx| {
                        let Some(pet) = app.pet.as_mut() else {
                            app.pet_ticking = false;
                            return false;
                        };
                        pet.tick();
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);

                if !alive {
                    break;
                }
            }
        })
        .detach();
    }
}
