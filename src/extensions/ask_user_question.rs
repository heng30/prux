//! `ask-user-question` 扩展：让模型用结构化问卷提问
//!
//! 模型调用 `ask_user_question`（1–4 题、每题 2–4 个写好的选项），TUI 弹出覆盖层问卷：
//! 单选用 Enter 选中并翻到下一题；多选用 Space/Enter 勾选、`Tab`/`→` 翻页；每题末尾自动带
//! 一行 `Type something.` 供自由作答；`n` 给当前题写备注（提交页写整份问卷的全局备注）。
//! 作答以结构化数据回传——`details.answers[]`（`kind` = `option` / `custom` / `multi`，
//! 可选 `selected` / `notes` / `preview`）+ `globalNote` + `unanswered`——同一份数据随
//! 工具结果落进会话历史，压缩后仍然可读。
//!
//! **默认不启用**：它给模型加一个「先问再动手」的工具并改写系统提示的 Guidelines，
//! 属用户显式开启的能力。**没有 UI 就不弹问卷**：非交互运行下工具返回 `no_ui` 错误。
//!
//! 与上游相比有意收窄的部分：备注是单行（覆盖层内联输入行只有一行）；`preview` 以纯文本
//! 行显示在选项下方，不做并排预览框；不提供 i18n、折叠键、外部编辑器、RPC 宿主的原生
//! 对话框降级与 `rpiv:ask-user:*` 事件总线。

use crate::{
    core::{
        extensions::{
            self, DockLine, DockSpan, Extension, ExtensionHook, ExtensionMode, ExtensionTool,
            ExtensionUiRequest, ForkProjectInfo, OverlayEvent, OverlayInput, OverlayKey,
            OverlaySize, OverlayView, ToolExecCtx, next_ui_id,
        },
        tools::{ToolError, ToolResult},
    },
    extensions::{
        EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_ASK_USER_QUESTION,
        util::{plain, styled},
    },
    utils::display::{normalize_newlines, wrap_text},
};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex, MutexGuard, OnceLock},
};
use tokio::sync::oneshot;

/// 扩展名（`/extension` 面板与 settings.json 的 `enabledExtensions`）。
const EXT: &str = "ask-user-question";
/// 工具名。
const TOOL: &str = "ask_user_question";
/// 单次调用最多几题。
const MAX_QUESTIONS: usize = 4;
/// 每题最少几个选项。
const MIN_OPTIONS: usize = 2;
/// 每题最多几个选项。
const MAX_OPTIONS: usize = 4;
/// `header` 的最大字符数。
const MAX_HEADER: usize = 16;
/// 选项 `label` 的最大字符数。
const MAX_LABEL: usize = 60;
/// 每题末尾自动追加的自由作答行；同时是保留标签。
const TYPE_SOMETHING: &str = "Type something.";
/// 保留标签：模型自己写这些会被拒绝，运行时标位是唯一真源。
const RESERVED_LABELS: [&str; 3] = ["Other", TYPE_SOMETHING, "Next"];
/// 作答为空时的占位串。
const NO_INPUT: &str = "(no input)";
/// 没答出任何东西（取消 / 全空）时给模型的唯一文案。
const DECLINE: &str = "User declined to answer questions";
/// 成功信封前缀。
const ENVELOPE_PREFIX: &str = "User has answered your questions:";
/// 成功信封后缀。
const ENVELOPE_SUFFIX: &str = "You can now continue with the user's answers in mind.";
/// 覆盖层占用的最大行数（选项 + 说明 + 备注放不下时由 TUI 滚动）。
const PANEL_ROWS: u16 = 22;

/// 系统提示 Available tools 段的单行片段。
const PROMPT_SNIPPET: &str =
    "Ask the user up to 4 structured questions (2-4 options each) when requirements are ambiguous";

/// 工具描述（模型侧）。
const TOOL_DESCRIPTION: &str = "Ask the user one or more structured questions during execution. Use when you need to:
1. Gather user preferences or requirements
2. Clarify ambiguous instructions
3. Get decisions on implementation choices as you work
4. Offer choices to the user about what direction to take

Usage notes:
- The user answers in a dialog of 1-4 tabs, one question each. Users can type their own answer via the automatically appended \"Type something.\" row on every question, or press Esc to abandon the questionnaire.
- Do NOT author \"Other\" or \"Type something.\" labels yourself — reserved labels are rejected at runtime.
- Use multiSelect: true when multiple answers are valid.
- If you recommend a specific option, make that the first option in the list and add \"(Recommended)\" at the end of the label.
- Answers come back as structured data; do not re-ask what the user already answered.

Preview feature:
- An optional `preview` field on options carries a concrete artifact to compare (ASCII mockups, code snippets, diagrams, configuration examples). It is shown as plain text under the option list, single-select questions only. Do not use previews for simple preference questions where labels and descriptions suffice.";

/// 系统提示 Guidelines 段追加的要点。
const PROMPT_GUIDELINES: &[&str] = &[
    "Use ask_user_question whenever the user's request is underspecified and you cannot proceed without concrete decisions — you can ask up to 4 questions per invocation.",
    "Each question MUST have 2-4 options. Every option requires a concise label (1-5 words) and a description explaining what the choice means or its trade-offs. The user can type a custom answer via the automatically appended \"Type something.\" row, or press Esc to abandon the questionnaire. Do NOT author \"Other\" or \"Type something.\" labels yourself — reserved labels are rejected at runtime.",
    "Set multiSelect: true when multiple answers are valid. Provide an options[].preview string when an option benefits from richer context (mockups, code snippets, diagrams, configs) — single-select only. If you recommend a specific option, make that the first option and append \"(Recommended)\" to its label.",
    "Do not stack multiple ask_user_question calls back-to-back — group all clarifying questions into one invocation.",
];

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static ASK_USER_QUESTION_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_ASK_USER_QUESTION,
    make: || -> Arc<dyn Extension> { Arc::new(AskUserQuestion) },
};

/// 一个选项：写好的标签、说明，以及可选的多行预览（ASCII 草图 / 代码 / 配置）。
#[derive(Clone)]
struct OptionItem {
    /// 展示与回传用的标签（1–5 词）。
    label: String,
    /// 选它意味着什么、代价是什么。
    description: String,
    /// 聚焦该选项时显示的预览正文；None = 无预览。
    preview: Option<String>,
}

