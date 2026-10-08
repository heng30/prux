//! `/agents → Create new agent` 向导：用一套分步表单收集并落盘一个新的子代理定义。
//!
//! 何时使用：在 `/agents` 覆盖层里选中 `Create new agent`（`fleet::open_wizard`）时打开一个
//! 向导覆盖层；此后该覆盖层的事件（`OverlayEvent::Key` / `EditorSubmit`）都转到这里的状态机，
//! 写完落盘或按 Esc 关闭即结束。产出文件为 `<cwd>/<PROJECT_SCOPE_NAME>/agents/<name>.md`
//! （项目级）或 `<agent_dir()>/extensions/agent/<name>.md`（个人级），格式与 `agent_files`/`agent_types`
//! 的读取路径一致：YAML frontmatter + 系统提示正文。
//!
//! 实现要点：纯扩展侧，用覆盖层 + `on_overlay_event` 自持一个多步状态机（[`WizardState`]），
//! **不走** TUI 的内联输入（那个只支持单行且 focus 有边沿语义），而是自己处理
//! `Char`/`Backspace`，这样每一步的推进/回退都不依赖 TUI 的 focus 边沿。
//!
//! 两条落盘路径：Enter 在汇总步（[`Step::Review`]）**直接写文件**（manual）；
//! `g` 在汇总步把一份生成提示交给主模型，由模型用 `write` 工具落地——系统提示正文交给模型写。
//! manual 路径的 `Description` 兼作初始系统提示。
//!
//! 步骤定义见 [`Step`]：成员判别值即下标，名字与字符串的互转由 `strum` 派生。

use super::{
    agent_files,
    types::{AgentSource, AgentType, PromptMode},
    util_notify,
};
use crate::{
    core::{
        extensions::{
            DockLine, DockSpan, ExtensionUiRequest, OverlayEditor, OverlayKey, OverlaySize,
            OverlayView, UiNotifyLevel, request_ui,
        },
        project_trust::is_project_trusted,
        provider::AgentMessage,
        settings_manager::agent_dir,
    },
    utils::glyphs::{DEF_ARROW_UP_DOWN, DEF_SELECTED_MARK},
};
use std::path::{Path, PathBuf};
use strum::IntoEnumIterator;
use strum_macros::{EnumIter, EnumString, IntoStaticStr};

/// 写入目标根的展示名（项目级 / 个人级）。
const TARGETS: [&str; 2] = [
    concat!("project(.", env!("CARGO_PKG_NAME"), "/agents/"),
    "personal (<agent_dir>/extensions/agent/)",
];

/// 思考级别（`inherit` = 不写该字段）。
const THINKING: [&str; 8] = [
    "inherit", "off", "minimal", "low", "medium", "high", "xhigh", "max",
];

/// 工具输入提示。
const TOOLS_HINT: &str = "all | none | comma list (e.g. read, bash, grep, find, ls)";

/// 型号输入提示。
const MODEL_HINT: &str = "provider/modelId, or empty to inherit";

/// 向导步骤：成员判别值即下标（0 起），
/// 推进/回退由 [`Step::next`]/[`Step::prev`] 维持，末尾的 [`Step::Review`] 是汇总页。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, EnumString, IntoStaticStr)]
#[strum(ascii_case_insensitive)]
pub(crate) enum Step {
    /// 写入目标根（项目级 / 个人级）
    Location = 0,
    /// 类型名
    Name = 1,
    /// 简述（manual 路径兼作初始系统提示）
    Description = 2,
    /// 工具白名单（逗号分隔）
    Tools = 3,
    /// 型号（空 = 继承）
    Model = 4,
    /// 思考档位
    Thinking = 5,
    /// 系统提示正文（这一步用核心多行编辑器，而非逐键单行输入）
    #[strum(
        to_string = "System prompt",
        serialize = "SystemPrompt",
        serialize = "system_prompt"
    )]
    SystemPrompt = 6,
    /// 汇总/待写页：Enter 写盘、`g` 生成；不占表单行
    Review = 7,
}

