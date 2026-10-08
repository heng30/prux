//! 图片输入规范化：把图片压到 provider 内联限制内。
//!
//! 限制来自模型目录 `inputLimits.images.resize`（`maxWidth` / `maxHeight` / `maxBytes` / `jpegQuality`），
//! 缺失字段回退默认值（2000x2000 / 4.5MB base64 / quality 80）。
//!
//! 策略：
//! 1. 已在内联格式且尺寸/字节都合规 → 原样返回（无损，不重新编码）；
//! 2. 否则按 maxWidth/maxHeight 等比缩放，同时尝试 PNG 与多档 JPEG，取先满足字节上限者；
//! 3. 仍超限则按 0.75 逐步降尺寸，直到 1x1；始终无法满足返回 `None`。
//!
//! 非内联格式（bmp 等）先转 PNG（`converted_from` 记录原 mime）。

use base64::Engine as _;
use image::{DynamicImage, ImageFormat, imageops::FilterType};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 可直接内联的图片 mime（其余先转 PNG）。
pub const INLINE_IMAGE_MIMES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// 图片输入限制（模型目录 `inputLimits.images`）。
///
/// `resize` 参与实际缩放；`maxPerMessage` / `maxPerRequest` 是 provider 声明的
/// 单条消息 / 单次请求图片数量上限。**仅解析与透传，不做重写或拒绝**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ImageInputLimits {
    /// 尺寸与编码质量限制（`inputLimits.images.resize`）。
    pub resize: ImageResizeLimits,
    /// 单条 provider 消息允许的图片数（`images.maxPerMessage`）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_per_message: Option<u32>,
    /// 单次 provider 请求允许的图片数（`images.maxPerRequest`）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_per_request: Option<u32>,
}

/// 模型目录 `inputLimits` 全量解析结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct InputLimits {
    /// 单次 provider 请求序列化后的字节上限（`inputLimits.maxRequestBytes`，顶层键）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_request_bytes: Option<u64>,
    /// 图片相关限制（`inputLimits.images`）。
    pub images: ImageInputLimits,
}

impl InputLimits {
    /// 从目录条目的 `inputLimits` 解析（缺失字段回退默认值 / None）。
    pub fn from_catalog(input_limits: Option<&Value>) -> Self {
        let Some(v) = input_limits else {
            return Self::default();
        };
        let images = v.get("images");
        let count = |key: &str| {
            images
                .and_then(|i| i.get(key))
                .and_then(|n| n.as_u64())
                .filter(|n| *n > 0)
                .map(|n| n as u32)
        };

        Self {
            max_request_bytes: v
                .get("maxRequestBytes")
                .and_then(|n| n.as_u64())
                .filter(|n| *n > 0),
            images: ImageInputLimits {
                resize: ImageResizeLimits::from_catalog(input_limits),
                max_per_message: count("maxPerMessage"),
                max_per_request: count("maxPerRequest"),
            },
        }
    }
}

/// 图片内联限制（模型目录 `inputLimits.images.resize`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ImageResizeLimits {
    /// 缩放后宽度上限（像素），来自 `resize.maxWidth`，默认 2000。
    pub max_width: u32,
    /// 缩放后高度上限（像素），来自 `resize.maxHeight`，默认 2000。
    pub max_height: u32,
    /// base64 载荷字节上限（**不是**原始图片字节）
    pub max_bytes: usize,
    /// JPEG 重编码质量（1~100），来自 `resize.jpegQuality`，默认 80。
    pub jpeg_quality: u8,
}

impl Default for ImageResizeLimits {
    /// 未配置目录条目时使用的保守默认值（2000×2000、约 4.5MB 载荷、JPEG 质量 80）
    fn default() -> Self {
        Self {
            max_width: 2000,
            max_height: 2000,
            max_bytes: 4_718_592,
            jpeg_quality: 80,
        }
    }
}

