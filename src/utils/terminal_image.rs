use crate::utils::{color, terminal_caps};
use base64::Engine as _;
use image::DynamicImage;
use ratatui::style::Color;
use ratatui_image::{
    FontSize,
    picker::{Picker, ProtocolType},
};
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    sync::OnceLock,
};

/// 图片透明补边要填的底色（sRGB 三分量）。
///
/// halfblocks 没有 alpha 通道：库在编码前把透明补边 `to_rgb8()` 成黑，补边因此会露出
/// 一条黑边（sixel 走 P2=1 透明，看到的是背景色）。补边填成图片所在块的底色，两个协议
/// 才看到一致的结果；`None` 表示不填（保持透明补边的黑色）。
pub type PadRgb = Option<[u8; 3]>;

/// `PRUX_IMAGE_PROTOCOL`：强制图形协议，取值 `kitty` / `iterm2` / `sixel` / `halfblocks` /
/// `none`（`none` / `0` 关闭内联图片，图片块整块不渲染）。无法识别时按未设置处理。
const ENV_PROTOCOL: &str = "PRUX_IMAGE_PROTOCOL";

/// 进程级缓存的 picker；`None` 表示不渲染内联图片（被 `PRUX_IMAGE_PROTOCOL=none` 关闭）。
static PICKER: OnceLock<Option<Picker>> = OnceLock::new();

/// TUI 接管输入前调用一次：`enabled` 为真时确定图片协议与单元格像素尺寸。
///
/// `enabled` 为假（默认）时什么都不做，之后 [`image_picker`] 会按环境变量兜底。
/// 普通终端下查询终端图形能力（要读写 stdin，必须早于 TUI 接管输入），查询失败回落环境变量
/// 判定；tmux 内不查询（passthrough 查询会改 tmux 配置且不可靠），只认 tmux 自报的能力。
pub fn prime_image_picker(enabled: bool) {
    if !enabled {
        return;
    }

    _ = PICKER.get_or_init(|| {
        if terminal_caps::in_tmux() {
            // tmux 内不查询 stdio：passthrough 查询会改 tmux 配置且不可靠，只认 tmux 自报的能力
            apply_override(env_picker())
        } else {
            apply_override(Picker::from_query_stdio().unwrap_or_else(|_| env_picker()))
        }
    });
}

/// 当前生效的 picker；`None` 表示本终端不渲染内联图片（调用方不渲染图片块）。
///
/// 首次调用（`showImages` 在运行中被打开、启动时没探测）时按环境变量惰性构建：
/// 这条路径不查询 stdio（事件流已在读输入），协议靠 `TERM` / `TERM_PROGRAM` 等判定，
/// 单元格像素尺寸用 ratatui-image 的默认值。
pub fn image_picker() -> Option<&'static Picker> {
    PICKER.get_or_init(|| apply_override(env_picker())).as_ref()
}

/// 单元格像素尺寸 (宽, 高)；不渲染图片时为 `None`。
pub fn image_font_size() -> Option<(u16, u16)> {
    image_picker().map(|p| (p.font_size().width.max(1), p.font_size().height.max(1)))
}

/// 按单元格像素尺寸把图片等比缩放进 `max_cols` × `max_rows` 的单元格盒子。
///
/// 口径必须与 ratatui-image 的 `Resize::Fit` **完全一致**，否则预留的行数与真正画出来的
/// 行数对不上：图片画在预留区左上角，右下留出一大块消息背景色。库的口径是
/// `fit_area_proportionally(w, h, min(盒宽, w), min(盒高, h))` 再向上取整到单元格——
/// 也就是**永不放大**，比盒子小的图保持原始像素尺寸。
///
/// 返回实际占用的 (列, 行)：至少 1×1；任一入参为 0 时返回 `(0, 0)`
/// （表示放不下，调用方回落到 `[image]` 占位）。
pub fn fit_cell_size(
    img_w: u32,
    img_h: u32,
    font: (u16, u16),
    max_cols: u16,
    max_rows: u16,
) -> (u16, u16) {
    if max_cols == 0 || max_rows == 0 || img_w == 0 || img_h == 0 {
        return (0, 0);
    }

    let font_w = font.0.max(1) as u32;
    let font_h = font.1.max(1) as u32;
    let (px_w, px_h) = fit_pixel_size(
        img_w,
        img_h,
        max_cols as u32 * font_w,
        max_rows as u32 * font_h,
    );

    (
        px_w.div_ceil(font_w).max(1) as u16,
        px_h.div_ceil(font_h).max(1) as u16,
    )
}

