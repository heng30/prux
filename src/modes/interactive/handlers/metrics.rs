//! `App` 派生数据刷新：状态栏用的 git 分支、context 占用率、流式 token 计数。
//!
//! 字段所有权仍在 app.rs 的 `App`（`git_branch` / `context_percent` / `stream_meter`…），
//! 本模块只提供刷新与读取；调用方是渲染层（`render/status.rs`、`render.rs`）与事件处理。
//!
//! `stream_meter` 是 `App` 上唯一以 crate 可见性暴露的字段，跨模块访问统一走
//! [`App::stream_meter_mut`] 与 [`App::streaming_output_tokens`]。

use super::super::app::App;
use crate::{
    core::{self, compaction},
    utils::tokens::{self, TokenMeter},
};

impl App {
    /// 读取工作目录的 git 分支（解析 .git/HEAD；支持 worktree 的 gitdir 指向）。
    /// 与上次不同则置 dirty，触发重绘（对齐 footer 扩展的 onBranchChange）。
    pub fn refresh_git_branch(&mut self) {
        let branch = core::extensions::git_branch(&self.cwd);
        if branch != self.git_branch {
            self.git_branch = branch;
            self.dirty = true;
        }
    }

    /// 依据消息历史与上下文窗口刷新 context 使用率（0-100）。
    /// 压缩后（`context_tokens_override` 有值）以压缩后的估算为准，直到新 usage 落地。
    pub fn refresh_context_percent(&mut self) {
        if self.context_window == 0 {
            self.context_percent = 0.0;
            return;
        }
        let tokens = self
            .context_tokens_override
            .unwrap_or_else(|| compaction::estimate_context_tokens(&self.messages).tokens as u64);
        self.context_percent = (tokens as f64 / self.context_window as f64) * 100.0;
    }

    /// 在途流式输出的 token 数（tiktoken 真实计数，含未闭合尾部）。
    /// 底栏扩展据此算实时速率（流式期间消息 usage 尚未落地）。
    pub fn streaming_output_tokens(&self) -> u64 {
        self.stream_meter.as_ref().map_or(0, |m| m.count())
    }

    /// 流式 token 计数器（按当前模型选词表；首次调用时才解析词表）
    pub(crate) fn stream_meter_mut(&mut self) -> &mut TokenMeter {
        let enc = tokens::encoding_for(self.current_model.as_deref());
        self.stream_meter
            .get_or_insert_with(|| TokenMeter::new(enc))
    }

    /// 清空流式缓冲与 token 计数（消息落地 / 新消息 / 中断 / 新会话）
    pub fn clear_streaming(&mut self) {
        self.streaming_text.clear();
        self.streaming_thinking.clear();
        self.streaming_tools.clear();
        if let Some(meter) = &mut self.stream_meter {
            meter.reset();
        }
    }
}