/// 一道校验后的题目。
#[derive(Clone)]
struct Question {
    /// 完整问句。
    question: String,
    /// 标签页上的短标题（≤ [`MAX_HEADER`] 字符）。
    header: String,
    /// true = 可多选。
    multi: bool,
    /// 2–4 个选项。
    options: Vec<OptionItem>,
}

/// 一道题的作答：勾选的选项下标、自由作答文本、逐题备注。
#[derive(Default, Clone)]
struct Answer {
    /// 已勾选的选项下标（单选至多一个，多选按勾选顺序）。
    picked: Vec<usize>,
    /// `Type something.` 的自由作答文本；空串 = 没写。
    custom: String,
    /// 这一题的备注（不参与「是否已作答」判定）。
    note: String,
}

impl Answer {
    /// 这道题是否算「已作答」（备注不算）。
    fn answered(&self) -> bool {
        !self.picked.is_empty() || !self.custom.trim().is_empty()
    }
}

/// 正在编辑的内联输入行目标：下标等于题目数表示整份问卷的全局备注。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Edit {
    /// 第 N 题的自由作答。
    Custom(usize),
    /// 第 N 题的备注（N == 题目数 = 全局备注）。
    Note(usize),
}

/// 用户提交的作答。
struct Submitted {
    /// 逐题作答（与题目一一对应）。
    answers: Vec<Answer>,
    /// 提交页写的全局备注；空串 = 没写。
    global_note: String,
}

/// 一份进行中的问卷（`overlay_view` / `on_overlay_event` / 工具等待方共享）。
struct Questionnaire {
    /// 覆盖层 id（与 [`next_ui_id`] 同一 id 空间）。
    id: u64,
    /// 校验后的题目。
    questions: Vec<Question>,
    /// 逐题作答，长度与 `questions` 相同。
    answers: Vec<Answer>,
    /// 提交页的全局备注。
    global_note: String,
    /// 当前标签页：`0..questions.len()` 是题目，`questions.len()` 是提交页。
    tab: usize,
    /// 当前页里的行下标（选项行 + 末尾的自由作答行；提交页 0 = 提交、1 = 取消）。
    cursor: usize,
    /// 正在编辑的内联输入行；None = 正常浏览。
    editing: Option<Edit>,
    /// 结果回传口（提交或取消时消费）。
    reply: Option<oneshot::Sender<Option<Submitted>>>,
}

/// 取走覆盖层后的善后动作（先释放状态锁再回传/收面板）。
enum Action {
    /// 按键已被消费。
    Consumed,
    /// 不认领这个键，交还 TUI 默认语义。
    Pass,
    /// 结束问卷：`Some` = 提交作答，`None` = 取消。
    Finish(Option<Submitted>),
}

/// 进程级单例：同一时刻只允许一份问卷（覆盖层一次只显示一个）。
fn slot() -> &'static Mutex<Option<Questionnaire>> {
    /// 进行中的问卷（`None` = 没有问卷在等作答）。
    static SLOT: OnceLock<Mutex<Option<Questionnaire>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// 取问卷状态锁；持锁线程 panic 过（中毒）时取回内部数据继续用。
fn slot_guard() -> MutexGuard<'static, Option<Questionnaire>> {
    slot().lock().unwrap_or_else(|e| e.into_inner())
}

/// 工具等待方的守卫：future 被 drop（回合 abort / 面板被顶掉）时注销问卷并收起覆盖层。
struct OverlayGuard(u64);

impl Drop for OverlayGuard {
    /// 只清理仍属于本次调用的问卷；正常提交时槽位已被 [`finish`] 取走，这里什么也不做。
    fn drop(&mut self) {
        let stale = {
            let mut guard = slot_guard();
            match guard.as_ref() {
                Some(q) if q.id == self.0 => {
                    guard.take();
                    true
                }
                _ => false,
            }
        };

        if stale {
            extensions::request_ui(ExtensionUiRequest::HideOverlay { id: self.0 });
        }
    }
}

/// `ask-user-question` 扩展。
pub struct AskUserQuestion;

impl Extension for AskUserQuestion {
    /// 扩展标识名 `ask-user-question`。
    fn name(&self) -> &str {
        EXT
    }

    /// `/extension` 面板展示的来源与机制说明。
    fn description(&self) -> &str {
        "Structured questionnaire: the model asks up to 4 questions with 2-4 written options \
         each and gets your choices back as data. Single-select, multi-select, a free-text row \
         on every question, per-question notes and one global note. It only asks when the \
         requirement is genuinely ambiguous."
    }

    /// 来源插件（npm 上的移植原点）。
    fn fork_project(&self) -> Option<ForkProjectInfo> {
        Some(ForkProjectInfo {
            plugin_name: "@juicesharp/rpiv-ask-user-question".to_string(),
            plugin_version: "2.15.0".to_string(),
            url: "https://www.npmjs.com/package/@juicesharp/rpiv-ask-user-question".to_string(),
        })
    }

    /// 只在 `Dev` / `Creator` 扩展模式下可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 默认不启用：它给模型加工具并改写 Guidelines，属用户显式开启的能力。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 唯一的工具 `ask_user_question`。
    fn tools(&self) -> Vec<ExtensionTool> {
        vec![ExtensionTool {
            prepare_arguments: Some(prepare_arguments),
            prompt_guidelines: PROMPT_GUIDELINES.iter().map(|s| s.to_string()).collect(),
            ..ExtensionTool::simple(TOOL, TOOL_DESCRIPTION, parameters(), PROMPT_SNIPPET)
        }]
    }

    /// 只声明覆盖层钩子（问卷不常驻、不订阅事件）。
    fn hooks(&self) -> Vec<ExtensionHook> {
        vec![ExtensionHook::Overlay]
    }

    /// 拉取问卷的当前帧（TUI 每帧调用；只读扩展自身状态）。
    fn overlay_view(&self, id: u64, cols: u16) -> Option<OverlayView> {
        let guard = slot_guard();
        let q = guard.as_ref().filter(|q| q.id == id)?;
        Some(view_for(q, cols))
    }

    /// 问卷按键：导航、勾选、翻页、备注编辑、提交/取消。
    fn on_overlay_event(&self, id: u64, ev: &OverlayEvent) -> bool {
        let action = {
            let mut guard = slot_guard();
            let Some(q) = guard.as_mut().filter(|q| q.id == id) else {
                return false;
            };
            handle_event(q, ev)
        };

        match action {
            Action::Consumed => true,
            Action::Pass => false,
            Action::Finish(outcome) => finish(id, outcome),
        }
    }