impl Step {
    /// 表单步骤数（不含汇总步；汇总步的判别值恰好排在最后一个表单步之后）。
    const COUNT: usize = Step::Review as usize;

    /// 规范名（`strum` 派生）：表单行标签与标题都用它。
    fn as_str(self) -> &'static str {
        self.into()
    }

    /// 表单行位置（1 起，用于标题的 `(n/COUNT)`）；汇总步与最后一个表单步同为 `COUNT`。
    fn number(self) -> usize {
        (self as usize + 1).min(Self::COUNT)
    }

    /// 下一步；`System prompt` 之后是汇总页，汇总页不再前进。
    fn next(self) -> Step {
        match self {
            Step::Location => Step::Name,
            Step::Name => Step::Description,
            Step::Description => Step::Tools,
            Step::Tools => Step::Model,
            Step::Model => Step::Thinking,
            Step::Thinking => Step::SystemPrompt,
            Step::SystemPrompt | Step::Review => Step::Review,
        }
    }

    /// 上一步；`Location` 到顶（Esc 交给 TUI 关闭覆盖层），汇总页回到 `System prompt`。
    fn prev(self) -> Step {
        match self {
            Step::Location => Step::Location,
            Step::Name => Step::Location,
            Step::Description => Step::Name,
            Step::Tools => Step::Description,
            Step::Model => Step::Tools,
            Step::Thinking => Step::Model,
            Step::SystemPrompt => Step::Thinking,
            Step::Review => Step::SystemPrompt,
        }
    }

    /// 表单行（不含汇总步），按声明顺序遍历 —— 顺序即判别值顺序。
    fn fields() -> impl Iterator<Item = Step> {
        Step::iter().filter(|step| *step != Step::Review)
    }
}

/// 手动向导状态（存在 `fleet` 的覆盖层状态里）。
#[derive(Clone, Debug)]
pub(crate) struct WizardState {
    /// 当前步骤；[`Step::Review`] = 汇总/待写
    pub(crate) step: Step,
    /// 写入目标根（`TARGETS` 下标）。
    target: usize,
    /// 类型名输入。
    name: String,
    /// 简述输入。
    description: String,
    /// 工具白名单输入（逗号分隔）。
    tools: String,
    /// 型号输入（空 = 继承）。
    model: String,
    /// 思考档位（`THINKING` 下标）。
    thinking: usize,
    /// 系统提示正文（多行；空 = 用 Description 派生）
    system_prompt: String,
    /// 当前错误（渲染成红色警示行；下次按键清除）
    error: Option<String>,
}

impl Default for WizardState {
    /// 初始向导：停在 Location 步，工具白名单预填默认集合。
    fn default() -> Self {
        WizardState {
            step: Step::Location,
            target: 0,
            name: String::new(),
            description: String::new(),
            tools: "read, bash, grep, find, ls".to_string(),
            model: String::new(),
            thinking: 0,
            system_prompt: String::new(),
            error: None,
        }
    }
}

impl WizardState {
    /// 丢弃全部已输入内容，回到初始状态（Esc 取消时用）。
    fn restart(&mut self) {
        *self = WizardState::default();
    }

    /// 当前步骤是不是循环选择（Up/Down 改值），而不是自由文本。
    fn is_choice(&self) -> bool {
        matches!(self.step, Step::Location | Step::Thinking)
    }

    /// 在当前循环选择步上移动 `delta` 格（越界回绕）；非选择步不做事。
    fn cycle(&mut self, delta: isize) {
        let (len, slot): (usize, &mut usize) = match self.step {
            Step::Location => (TARGETS.len(), &mut self.target),
            Step::Thinking => (THINKING.len(), &mut self.thinking),
            _ => return,
        };
        let cur = *slot as isize;
        *slot = (cur + delta).rem_euclid(len as isize) as usize;
    }