impl ImageResizeLimits {
    /// 从目录条目的 `inputLimits` 解析（缺失字段回退默认值）。
    pub fn from_catalog(input_limits: Option<&Value>) -> Self {
        let d = Self::default();
        let Some(resize) = input_limits
            .and_then(|v| v.get("images"))
            .and_then(|v| v.get("resize"))
        else {
            return d;
        };

        let num = |key: &str| resize.get(key).and_then(|v| v.as_u64());
        Self {
            max_width: num("maxWidth")
                .map(|v| v as u32)
                .filter(|v| *v > 0)
                .unwrap_or(d.max_width),
            max_height: num("maxHeight")
                .map(|v| v as u32)
                .filter(|v| *v > 0)
                .unwrap_or(d.max_height),
            max_bytes: num("maxBytes")
                .map(|v| v as usize)
                .filter(|v| *v > 0)
                .unwrap_or(d.max_bytes),
            jpeg_quality: num("jpegQuality")
                .map(|v| v.clamp(1, 100) as u8)
                .unwrap_or(d.jpeg_quality),
        }
    }
}

/// 规范化后的图片（可直接内联进消息）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedImage {
    /// 规范化后图片的 base64 编码载荷。
    pub data_base64: String,
    /// 规范化后的 mime（`image/png` / `image/jpeg` 等可内联格式）。
    pub mime_type: String,
    /// 原图尺寸（解码失败时为 None）
    pub original_size: Option<(u32, u32)>,
    /// 实际发送尺寸（原样返回时与 original_size 相同）
    pub displayed_size: Option<(u32, u32)>,
    /// 由该 mime 转换而来（如 `image/bmp` → `image/png`）
    pub converted_from: Option<String>,
    /// 是否发生了缩放/重编码
    pub was_resized: bool,
}

/// base64 编码后的字节数（不实际编码）。
fn base64_len(raw: usize) -> usize {
    raw.div_ceil(3) * 4
}

/// mime 规范化：小写、去参数；内联格式归一（`image/jpg` → `image/jpeg`）。
fn normalize_mime(mime: &str) -> String {
    let base = mime
        .split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase();
    match base.as_str() {
        "image/jpg" => "image/jpeg".to_string(),
        other => other.to_string(),
    }
}

/// 该 mime 是否属于可直接内联进消息的图片格式（见 [`INLINE_IMAGE_MIMES`]）
fn is_inline(mime: &str) -> bool {
    INLINE_IMAGE_MIMES.contains(&mime)
}

/// 编码候选：PNG + 多档 JPEG（质量由高到低），返回第一个满足 `max_bytes` 者。
fn encode_within_limit(img: &DynamicImage, limits: ImageResizeLimits) -> Option<(Vec<u8>, String)> {
    let mut candidates: Vec<(Vec<u8>, &'static str)> = Vec::new();

    let mut png = Vec::new();
    if img
        .write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png)
        .is_ok()
    {
        candidates.push((png, "image/png"));
    }

    // JPEG 不支持 alpha：先转 RGB8
    let rgb = img.to_rgb8();
    let mut qualities: Vec<u8> = vec![limits.jpeg_quality, 85, 70, 55, 40];
    qualities.dedup();
    for quality in qualities {
        let mut jpeg = Vec::new();
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, quality);
        if rgb.write_with_encoder(encoder).is_ok() {
            candidates.push((jpeg, "image/jpeg"));
        }
    }

    candidates
        .into_iter()
        .find(|(bytes, _)| base64_len(bytes.len()) < limits.max_bytes)
        .map(|(bytes, mime)| (bytes, mime.to_string()))
}