    /// 执行 `ask_user_question`：校验参数 → 弹问卷 → 等用户作答（期间不占线程）。
    fn execute_tool_async(
        &self,
        name: String,
        args: Value,
        _ctx: ToolExecCtx,
    ) -> BoxFuture<'static, std::result::Result<ToolResult, ToolError>> {
        Box::pin(async move {
            if name != TOOL {
                return Err(ToolError(format!("extension {EXT}: unknown tool: {name}")));
            }

            Ok(run(args).await)
        })
    }
}

/// 校验并规范化模型参数；`Err` 是（错误码，给模型看的消息）。
fn parse_questions(args: &Value) -> std::result::Result<Vec<Question>, (&'static str, String)> {
    let Some(list) = args.get("questions").and_then(Value::as_array) else {
        return Err((
            "invalid_params",
            "Error: `questions` must be an array of question objects".to_string(),
        ));
    };

    if list.is_empty() {
        return Err((
            "no_questions",
            "Error: At least one question is required".to_string(),
        ));
    }

    if list.len() > MAX_QUESTIONS {
        return Err((
            "too_many_questions",
            format!("Error: At most {MAX_QUESTIONS} questions are allowed per invocation"),
        ));
    }

    let mut questions = Vec::with_capacity(list.len());
    let mut seen = HashSet::new();

    for raw in list {
        let question = text_field(raw, "question")?;
        let header = text_field(raw, "header")?;
        if header.chars().count() > MAX_HEADER {
            return Err((
                "invalid_params",
                format!("Error: `header` must be at most {MAX_HEADER} characters: \"{header}\""),
            ));
        }

        if !seen.insert(question.clone()) {
            return Err((
                "duplicate_question",
                "Error: Question text must be unique within an invocation".to_string(),
            ));
        }

        let Some(options) = raw.get("options").and_then(Value::as_array) else {
            return Err((
                "invalid_params",
                format!("Error: question \"{question}\" needs an `options` array"),
            ));
        };

        if options.len() < MIN_OPTIONS {
            return Err((
                "empty_options",
                format!("Error: Each question requires at least {MIN_OPTIONS} options"),
            ));
        }

        if options.len() > MAX_OPTIONS {
            return Err((
                "too_many_options",
                format!("Error: Each question allows at most {MAX_OPTIONS} options"),
            ));
        }

        let mut labels = HashSet::new();
        let mut items = Vec::with_capacity(options.len());

        for option in options {
            let label = text_field(option, "label")?;
            if label.chars().count() > MAX_LABEL {
                return Err((
                    "invalid_params",
                    format!(
                        "Error: option label must be at most {MAX_LABEL} characters: \"{label}\""
                    ),
                ));
            }

            if RESERVED_LABELS.contains(&label.as_str()) {
                return Err((
                    "reserved_label",
                    format!(
                        "Error: Option label is reserved ({})",
                        RESERVED_LABELS.join(", ")
                    ),
                ));
            }

            if !labels.insert(label.clone()) {
                return Err((
                    "duplicate_option_label",
                    "Error: Option labels must be unique within a question".to_string(),
                ));
            }

            items.push(OptionItem {
                label,
                description: text_field(option, "description")?,
                preview: option
                    .get("preview")
                    .and_then(Value::as_str)
                    .map(normalize_newlines)
                    .filter(|p| !p.trim().is_empty()),
            });
        }

        questions.push(Question {
            question,
            header,
            multi: raw
                .get("multiSelect")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            options: items,
        });
    }

    Ok(questions)
}

/// 取一个必填字符串字段并统一换行（`\r` 会把终端光标拨回行首，撕碎选项行）。
fn text_field(raw: &Value, key: &str) -> std::result::Result<String, (&'static str, String)> {
    raw.get(key)
        .and_then(Value::as_str)
        .map(normalize_newlines)
        .ok_or((
            "invalid_params",
            format!("Error: every question and option needs a string `{key}` field"),
        ))
}

/// `prepareArguments`：把模型偶尔当字符串发来的嵌套结构（vLLM/LiteLLM 下的 GLM/Qwen）
/// 解回来；只解能解析成目标形状的，其余原样传下去让校验报真实问题。
fn prepare_arguments(args: &Value) -> Value {
    /// 字符串形式的 JSON 数组 / 对象 → 解析结果；形状不符则原样返回。
    fn decode(value: &Value, array: bool) -> Value {
        let Some(text) = value.as_str() else {
            return value.clone();
        };
        match serde_json::from_str::<Value>(text) {
            Ok(parsed) if (array && parsed.is_array()) || (!array && parsed.is_object()) => parsed,
            _ => value.clone(),
        }
    }

    let Some(questions) = args.get("questions") else {
        return args.clone();
    };

    let decoded = decode(questions, true);
    let Some(list) = decoded.as_array() else {
        return args.clone();
    };

    let list: Vec<Value> = list
        .iter()
        .map(|question| {
            let Some(obj) = question.as_object() else {
                return question.clone();
            };
            let mut out = obj.clone();
            if let Some(options) = obj.get("options") {
                out.insert("options".to_string(), decode(options, true));
            }
            Value::Object(out)
        })
        .collect();

    let mut out = args.as_object().cloned().unwrap_or_default();
    out.insert("questions".to_string(), Value::Array(list));
    Value::Object(out)
}

/// 工具入口：校验 → 入队覆盖层 → 等用户作答 → 组装结果。
async fn run(args: Value) -> ToolResult {
    let questions = match parse_questions(&args) {
        Ok(questions) => questions,
        Err((code, message)) => return reject(code, message),
    };

    if !extensions::ui_ready() {
        return reject(
            "no_ui",
            "Error: UI not available (running in non-interactive mode)".to_string(),
        );
    }

    let id = next_ui_id();
    let (tx, rx) = oneshot::channel();

    {
        let mut guard = slot_guard();
        if guard.is_some() {
            return ToolResult::error(
                "Error: another questionnaire is already open — answer or cancel it first",
            );
        }

        *guard = Some(Questionnaire {
            id,
            answers: questions.iter().map(|_| Answer::default()).collect(),
            questions: questions.clone(),
            global_note: String::new(),
            tab: 0,
            cursor: 0,
            editing: None,
            reply: Some(tx),
        });
    }

    let _guard = OverlayGuard(id);
    extensions::request_ui(ExtensionUiRequest::ShowOverlay { id });

    let outcome = rx.await.ok().flatten();
    build_result(&questions, outcome)
}

