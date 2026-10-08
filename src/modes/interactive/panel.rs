//! 模态选择面板（/model /theme /session /login /extension）的数据结构。
//!
//! 键盘独占，多级菜单用栈（layers）表示，每层一个过滤器与选中项。
//! 渲染在 `render::panel`，事件处理在 `handlers::panels`。

use super::line_input::InputBox;

/// 模态选择面板类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelKind {
    /// /theme：主题选择。
    Theme,
    /// /model：模型选择（Tab 切换全部/循环名单）。
    Model,
    /// /session：历史会话选择（恢复）。
    Session,
    /// /scoped-models：启用/禁用 Ctrl+P 循环名单（多选，Enter 切换）
    ScopedModels,
    /// /thinking：思考级别选择（off/low/medium/high/xhigh/max）
    Thinking,
    /// /login：认证方式选择（Sign in with an account / API key）
    LoginAuthType,
    /// /login：provider 选择（带 not configured / stored 状态）
    LoginProvider,
    /// /login：OAuth 登录方式选择（浏览器登录 / 复制授权码；仅 anthropic 有这一步）
    LoginMethod,
    /// /login：API key 输入（filter 行作为输入框）
    LoginKey,
    /// /login：OAuth 登录（标题含授权 URL，filter 行作为手动粘贴输入框）
    LoginOauth,
    /// /logout：已存凭据的 provider 选择
    LogoutProvider,
    /// /extension：扩展列表（checkbox + 名称 + ℹ 详情图标，空格切换启用）
    Extension,
    /// /extension：二级菜单，扩展描述信息
    ExtensionDetail,
    /// /skills：技能列表（checkbox + 名称 + 注记，空格切换启用、Ctrl+S 应用）
    Skill,
    /// /skills：二级菜单，SKILL.md 头信息与来源
    SkillDetail,
    /// 会话损坏修复确认面板（Enter 确认截断恢复 / Esc 取消）
    SessionRepair,
    /// 会话存在多个未完成 operation（崩溃残留）时的恢复方式选择面板
    ZombieOperations,
    /// /import 确认面板：替换当前会话前 Yes/No 确认
    ImportConfirm,
    /// /import 会话 cwd 缺失确认面板：继续在当前 cwd 恢复的 Yes/No 确认
    ImportCwdConfirm,
    /// settings.json 损坏时的覆写确认面板：Overwrite / Cancel
    SettingsOverwrite,
    /// `/history @clear` / `@clear-all` 的删除确认面板：Yes / No
    HistoryClearConfirm,
    /// `/bug`：描述输入（filter 行作为输入框）
    BugReportHint,
    /// `/bug`：是否附带 session transcript（Yes / No）
    BugReportTranscript,
    /// `/bug`：不附 transcript 时是否用当前模型生成摘要（Yes / No）
    BugReportSummary,
    /// `/bug`：导出方式（Export as Zip / Cancel）
    BugReportDelivery,
    /// 启动时项目信任选择面板（Trust / Trust parent / 仅本次 / Do not trust …）
    ProjectTrust,
    /// `/trust` 面板：保存信任决策（Trust / Trust parent / Do not trust），带已保存决策与当前会话状态
    ProjectTrustDialog,
    /// `/trust` 面板选「Remove all saved trust decisions」后的 Yes/No 确认：清空 trust.json
    TrustClearConfirm,
    /// 扩展 UI 请求的通用选择面板（值经 on_ui_choice 按 request id 回传）
    Custom(u64),
}

impl PanelKind {
    /// 该面板渲染/接受一个可见的搜索输入行（`filter`）。
    ///
    /// 无输入行的面板（确认框 / 详情 / 自定选择）：文本键不得落进不可见的 filter，
    /// 否则字符会悄悄把列表项过滤掉（`Yes`/`No` 消失）。渲染（[`super::render::panel`]）
    /// 与事件处理（[`super::handlers::panels`]）共用此判定，避免两处名单漂移。
    pub fn has_filter_input(self) -> bool {
        !matches!(
            self,
            PanelKind::LoginAuthType
                | PanelKind::LoginMethod
                | PanelKind::ExtensionDetail
                | PanelKind::SkillDetail
                | PanelKind::SessionRepair
                | PanelKind::ZombieOperations
                | PanelKind::ImportConfirm
                | PanelKind::ImportCwdConfirm
                | PanelKind::SettingsOverwrite
                | PanelKind::HistoryClearConfirm
                | PanelKind::BugReportTranscript
                | PanelKind::BugReportSummary
                | PanelKind::BugReportDelivery
                | PanelKind::ProjectTrust
                | PanelKind::ProjectTrustDialog
                | PanelKind::TrustClearConfirm
                | PanelKind::Custom(_)
        )
    }
}

