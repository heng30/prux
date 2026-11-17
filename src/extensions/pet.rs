//! `pet` 扩展：跟着 agent 状态做动作的宠物（桌面 GUI 里画成主窗口内的一层）。
//!
//! 资源（spritesheet）**不内嵌**：`assets/pet/<桌宠>/anim/*.webp` 只作为仓库发布的
//! 资源存在，用户自己放到 `agent_dir()/extensions/pet/<桌宠>/anim/`。缺资源时不画、
//! 不报错，只在聊天区给一条带下载链接的提示——桌宠缺图不该让界面起不来。
//!
//! 配置写在自己的文件里（`agent_dir()/extensions/pet/config.json`），
//! 不进 settings.json：桌宠是可关掉的装饰，不该占全局配置的版面。
//!
//! **默认不启用**：属于抢行为的扩展，由用户在 `/extension` 里开启，开关即时生效。

use crate::{
    core::{
        config_cache::ConfigCache,
        extensions::{
            CommandHost, Extension, ExtensionCommand, ExtensionHook, ExtensionMode, ExtensionTool,
            ExtensionUiRequest, SubcommandDef, UiNotifyLevel, request_ui,
        },
        provider::AgentMessage,
        settings_manager::agent_dir,
    },
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_PET, command_arg},
    modes::tui::handlers::register_host_command,
};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock, RwLock,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::Instant,
};
use strum_macros::{AsRefStr, EnumString};

/// 扩展名（注册名，也是 `/extension` 面板里的名字）。
const EXT: &str = "pet";

/// 扩展名（对外可引用，GUI 建窗前用它查启用态）。
pub const NAME: &str = EXT;

/// 斜杠命令名（`/pet`）。
const CMD: &str = "pet";

/// 资源包下载地址（缺资源时提示用户从这里下）。
const ASSET_URL: &str = "https://github.com/heng30/prux/raw/main/assets/pet/anim.tar.gz";

/// 资源根目录（相对 `agent_dir()`）：下面每个子目录是一个桌宠。
const ROOT_REL: &str = "extensions/pet";

/// 配置文件（相对 `agent_dir()`）。
const CONFIG_REL: &str = "extensions/pet/config.json";

/// 配置键：当前桌宠（资源根目录下的子目录名）。
const CONFIG_PET: &str = "pet";

/// 配置键：宠物左上角在主窗口内的横向比例（0–1，乘窗口宽度得逻辑像素）。
const CONFIG_POS_X: &str = "posX";

/// 配置键：宠物左上角在主窗口内的纵向比例（0–1，乘窗口高度得逻辑像素）。
const CONFIG_POS_Y: &str = "posY";

/// 配置键：资源根目录覆盖（空 = 用 [`ROOT_REL`]）。
const CONFIG_MEDIA: &str = "media";

/// 没有配置也没有已安装资源时假定的桌宠名（提示文案里用）。
const DEFAULT_PET: &str = "claude-girl";

/// 动画文件所在子目录名。
const ANIM_SUBDIR: &str = "anim";

/// 无事件多久后进入「睡着」（毫秒）。
const SLEEP_AFTER_MS: u64 = 120_000;

/// 动画目录的 spritesheet 文件名。
const WEBP: &str = "webp";

/// `/pet` 的子命令（顺序 = 输入框候选顺序）。
const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "download",
        description: "Print where to download the pet animations (--open opens the page)",
    },
    SubcommandDef {
        name: "switch",
        description: "Switch the current pet: /pet switch <name> (no name = list them)",
    },
];

/// 配置文件的读写锁（命令写盘与读盘互斥）。
static CONFIG_LOCK: RwLock<()> = RwLock::new(());

/// 配置文件的读缓存（锁序恒为「RwLock → 缓存内层 Mutex」）。
static CONFIG_CACHE: ConfigCache = ConfigCache::new();

/// 当前姿态（[`Pose`] 的 `u8` 值，无锁供 GUI 每帧读）。
static POSE: AtomicU8 = AtomicU8::new(Pose::Idle as u8);