/// 把图片压到内联限制内。
///
/// - `auto_resize = false`：只做格式规范化（内联格式原样返回；非内联格式转 PNG），不缩放；
/// - 无法解码/无法压到上限内返回 `None`（调用方应给出"图片过大/无法处理"的提示）。
pub fn prepare_image(
    bytes: &[u8],
    mime: &str,
    limits: ImageResizeLimits,
    auto_resize: bool,
) -> Option<PreparedImage> {
    let mime = normalize_mime(mime);

    // 内联格式 + 不缩放：原样返回
    if is_inline(&mime) && !auto_resize {
        return Some(PreparedImage {
            data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            mime_type: mime,
            original_size: None,
            displayed_size: None,
            converted_from: None,
            was_resized: false,
        });
    }

    let img = image::load_from_memory(bytes).ok()?;
    let (ow, oh) = (img.width(), img.height());

    if !auto_resize {
        // 非内联格式：转 PNG
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png)
            .ok()?;
        return Some(PreparedImage {
            data_base64: base64::engine::general_purpose::STANDARD.encode(&png),
            mime_type: "image/png".to_string(),
            original_size: Some((ow, oh)),
            displayed_size: Some((ow, oh)),
            converted_from: Some(mime),
            was_resized: false,
        });
    }

    // 快路径：内联格式且尺寸与字节都合规 → 原样（无损）
    // 非内联格式必须先转 PNG，不能走快路径
    if is_inline(&mime)
        && ow <= limits.max_width
        && oh <= limits.max_height
        && base64_len(bytes.len()) < limits.max_bytes
    {
        return Some(PreparedImage {
            data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            mime_type: mime,
            original_size: Some((ow, oh)),
            displayed_size: Some((ow, oh)),
            converted_from: None,
            was_resized: false,
        });
    }

    // 初始目标尺寸：等比缩到 maxWidth/maxHeight 内
    let (mut w, mut h) = (ow, oh);
    if w > limits.max_width {
        h = ((h as f64 * limits.max_width as f64) / w as f64)
            .round()
            .max(1.0) as u32;
        w = limits.max_width;
    }
    if h > limits.max_height {
        w = ((w as f64 * limits.max_height as f64) / h as f64)
            .round()
            .max(1.0) as u32;
        h = limits.max_height;
    }

    loop {
        let scaled = img.resize(w, h, FilterType::Lanczos3);
        if let Some((data, out_mime)) = encode_within_limit(&scaled, limits) {
            let converted_from = (out_mime != mime).then(|| mime.clone());
            return Some(PreparedImage {
                data_base64: base64::engine::general_purpose::STANDARD.encode(&data),
                mime_type: out_mime,
                original_size: Some((ow, oh)),
                displayed_size: Some((w, h)),
                converted_from,
                was_resized: true,
            });
        }

        if w == 1 && h == 1 {
            return None;
        }
        let next_w = if w == 1 {
            1
        } else {
            ((w as f64 * 0.75).floor() as u32).max(1)
        };
        let next_h = if h == 1 {
            1
        } else {
            ((h as f64 * 0.75).floor() as u32).max(1)
        };
        if next_w == w && next_h == h {
            return None;
        }
        w = next_w;
        h = next_h;
    }
}

/// 按限制兜底规范化 base64 附件。
///
/// 已合规（base64 长度 ≤ `max_bytes`）时**原样返回**，避免对已规范化的附件重复解码；
/// 超限时解码 → [`prepare_image`]；仍失败则原样返回（由 provider 报错，不静默丢图）。
/// 返回 `(data_base64, mime_type)`。
pub fn normalize_base64_attachment(
    data_base64: &str,
    mime: &str,
    limits: ImageResizeLimits,
) -> (String, String) {
    if data_base64.len() <= limits.max_bytes {
        return (data_base64.to_string(), mime.to_string());
    }

    let prepared = base64::engine::general_purpose::STANDARD
        .decode(data_base64)
        .ok()
        .and_then(|bytes| prepare_image(&bytes, mime, limits, true));
    match prepared {
        Some(p) => (p.data_base64, p.mime_type),
        None => (data_base64.to_string(), mime.to_string()),
    }
}