/// 选择面板条目
#[derive(Debug, Clone, Default)]
pub struct PanelItem {
    /// 列表显示标签
    pub label: String,
    /// 确认时用于执行的值
    pub value: String,
    /// 描述（model 面板为 [provider]）
    pub desc: String,
    /// 模型显示名（Model Name 行用；非 model 面板为空）
    pub name: String,
    /// 逐项展示色（主题键或 hex；`None` = 面板默认配色）。
    /// 目前只有扩展自定义面板（`PanelKind::Custom`）会设，用于状态着色（如 `/tasks` 的完成/进行中/待办）。
    pub fg: Option<String>,
}

impl PanelItem {
    /// 只有 label/value 的条目：确认框（Yes/No、Overwrite/Cancel）、信任面板等
    /// 大量条目 `desc`/`name` 都为空，用这个构造子避免每处手写三个空字段。
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            ..Default::default()
        }
    }

    /// 指定展示色（主题键或 hex）的条目。
    pub fn styled(
        label: impl Into<String>,
        value: impl Into<String>,
        fg: impl Into<String>,
    ) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            fg: Some(fg.into()),
            ..Default::default()
        }
    }
}

/// 选择面板的一层（多级菜单：每层一个过滤器与选中项）
#[derive(Debug, Clone)]
pub struct PanelLayer {
    /// 该层的面板类型，决定渲染与按键处理。
    pub kind: PanelKind,
    /// 面板顶部标题。
    pub title: String,
    /// 该层未经过滤的全部候选条目。
    pub items: Vec<PanelItem>,
    /// 过滤/输入框（Shift 层映射与编辑键统一处理）
    pub filter: InputBox,
    /// 当前选中项在过滤后列表中的下标。
    pub selected: usize,
}

/// 模态选择面板（键盘独占）
#[derive(Debug, Clone)]
pub struct Panel {
    /// 面板是否处于打开状态（打开时独占键盘）。
    pub active: bool,
    /// 多级菜单栈，末层为当前显示层。
    pub layers: Vec<PanelLayer>,
}

impl Panel {
    /// 新建空面板（未激活、无层）。
    pub fn new() -> Self {
        Panel {
            active: false,
            layers: Vec::new(),
        }
    }

    /// 打开面板（单级入口：清空栈）
    pub fn open(&mut self, kind: PanelKind, title: String, items: Vec<PanelItem>) {
        self.layers.clear();
        self.layers.push(PanelLayer {
            kind,
            title,
            items,
            filter: Default::default(),
            selected: 0,
        });
        self.active = true;
    }

    /// 在当前面板栈上推入下一级（二级菜单：Esc/cancel 逐级返回）
    pub fn push(&mut self, kind: PanelKind, title: String, items: Vec<PanelItem>) {
        self.layers.push(PanelLayer {
            kind,
            title,
            items,
            filter: Default::default(),
            selected: 0,
        });
        self.active = true;
    }

    /// 当前显示层（栈顶）；面板未打开时为 `None`。
    pub fn top(&self) -> Option<&PanelLayer> {
        if self.active {
            self.layers.last()
        } else {
            None
        }
    }

    /// 当前显示层的可变引用（栈顶）；面板未打开时为 `None`。
    pub fn top_mut(&mut self) -> Option<&mut PanelLayer> {
        if self.active {
            self.layers.last_mut()
        } else {
            None
        }
    }

    /// 当前层按 filter 过滤后的条目（引用）
    pub fn filtered_items(&self) -> Vec<&PanelItem> {
        let Some(layer) = self.top() else {
            return Vec::new();
        };
        let f = layer.filter.value.trim().to_lowercase();
        layer
            .items
            .iter()
            .filter(|it| {
                f.is_empty()
                    || it.label.to_lowercase().contains(&f)
                    || it.value.to_lowercase().contains(&f)
            })
            .collect()
    }

    /// Esc：多级回退上一层；顶层退出（无事发生）
    pub fn cancel(&mut self) {
        if self.layers.len() > 1 {
            self.layers.pop();
        } else {
            self.active = false;
            self.layers.clear();
        }
    }

    /// 关闭整个面板
    pub fn close(&mut self) {
        self.active = false;
        self.layers.clear();
    }
}

impl Default for Panel {
    /// 等价于 [`Panel::new`]。
    fn default() -> Self {
        Self::new()
    }
}