    /// 当前文本步对应的输入缓冲区可变引用；选择/汇总步（无输入框）返回 None。
    fn text_mut(&mut self) -> Option<&mut String> {
        match self.step {
            Step::Name => Some(&mut self.name),
            Step::Description => Some(&mut self.description),
            Step::Tools => Some(&mut self.tools),
            Step::Model => Some(&mut self.model),
            _ => None,
        }
    }

    /// 多行编辑器保存（Ctrl+S）：记录正文并推进到汇总步。
    pub(crate) fn editor_submit(&mut self, text: String) {
        if self.step == Step::SystemPrompt {
            self.system_prompt = text;
            self.step = self.step.next();
        }
    }

    /// 处理一个覆盖层按键；返回 `true` = 已消费（TUI 不再走默认语义）。
    ///
    /// `id` 用于写入/生成成功后自行关闭覆盖层。
    pub(crate) fn key(&mut self, id: u64, key: &OverlayKey) -> bool {
        use OverlayKey as K;

        self.error = None;

        // 汇总步的 `g` = Generate （把生成提示交给主模型）
        if self.step == Step::Review
            && let K::Char('g') = key
        {
            self.generate(id);
            return true;
        }

        match key {
            K::Esc => {
                if self.step == Step::Location {
                    return false; // 第一步 Esc = 让 TUI 关闭
                }
                self.step = self.step.prev();
                true
            }
            K::Up if self.is_choice() => {
                self.cycle(-1);
                true
            }
            K::Down if self.is_choice() => {
                self.cycle(1);
                true
            }
            K::Backspace => {
                if let Some(t) = self.text_mut() {
                    t.pop();
                    true
                } else {
                    false
                }
            }
            K::Char(c) => {
                if let Some(t) = self.text_mut() {
                    if !c.is_control() {
                        t.push(*c);
                    }
                    true
                } else {
                    false
                }
            }
            K::Enter => {
                self.advance(id);
                true
            }
            _ => false,
        }
    }

    /// Enter：校验并推进；最后两步（系统提示步 / 汇总步）写文件。
    fn advance(&mut self, id: u64) {
        match self.step {
            Step::Name if self.name.trim().is_empty() => {
                self.error = Some("Name is required".to_string());
            }
            Step::Name if !valid_name(self.name.trim()) => {
                self.error =
                    Some("Name may only contain letters, digits, '.', '_', '-'".to_string());
            }
            // 系统提示步的正文由多行编辑器的 Ctrl+S 保存，这里 Enter 直接落盘（不覆盖已存在文件）
            Step::SystemPrompt | Step::Review => match self.write() {
                Ok(path) => {
                    util_notify(
                        &format!("Created agent definition at {}", path.display()),
                        UiNotifyLevel::Info,
                    );
                    self.restart();
                    request_ui(ExtensionUiRequest::HideOverlay { id });
                }
                Err(e) => self.error = Some(e),
            },
            step => self.step = step.next(),
        }
    }

