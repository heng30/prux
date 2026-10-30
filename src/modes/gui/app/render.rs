//! GUI 渲染：对话流、输入卡片、覆盖层与滚动条。
//!
//! 拆自 [`super::app`]：只搬渲染代码，状态与按键分发仍在父模块。

use super::theme::{
    CONTROL_H_MD, CONTROL_H_SM, DOCK_MAX_H, FONT_BODY, FONT_CAPTION, FONT_HERO, FONT_SMALL,
    RADIUS_LG, RADIUS_MD, RADIUS_SM, SPACE_LG, SPACE_MD, SPACE_SM, SPACE_XL, SPACE_XS,
};
use super::{icon, status::BLINK_DIM_OPACITY, *};
use std::rc::Rc;

impl Render for GuiApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let transcript = self.render_transcript(cx);
        let composer = self.render_composer(window, cx);
        let main = self.render_main(transcript, composer);
        let body = self.render_body(main, cx);
        let sidebar = self.render_sidebar(cx);
        let root = self.render_root(sidebar, body, cx);
        self.render_overlays(root, window, cx)
    }
}

impl GuiApp {
    /// 补全面板：贴在输入卡片上沿的覆盖层（不占布局空间）。
    pub(super) fn suggest_panel(
        &self,
        suggest: &Suggest,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut column = div()
            .w_full()
            .rounded(px(RADIUS_MD))
            .bg(theme.composer)
            .border_1()
            .border_color(theme.border_strong)
            .p(px(SPACE_XS))
            .flex()
            .flex_col()
            .text_size(px(FONT_SMALL));
        column = column.child(
            div()
                .w_full()
                .rounded(px(RADIUS_MD))
                .bg(theme.canvas)
                .border_1()
                .border_color(theme.border)
                .px(px(SPACE_SM))
                .py(px(SPACE_XS))
                .text_size(px(FONT_SMALL))
                .text_color(theme.text)
                .child(composer::render(&self.suggest_input, theme)),
        );

        if suggest.items.is_empty() {
            column = column.child(
                div()
                    .px(px(SPACE_SM))
                    .py(px(SPACE_XS))
                    .text_size(px(FONT_SMALL))
                    .text_color(theme.text_ghost)
                    .child(SharedString::from("No matches")),
            );
        }

        // 候选区与 Ctrl-M 面板同一套行渲染 + 可滚动容器：条目多时滚轮能翻到
        let mut rows = overlay::rows_container(
            "suggest-rows",
            &self.suggest_scroll,
            SUGGEST_ROW_HEIGHT * SUGGEST_ROWS as f32,
        );

        for (index, item) in suggest.items.iter().enumerate() {
            rows = rows.child(overlay::list_row(
                theme,
                overlay::RowSpec {
                    label: &item.label,
                    columns: &[],
                    cells: &[],
                    detail: &item.detail,
                    // 补全面板：说明紧跟命令名，不在中间留大片空白
                    detail_inline: true,
                    checked: None,
                    selected: index == suggest.selected,
                    height: SUGGEST_ROW_HEIGHT,
                },
                ("suggest-row", index),
                cx.listener(move |this, _, _, cx| this.pick_suggestion(index, cx)),
            ));
        }

        column = column.child(rows);
        column.into_any_element()
    }