/// 校验失败的统一结果：给模型的文案 + `cancelled: true` + 错误码。
fn reject(code: &str, message: String) -> ToolResult {
    ToolResult {
        text: message,
        details: Some(json!({ "answers": [], "cancelled": true, "error": code })),
        ..ToolResult::default()
    }
}

/// 结束问卷：取走状态、回传结果、收起覆盖层。返回是否由本扩展认领。
fn finish(id: u64, outcome: Option<Submitted>) -> bool {
    let taken = {
        let mut guard = slot_guard();
        match guard.as_ref() {
            Some(q) if q.id == id => guard.take(),
            _ => None,
        }
    };

    let Some(q) = taken else {
        return false;
    };

    if let Some(reply) = q.reply {
        _ = reply.send(outcome);
    }

    extensions::request_ui(ExtensionUiRequest::HideOverlay { id });
    true
}

/// 把作答组装成工具结果：成功信封 + `details`；取消或全空折叠成唯一文案。
fn build_result(questions: &[Question], outcome: Option<Submitted>) -> ToolResult {
    let Some(submitted) = outcome else {
        return ToolResult {
            text: DECLINE.to_string(),
            details: Some(json!({ "answers": [], "cancelled": true })),
            ..ToolResult::default()
        };
    };

    let global_note = submitted.global_note.trim().to_string();
    let mut segments = Vec::new();
    let mut answers = Vec::new();
    let mut unanswered = Vec::new();

    for (index, (question, answer)) in questions.iter().zip(&submitted.answers).enumerate() {
        if !answer.answered() {
            unanswered.push(index);
            continue;
        }

        let (segment, payload) = describe_answer(index, question, answer);
        segments.push(segment);
        answers.push(payload);
    }

    if segments.is_empty() && global_note.is_empty() {
        return ToolResult {
            text: DECLINE.to_string(),
            details: Some(json!({ "answers": [], "cancelled": true })),
            ..ToolResult::default()
        };
    }

    if !unanswered.is_empty() {
        let named: Vec<String> = unanswered
            .iter()
            .map(|i| format!("\"{}\"", questions[*i].question))
            .collect();
        segments.push(format!("unanswered: {}.", named.join(", ")));
    }

    if !global_note.is_empty() {
        segments.push(format!("global note: {global_note}."));
    }

    let mut details = Map::new();
    details.insert("answers".to_string(), Value::Array(answers));
    details.insert("cancelled".to_string(), Value::Bool(false));
    if !unanswered.is_empty() {
        details.insert("unanswered".to_string(), json!(unanswered));
    }
    if !global_note.is_empty() {
        details.insert("globalNote".to_string(), Value::String(global_note));
    }

    ToolResult {
        text: format!("{ENVELOPE_PREFIX} {} {ENVELOPE_SUFFIX}", segments.join(" ")),
        details: Some(Value::Object(details)),
        ..ToolResult::default()
    }
}

/// 一道已作答的题 → 信封片段 + `details.answers[]` 里的一条。
fn describe_answer(index: usize, question: &Question, answer: &Answer) -> (String, Value) {
    let custom = answer.custom.trim().to_string();
    let note = answer.note.trim().to_string();
    let mut preview = None;

    // `scalar` 是信封里的答案串；`answer_field` 是 `details.answers[].answer`
    // （多选的 `answer` 只承载自由作答文本，勾选的选项在 `selected` 里）。
    let (kind, scalar, answer_field, selected) = if question.multi {
        let labels: Vec<String> = answer
            .picked
            .iter()
            .filter_map(|i| question.options.get(*i).map(|o| o.label.clone()))
            .collect();
        let mut joined = labels.clone();
        if !custom.is_empty() {
            joined.push(custom.clone());
        }
        let scalar = if joined.is_empty() {
            NO_INPUT.to_string()
        } else {
            joined.join(", ")
        };
        let field = if custom.is_empty() {
            Value::Null
        } else {
            Value::String(custom.clone())
        };
        ("multi", scalar, field, Some(labels))
    } else if !custom.is_empty() {
        (
            "custom",
            custom.clone(),
            Value::String(custom.clone()),
            None,
        )
    } else {
        let option = answer.picked.first().and_then(|i| question.options.get(*i));
        preview = option.and_then(|o| o.preview.clone());
        let label = option.map(|o| o.label.clone()).unwrap_or_default();
        ("option", label.clone(), Value::String(label), None)
    };

    let mut segment = format!("\"{}\"=\"{scalar}\"", question.question);
    if let Some(preview) = &preview {
        segment.push_str(&format!(". selected preview: {preview}"));
    }
    if !note.is_empty() {
        segment.push_str(&format!(". user notes: {note}"));
    }
    segment.push('.');

    let mut payload = Map::new();
    payload.insert("questionIndex".to_string(), json!(index));
    payload.insert("question".to_string(), json!(question.question));
    payload.insert("kind".to_string(), json!(kind));
    payload.insert("answer".to_string(), answer_field);
    if let Some(selected) = selected {
        payload.insert("selected".to_string(), json!(selected));
    }
    if !note.is_empty() {
        payload.insert("notes".to_string(), json!(note));
    }
    if let Some(preview) = preview {
        payload.insert("preview".to_string(), json!(preview));
    }

    (segment, Value::Object(payload))
}

/// 当前页的行数（选项行 + 自由作答行；提交页 2 行）。
fn row_count(q: &Questionnaire) -> usize {
    match q.questions.get(q.tab) {
        Some(question) => question.options.len() + 1,
        None => 2,
    }
}

/// 上下移动光标（首尾循环）。
fn step(q: &mut Questionnaire, delta: isize) {
    let rows = row_count(q);
    if rows == 0 {
        return;
    }

    q.cursor = (q.cursor as isize + delta).rem_euclid(rows as isize) as usize;
}

/// 翻页（首尾循环）并复位光标。
fn switch_tab(q: &mut Questionnaire, delta: isize) {
    let tabs = q.questions.len() + 1;
    q.tab = (q.tab as isize + delta).rem_euclid(tabs as isize) as usize;
    q.cursor = 0;
    q.editing = None;
}

/// 前进到下一题（最后一题的前进目标是提交页，不循环）。
fn advance(q: &mut Questionnaire) {
    q.tab = (q.tab + 1).min(q.questions.len());
    q.cursor = 0;
    q.editing = None;
}

