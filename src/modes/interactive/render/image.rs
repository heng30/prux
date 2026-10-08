//! 内联图片的协议数据缓存：把消息区收集到的图片槽位编码成终端图形协议数据。
//!
//! 编码成本高——一张 1726×778 的截图光解码就要 ~50ms、缩放到目标尺寸 ~200ms 起，sixel 编码
//! 还要更久；放在渲染线程上会把界面卡住（发送图片后要等好几秒才有反应）。所以这里是
//! **渲染线程只取、后台线程编码**：命中缓存直接拿数据；未命中就登记任务并立即返回 `None`，
//! 调用方先画占位，编码完成后经 [`run_in_event_loop`] 把界面标脏，下一帧画上真图。
//!
//! 缓存按条目数与源数据字节数双向设限：条目数由 `LruCache` 的容量兜住，源数据字节数在写入后
//! 手工挤出最久未用项；长会话里图片因此不会无限堆积。

use crate::{modes::interactive::run_in_event_loop, utils::terminal_image};
use image::Rgba;
use lru::LruCache;
use ratatui::layout::Size;
use ratatui_image::{
    FilterType, Resize,
    picker::{Picker, ProtocolType},
    sliced::{SignedPosition, SlicedProtocol},
};
use std::{
    collections::HashSet,
    num::NonZeroUsize,
    sync::{Arc, Mutex, OnceLock},
};
use terminal_image::PadRgb;

/// 缓存源数据（base64 文本）字节数上限；超出后继续淘汰最久未用项，直至只剩一项
const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;

/// 缓存条目数上限（即 `LruCache` 容量，超限时淘汰最久未用的一项）
const MAX_ENTRIES: NonZeroUsize = NonZeroUsize::new(32).unwrap();

/// 已编码的图片协议数据与它的目标单元格尺寸。
struct Entry {
    /// 编码时使用的目标尺寸（列 × 行）；与请求不一致时视为未命中
    size: Size,
    /// 编码时使用的补边底色；与请求不一致时视为未命中
    pad: PadRgb,
    /// 该尺寸下已编码的协议数据
    protocol: Arc<SlicedProtocol>,
    /// 源 base64 文本长度（字节预算用）
    source_bytes: usize,
}

/// 内联图片协议缓存 + 后台编码任务登记（渲染线程与编码线程共享）。
struct ImageCache {
    /// 指纹 → 已编码数据，容量 MAX_ENTRIES，读取即提升为最近使用
    entries: LruCache<u64, Entry>,
    /// 正在后台编码的 (指纹, 尺寸, 补边底色)：同参数只起一个线程
    pending: HashSet<(u64, Size, PadRgb)>,
    /// 编码失败过的 (指纹, 尺寸, 补边底色)：失败不重试，避免每帧重开线程
    failed: HashSet<(u64, Size, PadRgb)>,
    /// 当前缓存占用的源数据字节数
    bytes: usize,
}

impl Default for ImageCache {
    /// 空缓存，条目容量取 MAX_ENTRIES。
    fn default() -> Self {
        Self {
            entries: LruCache::new(MAX_ENTRIES),
            pending: HashSet::new(),
            failed: HashSet::new(),
            bytes: 0,
        }
    }
}

impl ImageCache {
    /// 取该尺寸与补边底色下已编码的数据；命中时把它提升为最近使用，
    /// 尺寸/底色不符或没有则返回 `None`。
    fn take(&mut self, key: u64, size: Size, pad: PadRgb) -> Option<Arc<SlicedProtocol>> {
        let entry = self.entries.get(&key)?;
        if entry.size != size || entry.pad != pad {
            return None;
        }
        Some(entry.protocol.clone())
    }

    /// 登记后台编码任务；已有同参数任务或该参数曾失败时返回 `false`（调用方画占位即可）。
    fn start(&mut self, key: u64, size: Size, pad: PadRgb) -> bool {
        if self.failed.contains(&(key, size, pad)) {
            return false;
        }
        self.pending.insert((key, size, pad))
    }

    /// 后台编码结束：`None` 表示解码或编码失败（记入失败集合，不再重试）。
    fn finish(
        &mut self,
        key: u64,
        size: Size,
        pad: PadRgb,
        protocol: Option<SlicedProtocol>,
        source_bytes: usize,
    ) {
        self.pending.remove(&(key, size, pad));

        let Some(protocol) = protocol else {
            self.failed.insert((key, size, pad));
            return;
        };

        if let Some(old) = self.entries.put(
            key,
            Entry {
                size,
                pad,
                protocol: Arc::new(protocol),
                source_bytes,
            },
        ) {
            self.bytes = self.bytes.saturating_sub(old.source_bytes);
        }
        self.bytes += source_bytes;
        self.evict();
    }