/// `Resize::Fit`（**不放大**）把 `img_w × img_h` 缩进 `box_w × box_h` 后的实际像素尺寸。
///
/// 镜像库的 `fit_area_proportionally`：先把目标框按图片自身尺寸截断，再取最小比例、四舍五入。
/// 任一入参为 0 时返回 `(0, 0)`。
pub fn fit_pixel_size(img_w: u32, img_h: u32, box_w: u32, box_h: u32) -> (u32, u32) {
    if img_w == 0 || img_h == 0 || box_w == 0 || box_h == 0 {
        return (0, 0);
    }

    let ratio =
        (box_w.min(img_w) as f64 / img_w as f64).min(box_h.min(img_h) as f64 / img_h as f64);

    (
        ((img_w as f64 * ratio).round() as u32).max(1),
        ((img_h as f64 * ratio).round() as u32).max(1),
    )
}

/// 图片在自己单元格盒子底部的补边高度（像素）：盒高 − [`fit_pixel_size`] 的实际高度。
///
/// 补边是**透明**的（库的 `DEFAULT_BACKGROUND` 是 `Rgba([0,0,0,0])`，sixel 走 P2=1 不画它们），
/// 肉眼看到的是背景色——但**照样占位置**。调用方用它判断还要不要再补一行空行，
/// 否则图片之间会空出两行（回归：多图之间间隔两行）。
pub fn image_bottom_pad(img_w: u32, img_h: u32, cols: u16, rows: u16, font: (u16, u16)) -> u32 {
    let box_w = cols.max(1) as u32 * font.0.max(1) as u32;
    let box_h = rows.max(1) as u32 * font.1.max(1) as u32;

    box_h.saturating_sub(fit_pixel_size(img_w, img_h, box_w, box_h).1)
}

/// 渲染样式里的底色 → [`PadRgb`]：`Rgb` 取原值，`Indexed` 按标准 xterm 调色板近似。
///
/// `Reset`（终端默认色，实际显示什么由终端决定）与命名的 ANSI 色没有可信 RGB，返回 `None`。
/// 主题色经 [`crate::modes::interactive::theme::color`] 只会得到 `Rgb` / `Indexed` / `Reset`。
pub fn style_bg_rgb(color: Color) -> PadRgb {
    match color {
        Color::Rgb(r, g, b) => Some([r, g, b]),
        Color::Indexed(index) => {
            let rgb = color::indexed_to_rgb(index);
            Some([rgb.r, rgb.g, rgb.b])
        }
        _ => None,
    }
}

/// 图片内容指纹（base64 文本的哈希）：图片协议缓存与槽位匹配用。
pub fn image_key(data_base64: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    data_base64.hash(&mut hasher);
    hasher.finish()
}

/// 图片像素尺寸 (宽, 高)；base64 非法或格式无法识别时为 `None`。
pub fn image_dimensions(data_base64: &str) -> Option<(u32, u32)> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_base64)
        .ok()?;
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.into_dimensions().ok()
}

/// 解码图片；base64 非法或格式无法识别时为 `None`。
pub fn decode_image(data_base64: &str) -> Option<DynamicImage> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_base64)
        .ok()?;
    image::load_from_memory(&bytes).ok()
}