    /// IME（中文/日文组字）只在绘制阶段能挂到窗口：这里放一层透明画布，
    /// 命中区域就是当前输入框，候选窗会贴着它弹。
    pub(super) fn ime_layer(&self, cx: &mut Context<Self>) -> AnyElement {
        let focus = self.focus.clone();
        let view = cx.entity();
        canvas(
            |_, _, _| (),
            move |bounds, _, window, cx| {
                window.handle_input(&focus, ElementInputHandler::new(bounds, view.clone()), cx);
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .into_any_element()
    }

    /// 滚动条：滑块位置按上一帧的滚动状态算，拖动时把鼠标位移换算成内容位移。
    pub(super) fn scrollbar(
        &self,
        area: ScrollArea,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.scrollbar_visible(area) {
            return div().into_any_element();
        }

        let scroll = self.scroll_of(area);
        let viewport = f32::from(scroll.bounds().size.height);
        let max = f32::from(scroll.max_offset().height);
        if max <= 0. || viewport <= 0. {
            return div().into_any_element();
        }

        let height = geometry::thumb_height(viewport, max, min_thumb());
        let top = geometry::thumb_top(
            f32::from(scroll.offset().y),
            max,
            geometry::thumb_travel(viewport, max, min_thumb()),
        );

        div()
            .id(("scrollbar", area.index()))
            .absolute()
            .top_0()
            .right_0()
            .h_full()
            .w(px(SCROLLBAR_HIT_WIDTH))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event: &gpui::MouseDownEvent, _, cx| {
                    let offset = this.scroll_of(area).offset().y;
                    this.scroll_drag = Some(ScrollDrag {
                        area,
                        anchor_y: None,
                        offset,
                    });
                    cx.notify();
                }),
            )
            .on_mouse_move(
                cx.listener(move |this, event: &gpui::MouseMoveEvent, _, cx| {
                    if this.scroll_drag.is_some_and(|drag| drag.area == area) {
                        this.drag_scrollbar(event.position, cx);
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.scroll_drag = None;
                    cx.notify();
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.scroll_drag = None;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .absolute()
                    // 尺寸来自上一帧实测的像素值，不能再过一次界面缩放
                    .top(gpui::px(top))
                    .right(px(1.))
                    .w(px(SCROLLBAR_WIDTH))
                    .h(gpui::px(height))
                    .rounded(px(RADIUS_SM))
                    .bg(theme.text_ghost),
            )
            .into_any_element()
    }

    /// toast 的堆叠渲染（右上角，不挡输入卡片）。
    pub(super) fn toast_layer(&self, theme: &Theme) -> AnyElement {
        let mut column = div()
            .absolute()
            .top(px(48.))
            .right(px(20.))
            .w(px(320.))
            .flex()
            .flex_col()
            .items_end()
            .gap(px(SPACE_XS));

        for toast in &self.toasts {
            let (base, text_color) = match toast.kind {
                ToastKind::Info => (theme.raised, theme.text),
                ToastKind::Success => (theme.raised, theme.success),
                ToastKind::Error => (theme.raised, theme.danger),
            };

            column = column.child(
                div()
                    .w_full()
                    .px(px(SPACE_MD))
                    .py(px(SPACE_SM))
                    .rounded(px(RADIUS_MD))
                    .bg(base)
                    .border_1()
                    .border_color(theme.border_strong)
                    .text_size(px(FONT_SMALL))
                    .text_color(text_color)
                    .child(SharedString::from(toast.text.clone())),
            );
        }
        column.into_any_element()
    }

    /// 对话流：逐块渲染 + 钉底滚动 + 滚动条。
    fn render_transcript(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme.clone();
        let cwd = self.cwd();

        let pane_width = self.pane_width.get();
        // 内容的可用内宽：对话区宽度减去左右内边距（首帧还没量到时按最大宽度估）
        let flow_width = if pane_width > px(0.) {
            (pane_width - px(SPACE_XL * 2. + SPACE_SM)).min(px(CONTENT_MAX_WIDTH))
        } else {
            px(CONTENT_MAX_WIDTH)
        };

        let expand_tools = self.expand_tools;
        let show_thinking = self.show_thinking;

        // 行登记每帧重建：行序号/边界/文本必须与本帧渲染结果一致
        self.line_bounds.borrow_mut().clear();
        self.line_text_bounds.borrow_mut().clear();
        self.line_rows.borrow_mut().clear();

        let mut lines = transcript::TextLines {
            next: 0,
            selection: self.text_sel,
            bounds: self.line_bounds.clone(),
            text_bounds: self.line_text_bounds.clone(),
            rows: self.line_rows.clone(),
        };
        let mut flow_ctx = transcript::FlowCtx {
            width: flow_width,
            lines: &mut lines,
        };

        // 默认行高是 φ（1.618em），行内代码/高亮背景会又高又连在一起，
        // 文字也不在背景里居中；正文统一用更紧的行高
        let mut flow = div().w_full().flex().flex_col().gap(px(SPACE_MD));
        for (index, block) in self.transcript.blocks_mut().iter_mut().enumerate() {
            flow = flow.child(transcript::render_block(
                block,
                &theme,
                (expand_tools, show_thinking),
                index,
                &cwd,
                &mut flow_ctx,
                cx,
            ));
        }

        let measured_height = self.flow_height.clone();
        let mut content = flow
            .flex_shrink_0()
            .max_w(px(CONTENT_MAX_WIDTH))
            .mx_auto()
            .px(px(SPACE_XL))
            .pt(px(FLOW_PADDING_TOP))
            .pb(px(FLOW_PADDING_BOTTOM));

        // 滚动容器里的内容会被按「无限宽」测量，之后又按实际宽度换行画出来：
        // 长文本的实测高度偏小，往下压到后一个块上、展开时底部还会被截。
        // 用上一帧实测到的对话区宽度把内容定宽，测量和绘制就用同一个换行宽度。
        content = if pane_width > px(0.) {
            content.w(pane_width)
        } else {
            content.w_full()
        };

        // 同一处测量误差也在高度上：内容盒的实测高度会比子块实际占据的还小，
        // 滚到底仍有最后几行被裁掉。按上一帧子块实际的跨度补 min-height。
        if let Some(height) = measured_height.get() {
            content = content.min_h(height);
        }

        content =
            content.on_children_prepainted(move |children: Vec<Bounds<Pixels>>, window, _| {
                let (Some(first), Some(last)) = (children.first(), children.last()) else {
                    return;
                };
                let needed =
                    last.bottom() - first.top() + px(FLOW_PADDING_TOP + FLOW_PADDING_BOTTOM);
                if measured_height.get() != Some(needed) {
                    measured_height.set(Some(needed));
                    window.request_animation_frame();
                }
            });

        // 钉底：事件里那次 `scroll_to_bottom` 会停在旧的底（内容高度还会被二次测量改掉），
        // 这里在布局结束后按实际内容底边对一次；只在意内容变高变矮，用户自己滚开就不管了。
        let scroll = self.scroll.clone();
        let pinned = self.pinned_bottom.clone();
        let pane_width = self.pane_width.clone();
        let pin_bottom = move |_: Vec<Bounds<Pixels>>, window: &mut Window, _: &mut App| {
            let viewport_bounds = scroll.bounds();
            if (pane_width.get() - viewport_bounds.size.width).abs() > px(0.5) {
                pane_width.set(viewport_bounds.size.width);
                window.request_animation_frame();
            }

            let (Some(content), viewport) = (scroll.bounds_for_item(0), scroll.bounds()) else {
                return;
            };

            // 内容不足一屏时底边在视口下方，差值才是正的：夹到 0 才算「已经在底部」，
            // 否则空会话里永远判定为已滚开，新内容来了不跟随。
            let target = (viewport.bottom() - content.bottom()).min(px(0.));
            let offset = scroll.offset().y;

            // 「上次钉底的偏移」还在原位 = 这仍是我们的地盘；否则是用户滚轮/拖滚动条动过了
            let owned = pinned.get().filter(|last| (*last - offset).abs() <= px(1.));

            // 用户自己滚回底部时重新接管，这样新内容来了还会继续跟
            let following = owned.is_some() || (offset - target).abs() <= px(1.);
            if following && owned != Some(target) {
                scroll.set_offset(gpui::point(px(0.), target));
                window.request_animation_frame();
            }

            let next = following.then_some(target);
            if next != pinned.get() {
                pinned.set(next);
            }
        };

        let transcript_area = div()
            .on_children_prepainted(pin_bottom)
            .id("transcript")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            // 滚轮滚动不会通知视图，这里补一次重绘，滚动条滑块才跟得上
            .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.notify()))
            // 悬浮探测走全局鼠标监听（见 `hover_probe`）：滚动容器的 hitbox 判定偏宽
            .child(content);

        let transcript = div()
            .relative()
            .flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(transcript_area)
            .child(self.scrollbar(ScrollArea::Transcript, &theme, cx));

        transcript.into_any_element()
    }

    /// 待发送区（输入框上方）：忙碌时排队的 steer / follow-up。
    /// 未发送前不进对话流，只在这里列出（同 TUI 的待发送区）。
    fn render_pending(&self) -> Option<AnyElement> {
        let lines = self.pending_lines();
        if lines.is_empty() {
            return None;
        }

        let theme = self.theme.clone();
        Some(
            div()
                .flex_none()
                .w_full()
                .px(px(SPACE_XL))
                .child(
                    div()
                        .max_w(px(CONTENT_MAX_WIDTH))
                        .mx_auto()
                        .px(px(SPACE_MD))
                        .flex()
                        .flex_col()
                        .text_size(px(FONT_CAPTION))
                        .text_color(theme.text_ghost)
                        .children(
                            lines
                                .into_iter()
                                .map(|line| div().child(SharedString::from(line))),
                        )
                        .child(
                            div().child(SharedString::from("↳ Alt+Up to edit all queued messages")),
                        ),
                )
                .into_any_element(),
        )
    }

    /// 输入卡片：文本框 + 一行提示与动作。
    fn render_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::Div {
        let theme = self.theme.clone();

        let model = self
            .snapshot
            .as_ref()
            .map(|s| format!("{}/{}", s.provider, s.model))
            .unwrap_or_else(|| "No model selected".to_string());
        let thinking = self
            .snapshot
            .as_ref()
            .and_then(|s| s.thinking.clone())
            .filter(|t| t != "off");

        let mut hint = div().flex().items_center().gap(px(SPACE_SM));
        hint = hint.child(
            div()
                .id("attach")
                .h(px(CONTROL_H_SM))
                .px(px(SPACE_SM))
                .rounded(px(RADIUS_SM))
                .flex()
                .items_center()
                .border_1()
                .border_color(theme.border)
                .text_size(px(FONT_CAPTION))
                .text_color(theme.text_secondary)
                .cursor_pointer()
                .hover(|style| style.bg(theme.overlay).text_color(theme.text))
                .on_click(cx.listener(|this, event: &gpui::ClickEvent, _, cx| {
                    this.open_attach_menu(event.position(), cx);
                }))
                .child(SharedString::from("+ Attach")),
        );
        hint = hint.child(
            chip(&theme, model, false)
                .id("model-chip")
                .cursor_pointer()
                .hover(|style| style.bg(theme.overlay).text_color(theme.text))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                        this.open_model_menu(event.position, cx);
                    }),
                ),
        );
        hint = hint.child(
            chip(
                &theme,
                match &thinking {
                    Some(level) => format!("thinking {level}"),
                    None => "thinking off".to_string(),
                },
                false,
            )
            .id("thinking-chip")
            .cursor_pointer()
            .hover(|style| style.bg(theme.overlay).text_color(theme.text))
            .on_click(cx.listener(|this, event: &gpui::ClickEvent, _, cx| {
                this.open_thinking_menu(event.position(), cx);
            })),
        );
        hint = hint.child(div().flex_1());

        let busy = self.busy;
        let send = div()
            .id("send")
            .h(px(CONTROL_H_MD))
            .px(px(SPACE_MD))
            .rounded(px(RADIUS_MD))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(FONT_SMALL))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .bg(theme.inverse)
            .text_color(theme.on_inverse)
            .cursor_pointer()
            .hover(|style| style.opacity(0.86))
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.busy {
                    this.abort_turn(cx);
                } else {
                    this.submit(false, cx);
                    cx.notify();
                }
            }))
            .child(if busy {
                icon::stop(&theme)
            } else {
                SharedString::from("Send").into_any_element()
            });
        hint = hint.child(send);

        let ime_layer = self.ime_layer(cx);

        // 输入框可用内宽：上一帧实测的卡片宽减左右内边距（首帧还没量到时按窗口估算）
        let composer_wrap_width = self
            .composer_bounds
            .get()
            .map(|bounds| bounds.size.width - px(COMPOSER_H_PADDING * 2.))
            .filter(|width| *width > px(0.))
            .unwrap_or_else(|| px(600.));

        div()
            .relative()
            .w_full()
            .max_w(px(CONTENT_MAX_WIDTH))
            .mx_auto()
            .rounded(px(RADIUS_LG))
            .bg(theme.composer)
            .border_1()
            .border_color(theme.border_strong)
            .flex()
            .flex_col()
            .gap(px(SPACE_SM))
            .px(px(SPACE_MD))
            .pt(px(SPACE_SM))
            .pb(px(SPACE_SM))
            .child(composer::render_wrapped(
                &self.editor,
                &theme,
                {
                    let app = cx.entity();
                    Rc::new(move |event, row, col, _, cx| {
                        app.update(cx, |this, cx| {
                            this.set_cursor_at(row, col, event.modifiers.shift, cx);
                        });
                    })
                },
                {
                    let app = cx.entity();
                    Rc::new(move |_, row, col, _, cx| {
                        // 拖动选区：锚点留在按下处，光标跟着走
                        app.update(cx, |this, cx| this.set_cursor_at(row, col, true, cx));
                    })
                },
                composer_wrap_width,
                &self.composer_scroll,
                &self.composer_cursor,
                window,
            ))
            .child(hint)
            .child(ime_layer)
    }

    /// 主区：空态欢迎页，或对话流 + 输入卡片 + 底部状态。
    /// 停靠面板：扩展提供的只读状态行（段间空一行），贴在输入卡片上方，超过高度就内部滚动。
    ///
    /// 与 TUI 同语义：`dock_visible` 为假或没有扩展提供内容时整块不渲染（不占布局）。
    fn render_dock(&self, theme: &Theme) -> Option<AnyElement> {
        if !self.dock_visible {
            return None;
        }

        let sections = extensions::dock_sections();
        if sections.is_empty() {
            return None;
        }

        let mut body = div()
            .id("dock")
            .max_h(px(DOCK_MAX_H))
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .track_scroll(&self.dock_scroll);

        for (index, section) in sections.iter().enumerate() {
            if index > 0 {
                // 段间一行空白（TUI 用空行分隔，段内行不空）
                body = body.child(div().h(px(SPACE_XS)));
            }

            for line in &section.lines {
                body = body.child(overlay::render_dock_line(line, theme));
            }
        }

        Some(
            div()
                .flex_none()
                .w_full()
                .px(px(SPACE_XL))
                .pb(px(SPACE_XS))
                .child(
                    div()
                        .max_w(px(CONTENT_MAX_WIDTH))
                        .mx_auto()
                        .rounded(px(RADIUS_MD))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.inset)
                        .px(px(SPACE_MD))
                        .py(px(SPACE_SM))
                        .text_size(px(FONT_SMALL))
                        .child(body),
                )
                .into_any_element(),
        )
    }

    fn render_main(&mut self, transcript: AnyElement, composer_card: gpui::Div) -> AnyElement {
        let theme = self.theme.clone();
        let empty = self.transcript.blocks().is_empty();
        let (status_left, status_right) = self.status_text();

        // 忙碌指示单独一块：● 与文字一起按闪烁相位上色
        let busy_text = self.busy_text();
        let blink = self.blink;

        let main = if empty {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .bg(theme.canvas)
                .justify_center()
                .child(
                    div()
                        .w_full()
                        .max_w(px(CONTENT_MAX_WIDTH))
                        .mx_auto()
                        .px(px(SPACE_XL))
                        .flex()
                        .flex_col()
                        .gap(px(SPACE_LG))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(SPACE_XS))
                                .child(
                                    div()
                                        .text_size(px(FONT_HERO))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(theme.text)
                                        .child(SharedString::from("What should we build today?")),
                                )
                                .child(
                                    div()
                                        .text_size(px(FONT_SMALL))
                                        .text_color(theme.text_tertiary)
                                        .child(SharedString::from(
                                            "Ctrl-M for commands · Ctrl-, for settings",
                                        )),
                                ),
                        )
                        .children(self.render_dock(&theme))
                        .child(measured_composer(
                            composer_card,
                            self.composer_bounds.clone(),
                        ))
                        .child(
                            div()
                                .px(px(SPACE_MD))
                                .text_size(px(FONT_CAPTION))
                                .text_color(theme.text_ghost)
                                .child(SharedString::from(
                                    "Enter send · Shift-Enter new line · Alt-Enter queue follow-up",
                                )),
                        ),
                )
        } else {
            let mut column = div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .bg(theme.canvas)
                .child(transcript);

            if let Some(pending) = self.render_pending() {
                column = column.child(pending);
            }

            if let Some(dock) = self.render_dock(&theme) {
                column = column.child(dock);
            }

            column
                .child(measured_composer(
                    div()
                        .flex_none()
                        .w_full()
                        .px(px(SPACE_XL))
                        .pb(px(SPACE_XS))
                        .child(composer_card),
                    self.composer_bounds.clone(),
                ))
                .child(
                    div()
                        .flex_none()
                        .w_full()
                        .px(px(SPACE_XL))
                        .pb(px(SPACE_XS))
                        .child(
                            div()
                                .max_w(px(CONTENT_MAX_WIDTH))
                                .mx_auto()
                                .px(px(SPACE_MD))
                                .flex()
                                .items_center()
                                .justify_between()
                                .text_size(px(FONT_CAPTION))
                                .text_color(theme.text_ghost)
                                .child({
                                    let mut left = div().flex().items_center();
                                    if let Some(text) = busy_text {
                                        left = left
                                            .child(
                                                div()
                                                    .text_color(theme.accent)
                                                    .opacity(if blink {
                                                        1.0
                                                    } else {
                                                        BLINK_DIM_OPACITY
                                                    })
                                                    .child(SharedString::from("● ")),
                                            )
                                            .child(
                                                div()
                                                    .text_color(theme.accent)
                                                    .child(SharedString::from(text)),
                                            )
                                            .child(SharedString::from(" · "));
                                    }
                                    left.child(SharedString::from(status_left))
                                })
                                .child(SharedString::from(status_right)),
                        ),
                )
        };

        main.into_any_element()
    }

    /// 整行布局：侧栏 + 主区 + 右侧面板，两侧带拖拽热区。
    /// 侧边栏（全高）：会话列表 + 底部工作目录，右侧带拖拽热区。
    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if !self.show_sidebar {
            return gpui::Empty.into_any_element();
        }

        let cwd = self.cwd();
        let theme = self.theme.clone();

        div()
            .relative()
            .h_full()
            .flex_none()
            .child(sidebar::render(sidebar::SidebarCtx {
                theme: &theme,
                cwd: &cwd,
                branch: self.branch.as_deref(),
                width: self.sidebar_width,
                rows: &self.sessions,
                current: self
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.session_path.as_deref()),
                loading: self.sessions_loading,
                scroll: &self.sessions_scroll,
                scrollbar: self.scrollbar(ScrollArea::Sidebar, &theme, cx),
                cmd_tx: self.cmd_tx.clone(),
                new_session: Box::new(cx.listener(|this, _, _, cx| {
                    _ = this.cmd_tx.send(AgentCommand::NewSession);
                    this.transcript.clear();
                    this.flow_height.set(None);
                    this.reload_sessions(cx);
                    cx.notify();
                })),
                change_cwd: Box::new(cx.listener(|this, _, _, cx| this.open_cwd_menu(cx))),
                context_menu: {
                    let app = cx.entity();
                    std::rc::Rc::new(move |path: &str, event: &gpui::MouseDownEvent, _, cx| {
                        let path = path.to_string();
                        let position = event.position;
                        app.update(cx, |this, cx| this.open_session_menu(&path, position, cx));
                    })
                },
            }))
            .child(
                div()
                    .id("sidebar-resize")
                    .absolute()
                    .top_0()
                    .right(px(-SIDEBAR_HANDLE_WIDTH / 2.))
                    .h_full()
                    .w(px(SIDEBAR_HANDLE_WIDTH))
                    .cursor(gpui::CursorStyle::ResizeLeftRight)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                            this.resize_drag = Some(ResizeDrag {
                                anchor_x: None,
                                width: this.sidebar_width,
                            });
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    ),
            )
            .into_any_element()
    }

    /// 主区行：对话/输入 + 右侧面板，面板左边带拖拽热区。
    fn render_body(&mut self, main: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme.clone();

        // 点主区就交回键盘（否则搜索框一直吃键，看着像输入卡片坏了）；面板里的点击是
        // 先 mouse_down 再 click，搜索框自己的 on_click 会重新聚焦回来
        let mut body = div().flex().flex_1().min_h_0().w_full().on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _, _, cx| {
                if this.panel.search_focus {
                    this.panel.search_focus = false;
                    cx.notify();
                }
            }),
        );
        body = body.child(main);

        if self.panel.visible {
            body = body.child(
                div()
                    .relative()
                    .h_full()
                    .flex_none()
                    .min_w_0()
                    .child(giff::render(&theme, &self.panel, self.panel_width, cx))
                    .child(
                        // 左边框拖拽热区：往左拉变宽，指针变左右缩放
                        div()
                            .id("panel-resize")
                            .absolute()
                            .top_0()
                            .left(px(-SIDEBAR_HANDLE_WIDTH / 2.))
                            .h_full()
                            .w(px(SIDEBAR_HANDLE_WIDTH))
                            .cursor(gpui::CursorStyle::ResizeLeftRight)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                                    this.panel_resize = Some(ResizeDrag {
                                        anchor_x: None,
                                        width: this.panel_width,
                                    });
                                    cx.stop_propagation();
                                    cx.notify();
                                }),
                            ),
                    ),
            );
        }

        body.into_any_element()
    }

    /// 根节点：侧栏 + 主区并排，挂焦点与键盘监听。
    fn render_root(
        &mut self,
        sidebar: AnyElement,
        body: AnyElement,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let theme = self.theme.clone();

        div()
            .relative()
            .flex()
            .size_full()
            .bg(theme.canvas)
            .text_color(theme.text)
            .text_size(px(FONT_BODY))
            .font_family(theme::UI_FONT_FAMILY)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .child(sidebar)
            .child(body)
    }

    /// 盖在根节点上的浮层：模态、toast、下拉菜单、补全面板、会话菜单、对话框。
    fn render_overlays(
        &mut self,
        root: gpui::Div,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let theme = self.theme.clone();
        let empty = self.transcript.blocks().is_empty();

        let root = self.render_modal(root, &theme, cx);
        let root = root.child(self.toast_layer(&theme));
        let root = root.child(self.hover_probe(!empty, cx));
        let root = self.render_dropdown(root, &theme, window, cx);
        let root = self.render_suggest_overlay(root, &theme, cx);
        let root = self.render_extension_overlay(root, &theme, window, cx);
        let root = self.render_session_menu(root, &theme, cx);
        self.render_dialog(root, &theme, cx)
    }

    /// 模态覆盖层（命令面板 / 设置 / 列表选择页）：遮罩 + 卡片。
    fn render_modal(
        &mut self,
        root: gpui::Div,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        // 扩展覆盖层贴在输入框上方（见 `render_extension_overlay`），不走居中模态
        if let Some(overlay) = self
            .overlay
            .as_ref()
            .filter(|overlay| !matches!(overlay, Overlay::Extension(_)))
        {
            return root.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .w_full()
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(gpui::rgba(0x00000066))
                    .occlude()
                    // 点遮罩（卡片外）关闭；卡片自己拦下事件，点里面不关
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.dismiss_overlay();
                            this.skills_draft = None;
                            this.extensions_draft = None;
                            cx.notify();
                        }),
                    )
                    .child(
                        div()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|_, _, _, cx| cx.stop_propagation()),
                            )
                            .child(overlay::render(overlay, theme, cx)),
                    ),
            );
        }
        root
    }

    /// 标题栏 / 输入区的下拉菜单（模型、思考级别、附件）。
    fn render_dropdown(
        &mut self,
        root: gpui::Div,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        if let Some(dropdown) = &self.dropdown {
            let (anchor, width, scroll, selected) = (
                dropdown.anchor,
                dropdown.width,
                dropdown.scroll,
                dropdown.selected,
            );
            let handle = dropdown.handle.clone();
            // 锚点来自鼠标事件，是窗口的物理像素；尺寸按 px() 走的是「逻辑像素 ×ui_scale」。
            // 统一换到物理像素再算位置，否则菜单会被缩放顶到窗口外（曾整块看不见）。
            let scale = theme::ui_scale();
            let width_px = width * scale;
            let left = geometry::clamp_within(
                f32::from(anchor.x),
                width_px,
                f32::from(window.viewport_size().width),
                MENU_EDGE_MARGIN * scale,
            );
            // 菜单比窗口还高时也别顶出去：按行数估个高度，底部同样让出边距
            let estimate = ((dropdown.items.len() as f32 * DROPDOWN_ROW_HEIGHT + 8.)
                .min(DROPDOWN_MAX_HEIGHT))
                * scale;
            let top = geometry::clamp_within(
                f32::from(anchor.y) + 4. * scale,
                estimate,
                f32::from(window.viewport_size().height),
                MENU_EDGE_MARGIN * scale,
            );
            let mut card = div()
                .id("dropdown-menu")
                .absolute()
                .left(gpui::px(left))
                .top(gpui::px(top))
                .w(gpui::px(width_px))
                .p(px(SPACE_XS))
                .rounded(px(RADIUS_MD))
                .bg(theme.composer)
                .border_1()
                .border_color(theme.border_strong)
                .flex()
                .flex_col()
                .occlude()
                // 同会话菜单：按下菜单不能冒泡给遮罩，否则菜单先关、click 不触发
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|_, _, _, cx| cx.stop_propagation()),
                );

            if scroll {
                card = card
                    .max_h(px(DROPDOWN_MAX_HEIGHT))
                    .overflow_y_scroll()
                    .track_scroll(&handle);
            }

            for (index, item) in dropdown.items.iter().enumerate() {
                let action = item.action.clone();
                let mut row = div()
                    .id(("dropdown", index))
                    .min_h(px(CONTROL_H_MD))
                    .px(px(SPACE_SM))
                    .rounded(px(RADIUS_MD))
                    .flex()
                    .items_center()
                    .gap(px(SPACE_SM))
                    .text_size(px(FONT_SMALL))
                    .text_color(theme.text_secondary)
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.overlay).text_color(theme.text))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .child(SharedString::from(item.label.clone())),
                    );

                // 键盘高亮的行（Ctrl-J/K 与方向键移动）
                if index == selected {
                    row = row.bg(theme.overlay).text_color(theme.text);
                }

                if !item.detail.is_empty() {
                    row = row.child(
                        div()
                            .flex_none()
                            .text_size(px(FONT_CAPTION))
                            .text_color(theme.text_ghost)
                            .child(SharedString::from(item.detail.clone())),
                    );
                }

                card = card.child(row.on_click(cx.listener(move |this, _, _, cx| {
                    this.dropdown = None;
                    let action = action.clone();
                    this.run_action(action, cx);
                    cx.notify();
                })));
            }

            return root.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .occlude()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.dropdown = None;
                            cx.notify();
                        }),
                    )
                    .child(card),
            );
        }
        root
    }

    /// 输入补全面板：贴在输入卡片上沿的浮层。
    fn render_suggest_overlay(
        &mut self,
        root: gpui::Div,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        if let (Some(suggest), Some(card)) = (self.suggest.clone(), self.composer_bounds.get()) {
            let rows = suggest.items.len().clamp(1, SUGGEST_ROWS) as f32;
            // 面板高度 = 输入框 + 行区（行数超上限时只按上限算，其余靠滚动）
            let height = px(SUGGEST_ROW_HEIGHT * rows) + px(SUGGEST_CHROME_HEIGHT);
            let top = (card.origin.y - height - px(SUGGEST_GAP)).max(px(0.));

            return root.child(
                div()
                    .absolute()
                    .left(card.origin.x)
                    .w(card.size.width)
                    .top(top)
                    // 挡住下层：否则点在面板上的鼠标事件会穿到对话流/输入框
                    .occlude()
                    .child(self.suggest_panel(&suggest, theme, cx)),
            );
        }
        root
    }

    /// 扩展覆盖层（`ShowOverlay`）：贴在输入卡片上沿弹出，内容由扩展持有。
    fn render_extension_overlay(
        &mut self,
        root: gpui::Div,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let (Some(card), Some(overlay)) = (self.composer_bounds.get(), self.overlay.as_ref())
        else {
            return root;
        };

        if !matches!(overlay, Overlay::Extension(_)) {
            return root;
        }

        // 底边贴着输入卡片上沿：覆盖层高度由内容决定，用 bottom 定位才不会被估高
        let bottom = window.viewport_size().height - card.origin.y + px(SUGGEST_GAP);
        root.child(
            div()
                .absolute()
                .left(card.origin.x)
                .w(card.size.width)
                .bottom(bottom)
                .occlude()
                .child(overlay::render(overlay, theme, cx)),
        )
    }

    /// 会话行右键菜单（重命名 / 删除）。
    fn render_session_menu(
        &mut self,
        root: gpui::Div,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        if let Some(menu) = &self.session_menu {
            let anchor = menu.position;
            let path = menu.path.clone();
            let rename_path = path.clone();
            let selected = menu.selected;

            return root.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .occlude()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.close_session_menu(cx)),
                    )
                    .child(
                        div()
                            .absolute()
                            .left(anchor.x)
                            .top(anchor.y)
                            .w(px(132.))
                            .p(px(SPACE_XS))
                            .rounded(px(RADIUS_MD))
                            .bg(theme.composer)
                            .border_1()
                            .border_color(theme.border_strong)
                            .flex()
                            .flex_col()
                            // 按下菜单本身不能冒泡到遮罩：遮罩会先关菜单，随后的 click 就落不到条目上
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|_, _, _, cx| cx.stop_propagation()),
                            )
                            .child(
                                div()
                                    .id("session-menu-rename")
                                    .h(px(CONTROL_H_MD))
                                    .px(px(SPACE_SM))
                                    .rounded(px(RADIUS_MD))
                                    .flex()
                                    .items_center()
                                    .text_size(px(FONT_SMALL))
                                    .text_color(theme.text_secondary)
                                    .cursor_pointer()
                                    .hover(|style| style.bg(theme.overlay).text_color(theme.text))
                                    // 键盘高亮（上下 / Ctrl-J、Ctrl-K）
                                    .when(selected == 0, |row| {
                                        row.bg(theme.overlay).text_color(theme.text)
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.start_rename(&rename_path, cx);
                                    }))
                                    .child(SharedString::from("Rename")),
                            )
                            .child(
                                div()
                                    .id("session-menu-delete")
                                    .h(px(CONTROL_H_MD))
                                    .px(px(SPACE_SM))
                                    .rounded(px(RADIUS_MD))
                                    .flex()
                                    .items_center()
                                    .text_size(px(FONT_SMALL))
                                    .text_color(theme.text_secondary)
                                    .cursor_pointer()
                                    .hover(|style| style.bg(theme.overlay).text_color(theme.danger))
                                    .when(selected == 1, |row| row.bg(theme.overlay))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.ask_delete(&path, cx);
                                    }))
                                    .child(SharedString::from("Delete")),
                            ),
                    ),
            );
        }
        root
    }

    /// 重命名 / 删除确认 / 报表单：居中对话框 + IME 层。
    fn render_dialog(
        &mut self,
        root: gpui::Div,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        if self.dialog.is_some() {
            let (title, body, field) = match &self.dialog {
                Some(SessionDialog::Rename { editor, .. }) => (
                    "Rename session",
                    String::new(),
                    Some(composer::render(editor, theme)),
                ),
                Some(SessionDialog::BugHint { editor }) => (
                    "Report a bug",
                    "Describe what happened (bundled with redacted metadata)".to_string(),
                    Some(composer::render(editor, theme)),
                ),
                Some(SessionDialog::Label { editor }) => (
                    "Set label",
                    "Leave empty to clear the label on this branch".to_string(),
                    Some(composer::render(editor, theme)),
                ),
                Some(SessionDialog::Import { editor }) => (
                    "Import session",
                    "Path to a jsonl session (~ and relative paths work)".to_string(),
                    Some(composer::render(editor, theme)),
                ),
                Some(SessionDialog::ApiKey { provider, editor }) => (
                    "Set API key",
                    format!("{provider} key is written to the auth file in the config directory"),
                    Some(composer::render(editor, theme)),
                ),
                Some(SessionDialog::Number { editor, .. }) => (
                    "Edit setting",
                    String::new(),
                    Some(composer::render(editor, theme)),
                ),
                Some(SessionDialog::Delete { title, .. }) => (
                    "Delete session",
                    format!("Delete \"{title}\"? This cannot be undone."),
                    None,
                ),
                None => ("", String::new(), None),
            };

            let ime = self.ime_layer(cx);
            let danger = matches!(self.dialog, Some(SessionDialog::Delete { .. }));
            let mut card = div()
                .relative()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|_, _, _, cx| cx.stop_propagation()),
                )
                .w(px(380.))
                .rounded(px(RADIUS_LG))
                .bg(theme.composer)
                .border_1()
                .border_color(theme.border_strong)
                .p(px(SPACE_LG))
                .flex()
                .flex_col()
                .gap(px(SPACE_SM))
                .child(
                    div()
                        .text_size(px(FONT_BODY))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text)
                        .child(SharedString::from(title)),
                );

            if let Some(field) = field {
                card = card.child(
                    div()
                        .w_full()
                        .rounded(px(RADIUS_MD))
                        .bg(theme.canvas)
                        .border_1()
                        .border_color(theme.border)
                        .px(px(SPACE_SM))
                        .py(px(SPACE_SM))
                        .child(field),
                );
            } else {
                card = card.child(
                    div()
                        .text_size(px(FONT_SMALL))
                        .text_color(theme.text_secondary)
                        .child(SharedString::from(body)),
                );
            }

            card = card
                .child(
                    div()
                        .text_size(px(FONT_CAPTION))
                        .text_color(theme.text_ghost)
                        .child(SharedString::from(if danger {
                            "Enter to delete · Esc cancel"
                        } else {
                            "Enter confirm · Esc cancel"
                        })),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_end()
                        .gap(px(SPACE_SM))
                        .child(
                            div()
                                .id("dialog-cancel")
                                .h(px(CONTROL_H_MD))
                                .px(px(SPACE_MD))
                                .rounded(px(RADIUS_MD))
                                .flex()
                                .items_center()
                                .text_size(px(FONT_SMALL))
                                .bg(theme.overlay)
                                .text_color(theme.text_secondary)
                                .cursor_pointer()
                                .hover(|style| style.bg(theme.overlay_strong))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.dialog = None;
                                    cx.notify();
                                }))
                                .child(SharedString::from("Cancel")),
                        )
                        .child(
                            div()
                                .id("dialog-confirm")
                                .h(px(CONTROL_H_MD))
                                .px(px(SPACE_MD))
                                .rounded(px(RADIUS_MD))
                                .flex()
                                .items_center()
                                .text_size(px(FONT_SMALL))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .bg(theme.inverse)
                                .text_color(theme.on_inverse)
                                .cursor_pointer()
                                .hover(|style| style.opacity(0.86))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.confirm_dialog(cx);
                                }))
                                .child(SharedString::from(if danger {
                                    "Delete"
                                } else {
                                    "Confirm"
                                })),
                        ),
                )
                .child(ime);

            return root.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(gpui::rgba(0x00000066))
                    .occlude()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.dialog = None;
                            cx.notify();
                        }),
                    )
                    .child(card),
            );
        }
        root
    }
}

/// 给输入卡片的外层挂位置实测：补全面板要贴着它上沿弹出来。
pub(super) fn measured_composer(
    wrapper: gpui::Div,
    cell: Rc<Cell<Option<Bounds<Pixels>>>>,
) -> gpui::Div {
    wrapper.on_children_prepainted(move |bounds: Vec<Bounds<Pixels>>, window, _| {
        if let Some(card) = bounds.first()
            && cell.get() != Some(*card)
        {
            cell.set(Some(*card));
            window.request_animation_frame();
        }
    })
}