    /// 清空缓存与任务登记（切换图片开关 / 单元格尺寸变化时，编码结果不再适用）。
    fn clear(&mut self) {
        self.entries.clear();
        self.pending.clear();
        self.failed.clear();
        self.bytes = 0;
    }

    /// 超出源数据字节上限时淘汰最久未用的一项；只剩一项时不再淘汰。
    fn evict(&mut self) {
        while self.bytes > MAX_SOURCE_BYTES && self.entries.len() > 1 {
            let Some((_, entry)) = self.entries.pop_lru() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(entry.source_bytes);
        }
    }
}

/// 进程级缓存句柄（渲染线程与后台编码线程共享）。
fn cache() -> &'static Mutex<ImageCache> {
    static CACHE: OnceLock<Mutex<ImageCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(ImageCache::default()))
}

/// 清空图片协议缓存（`showImages` 开关变化时调用）。
pub fn reset_cache() {
    cache().lock().unwrap().clear();
}

/// 取该图片在 `size` 尺寸、`pad` 补边底色下，按 `picker` 选定协议的编码数据。
///
/// 命中缓存直接返回；未命中时登记后台编码任务并返回 `None`——调用方先画占位，
/// 编码完成后后台线程经 [`run_in_event_loop`] 把界面标脏，下一帧就能拿到数据。
/// 图片无法解码或编码失败时同样返回 `None`（该参数记入失败集合，不反复重试）。
///
/// `pad` 是补边填充色（见 [`terminal_image::PadRgb`]）：只对 **halfblocks** 生效——它没有
/// alpha 通道，透明补边会被编码成黑；sixel / kitty / iterm2 自带透明，补边交给终端显示
/// 真实背景色，填色反而会在底色不匹配时画出错色块。
pub fn request(
    picker: &'static Picker,
    key: u64,
    data: &Arc<str>,
    size: Size,
    pad: PadRgb,
) -> Option<Arc<SlicedProtocol>> {
    let cache = cache();
    {
        let mut guard = cache.lock().unwrap();
        if let Some(protocol) = guard.take(key, size, pad) {
            return Some(protocol);
        }
        if !guard.start(key, size, pad) {
            return None;
        }
    }

    let data = data.clone();
    std::thread::spawn(move || {
        let encoded = terminal_image::decode_image(&data).and_then(|image| {
            let mut picker = picker.clone();
            if picker.protocol_type() == ProtocolType::Halfblocks
                && let Some([r, g, b]) = pad
            {
                picker.set_background_color(Some(Rgba([r, g, b, 255])));
            }

            // 缩放用 Triangle：`Resize::Fit(None)` 默认的 `Nearest` 在源图远大于目标框时
            // （截图 / 照片常见）是点抽样，糊且带锯齿；Lanczos3 更清晰但一张 1726×778 的截图
            // 要 500ms 左右，后台线程里也不值当。
            SlicedProtocol::new_with_resize(
                &picker,
                image,
                size,
                Resize::Fit(Some(FilterType::Triangle)),
            )
            .ok()
        });

        let source_bytes = data.len();
        cache
            .lock()
            .unwrap()
            .finish(key, size, pad, encoded, source_bytes);

        // 后台线程不直接碰 UI 状态：投递闭包让主循环标脏，下一帧画上真图
        _ = run_in_event_loop(|app| app.dirty = true);
    });

    None
}

/// 等后台编码任务跑完（测试用：断言真图前先让编码落地）。超时返回 `false`。
#[cfg(test)]
pub fn wait_idle(timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if cache().lock().unwrap().pending.is_empty() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    cache().lock().unwrap().pending.is_empty()
}

/// 一帧内要绘制的图片：绘制区域 + 相对该区域的锚点（行可为负，表示图片上端在视口之上）。
pub struct ImageRect {
    /// 绘制区域（消息文本区）；超出部分由 [`SlicedImage`](ratatui_image::sliced::SlicedImage) 裁剪
    pub area: ratatui::layout::Rect,
    /// 相对 `area` 的左上角（`x` 为列、`y` 为行；负值表示起点在区域之外）
    pub position: SignedPosition,
    /// 图片占用的单元格尺寸（列 × 行）
    pub size: Size,
    /// 图片内容指纹（缓存键）
    pub key: u64,
    /// 图片 base64 数据（不含 data URI 前缀）；Arc 共享避免逐帧复制
    pub data: Arc<str>,
    /// 透明补边要填的底色（halfblocks 无 alpha）；`None` 表示不填
    pub pad: PadRgb,
}