/// 应用 `PRUX_IMAGE_PROTOCOL` 覆盖：`none` 返回 `None`（关闭），已知协议名改写协议类型，
/// 未设置或无法识别时原样返回。
fn apply_override(picker: Picker) -> Option<Picker> {
    let mut picker = picker;

    match protocol_override().as_deref() {
        Some("none" | "0") => None,
        Some("kitty") => {
            picker.set_protocol_type(ProtocolType::Kitty);
            Some(picker)
        }
        Some("iterm2") => {
            picker.set_protocol_type(ProtocolType::Iterm2);
            Some(picker)
        }
        Some("sixel") => {
            picker.set_protocol_type(ProtocolType::Sixel);
            Some(picker)
        }
        Some("halfblocks") => {
            picker.set_protocol_type(ProtocolType::Halfblocks);
            Some(picker)
        }
        _ => Some(picker),
    }
}

/// `PRUX_IMAGE_PROTOCOL` 的归一化取值（去空白、小写）；未设置或为空时 `None`。
fn protocol_override() -> Option<String> {
    let value = std::env::var(ENV_PROTOCOL).ok()?;
    let value = value.trim().to_ascii_lowercase();
    (!value.is_empty()).then_some(value)
}

/// 按环境变量构建 picker（不查询 stdio）：协议来自终端特征，尺寸用默认字体。
///
/// 命中不了任何终端特征时用 [`ProtocolType::Halfblocks`]（半个方块字符 + 真彩色，
/// 任何终端都能显示，只是分辨率只有「列数 × 行数×2」个采样点）；tmux 内见 [`tmux_picker`]。
fn env_picker() -> Picker {
    if terminal_caps::in_tmux() {
        return tmux_picker();
    }

    let mut picker = Picker::halfblocks();

    if let Some(protocol) = env_protocol() {
        picker.set_protocol_type(protocol);
    }

    picker
}

/// tmux 内的 picker：能用 sixel 就用 **裸 sixel**（像素级清晰），否则回落 halfblocks。
///
/// halfblocks 的分辨率只有「列数 × 行数×2」个采样点（60 列的图约 60×50 个色块），怎么调滤镜
/// 都是糊的；sixel 是把真实像素交给外层终端画，是 tmux 里唯一能看清图片的路。
///
/// 单元格像素尺寸必须取外层终端的真实值（[`tmux_cell_pixels`]）：sixel 按像素画，
/// 字体尺寸偏小会让图只占预留区的左上角一小块、右下留一大片空。
fn tmux_picker() -> Picker {
    let mut picker = picker_outside_tmux(tmux_cell_pixels());

    if tmux_sixel_ready() {
        picker.set_protocol_type(ProtocolType::Sixel);
    }

    picker
}

/// tmux 内单元格的像素尺寸 (宽, 高)：当前 pane 的像素尺寸 ÷ 单元格数。
///
/// tmux 会把客户端的像素尺寸传播到 pane 的 pty（`ws_xpixel` / `ws_ypixel`），所以这里拿到的
/// 就是外层终端真实的单元格像素尺寸（等价于 tmux 的 `#{client_cell_width}`）。
/// 终端不上报像素尺寸时返回 `None`，调用方回落 ratatui-image 的默认 10×20。
fn tmux_cell_pixels() -> Option<(u16, u16)> {
    let ws = crossterm::terminal::window_size().ok()?;
    cell_pixels_from_window(ws.columns, ws.rows, ws.width, ws.height)
}

/// 由「单元格数 + 像素尺寸」算出单个单元格的像素尺寸；任一维为 0 时返回 `None`。
fn cell_pixels_from_window(cols: u16, rows: u16, px_w: u16, px_h: u16) -> Option<(u16, u16)> {
    if cols == 0 || rows == 0 || px_w == 0 || px_h == 0 {
        return None;
    }

    Some(((px_w / cols).max(1), (px_h / rows).max(1)))
}