    /// 汇总步 `g`：把一份生成提示交给主模型。模型用 `write` 工具在目标路径落地；向导本身不写盘。
    fn generate(&mut self, id: u64) {
        let cwd = super::discovery_cwd();
        let project = self.target == 0;
        if project {
            let agent_dir = agent_dir();
            if !is_project_trusted(Path::new(&cwd), &agent_dir) {
                self.error = Some(
                    "project is not trusted; choose personal or trust the project".to_string(),
                );
                return;
            }
        }

        let name = self.name.trim().to_string();
        let path = agent_files::eject_path(&name, &cwd, project);
        if path.exists() {
            self.error = Some(format!(
                "{} already exists; delete it first or edit it directly",
                path.display()
            ));
            return;
        }

        let tools = self.tools.trim();
        let tools = if tools.is_empty() { "all" } else { tools };
        let model = if self.model.trim().is_empty() {
            "inherit".to_string()
        } else {
            self.model.trim().to_string()
        };

        let provided = self.system_prompt.trim();
        let body_hint = if provided.is_empty() {
            "Write a focused system prompt that makes the agent good at that purpose.".to_string()
        } else {
            format!(
                "Use this as the system prompt body (refine it if useful):\n---\n{provided}\n---"
            )
        };

        let prompt = format!(
            "Create a new sub-agent definition file at {path}.\n\n\
             Write it as YAML frontmatter delimited by `---` lines, then a blank line, \
             then the agent's system prompt body. Frontmatter fields:\n\
             - name: {name}\n\
             - description: a one-line summary of when to use it\n\
             - tools: {tools}\n\
             - thinking: {thinking}\n\
             - model: {model}\n\n\
             Intended purpose: {description}\n\n\
             {body_hint} \
             Use the write tool to create the file, then confirm the path.",
            path = path.display(),
            name = name,
            tools = tools,
            thinking = THINKING[self.thinking],
            model = model,
            description = self.description.trim(),
            body_hint = body_hint,
        );

        request_ui(ExtensionUiRequest::Continuation {
            message: Box::new(AgentMessage::user_text(&prompt)),
        });

        self.restart();

        request_ui(ExtensionUiRequest::HideOverlay { id });
    }

    /// 写 `<target>/<name>.md`（不覆盖已存在文件）。
    fn write(&self) -> Result<PathBuf, String> {
        let cwd = super::discovery_cwd();
        let project = self.target == 0;
        if project {
            let agent_dir = agent_dir();
            if !is_project_trusted(Path::new(&cwd), &agent_dir) {
                return Err(
                    "project is not trusted; choose personal or trust the project".to_string(),
                );
            }
        }

        let name = self.name.trim().to_string();
        let path = agent_files::eject_path(&name, &cwd, project);
        if path.exists() {
            return Err(format!(
                "{} already exists; delete it first or edit it directly",
                path.display()
            ));
        }

        let ty = self.to_agent_type(project);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        std::fs::write(&path, agent_files::serialize_agent_file(&ty))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(path)
    }

    /// 把向导表单转成 `AgentType`：解析工具白名单（空/all/\* = 全部，none = 空）、
    /// 推导 system_prompt 与思考档位；`project` 决定来源标记为项目级还是用户级。
    fn to_agent_type(&self, project: bool) -> AgentType {
        let tools = match self.tools.trim().to_ascii_lowercase().as_str() {
            "" | "all" | "*" => None,
            "none" => Some(Vec::new()),
            _ => Some(
                self.tools
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect(),
            ),
        };
        let instructions = self.description.trim();
        let provided = self.system_prompt.trim();
        let system_prompt = if !provided.is_empty() {
            provided.to_string()
        } else if instructions.is_empty() {
            format!(
                "You are {} — a specialized sub-agent. Follow the task you are given and report back concisely.",
                self.name.trim()
            )
        } else {
            instructions.to_string()
        };

        AgentType {
            name: self.name.trim().to_string(),
            display_name: String::new(),
            description: self.description.trim().to_string(),
            color: None,
            tools,
            model: Some(self.model.trim().to_string()).filter(|m| !m.is_empty()),
            thinking: (self.thinking != 0).then(|| THINKING[self.thinking].to_string()),
            max_turns: None,
            prompt_mode: PromptMode::Replace,
            enabled: true,
            persist_session: None,
            session_dir: None,
            disallowed_tools: None,
            allowed_subagents: None,
            memory: None,
            isolated: None,
            isolation: None,
            extensions: None,
            extensions_none: false,
            exclude_extensions: Vec::new(),
            skills: None,
            output_transcript: None,
            run_in_background: None,
            inherit_context: None,
            system_prompt,
            source: if project {
                AgentSource::Project
            } else {
                AgentSource::Global
            },
            source_path: None,
            ignored_fields: Vec::new(),
        }
    }

