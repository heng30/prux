/// 任务完成
pub const DEF_COMPLETED: &str = "✔";
/// 任务进行中
pub const DEF_IN_PROGRESS: &str = "◼";
/// 任务待办
pub const DEF_PENDING: &str = "◻";
/// 任务依赖阻塞的箭头
pub const DEF_BLOCKED: &str = "›";
/// 完成 / 成功
pub const DEF_DONE: &str = "✓";
/// 失败 / 出错 / 删除
pub const DEF_FAILED: &str = "✗";
/// 停止 / 被杀 / 中断
pub const DEF_STOPPED: &str = "■";
/// 运行中
pub const DEF_RUNNING: &str = "◐";
/// 暂停
pub const DEF_PAUSED: &str = "‖";
/// 跳过
pub const DEF_SKIPPED: &str = "⤫";
/// 阻塞
pub const DEF_BLOCKED_STATUS: &str = "⊘";
/// 排队 / 占位省略号
pub const DEF_QUEUED: &str = DEF_ELLIPSIS;
/// 未开始 / 中性状态的间隔点
pub const DEF_MIDDOT: &str = "·";
/// 待办复选框（未完成）
pub const DEF_CHECKBOX_EMPTY: &str = "☐";
/// 统计行分隔符
pub const DEF_STATS_SEPARATOR: &str = DEF_MIDDOT;
/// 列表选中行前缀（含尾随空格）
pub const DEF_SELECTED_MARK: &str = "▶ ";
/// 开始 / 执行（行动菜单的 Start）
pub const DEF_PLAY: &str = "▸";
/// 光标前缀（会话列表当前行，含尾随空格）
pub const DEF_CURSOR: &str = "→ ";
/// 嵌套子代理缩进标记
pub const DEF_NESTED: &str = "↳";
/// 工具活动图标
pub const DEF_TOOL: &str = "⚙";
/// 上箭头
pub const DEF_ARROW_UP: &str = "↑";
/// 下箭头
pub const DEF_ARROW_DOWN: &str = "↓";
/// 上下箭头组合
pub const DEF_ARROW_UP_DOWN: &str = "↑↓";
/// 左箭头（返回 / 后退）
pub const DEF_ARROW_LEFT: &str = "←";
/// 输入 token 数前缀
pub const DEF_INPUT_TOKENS: &str = DEF_ARROW_UP;
/// 输出 token 数前缀
pub const DEF_OUTPUT_TOKENS: &str = DEF_ARROW_DOWN;
/// 省略号
pub const DEF_ELLIPSIS: &str = "…";
/// 溢出折叠提示
pub const DEF_OVERFLOW: &str = DEF_ELLIPSIS;
/// 尾随省略号
pub const DEF_TRAILING_ELLIPSIS: &str = DEF_ELLIPSIS;
/// 裁断标记
pub const DEF_TRUNCATION: &str = "...";
/// 圆环（单选项选中）
pub const DEF_DOT_RING: &str = "◉";
/// 空心圆（未点亮 / 未选中）。
pub const DEF_DOT_EMPTY: &str = "○";
/// 实心圆（点亮 / 就绪 / 监听中）
pub const DEF_DOT_FILLED: &str = "●";
/// 任务列表标题圆点
pub const DEF_HEADER: &str = DEF_DOT_FILLED;
/// 信息图标（`/extension` 条目后的详情入口；终端按 1 列宽渲染）
pub const DEF_INFO: &str = "𝒊";
/// 滚动条滑块
pub const DEF_SCROLLBAR_THUMB: &str = "█";
/// 密集 braille spinner（8 帧）
pub const DEF_SPINNER_DENSE: &[&str] = &["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"];
/// 任务执行中的 spinner 帧
pub const DEF_SPINNER: &[&str] = &["✳", "✴", "✵", "✶", "✷", "✸", "✹", "✺", "✻", "✼", "✽"];
/// 状态栏忙碌、重试提示、agent 运行图标共用（10 帧）
pub const DEF_SPINNER_BRAILLE: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