/// tmux 内是否可用 sixel：tmux 得编译了 sixel 支持（`#{sixel_support}` 为 `1`），
/// 且当前 pane 已开 `allow-passthrough`——tmux 转发 sixel 仍受这个选项管辖，没开只会把
/// 序列吞掉（图片区一片空白）。没开时 prux 会显式打开它（只改当前 pane，不动全局配置）。
fn tmux_sixel_ready() -> bool {
    if !terminal_caps::tmux_display("#{sixel_support}").is_some_and(|v| parse_sixel_support(&v)) {
        return false;
    }

    // 查不到 pane 选项（异常环境）时不敢上 sixel：宁可用 halfblocks 也不能画出一片空白
    let Some(passthrough) = terminal_caps::tmux_show_pane_value("allow-passthrough") else {
        return false;
    };
    if parse_on(&passthrough) {
        return true;
    }

    terminal_caps::tmux_set_pane_value("allow-passthrough", "on");
    terminal_caps::tmux_show_pane_value("allow-passthrough").is_some_and(|v| parse_on(&v))
}

/// 构造一个「不在 tmux 里」的 picker（可指定单元格像素尺寸）：库只看构造时的
/// `TERM` / `TERM_PROGRAM` 判断 tmux，没有别的开关。
///
/// 为什么必须骗过它：tmux 3.6 有原生 sixel（自己解析、纳入屏幕模型），要的是**裸 sixel**；
/// 而库在 tmux 里会把 sixel 包进 DCS passthrough（`\x1bPtmux;…`），那条路径实测会被 tmux
/// 当普通文本打到屏幕上（界面刷花）并阻塞写入（界面假死）。顺带，库在 tmux 里构造 picker
/// 时会执行 `tmux set -p allow-passthrough on`，骗过之后这条副作用也没有了——
/// 要不要开由 [`tmux_sixel_ready`] 显式决定。
#[allow(deprecated)] // 唯一的「自定义字体 + 不查 stdio」构造入口，新 API 只认查询结果
fn picker_outside_tmux(font: Option<(u16, u16)>) -> Picker {
    let term = std::env::var_os("TERM");
    let term_program = std::env::var_os("TERM_PROGRAM");

    // SAFETY: 只在 picker 构造期间（进程内一次）临时改写并立刻还原；本进程读这两个变量的
    // 地方（终端能力探测、`in_tmux`）都在启动时跑过并已缓存，不会与这里并发。
    unsafe {
        std::env::set_var("TERM", "xterm-256color");
        std::env::remove_var("TERM_PROGRAM");
    }

    let picker = match font {
        Some((w, h)) => Picker::from_fontsize(FontSize::new(w, h)),
        None => Picker::halfblocks(),
    };

    unsafe {
        match term {
            Some(value) => std::env::set_var("TERM", value),
            None => std::env::remove_var("TERM"),
        }
        if let Some(value) = term_program {
            std::env::set_var("TERM_PROGRAM", value);
        }
    }

    picker
}

/// tmux `#{sixel_support}` 的取值是否表示支持 sixel（`1`；老 tmux 没有该变量 → 空串）。
fn parse_sixel_support(value: &str) -> bool {
    value.trim() == "1"
}

/// tmux 布尔选项的取值是否表示开启（`on` / `1`）。
fn parse_on(value: &str) -> bool {
    matches!(value.trim(), "on" | "1")
}

/// 环境变量能识别出的图形协议；识别不出时 `None`（由调用方决定回落到什么）。
///
/// kitty 系（kitty / ghostty / wezterm / Warp）走 kitty 协议，
/// iTerm2 走自有协议，其余（含 VS Code、Alacritty、Windows Terminal）没有可用的内联图片协议。
fn env_protocol() -> Option<ProtocolType> {
    let term_program = env_lower("TERM_PROGRAM");
    let term = env_lower("TERM");
    let has = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());

    let kitty = has("KITTY_WINDOW_ID")
        || term_program == "kitty"
        || term_program == "ghostty"
        || term.contains("ghostty")
        || has("GHOSTTY_RESOURCES_DIR")
        || has("WEZTERM_PANE")
        || term_program == "wezterm"
        || term_program == "warpterminal"
        || has("WARP_SESSION_ID")
        || has("WARP_TERMINAL_SESSION_UUID");

    if kitty {
        return Some(ProtocolType::Kitty);
    }

    if has("ITERM_SESSION_ID") || term_program == "iterm.app" {
        return Some(ProtocolType::Iterm2);
    }

    None
}