/// 多选：切换当前选项的勾选状态（单选与自由作答行不响应）。
fn toggle_picked(q: &mut Questionnaire) {
    let Some(question) = q.questions.get(q.tab) else {
        return;
    };

    if !question.multi || q.cursor >= question.options.len() {
        return;
    }

    let cursor = q.cursor;
    let answer = &mut q.answers[q.tab];
    match answer.picked.iter().position(|i| *i == cursor) {
        Some(pos) => {
            answer.picked.remove(pos);
        }
        None => answer.picked.push(cursor),
    }
}

/// 覆盖层事件 → 状态变化 / 结束动作。
fn handle_event(q: &mut Questionnaire, ev: &OverlayEvent) -> Action {
    match ev {
        OverlayEvent::Key { key, .. } => {
            if q.editing.is_some() {
                // 输入行聚焦时字符由 TUI 编辑，扩展只接管 Esc（退出编辑、不取消问卷）。
                if *key == OverlayKey::Esc {
                    q.editing = None;
                    return Action::Consumed;
                }
                return Action::Pass;
            }

            match key {
                OverlayKey::Up => {
                    step(q, -1);
                    Action::Consumed
                }
                OverlayKey::Down => {
                    step(q, 1);
                    Action::Consumed
                }
                OverlayKey::Left => {
                    switch_tab(q, -1);
                    Action::Consumed
                }
                OverlayKey::Right | OverlayKey::Tab => {
                    switch_tab(q, 1);
                    Action::Consumed
                }
                OverlayKey::Char(' ') => {
                    toggle_picked(q);
                    Action::Consumed
                }
                OverlayKey::Char('n') => {
                    q.editing = Some(Edit::Note(q.tab));
                    Action::Consumed
                }
                OverlayKey::Esc => Action::Finish(None),
                OverlayKey::Enter => enter(q),
                _ => Action::Pass,
            }
        }
        OverlayEvent::Submit { text } => {
            let Some(edit) = q.editing.take() else {
                return Action::Pass;
            };

            let text = text.trim().to_string();
            match edit {
                Edit::Custom(index) => {
                    let multi = q.questions[index].multi;
                    q.answers[index].custom = text;
                    // 单选写了自己的答案就不再保留旧选项，并直接进入下一题；
                    // 多选留在原页继续勾选。
                    if !multi && !q.answers[index].custom.is_empty() {
                        q.answers[index].picked.clear();
                        advance(q);
                    }
                }
                Edit::Note(index) if index < q.questions.len() => {
                    q.answers[index].note = text;
                }
                Edit::Note(_) => q.global_note = text,
            }

            Action::Consumed
        }
        OverlayEvent::Closed => Action::Finish(None),
        _ => Action::Pass,
    }
}

/// Enter：提交页确认 / 取消；题目页选中、勾选或开始自由作答。
fn enter(q: &mut Questionnaire) -> Action {
    if q.tab >= q.questions.len() {
        return match q.cursor {
            0 => Action::Finish(Some(Submitted {
                answers: q.answers.clone(),
                global_note: q.global_note.clone(),
            })),
            _ => Action::Finish(None),
        };
    }

    let Some(question) = q.questions.get(q.tab) else {
        return Action::Pass;
    };

    if q.cursor >= question.options.len() {
        q.editing = Some(Edit::Custom(q.tab));
        return Action::Consumed;
    }

    if question.multi {
        toggle_picked(q);
        return Action::Consumed;
    }

    q.answers[q.tab].picked = vec![q.cursor];
    q.answers[q.tab].custom.clear();
    advance(q);
    Action::Consumed
}

/// 选项左侧的勾选标记：多选画方框，单选画圆点。
fn marker(multi: bool, picked: bool) -> &'static str {
    match (multi, picked) {
        (true, true) => "[x] ",
        (true, false) => "[ ] ",
        (false, true) => "(•) ",
        (false, false) => "( ) ",
    }
}