    /// 表单行展示的当前值（当前行额外附上输入提示）。
    fn row_value(&self, step: Step) -> String {
        let current = self.step == step;
        match step {
            Step::Location => TARGETS[self.target].to_string(),
            Step::Name => self.name.clone(),
            Step::Description => self.description.clone(),
            Step::Tools if current => format!("{}   ({TOOLS_HINT})", self.tools),
            Step::Tools => self.tools.clone(),
            Step::Model if self.model.is_empty() && current => {
                format!("(inherit)   ({MODEL_HINT})")
            }
            Step::Model if self.model.is_empty() => "(inherit)".to_string(),
            Step::Model => self.model.clone(),
            Step::Thinking => THINKING[self.thinking].to_string(),
            Step::SystemPrompt => self.prompt_preview(),
            Step::Review => String::new(), // 汇总步不占表单行
        }
    }

    /// 系统提示行预览：正文首行 +（多行时）剩余行数；空正文回退到 Description。
    fn prompt_preview(&self) -> String {
        match self.system_prompt.lines().next() {
            Some(l) if !l.trim().is_empty() => {
                let count = self.system_prompt.lines().count();
                if count > 1 {
                    format!("{} … (+{} lines)", l.trim(), count - 1)
                } else {
                    l.trim().to_string()
                }
            }
            _ => "(derived from Description)".to_string(),
        }
    }

    /// 渲染当前帧。
    pub(crate) fn view(&self) -> OverlayView {
        let mut lines: Vec<DockLine> = Vec::new();
        let field = |label: &str, value: String, current: bool| -> DockLine {
            let mark = if current { DEF_SELECTED_MARK } else { "  " };
            let key = if current { "accent" } else { "muted" };
            let fallback = if current { "#8abeb7" } else { "#999999" };
            vec![
                DockSpan::new(key, fallback, mark),
                DockSpan::new("text", "#d0d0d0", format!("{label}: ")),
                DockSpan::plain(value),
            ]
        };
        for step in Step::fields() {
            lines.push(field(
                step.as_str(),
                self.row_value(step),
                self.step == step,
            ));
        }
        if self.step == Step::Review {
            lines.push(vec![DockSpan::new(
                "muted",
                "#999999",
                "  Enter: write manually · g: Generate with Claude · Esc: back",
            )]);
        }
        if let Some(err) = &self.error {
            lines.push(vec![DockSpan::new(
                "error",
                "#cc6666",
                format!("  ⚠ {err}"),
            )]);
        }
        let title = if self.step == Step::Review {
            format!("Create agent · review ({}/{})", Step::COUNT, Step::COUNT)
        } else {
            format!(
                "Create agent · {} ({}/{})",
                self.step.as_str(),
                self.step.number(),
                Step::COUNT
            )
        };
        let editing = self.step == Step::SystemPrompt;
        let editor = editing.then(|| OverlayEditor {
            label: Step::SystemPrompt.as_str().to_string(),
            value: self.system_prompt.clone(),
            placeholder: "(empty — falls back to the Description)".to_string(),
            rows: 5,
            generation: 0,
        });
        let footer = if editing {
            vec![
                ("Ctrl+S".to_string(), "save & continue".to_string()),
                ("Esc".to_string(), "back".to_string()),
            ]
        } else {
            vec![
                (DEF_ARROW_UP_DOWN.to_string(), "choose".to_string()),
                ("type".to_string(), "edit".to_string()),
                ("Enter".to_string(), "next".to_string()),
                ("Esc".to_string(), "back".to_string()),
            ]
        };
        OverlayView {
            title,
            lines,
            footer,
            input: None,
            editor,
            size: OverlaySize::Inline { max_rows: 14 },
            header: Vec::new(),
            messages: Vec::new(),
            selected: None,
            input_focus: false,
        }
    }
}