/// 最近一次 agent 事件的时间戳（进程启动起的毫秒数）。
static LAST_EVENT_MS: AtomicU64 = AtomicU64::new(0);

/// 进程内的计时原点（姿态超时按它换算）。
static START: OnceLock<Instant> = OnceLock::new();

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static DESKTOP_PET_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_PET,
    make: || -> Arc<dyn Extension> { Arc::new(DesktopPet) },
};

/// 宠物动作：一个动作对应一张 spritesheet（文件名见 [`Pose::as_ref`] + `.webp`）。
///
/// 桌面 GUI 只认这 9 个名字；资源包里多出来的动画不会播放，少一个则该动作退回 [`Self::Idle`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, AsRefStr, EnumString)]
#[strum(serialize_all = "kebab-case")]
pub enum Pose {
    /// 空闲
    Idle,
    /// 模型思考中
    Thinking,
    /// 工具执行中
    Coding,
    /// 工具报错
    ToolError,
    /// 一轮正常结束
    Celebrate,
    /// 等待重试 / 上下文压缩
    Yawn,
    /// 长时间无事件
    Sleep,
    /// 被点击
    Click,
    /// 走动
    Walk,
}

impl Pose {
    /// 这张动作对应的文件名（`idle.webp` 等）。
    pub fn file_name(self) -> String {
        format!("{}.webp", self.as_ref())
    }
}

/// 计时原点（首次调用时初始化）。
fn start() -> Instant {
    *START.get_or_init(Instant::now)
}

/// 距今毫秒数（进程启动起算）。
fn now_ms() -> u64 {
    start().elapsed().as_millis() as u64
}

/// 配置文件路径：`agent_dir()/extensions/pet/config.json`。
fn config_path() -> PathBuf {
    agent_dir().join(CONFIG_REL)
}

/// 读配置对象；文件缺失或非法时为空对象（调用方按默认值处理）。
fn config() -> Value {
    CONFIG_CACHE
        .read_object(&CONFIG_LOCK, &config_path())
        .unwrap_or_else(|| serde_json::json!({}))
}

/// 读配置里的非空字符串键。
fn config_str(key: &str) -> Option<String> {
    config()
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 读改写配置：`edit` 拿到整份对象，改完落盘并回填缓存。
///
/// 落盘失败返回错误（调用方提示用户即可，内存态不写回）。
fn update_config(edit: impl FnOnce(&mut serde_json::Map<String, Value>)) -> std::io::Result<()> {
    let path = config_path();
    let mut root = CONFIG_CACHE
        .read_object(&CONFIG_LOCK, &path)
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(map) = root.as_object_mut() {
        edit(map);
    }
    CONFIG_CACHE.write_object(&CONFIG_LOCK, &path, root)
}

/// 资源根目录：配置里的 `media` 非空就用它，否则用 `agent_dir()/extensions/pet`。
fn anim_root() -> PathBuf {
    config_str(CONFIG_MEDIA)
        .map(PathBuf::from)
        .unwrap_or_else(|| agent_dir().join(ROOT_REL))
}

/// 已安装的桌宠名（资源根目录下带 `anim/` 的子目录，按名字排序）。
pub fn pets() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(anim_root()) else {
        return Vec::new();
    };
    let mut found: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().join(ANIM_SUBDIR).is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    found.sort();
    found
}

/// 当前桌宠：配置里的 `pet`（有资源就用它，没装资源也照它提示）；
/// 没配置时取第一个已安装的，一个都没装则退回 [`DEFAULT_PET`]。
pub fn current_pet() -> String {
    if let Some(name) = config_str(CONFIG_PET) {
        return name;
    }
    pets()
        .into_iter()
        .next()
        .unwrap_or_else(|| DEFAULT_PET.to_string())
}

/// 当前桌宠的动画目录（`<root>/<桌宠>/anim`）。
pub fn anim_dir() -> PathBuf {
    anim_root().join(current_pet()).join(ANIM_SUBDIR)
}

/// 某个动作的 spritesheet 路径。
pub fn anim_path(pose: Pose) -> PathBuf {
    anim_dir().join(pose.file_name())
}