/// 缩放提示（帮助模型换算坐标）：`[Image: original WxH, displayed at WxH. Multiply coordinates by S ...]`
pub fn format_dimension_note(original: (u32, u32), displayed: (u32, u32)) -> Option<String> {
    let (ow, oh) = original;
    let (dw, dh) = displayed;
    if dw == 0 || dh == 0 || (ow == dw && oh == dh) {
        return None;
    }

    let scale = ow as f64 / dw as f64;
    let scale = if (scale - scale.round()).abs() < 1e-9 {
        format!("{:.0}", scale)
    } else {
        format!("{:.2}", scale)
    };

    Some(format!(
        "[Image: original {ow}x{oh}, displayed at {dw}x{dh}. Multiply coordinates by {scale} to determine the original pixel coordinates.]"
    ))
}

/// 转换提示（`[Image converted from X to Y.]`）。
pub fn format_conversion_note(converted_from: Option<&str>, to: &str) -> Option<String> {
    let from = converted_from?;
    if from == to {
        return None;
    }
    Some(format!("[Image converted from {from} to {to}.]"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgba, RgbaImage};

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let img = RgbaImage::from_pixel(w, h, Rgba([10, 20, 30, 255]));
        let mut buf = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        buf
    }

    /// 噪声图（不可压缩，便于触发字节上限）
    fn noisy_png_bytes(w: u32, h: u32) -> Vec<u8> {
        let mut img = RgbaImage::new(w, h);
        let mut seed = 0x1234_5678u32;
        for px in img.pixels_mut() {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *px = Rgba([
                (seed >> 24) as u8,
                (seed >> 16) as u8,
                (seed >> 8) as u8,
                255,
            ]);
        }
        let mut buf = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        buf
    }

    #[test]
    fn limits_from_catalog_reads_resize_profile() {
        let catalog = serde_json::json!({
            "images": {
                "maxPerRequest": 3600,
                "resize": { "maxWidth": 800, "maxHeight": 600, "maxBytes": 12345, "jpegQuality": 60 }
            }
        });
        let l = ImageResizeLimits::from_catalog(Some(&catalog));
        assert_eq!(
            l,
            ImageResizeLimits {
                max_width: 800,
                max_height: 600,
                max_bytes: 12345,
                jpeg_quality: 60
            }
        );
        // 缺失 → pi 默认值
        assert_eq!(
            ImageResizeLimits::from_catalog(None),
            ImageResizeLimits::default()
        );
        assert_eq!(
            ImageResizeLimits::from_catalog(Some(&serde_json::json!({ "images": {} }))),
            ImageResizeLimits::default()
        );
    }

    #[test]
    fn small_inline_image_passes_through_unchanged() {
        let bytes = png_bytes(10, 10);
        let out = prepare_image(&bytes, "image/png", ImageResizeLimits::default(), true).unwrap();
        assert!(!out.was_resized);
        assert_eq!(out.mime_type, "image/png");
        assert_eq!(out.original_size, Some((10, 10)));
        assert_eq!(out.displayed_size, Some((10, 10)));
        assert_eq!(out.converted_from, None);
        // 原样返回：与直接 base64 一致
        assert_eq!(
            out.data_base64,
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        );
        assert_eq!(format_dimension_note((10, 10), (10, 10)), None);
    }

    #[test]
    fn oversized_dimensions_are_scaled_down() {
        let bytes = png_bytes(4000, 100);
        let out = prepare_image(&bytes, "image/png", ImageResizeLimits::default(), true).unwrap();
        assert!(out.was_resized);
        assert_eq!(out.original_size, Some((4000, 100)));
        assert_eq!(out.displayed_size, Some((2000, 50)));
        let note = format_dimension_note(out.original_size.unwrap(), out.displayed_size.unwrap());
        assert!(
            note.unwrap()
                .contains("original 4000x100, displayed at 2000x50")
        );
    }

    #[test]
    fn byte_limit_is_enforced_by_shrinking() {
        let bytes = noisy_png_bytes(400, 400);
        assert!(base64_len(bytes.len()) > 20_000, "噪声图应超过测试上限");
        let limits = ImageResizeLimits {
            max_bytes: 20_000,
            ..Default::default()
        };
        let out = prepare_image(&bytes, "image/png", limits, true).unwrap();
        assert!(out.was_resized);
        assert!(
            out.data_base64.len() < limits.max_bytes,
            "输出应满足字节上限: {}",
            out.data_base64.len()
        );
        assert!(out.displayed_size.unwrap().0 <= 400);
    }

    #[test]
    fn non_inline_format_converts_to_png() {
        let img = image::RgbImage::from_pixel(8, 8, image::Rgb([200, 100, 50]));
        let mut bmp = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut bmp), ImageFormat::Bmp)
            .unwrap();

        let out = prepare_image(&bmp, "image/bmp", ImageResizeLimits::default(), true).unwrap();
        assert_eq!(out.mime_type, "image/png");
        assert_eq!(out.converted_from.as_deref(), Some("image/bmp"));
        assert_eq!(out.original_size, Some((8, 8)));
        assert!(
            format_conversion_note(out.converted_from.as_deref(), &out.mime_type)
                .unwrap()
                .contains("image/bmp to image/png")
        );

        // auto_resize=false 时仍做格式转换（不缩放）
        let out = prepare_image(&bmp, "image/bmp", ImageResizeLimits::default(), false).unwrap();
        assert_eq!(out.mime_type, "image/png");
        assert!(!out.was_resized);
        assert_eq!(out.displayed_size, Some((8, 8)));
    }

    #[test]
    fn auto_resize_off_keeps_inline_bytes_verbatim() {
        let bytes = png_bytes(4000, 100);
        let out = prepare_image(&bytes, "image/png", ImageResizeLimits::default(), false).unwrap();
        assert!(!out.was_resized);
        assert_eq!(
            out.data_base64,
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        );
    }

    #[test]
    fn jpg_mime_is_normalized() {
        let bytes = png_bytes(4, 4);
        let out = prepare_image(&bytes, "image/jpg", ImageResizeLimits::default(), true).unwrap();
        assert_eq!(out.mime_type, "image/jpeg");
    }

    #[test]
    fn base64_attachment_passes_through_when_within_limit() {
        let bytes = png_bytes(10, 10);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let (out, mime) =
            normalize_base64_attachment(&b64, "image/png", ImageResizeLimits::default());
        assert_eq!(out, b64, "合规附件应原样返回（不重复编码）");
        assert_eq!(mime, "image/png");
    }

    #[test]
    fn base64_attachment_shrinks_when_over_limit() {
        let bytes = noisy_png_bytes(400, 400);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let limits = ImageResizeLimits {
            max_bytes: 20_000,
            ..Default::default()
        };
        let (out, mime) = normalize_base64_attachment(&b64, "image/png", limits);
        assert!(out.len() < limits.max_bytes, "应压到上限内: {}", out.len());
        assert!(mime == "image/png" || mime == "image/jpeg");
        // 仍可解码为图片
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&out)
            .unwrap();
        assert!(image::load_from_memory(&raw).is_ok());
    }

    #[test]
    fn base64_attachment_keeps_original_when_undecodable() {
        let junk = "!!!not-base64!!!";
        let (out, mime) = normalize_base64_attachment(
            junk,
            "image/png",
            ImageResizeLimits {
                max_bytes: 1,
                ..Default::default()
            },
        );
        assert_eq!(out, junk);
        assert_eq!(mime, "image/png");
    }

    #[test]
    fn corrupt_bytes_return_none() {
        assert!(
            prepare_image(
                b"not an image",
                "image/png",
                ImageResizeLimits::default(),
                true
            )
            .is_none()
        );
        // auto_resize=false + 内联 mime：与 pi 一致，不解码、原样透传（调用方负责前置校验）
        let out = prepare_image(
            b"not an image",
            "image/png",
            ImageResizeLimits::default(),
            false,
        )
        .unwrap();
        assert!(!out.was_resized);
        // 非内联 mime 必须解码 → 失败
        assert!(
            prepare_image(
                b"not an image",
                "image/bmp",
                ImageResizeLimits::default(),
                false
            )
            .is_none()
        );
    }
}