/// 构造覆盖层一帧内容（只读状态；`cols` 是终端宽度）。
fn view_for(q: &Questionnaire, cols: u16) -> OverlayView {
    let width = (cols as usize).saturating_sub(4).max(20);
    let mut header = Vec::with_capacity(2);
    header.push(tab_strip(q));

    let mut lines: Vec<DockLine> = Vec::new();
    let mut rows: Vec<usize> = Vec::new();

    match q.questions.get(q.tab) {
        Some(question) => {
            for line in wrap_text(&question.question, width) {
                header.push(styled("text", "#ffffff", line));
            }

            let answer = &q.answers[q.tab];
            for (index, option) in question.options.iter().enumerate() {
                rows.push(lines.len());
                let focused = q.cursor == index;
                let picked = answer.picked.contains(&index);
                let key = if focused { "accent" } else { "text" };
                let fallback = if focused { "#8abeb7" } else { "#ffffff" };
                lines.push(vec![DockSpan::new(
                    key,
                    fallback,
                    format!(
                        "{}{}{}",
                        if focused { "› " } else { "  " },
                        marker(question.multi, picked),
                        option.label
                    ),
                )]);

                for line in wrap_text(&option.description, width.saturating_sub(6)) {
                    lines.push(styled("dim", "#666666", format!("      {line}")));
                }
            }

            rows.push(lines.len());
            let focused = q.cursor >= question.options.len();
            let shown = if answer.custom.trim().is_empty() {
                TYPE_SOMETHING.to_string()
            } else {
                format!("{TYPE_SOMETHING} {}", answer.custom.trim())
            };
            lines.push(vec![DockSpan::new(
                if focused { "accent" } else { "dim" },
                if focused { "#8abeb7" } else { "#666666" },
                format!(
                    "{}{}{}",
                    if focused { "› " } else { "  " },
                    marker(question.multi, !answer.custom.trim().is_empty()),
                    shown
                ),
            )]);

            if !answer.note.trim().is_empty() {
                lines.push(plain(""));
                for line in wrap_text(answer.note.trim(), width.saturating_sub(6)) {
                    lines.push(styled("accent", "#8abeb7", format!("  note: {line}")));
                }
            }

            if !question.multi
                && let Some(option) = question.options.get(q.cursor)
                && let Some(preview) = &option.preview
            {
                lines.push(plain(""));
                for line in wrap_text(preview, width.saturating_sub(4)) {
                    lines.push(styled("dim", "#666666", format!("  │ {line}")));
                }
            }
        }
        None => {
            header.push(styled("accent", "#8abeb7", "Review your answers"));
            for (index, question) in q.questions.iter().enumerate() {
                let answer = &q.answers[index];
                let text = if answer.answered() {
                    summary(question, answer)
                } else {
                    format!("{}: (unanswered)", question.header)
                };
                lines.push(vec![DockSpan::new(
                    if answer.answered() { "text" } else { "dim" },
                    if answer.answered() {
                        "#ffffff"
                    } else {
                        "#666666"
                    },
                    format!("  {text}"),
                )]);
                if !answer.note.trim().is_empty() {
                    lines.push(styled(
                        "dim",
                        "#666666",
                        format!("      note: {}", answer.note.trim()),
                    ));
                }
            }

            if !q.global_note.trim().is_empty() {
                lines.push(styled(
                    "accent",
                    "#8abeb7",
                    format!("  global note: {}", q.global_note.trim()),
                ));
            }

            lines.push(plain(""));
            for (index, label) in ["Submit answers", "Cancel"].iter().enumerate() {
                rows.push(lines.len());
                let focused = q.cursor == index;
                lines.push(vec![DockSpan::new(
                    if focused { "accent" } else { "text" },
                    if focused { "#8abeb7" } else { "#ffffff" },
                    format!("{}{label}", if focused { "› " } else { "  " }),
                )]);
            }
        }
    }

    let editing = q.editing.map(|edit| match edit {
        Edit::Custom(index) => OverlayInput {
            label: "answer>".to_string(),
            value: q.answers[index].custom.clone(),
            placeholder: "your own answer, Enter to save".to_string(),
        },
        Edit::Note(index) if index < q.questions.len() => OverlayInput {
            label: "note>".to_string(),
            value: q.answers[index].note.clone(),
            placeholder: "note for this question, Enter to save".to_string(),
        },
        Edit::Note(_) => OverlayInput {
            label: "global note>".to_string(),
            value: q.global_note.clone(),
            placeholder: "note for the whole questionnaire, Enter to save".to_string(),
        },
    });

    let footer = if q.editing.is_some() {
        vec![
            ("Enter".to_string(), "save".to_string()),
            ("Esc".to_string(), "discard edit".to_string()),
        ]
    } else if let Some(question) = q.questions.get(q.tab) {
        let mut footer = Vec::new();
        if question.multi {
            footer.push(("Space/Enter".to_string(), "toggle".to_string()));
        } else {
            footer.push(("Enter".to_string(), "choose".to_string()));
        }
        if q.questions.len() > 1 {
            footer.push(("Tab/←→".to_string(), "switch question".to_string()));
        }
        footer.push(("n".to_string(), "note".to_string()));
        footer.push(("Esc".to_string(), "cancel".to_string()));
        footer
    } else {
        vec![
            ("Enter".to_string(), "confirm".to_string()),
            ("n".to_string(), "global note".to_string()),
            ("Esc".to_string(), "cancel".to_string()),
        ]
    };

    OverlayView {
        title: "ask_user_question".to_string(),
        header,
        lines,
        messages: Vec::new(),
        footer,
        input: editing,
        editor: None,
        size: OverlaySize::Panel {
            max_rows: PANEL_ROWS,
        },
        selected: rows.get(q.cursor).copied(),
        input_focus: q.editing.is_some(),
    }
}

/// 标签页条：每题一个短标题，末页是 Submit。
fn tab_strip(q: &Questionnaire) -> DockLine {
    let mut spans = Vec::new();
    for (index, question) in q.questions.iter().enumerate() {
        if index > 0 {
            spans.push(DockSpan::plain("  "));
        }
        let active = q.tab == index;
        spans.push(DockSpan::new(
            if active { "accent" } else { "dim" },
            if active { "#8abeb7" } else { "#666666" },
            format!("[{} {}]", index + 1, question.header),
        ));
    }

    spans.push(DockSpan::plain("  "));
    let active = q.tab >= q.questions.len();
    spans.push(DockSpan::new(
        if active { "accent" } else { "dim" },
        if active { "#8abeb7" } else { "#666666" },
        "[Submit]",
    ));
    spans
}

/// 提交页的一行摘要：`Header: answer`。
fn summary(question: &Question, answer: &Answer) -> String {
    let custom = answer.custom.trim();
    let text = if question.multi {
        let mut parts: Vec<String> = answer
            .picked
            .iter()
            .filter_map(|i| question.options.get(*i).map(|o| o.label.clone()))
            .collect();
        if !custom.is_empty() {
            parts.push(custom.to_string());
        }
        parts.join(", ")
    } else if !custom.is_empty() {
        custom.to_string()
    } else {
        answer
            .picked
            .first()
            .and_then(|i| question.options.get(*i))
            .map(|o| o.label.clone())
            .unwrap_or_default()
    };

    format!("{}: {text}", question.header)
}