/// 目录下已安装的动画文件（按文件名排序；目录不存在时为空）。
fn animations() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(anim_dir()) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_webp(p))
        .collect();
    found.sort();
    found
}

/// 是否为动画文件（按扩展名判定，不读文件内容）。
fn is_webp(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(WEBP))
}

/// 当前桌宠的资源是否已安装（至少一个 `.webp`）。
pub fn assets_ready() -> bool {
    !animations().is_empty()
}

/// 确保资源根目录存在（`agent_dir()/extensions/pet`），省得用户自己去翻路径。
///
/// `media` 指向自定义目录时不动手——那是用户自己挑的地方，凭空建目录只会盖住真正的问题。
fn ensure_root() {
    if config_str(CONFIG_MEDIA).is_none() {
        _ = std::fs::create_dir_all(anim_root());
    }
}

/// 缺资源时给用户的提示：下载地址 + 解压命令。
fn download_notice() -> String {
    let pet = current_pet();
    let dir = anim_dir();
    format!(
        "Desktop pet `{pet}` is not installed: {}\n\
         Download: {ASSET_URL}\n\
         Install: tar -xzf anim.tar.gz -C {}",
        dir.display(),
        anim_root().display()
    )
}

/// 记一次 agent 事件：按事件类型换算姿态，不认识的事件直接忽略。
///
/// 不按 `agentScope` 过滤：主 agent 的 `agent_start` / `message_start` /
/// `tool_execution_*` / `agent_settled` 根本不带这个字段（只有 `turn_end` 与
/// `agent_before_settle` 带），按它过滤等于把桌宠要的事件全丢掉。子代理的事件走
/// 各自独立的 sink，不会进到这里。
pub fn note_event(event: &Value) {
    let ty = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let pose = match ty {
        // 一轮开跑与模型开始产内容都是「在想」；`turn_end` 之后模型常常接着干活，不退回空闲
        "agent_start" | "message_start" => Pose::Thinking,
        "tool_execution_start" => Pose::Coding,
        "tool_execution_end" => {
            if event.get("isError").and_then(Value::as_bool) == Some(true) {
                Pose::ToolError
            } else {
                Pose::Coding
            }
        }
        // 被中断的一轮不算庆祝，用打哈欠过渡
        "agent_settled" => {
            if event.get("aborted").and_then(Value::as_bool) == Some(true) {
                Pose::Yawn
            } else {
                Pose::Celebrate
            }
        }
        "auto_retry_start" | "compaction_start" => Pose::Yawn,
        "auto_retry_end" | "compaction_end" => Pose::Thinking,
        _ => return,
    };
    LAST_EVENT_MS.store(now_ms(), Ordering::Relaxed);
    POSE.store(pose as u8, Ordering::Relaxed);
}

/// 当前该播的姿态：记住的姿态，长时间无事件后退回 [`Pose::Sleep`]。
pub fn pose() -> Pose {
    let age = now_ms().saturating_sub(LAST_EVENT_MS.load(Ordering::Relaxed));
    resolve(stored_pose(), age)
}

/// 记下的姿态（[`POSE`] 里存的 `u8` 还原成枚举）。
fn stored_pose() -> Pose {
    match POSE.load(Ordering::Relaxed) {
        1 => Pose::Thinking,
        2 => Pose::Coding,
        3 => Pose::ToolError,
        4 => Pose::Celebrate,
        5 => Pose::Yawn,
        6 => Pose::Sleep,
        7 => Pose::Click,
        8 => Pose::Walk,
        _ => Pose::Idle,
    }
}

/// 姿态决策（纯函数）：距上次事件超过 [`SLEEP_AFTER_MS`] 就睡，否则用记下的姿态。
fn resolve(stored: Pose, age_ms: u64) -> Pose {
    if age_ms >= SLEEP_AFTER_MS {
        Pose::Sleep
    } else {
        stored
    }
}

/// 会话切换后重置姿态（避免跨会话残留「正在写代码」）。
fn reset_pose() {
    LAST_EVENT_MS.store(now_ms(), Ordering::Relaxed);
    POSE.store(Pose::Idle as u8, Ordering::Relaxed);
}