/// 环境变量的小写取值；未设置时为空串。
fn env_lower(name: &str) -> String {
    std::env::var(name).unwrap_or_default().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 单元格 10×20、盒子 60×30：宽图按宽度顶满，高图按高度顶满。
    #[test]
    fn fit_cell_size_keeps_aspect_within_the_box() {
        // 600×200 像素 = 60×10 单元格：正好顶满宽度
        assert_eq!(fit_cell_size(600, 200, (10, 20), 60, 30), (60, 10));
        // 200×600 像素 = 20×30 单元格：顶满高度，列数按比例缩到 20
        assert_eq!(fit_cell_size(200, 600, (10, 20), 60, 30), (20, 30));
        // 宽高比 3:1 的巨图：宽度顶满、行数按比例
        assert_eq!(fit_cell_size(3000, 1000, (10, 20), 60, 30), (60, 10));
    }

    /// 单元格尺寸参与换算：同一张图在不同字体比例下占用的行数不同。
    #[test]
    fn fit_cell_size_uses_font_metrics() {
        // 100×100 像素、字体 10×20：宽度顶满 10 列时高度 = 100/(100/10)/20 = 5 行
        assert_eq!(fit_cell_size(100, 100, (10, 20), 10, 30), (10, 5));
        // 字体改为 10×10（方形单元格）时行数翻倍
        assert_eq!(fit_cell_size(100, 100, (10, 10), 10, 30), (10, 10));
    }

    /// 盒子放不下或入参非法时返回 (0, 0)（调用方回落 `[image]` 占位）。
    /// `fit_pixel_size` 同样在任一入参为 0 时返回 (0, 0)。
    #[test]
    fn fit_pixel_size_mirrors_the_library() {
        assert_eq!(fit_pixel_size(333, 299, 352, 329), (333, 299));
        assert_eq!(fit_pixel_size(1726, 778, 1320, 1363), (1320, 595));
        assert_eq!(fit_pixel_size(100, 100, 0, 100), (0, 0));
    }

    /// 底部补边 = 盒高 − 实际缩放高度；补边透明但占位置，用于决定是否还要补一行空行。
    /// 口径必须与库一致：`Resize::Fit` 把不足一格的余量补成 `Rgba([0,0,0,0])`。
    #[test]
    fn image_bottom_pad_counts_the_rounding_slack() {
        // 333×299 放进 16×7 格（10×20 字体 → 160×140 盒）：图片比盒子大，按宽度缩到 160×144 → 高度顶满
        assert_eq!(image_bottom_pad(333, 299, 16, 7, (10, 20)), 0);
        // 小图不放大：100×100 放进 10×5 格（100×100 盒）正好顶满
        assert_eq!(image_bottom_pad(100, 100, 10, 5, (10, 20)), 0);
        // 100×100 放进 10×6 格（100×120 盒）：高 100 < 120 → 补边 20
        assert_eq!(image_bottom_pad(100, 100, 10, 6, (10, 20)), 20);
        // 333×299 放进 16×7 格（22×47 字体 → 352×329 盒）：不放大 → 补边 30px ≈ 0.64 行
        assert_eq!(image_bottom_pad(333, 299, 16, 7, (22, 47)), 30);
    }

    #[test]
    fn fit_cell_size_rejects_empty_boxes() {
        assert_eq!(fit_cell_size(100, 100, (10, 20), 0, 30), (0, 0));
        assert_eq!(fit_cell_size(100, 100, (10, 20), 10, 0), (0, 0));
        assert_eq!(fit_cell_size(0, 100, (10, 20), 10, 30), (0, 0));
    }

    /// 比盒子小的图片保持原始像素尺寸（库的 `Fit` 不放大），但不会取整成 0×0。
    #[test]
    fn fit_cell_size_never_upscales_small_images() {
        assert_eq!(fit_cell_size(1, 1, (10, 20), 60, 30), (1, 1));
        assert_eq!(fit_cell_size(10, 1, (10, 20), 60, 30), (1, 1));
        assert_eq!(fit_cell_size(100, 100, (10, 20), 60, 30), (10, 5));
    }

    /// 预留的单元格尺寸必须与库真正画出来的协议尺寸一致——不一致就会在图片下方
    /// 留出一大块背景色（回归：333×299 的粘贴图在 foot 里预留 60×25、实际只画 28×12）。
    #[test]
    fn fit_cell_size_matches_library_protocol_size() {
        use ratatui::layout::Size;
        use ratatui_image::{FilterType, Resize, sliced::SlicedProtocol};

        let Some(picker) = image_picker() else {
            return;
        };
        let font = (picker.font_size().width, picker.font_size().height);

        for (w, h) in [
            (333u32, 299u32),
            (600, 200),
            (600, 2000),
            (100, 100),
            (1, 1),
            (2000, 500),
            (300, 900),
        ] {
            let data = crate::test_support::png_base64(w, h);
            let image = decode_image(&data).expect("测试图应能解码");
            for (cols, rows) in [(60u16, 30u16), (40, 20), (10, 5)] {
                let requested = Size::new(cols, rows);
                let protocol = SlicedProtocol::new_with_resize(
                    picker,
                    image.clone(),
                    requested,
                    Resize::Fit(Some(FilterType::Triangle)),
                )
                .expect("测试图应能编码");
                let got = protocol.size();

                assert_eq!(
                    (got.width, got.height),
                    fit_cell_size(w, h, font, cols, rows),
                    "图片 {w}x{h} 放进 {cols}x{rows} 的盒子"
                );
            }
        }
    }

    /// tmux 能力字符串的解析：`#{sixel_support}` 只有 `1` 算支持（老 tmux 无该变量 → 空串）；
    /// 布尔选项 `on` / `1` 算开启。
    #[test]
    fn tmux_capability_strings_are_parsed() {
        assert!(parse_sixel_support("1"));
        assert!(parse_sixel_support(" 1\n"));
        assert!(!parse_sixel_support("0"));
        assert!(!parse_sixel_support(""));

        assert!(parse_on("on"));
        assert!(parse_on("1"));
        assert!(!parse_on("off"));
        assert!(!parse_on(""));
    }

    /// 单元格像素尺寸由「单元格数 + 像素尺寸」算出；任一维为 0（终端不上报）时返回 `None`。
    #[test]
    fn cell_pixels_come_from_the_window_size() {
        assert_eq!(cell_pixels_from_window(136, 36, 2992, 1692), Some((22, 47)));
        assert_eq!(cell_pixels_from_window(80, 24, 800, 480), Some((10, 20)));
        // 像素尺寸不足一格时至少算 1，避免除出 0
        assert_eq!(cell_pixels_from_window(80, 24, 40, 24), Some((1, 1)));
        assert_eq!(cell_pixels_from_window(0, 24, 800, 480), None);
        assert_eq!(cell_pixels_from_window(80, 0, 800, 480), None);
        assert_eq!(cell_pixels_from_window(80, 24, 0, 480), None);
        assert_eq!(cell_pixels_from_window(80, 24, 800, 0), None);
    }

    /// 内容指纹只取决于 base64 文本：同图同键，不同图不同键。
    #[test]
    fn image_key_follows_content() {
        assert_eq!(image_key("AAAA"), image_key("AAAA"));
        assert_ne!(image_key("AAAA"), image_key("AAAB"));
    }

    /// 补边底色只认有可信 RGB 的取值：`Rgb` 原值、`Indexed` 按标准调色板近似，
    /// `Reset`（终端默认色）等返回 `None`。
    #[test]
    fn pad_color_only_for_concrete_colors() {
        use ratatui::style::Color;

        assert_eq!(style_bg_rgb(Color::Rgb(1, 2, 3)), Some([1, 2, 3]));
        // 标准 xterm 调色板：索引 8 是亮黑 #808080
        assert_eq!(style_bg_rgb(Color::Indexed(8)), Some([128, 128, 128]));
        assert_eq!(style_bg_rgb(Color::Reset), None);
    }
}