/// 文件名安全：字母/数字/`.`/`_`/`-`，且非空、有字母数字。
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().any(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::OverlayKey as K;
    use crate::test_support::AgentDirGuard;

    fn type_str(w: &mut WizardState, s: &str) {
        for c in s.chars() {
            w.key(0, &K::Char(c));
        }
    }

    #[test]
    fn name_validation() {
        assert!(valid_name("Explore"));
        assert!(valid_name("my-agent.v2"));
        assert!(!valid_name(""));
        assert!(!valid_name("../../etc/passwd"));
        assert!(!valid_name("a b"));
        assert!(!valid_name("---"));
    }

    #[test]
    fn step_machine_advances_and_goes_back() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut w = WizardState::default();
        // 第一步 Esc 不消费（让 TUI 关闭）
        assert!(!w.key(0, &K::Esc));
        // Location 是循环选择
        w.key(0, &K::Down);
        assert_eq!(w.target, 1);
        w.key(0, &K::Up);
        assert_eq!(w.target, 0);
        // 空名字不能推进：Enter 从 Location → Name，再 Enter 停住
        w.key(0, &K::Enter);
        assert_eq!(w.step, Step::Name);
        w.key(0, &K::Enter);
        assert_eq!(w.step, Step::Name, "Name 为空不应推进");
        assert!(w.error.is_some());
        // 输入名字后推进
        type_str(&mut w, "auditor");
        w.key(0, &K::Enter);
        assert_eq!(w.step, Step::Description);
        // Esc 回退
        w.key(0, &K::Esc);
        assert_eq!(w.step, Step::Name);
        assert_eq!(w.name, "auditor");
    }

    /// strum 派生的双向转换与判别值：名字 ↔ 变体、判别值即表单顺序。
    #[test]
    fn step_strum_conversions() {
        // 变体 → 规范名（表单标签、标题）
        assert_eq!(Step::Location.as_str(), "Location");
        assert_eq!(Step::Thinking.as_str(), "Thinking");
        assert_eq!(Step::SystemPrompt.as_str(), "System prompt");
        // 名字 → 变体（大小写不敏感，兼容 `system_prompt` 写法）
        assert_eq!(
            "System prompt".parse::<Step>().ok(),
            Some(Step::SystemPrompt)
        );
        assert_eq!(
            "SYSTEM_PROMPT".parse::<Step>().ok(),
            Some(Step::SystemPrompt)
        );
        assert_eq!("review".parse::<Step>().ok(), Some(Step::Review));
        assert_eq!("nope".parse::<Step>().ok(), None);
        // 判别值即下标，`iter()` 按声明顺序覆盖全部成员
        let values: Vec<usize> = Step::iter().map(|s| s as usize).collect();
        assert_eq!(values, (0..=Step::Review as usize).collect::<Vec<_>>());
        assert_eq!(Step::COUNT, 7, "表单步骤不含汇总步");
        assert_eq!(Step::fields().count(), Step::COUNT);
        assert_eq!(Step::SystemPrompt.number(), Step::COUNT);
        assert_eq!(Step::Location.number(), 1);
        assert_eq!(Step::Review.next(), Step::Review);
        assert_eq!(Step::Location.prev(), Step::Location);
    }

    #[test]
    fn write_creates_a_parseable_agent_file() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = AgentDirGuard::temp();
        let w = WizardState {
            target: 1,
            name: "auditor".to_string(),
            description: "Audit code".to_string(),
            thinking: 4, // medium
            ..WizardState::default()
        };
        let path = w.write().expect("写盘");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("name: auditor"), "{text}");
        assert!(text.contains("description: \"Audit code\"") || text.contains("Audit code"));
        assert!(text.contains("thinking: medium"), "{text}");
        assert!(path.starts_with(crate::core::settings_manager::agent_dir()));
        // 通过发现解析回一个真实类型
        let roster = super::super::agent_types::discover(".");
        let ty = roster
            .types
            .iter()
            .find(|t| t.name == "auditor")
            .expect("向导写出的文件应可被发现");
        assert_eq!(ty.description, "Audit code");
    }

    #[test]
    fn generate_requests_a_continuation_without_writing() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = AgentDirGuard::temp();
        while crate::core::extensions::take_pending_ui().is_some() {}
        let mut w = WizardState {
            target: 1,
            name: "auditor".to_string(),
            description: "Audit code".to_string(),
            thinking: 4,
            ..WizardState::default()
        };
        w.step = Step::Review;
        assert!(w.key(99, &K::Char('g')), "汇总步的 g 应被消费");
        let mut prompt = None;
        while let Some(r) = crate::core::extensions::take_pending_ui() {
            if let crate::core::extensions::ExtensionUiRequest::Continuation { message } = r {
                prompt = Some(message.text());
            }
        }
        let prompt = prompt.expect("应投递一条生成提示");
        assert!(prompt.contains("auditor"), "{prompt}");
        assert!(prompt.contains("Audit code"), "{prompt}");
        assert!(prompt.contains("thinking: medium"), "{prompt}");
        // 向导本身不写盘
        let path = agent_files::eject_path("auditor", &super::super::discovery_cwd(), false);
        assert!(!path.exists(), "generate 路径应由模型写盘，向导不写");
    }

    #[test]
    fn project_target_requires_trust() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = AgentDirGuard::temp();
        let dir = tempfile::tempdir().unwrap();
        let w = WizardState {
            target: 0, // project
            name: "p".to_string(),
            description: "d".to_string(),
            ..WizardState::default()
        };
        let slot = super::super::session_cwd_slot();
        let prev = slot.lock().unwrap().clone();
        *slot.lock().unwrap() = Some(dir.path().to_string_lossy().to_string());
        let err = w.write().unwrap_err();
        *slot.lock().unwrap() = prev;
        assert!(err.contains("not trusted"), "{err}");
    }

    /// 系统提示步用核心多行编辑器：`view` 给出编辑器，`editor_submit` 存正文并推进。
    #[test]
    fn system_prompt_step_uses_the_multiline_editor() {
        let mut w = WizardState {
            step: Step::SystemPrompt,
            ..WizardState::default()
        };
        let view = w.view();
        assert!(view.editor.is_some(), "系统提示步应提供编辑器");
        assert_eq!(view.editor.as_ref().unwrap().value, "");

        // 其他步骤不带编辑器
        w.step = Step::Location;
        assert!(w.view().editor.is_none());

        // 保存 → 存正文并进汇总步
        w.step = Step::SystemPrompt;
        w.editor_submit("line one\nline two".to_string());
        assert_eq!(w.system_prompt, "line one\nline two");
        assert_eq!(w.step, Step::Review, "保存后进汇总步");
        // 汇总步的预览显示正文首行与行数
        let review = w.view();
        assert!(
            review
                .lines
                .iter()
                .flatten()
                .any(|s| s.text.contains("line one"))
        );
    }

    /// 手写路径：提供了系统提示就用它当正文（否则回退 Description）。
    #[test]
    fn provided_system_prompt_becomes_the_body() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = AgentDirGuard::temp();
        let mut w = WizardState {
            target: 1,
            name: "auditor".to_string(),
            description: "Audit code".to_string(),
            system_prompt: "You are a meticulous auditor.\nReport only findings.".to_string(),
            ..WizardState::default()
        };
        let path = w.write().expect("写盘");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("You are a meticulous auditor."), "{text}");
        assert!(text.contains("Report only findings."), "{text}");

        // 空正文 → 回退到 Description
        w.system_prompt.clear();
        w.name = "auditor2".to_string();
        let path = w.write().expect("写盘");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("Audit code"), "{text}");
    }
}