/// 宠物在主窗口内的位置：左上角的横向/纵向比例（`0.0`–`1.0`，乘窗口尺寸即逻辑像素）。
///
/// 存比例而不是像素，换窗口尺寸或换显示器后位置仍然对得上。
/// 两个键缺任一或不是数字就当没存过（返回 `None`），由界面回落到默认位置。
pub fn saved_pos() -> Option<(f32, f32)> {
    let root = config();
    let x = root.get(CONFIG_POS_X)?.as_f64()? as f32;
    let y = root.get(CONFIG_POS_Y)?.as_f64()? as f32;
    Some((x.clamp(0.0, 1.0), y.clamp(0.0, 1.0)))
}

/// 记住宠物在主窗口内的位置（拖动松手时调用），取值是 `0.0`–`1.0` 的比例。
///
/// 写盘失败静默忽略：位置只是装饰，不该因此弹错误。
pub fn save_pos(x: f32, y: f32) {
    _ = update_config(|map| {
        map.insert(CONFIG_POS_X.to_string(), serde_json::json!(x));
        map.insert(CONFIG_POS_Y.to_string(), serde_json::json!(y));
    });
}

/// `/pet` 命令分发：`download`（默认）/ `switch <名字>`。
///
/// 不锁 agent，因此声明 `busy_safe`。
fn command_pet(host: &mut dyn CommandHost, raw: &str) -> bool {
    let arg = command_arg(raw).trim();
    let (sub, rest) = match arg.split_once(char::is_whitespace) {
        Some((head, tail)) => (head, tail.trim()),
        None => (arg, ""),
    };

    match sub {
        "" | "download" => {
            host.push_message(&download_notice(), UiNotifyLevel::Info);
            if rest.split_whitespace().any(|f| f == "--open") {
                if let Err(e) = webbrowser::open(ASSET_URL) {
                    host.push_message(
                        &format!("Could not open the browser ({e}); use the URL above."),
                        UiNotifyLevel::Warning,
                    );
                }
            }
        }
        "switch" => switch_pet(host, rest),
        other => host.push_message(
            &format!("pet: unknown subcommand `{other}`; use download | switch"),
            UiNotifyLevel::Warning,
        ),
    }
    false
}