/// 工具参数 schema。
fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "questions": {
                "type": "array",
                "minItems": 1,
                "maxItems": MAX_QUESTIONS,
                "description": "Questions to ask the user (1-4 questions)",
                "items": {
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "The complete question. Should be clear, specific and end with a question mark. Example: \"Which library should we use for date formatting?\" If multiSelect is true, phrase it accordingly, e.g. \"Which features do you want to enable?\""
                        },
                        "header": {
                            "type": "string",
                            "maxLength": MAX_HEADER,
                            "description": format!("MAX {MAX_HEADER} CHARACTERS — hard limit, requests over the limit are rejected. Very short chip/tag shown next to the question. Examples: \"Auth method\", \"Library\", \"Approach\".")
                        },
                        "options": {
                            "type": "array",
                            "minItems": MIN_OPTIONS,
                            "maxItems": MAX_OPTIONS,
                            "description": "The available choices for this question. Must have 2-4 options, each a distinct choice (unless multiSelect is enabled). The 'Type something.' row is appended automatically — do NOT author it.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "label": {
                                        "type": "string",
                                        "maxLength": MAX_LABEL,
                                        "description": format!("MAX {MAX_LABEL} CHARACTERS — hard limit, requests over the limit are rejected. The display text for this option that the user will see and select. Should be concise (1-5 words) and clearly describe the choice.")
                                    },
                                    "description": {
                                        "type": "string",
                                        "description": "Explanation of what this option means or what will happen if chosen. Useful for providing context about trade-offs or implications."
                                    },
                                    "preview": {
                                        "type": "string",
                                        "description": "Optional preview content shown under the option list when this option is focused. Use for mockups, code snippets or visual comparisons. Single-select questions only."
                                    }
                                },
                                "required": ["label", "description"]
                            }
                        },
                        "multiSelect": {
                            "type": "boolean",
                            "description": "Set to true to allow the user to select multiple options instead of just one. Use when choices are not mutually exclusive."
                        }
                    },
                    "required": ["question", "header", "options"]
                }
            }
        },
        "required": ["questions"]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::{set_ui_ready, take_pending_ui};

    /// 串行化用例：问卷状态与 UI 就绪标志都是进程级全局态。
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// 跑一个 async 测试体（独立 runtime，避免与其它测试共享全局 runtime）。
    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(fut)
    }

    /// 取走队列里第一条 ShowOverlay 请求的 id（丢弃其它请求）。
    async fn take_overlay_id() -> u64 {
        for _ in 0..200 {
            if let Some(req) = take_pending_ui() {
                if let ExtensionUiRequest::ShowOverlay { id } = req {
                    return id;
                }
                continue;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("no ShowOverlay request was queued");
    }

    /// 一个选项的 JSON。
    fn option(label: &str) -> Value {
        json!({ "label": label, "description": format!("{label} description") })
    }

    /// 一道题的 JSON。
    fn question(question: &str, multi: bool, labels: &[&str]) -> Value {
        json!({
            "question": question,
            "header": "Pick",
            "multiSelect": multi,
            "options": labels.iter().map(|l| option(l)).collect::<Vec<_>>(),
        })
    }

    /// 校验按上游口径报错：空 / 超量 / 重复 / 选项不足 / 保留标签 / 重复标签。
    #[test]
    fn validation_reports_the_same_codes() {
        let cases = [
            (json!({ "questions": [] }), "no_questions"),
            (
                json!({ "questions": (0..5).map(|i| question(&format!("q{i}"), false, &["a", "b"])).collect::<Vec<_>>() }),
                "too_many_questions",
            ),
            (
                json!({ "questions": [question("same", false, &["a", "b"]), question("same", false, &["c", "d"])] }),
                "duplicate_question",
            ),
            (
                json!({ "questions": [question("q", false, &["only"])] }),
                "empty_options",
            ),
            (
                json!({ "questions": [question("q", false, &["a", "b", "c", "d", "e"])] }),
                "too_many_options",
            ),
            (
                json!({ "questions": [question("q", false, &["Other", "b"])] }),
                "reserved_label",
            ),
            (
                json!({ "questions": [question("q", false, &[TYPE_SOMETHING, "b"])] }),
                "reserved_label",
            ),
            (
                json!({ "questions": [question("q", false, &["a", "a"])] }),
                "duplicate_option_label",
            ),
        ];

        for (args, expected) in cases {
            match parse_questions(&args) {
                Err((code, message)) => {
                    assert_eq!(code, expected, "payload: {args}");
                    assert!(
                        message.starts_with("Error: "),
                        "文案要能被模型读懂: {message}"
                    );
                }
                Ok(_) => panic!("{expected} 应当被拒绝：{args}"),
            }
        }

        assert!(
            parse_questions(&json!({ "questions": [question("q", true, &["a", "b"])] })).is_ok(),
            "合法载荷应当通过"
        );
    }

    /// 字符串化的嵌套结构（GLM/Qwen 常见）在 `prepare_arguments` 里被解回来。
    #[test]
    fn prepare_arguments_decodes_stringified_nesting() {
        let options = json!([option("a"), option("b")]).to_string();
        let questions = json!([{
            "question": "q",
            "header": "h",
            "options": options,
        }])
        .to_string();

        let args = prepare_arguments(&json!({ "questions": questions }));
        let parsed = parse_questions(&args).expect("解回来的参数应当合法");
        assert_eq!(parsed[0].options.len(), 2);

        // 形状不符的字符串原样保留，让校验报真实问题。
        let untouched = prepare_arguments(&json!({ "questions": "not json" }));
        assert_eq!(untouched["questions"], json!("not json"));
    }

    /// 作答信封：已答 / 未答 / 备注 / 全局备注各就其位。
    #[test]
    fn envelope_names_answers_and_gaps() {
        let questions = parse_questions(&json!({
            "questions": [
                question("Which one?", false, &["a", "b"]),
                question("Which ones?", true, &["x", "y"]),
            ]
        }))
        .expect("valid");

        let submitted = Submitted {
            answers: vec![
                Answer {
                    picked: vec![1],
                    custom: String::new(),
                    note: "  prefer b  ".to_string(),
                },
                Answer::default(),
            ],
            global_note: " ship it ".to_string(),
        };

        let result = build_result(&questions, Some(submitted));
        assert!(result.text.starts_with(ENVELOPE_PREFIX));
        assert!(result.text.contains("\"Which one?\"=\"b\""));
        assert!(result.text.contains("user notes: prefer b"));
        assert!(result.text.contains("unanswered: \"Which ones?\"."));
        assert!(result.text.contains("global note: ship it."));
        assert!(result.text.ends_with(ENVELOPE_SUFFIX));

        let details = result.details.expect("details");
        assert_eq!(details["answers"][0]["kind"], "option");
        assert_eq!(details["answers"][0]["notes"], "prefer b");
        assert_eq!(details["unanswered"], json!([1]));
        assert_eq!(details["globalNote"], "ship it");
    }

    /// 取消与「全空」都折叠成同一句 decline；多选带上 `selected`，自由作答走 `custom`。
    #[test]
    fn cancel_and_multi_and_custom_shapes() {
        let questions = parse_questions(&json!({
            "questions": [
                question("Multi?", true, &["x", "y"]),
                question("Free?", false, &["a", "b"]),
            ]
        }))
        .expect("valid");

        let cancelled = build_result(&questions, None);
        assert_eq!(cancelled.text, DECLINE);
        assert_eq!(cancelled.details.expect("details")["cancelled"], true);

        let empty = build_result(
            &questions,
            Some(Submitted {
                answers: vec![Answer::default(), Answer::default()],
                global_note: "   ".to_string(),
            }),
        );
        assert_eq!(empty.text, DECLINE);

        let multi = build_result(
            &questions,
            Some(Submitted {
                answers: vec![
                    Answer {
                        picked: vec![0, 1],
                        custom: "and z".to_string(),
                        note: String::new(),
                    },
                    Answer {
                        picked: Vec::new(),
                        custom: "written".to_string(),
                        note: String::new(),
                    },
                ],
                global_note: String::new(),
            }),
        );

        assert!(multi.text.contains("\"Multi?\"=\"x, y, and z\""));
        assert!(multi.text.contains("\"Free?\"=\"written\""));
        let details = multi.details.expect("details");
        assert_eq!(details["answers"][0]["kind"], "multi");
        assert_eq!(details["answers"][0]["selected"], json!(["x", "y"]));
        assert_eq!(details["answers"][0]["answer"], "and z");
        assert_eq!(details["answers"][1]["kind"], "custom");
    }

    /// 单选：Enter 选中并翻到提交页，再 Enter 提交；作答经 oneshot 回到工具结果。
    #[test]
    fn single_select_enter_answers_and_submits() {
        let _g = lock();
        while take_pending_ui().is_some() {}
        assert!(slot_guard().is_none(), "上一个用例应已清理");
        set_ui_ready(true);

        let ext = AskUserQuestion;
        let args = json!({ "questions": [question("Which?", false, &["a", "b"])] });

        let result = block_on(async {
            let waiting = tokio::spawn(async move { run(args).await });
            let id = take_overlay_id().await;

            assert!(ext.on_overlay_event(
                id,
                &OverlayEvent::Key {
                    key: OverlayKey::Down,
                    input: None
                }
            ));
            assert!(ext.on_overlay_event(
                id,
                &OverlayEvent::Key {
                    key: OverlayKey::Enter,
                    input: None
                }
            ));
            // 已翻到提交页：先按 n 写全局备注，再确认。
            assert!(ext.on_overlay_event(
                id,
                &OverlayEvent::Key {
                    key: OverlayKey::Char('n'),
                    input: None
                }
            ));
            assert!(ext.on_overlay_event(
                id,
                &OverlayEvent::Submit {
                    text: "ok".to_string()
                }
            ));
            assert!(ext.on_overlay_event(
                id,
                &OverlayEvent::Key {
                    key: OverlayKey::Enter,
                    input: None
                }
            ));

            waiting.await.expect("tool task")
        });

        set_ui_ready(false);
        assert!(result.text.contains("\"Which?\"=\"b\""), "{}", result.text);
        assert!(result.text.contains("global note: ok."));
        assert!(slot_guard().is_none(), "提交后问卷状态应清空");
    }

    /// 多选：Space 勾选、Tab 翻页、逐题备注；未作答的题进 `unanswered`。
    #[test]
    fn multi_select_toggles_and_notes_are_kept() {
        let _g = lock();
        while take_pending_ui().is_some() {}
        set_ui_ready(true);

        let ext = AskUserQuestion;
        let args = json!({
            "questions": [
                question("Which ones?", true, &["x", "y"]),
                question("Leave blank?", false, &["a", "b"]),
            ]
        });

        let result = block_on(async {
            let waiting = tokio::spawn(async move { run(args).await });
            let id = take_overlay_id().await;

            let key = |key| OverlayEvent::Key { key, input: None };
            assert!(ext.on_overlay_event(id, &key(OverlayKey::Char(' '))));
            assert!(ext.on_overlay_event(id, &key(OverlayKey::Char('n'))));
            assert!(ext.on_overlay_event(
                id,
                &OverlayEvent::Submit {
                    text: "first note".to_string()
                }
            ));
            assert!(ext.on_overlay_event(id, &key(OverlayKey::Tab)));
            // 第二题不答，直接 Tab 到提交页。
            assert!(ext.on_overlay_event(id, &key(OverlayKey::Tab)));
            assert!(ext.on_overlay_event(id, &key(OverlayKey::Enter)));

            waiting.await.expect("tool task")
        });

        set_ui_ready(false);
        assert!(
            result.text.contains("\"Which ones?\"=\"x\""),
            "{}",
            result.text
        );
        assert!(result.text.contains("user notes: first note"));
        assert!(result.text.contains("unanswered: \"Leave blank?\"."));
    }

    /// Esc 取消：退出编辑只收起输入行，再按一次才取消整份问卷。
    #[test]
    fn esc_cancels_and_editing_esc_only_closes_the_input() {
        let _g = lock();
        while take_pending_ui().is_some() {}
        set_ui_ready(true);

        let ext = AskUserQuestion;
        let args = json!({ "questions": [question("Which?", false, &["a", "b"])] });

        let result = block_on(async {
            let waiting = tokio::spawn(async move { run(args).await });
            let id = take_overlay_id().await;

            // 落到自由作答行 → 开始编辑 → Esc 只退出编辑。
            let key = |key| OverlayEvent::Key { key, input: None };
            ext.on_overlay_event(id, &key(OverlayKey::Down));
            ext.on_overlay_event(id, &key(OverlayKey::Down));
            ext.on_overlay_event(id, &key(OverlayKey::Enter));
            let view = ext.overlay_view(id, 80).expect("问卷仍在");
            assert!(view.input.is_some(), "编辑态要有输入行");
            assert!(ext.on_overlay_event(id, &key(OverlayKey::Esc)));
            assert!(
                ext.overlay_view(id, 80).expect("问卷仍在").input.is_none(),
                "Esc 只应退出编辑"
            );

            assert!(ext.on_overlay_event(id, &key(OverlayKey::Esc)));
            waiting.await.expect("tool task")
        });

        set_ui_ready(false);
        assert_eq!(result.text, DECLINE);
        assert_eq!(result.details.expect("details")["cancelled"], true);
    }

    /// 没有 UI 时直接报 `no_ui`，不入队覆盖层也不卡住。
    #[test]
    fn without_ui_the_tool_rejects() {
        let _g = lock();
        while take_pending_ui().is_some() {}
        set_ui_ready(false);

        let result = block_on(run(
            json!({ "questions": [question("Which?", false, &["a", "b"])] }),
        ));
        assert!(result.text.contains("non-interactive"));
        assert_eq!(result.details.expect("details")["error"], "no_ui");
        assert!(take_pending_ui().is_none());
    }

    /// 覆盖层只认领自己的 id，且提交页有可选行。
    #[test]
    fn overlay_view_only_answers_for_its_own_id() {
        let _g = lock();
        let ext = AskUserQuestion;
        let questions =
            parse_questions(&json!({ "questions": [question("Which?", false, &["a", "b"])] }))
                .expect("valid");
        *slot_guard() = Some(Questionnaire {
            id: 7,
            answers: vec![Answer::default()],
            questions,
            global_note: String::new(),
            tab: 0,
            cursor: 0,
            editing: None,
            reply: None,
        });

        let view = ext.overlay_view(7, 80).expect("认领 id=7");
        assert_eq!(
            view.size,
            OverlaySize::Panel {
                max_rows: PANEL_ROWS
            }
        );
        assert!(!view.lines.is_empty());
        assert!(view.selected.is_some(), "选中行要能被 TUI 跟进视口");
        assert!(ext.overlay_view(8, 80).is_none(), "未认领的 id 返回 None");

        *slot_guard() = None;
    }
}