/// `/pet switch [名字]`：不带名字时列出已安装的桌宠，带名字时切成它。
fn switch_pet(host: &mut dyn CommandHost, name: &str) {
    let installed = pets();
    if name.is_empty() {
        if installed.is_empty() {
            host.push_message(
                &format!(
                    "pet: no pet installed under {}; /pet download shows where to get them.",
                    anim_root().display()
                ),
                UiNotifyLevel::Info,
            );
            return;
        }
        let current = current_pet();
        let list = installed
            .iter()
            .map(|p| {
                if *p == current {
                    format!("{p} (current)")
                } else {
                    p.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        host.push_message(
            &format!("pet: installed pets: {list}; switch with /pet switch <name>."),
            UiNotifyLevel::Info,
        );
        return;
    }

    if !installed.iter().any(|p| p == name) {
        host.push_message(
            &format!(
                "pet: `{name}` is not installed under {} (installed: {}).",
                anim_root().display(),
                if installed.is_empty() {
                    "none".to_string()
                } else {
                    installed.join(", ")
                }
            ),
            UiNotifyLevel::Warning,
        );
        return;
    }

    match update_config(|map| {
        map.insert(CONFIG_PET.to_string(), Value::String(name.to_string()));
    }) {
        Ok(()) => {
            reset_pose();
            host.push_message(
                &format!("pet: switched to `{name}`."),
                UiNotifyLevel::Success,
            );
        }
        Err(e) => host.push_message(
            &format!("pet: cannot write config: {e}"),
            UiNotifyLevel::Error,
        ),
    }
}

/// `pet` 扩展。
pub struct DesktopPet;

impl Extension for DesktopPet {
    /// 扩展名（注册名）。
    fn name(&self) -> &str {
        EXT
    }

    /// `/extension` 面板里的一行说明。
    fn description(&self) -> &str {
        "A desktop pet that reacts to agent activity (desktop GUI only; assets downloaded separately)"
    }

    /// 会多开一个窗口，默认不启用。
    fn default_enabled(&self) -> bool {
        false
    }

    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 桌宠不提供工具。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// `/pet` 命令（子命令见 [`SUBCOMMANDS`]）。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description: "Desktop pet: /pet download | switch".to_string(),
            busy_safe: true,
            subcommands: SUBCOMMANDS.to_vec(),
        }]
    }

    /// 只订阅 agent 事件流（其余 hook 一概不声明）。
    fn hooks(&self) -> Vec<ExtensionHook> {
        vec![ExtensionHook::AgentEvent]
    }

    /// 接线 `/pet` 执行入口。
    fn on_registered(&self) {
        register_host_command(EXT, CMD, command_pet);
    }

    /// 确保资源根目录存在；开着却没装资源时给一条下载提示（不画宠物）。
    fn on_enabled_changed(&self, enabled: bool) {
        if !enabled {
            return;
        }

        ensure_root();
        if !assets_ready() {
            request_ui(ExtensionUiRequest::Notify {
                text: download_notice(),
                level: UiNotifyLevel::Warning,
            });
        }
    }

    /// 事件 → 姿态。
    fn on_agent_event(&self, event: &Value) {
        note_event(event);
    }

    /// 换会话时把姿态复位。
    fn on_session_switched(&self, _path: Option<&str>, _messages: &[AgentMessage]) {
        reset_pose();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::AgentDirGuard;

    /// 在临时 agent_dir 里造一个桌宠目录。
    fn install(pet: &str, files: &[&str]) {
        let dir = anim_root().join(pet).join(ANIM_SUBDIR);
        std::fs::create_dir_all(&dir).unwrap();
        for f in files {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
    }

    #[test]
    fn notice_names_the_download_url_and_target_dir() {
        let _guard = AgentDirGuard::temp();
        let notice = download_notice();
        assert!(notice.contains(ASSET_URL));
        assert!(notice.contains(&anim_dir().display().to_string()));
        assert!(!assets_ready(), "临时 agent_dir 里没有动画");
    }

    #[test]
    fn animations_lists_only_installed_webp() {
        let _guard = AgentDirGuard::temp();
        install(DEFAULT_PET, &["b.webp", "a.webp", "notes.txt"]);

        let names: Vec<String> = animations()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.webp", "b.webp"]);
        assert!(assets_ready());
    }

    #[test]
    fn pets_lists_directories_with_anim() {
        let _guard = AgentDirGuard::temp();
        install("zeta", &["idle.webp"]);
        install("alpha", &["idle.webp"]);
        std::fs::create_dir_all(anim_root().join("empty")).unwrap();

        assert_eq!(pets(), vec!["alpha", "zeta"], "空的目录不算桌宠");
    }

    #[test]
    fn current_pet_follows_config_then_installed() {
        let _guard = AgentDirGuard::temp();
        install("alpha", &["idle.webp"]);
        assert_eq!(current_pet(), "alpha", "没配置时取第一个已安装的");

        update_config(|map| {
            map.insert(CONFIG_PET.to_string(), Value::String("zeta".into()));
        })
        .unwrap();
        assert_eq!(current_pet(), "zeta", "配置优先（哪怕没装）");
    }

    #[test]
    fn position_round_trips_through_the_config() {
        let _guard = AgentDirGuard::temp();
        assert_eq!(saved_pos(), None, "没存过就没有位置");

        save_pos(0.25, 0.5);
        assert_eq!(saved_pos(), Some((0.25, 0.5)));

        // 手改配置写了越界值（换成像素的老值也算）：读出来夹回 0–1
        update_config(|map| {
            map.insert(CONFIG_POS_X.to_string(), serde_json::json!(2326.6));
            map.insert(CONFIG_POS_Y.to_string(), serde_json::json!(-3.0));
        })
        .unwrap();
        assert_eq!(saved_pos(), Some((1.0, 0.0)));
    }

    #[test]
    fn media_key_overrides_the_default_root() {
        let _guard = AgentDirGuard::temp();
        update_config(|map| {
            map.insert(
                CONFIG_MEDIA.to_string(),
                Value::String("/tmp/pet-anim".into()),
            );
        })
        .unwrap();
        assert_eq!(anim_root(), PathBuf::from("/tmp/pet-anim"));
    }

    #[test]
    fn events_map_to_poses() {
        let _guard = AgentDirGuard::temp();

        // 主 agent 的事件不带 agentScope：带了才处理等于全丢
        note_event(&serde_json::json!({"type": "message_start"}));
        assert_eq!(pose(), Pose::Thinking);

        note_event(&serde_json::json!({"type": "tool_execution_start"}));
        assert_eq!(pose(), Pose::Coding);

        note_event(&serde_json::json!({"type": "tool_execution_end", "isError": true}));
        assert_eq!(pose(), Pose::ToolError);

        note_event(&serde_json::json!({"type": "agent_settled"}));
        assert_eq!(pose(), Pose::Celebrate);

        note_event(&serde_json::json!({"type": "agent_settled", "aborted": true}));
        assert_eq!(pose(), Pose::Yawn, "被中断的一轮不庆祝");
    }

    #[test]
    fn turn_end_does_not_drop_back_to_idle_mid_round() {
        let _guard = AgentDirGuard::temp();

        note_event(&serde_json::json!({"type": "tool_execution_start"}));
        note_event(&serde_json::json!({"type": "turn_end"}));
        assert_eq!(pose(), Pose::Coding, "turn_end 之后模型常常接着干活");
    }

    #[test]
    fn pose_falls_back_to_sleep_when_idle() {
        assert_eq!(
            resolve(Pose::Coding, SLEEP_AFTER_MS - 1),
            Pose::Coding,
            "没超时就保持原姿态"
        );
        assert_eq!(
            resolve(Pose::Coding, SLEEP_AFTER_MS),
            Pose::Sleep,
            "超过阈值就睡着"
        );
    }

    #[test]
    fn stored_pose_round_trips_every_variant() {
        for pose in [
            Pose::Idle,
            Pose::Thinking,
            Pose::Coding,
            Pose::ToolError,
            Pose::Celebrate,
            Pose::Yawn,
            Pose::Sleep,
            Pose::Click,
            Pose::Walk,
        ] {
            POSE.store(pose as u8, Ordering::Relaxed);
            assert_eq!(stored_pose(), pose);
        }
    }

    #[test]
    fn enabling_creates_the_resource_root() {
        let _guard = AgentDirGuard::temp();
        assert!(!anim_root().exists(), "临时 agent_dir 里还没有目录");

        DesktopPet.on_enabled_changed(true);
        assert!(anim_root().is_dir(), "启用后应建好资源根目录");

        std::fs::remove_dir_all(anim_root()).ok();
        update_config(|map| {
            map.insert(
                CONFIG_MEDIA.to_string(),
                Value::String("/tmp/pet-anim-custom".into()),
            );
        })
        .unwrap();
        DesktopPet.on_enabled_changed(true);
        assert!(
            !PathBuf::from("/tmp/pet-anim-custom").exists(),
            "配了 media 就不该替用户建目录"
        );
    }

    #[test]
    fn available_in_dev_and_creator_modes() {
        let pet = DesktopPet;
        for mode in [ExtensionMode::Dev, ExtensionMode::Creator] {
            assert!(
                pet.modes().iter().any(|d| d.usable_in(mode)),
                "桌宠在 {mode:?} 模式下该出现在 /extension 面板里"
            );
        }
    }

    #[test]
    fn pose_file_names_are_english() {
        assert_eq!(Pose::Idle.file_name(), "idle.webp");
        assert_eq!(Pose::ToolError.file_name(), "tool-error.webp");
        assert_eq!(pose_from_str("walk"), Some(Pose::Walk));
    }

    /// 解析资源目录里的文件名（只用于测试）。
    fn pose_from_str(s: &str) -> Option<Pose> {
        use std::str::FromStr;
        Pose::from_str(s).ok()
    }
}
