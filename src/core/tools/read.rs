//! read 工具：文本/图片读取 + 行偏移分页 + 截断。

use crate::{
    core::tools::{
        index::{
            ToolError, ToolResult, ToolResultAttachment, detect_supported_image_mime_type,
            resolve_tool_path,
        },
        operations::{DEFAULT_TOOL_OPS, ToolOperations},
    },
    utils::{
        image::format_dimension_note,
        image::{ImageResizeLimits, format_conversion_note, prepare_image},
        truncate::{self, DEFAULT_MAX_BYTES, TruncatedBy},
    },
};
use std::{io::Cursor, path::PathBuf};
use unicode_normalization::UnicodeNormalization;

/// read 工具图片选项
#[derive(Debug, Clone, Copy)]
pub struct ReadImageOptions {
    /// 模型输入是否含 image（决定是否附加 vision 提示）
    pub model_supports_images: bool,
    /// 是否自动缩放（settings images.autoResize，默认 true）
    pub auto_resize: bool,
    /// 是否阻止向模型发送图片（settings images.blockImages，默认 false）
    pub block_images: bool,
    /// 该模型的内联限制（目录 `inputLimits.images.resize`）
    pub resize_limits: ImageResizeLimits,
}

impl Default for ReadImageOptions {
    /// 默认图片选项：模型不支持图片、开启自动缩放、不阻止图片，内联限制取默认值。
    fn default() -> Self {
        Self {
            model_supports_images: false,
            auto_resize: true,
            block_images: false,
            resize_limits: ImageResizeLimits::default(),
        }
    }
}

/// read 工具入口：按行范围（`offset`/`limit`，从 0 起）读取文件，图片按默认选项处理。
/// 路径会先尝试若干变体（空格/NFD/弯引号）定位实际文件；文件不存在或不可读时返回 `ToolError`。
pub async fn execute_read(
    path: &str,
    offset: Option<i64>,
    limit: Option<i64>,
    cwd: &str,
) -> Result<ToolResult, ToolError> {
    execute_read_with_options(path, offset, limit, cwd, ReadImageOptions::default()).await
}

/// 带图片选项的读取（agent 调用方按模型/设置注入选项）
pub async fn execute_read_with_options(
    path: &str,
    offset: Option<i64>,
    limit: Option<i64>,
    cwd: &str,
    image_opts: ReadImageOptions,
) -> Result<ToolResult, ToolError> {
    execute_read_with_ops(path, offset, limit, cwd, image_opts, &DEFAULT_TOOL_OPS).await
}

/// 带后端注入的读取
pub async fn execute_read_with_ops(
    path: &str,
    offset: Option<i64>,
    limit: Option<i64>,
    cwd: &str,
    image_opts: ReadImageOptions,
    ops: &dyn ToolOperations,
) -> Result<ToolResult, ToolError> {
    let absolute_path = resolve_read_path_variant(path, cwd, ops);
    let abs = absolute_path.to_string_lossy().to_string();
    ops.access(&abs).map_err(|e| {
        ToolError(format!(
            "Error reading file: {}. {}",
            absolute_path.display(),
            e
        ))
    })?;
    let bytes = match ops.read_file(&abs) {
        Ok(b) => b,
        Err(e) => {
            return Err(ToolError(format!(
                "Error reading file: {}. {}",
                absolute_path.display(),
                e
            )));
        }
    };
    let mime_type = detect_supported_image_mime_type(&bytes);
    if let Some(mime) = mime_type {
        return Ok(read_image_result(mime, &bytes, &image_opts));
    }

    // 非 UTF-8 用 U+FFFD 替换（尽力渲染乱码文本）
    let text_content = String::from_utf8_lossy(&bytes).to_string();
    let all_lines: Vec<&str> = text_content.split('\n').collect();
    let total_file_lines = all_lines.len();
    let start_line = match offset {
        Some(o) => (o - 1).max(0) as usize,
        None => 0,
    };
    let start_line_display = start_line + 1;
    if start_line >= all_lines.len() {
        return Err(ToolError(format!(
            "Offset {} is beyond end of file ({} lines total)",
            offset.unwrap_or(0),
            all_lines.len()
        )));
    }

    let selected_content: String;
    let mut user_limited_lines: Option<usize> = None;
    if let Some(limit) = limit {
        let end_line = (start_line + limit as usize).min(all_lines.len());
        selected_content = all_lines[start_line..end_line].join("\n");
        user_limited_lines = Some(end_line - start_line);
    } else {
        selected_content = all_lines[start_line..].join("\n");
    }

    let truncation = truncate::truncate_head(&selected_content, (None, None));
    let mut output_text: String;
    if truncation.first_line_exceeds_limit {
        let first_line_size = truncate::format_size(all_lines[start_line].len());
        output_text = format!(
            "[Line {} is {}, exceeds {} limit. Use bash: sed -n '{}p' {} | head -c {}]",
            start_line_display,
            first_line_size,
            truncate::format_size(DEFAULT_MAX_BYTES),
            start_line_display,
            path,
            DEFAULT_MAX_BYTES
        );
    } else if truncation.truncated {
        let end_line_display = start_line_display + truncation.output_lines - 1;
        let next_offset = end_line_display + 1;
        output_text = truncation.content.clone();
        match truncation.truncated_by {
            TruncatedBy::Lines => {
                output_text.push_str(&format!(
                    "\n\n[Showing lines {}-{} of {}. Use offset={} to continue.]",
                    start_line_display, end_line_display, total_file_lines, next_offset
                ));
            }
            _ => {
                output_text.push_str(&format!(
                    "\n\n[Showing lines {}-{} of {} ({} limit). Use offset={} to continue.]",
                    start_line_display,
                    end_line_display,
                    total_file_lines,
                    truncate::format_size(DEFAULT_MAX_BYTES),
                    next_offset
                ));
            }
        }
    } else if let Some(limited) = user_limited_lines {
        if start_line + limited < all_lines.len() {
            let remaining = all_lines.len() - (start_line + limited);
            let next_offset = start_line + limited + 1;
            output_text = format!(
                "{}\n\n[{} more lines in file. Use offset={} to continue.]",
                truncation.content, remaining, next_offset
            );
        } else {
            output_text = truncation.content;
        }
    } else {
        output_text = truncation.content;
    }

    Ok(ToolResult::text(output_text))
}

/// 路径变体解析：原路径存在则直接用；否则尝试 5 个变体后取第一个存在的
fn resolve_read_path_variant(path: &str, cwd: &str, ops: &dyn ToolOperations) -> PathBuf {
    let base = resolve_tool_path(path, cwd);
    let base_str = base.to_string_lossy().to_string();
    if ops.exists(&base_str) {
        return base;
    }
    let mut variants: Vec<String> = Vec::new();

    // 1) " (AM|PM)." 窄无断空格替换
    if let Some(idx) = find_am_pm(&base_str) {
        let mut v = String::new();
        v.push_str(&base_str[..idx]);
        v.push('\u{202F}');
        v.push_str(&base_str[idx + 1..]);
        variants.push(v);
    }

    // 2) NFD 归一
    let nfd: String = base_str.nfd().collect();
    if nfd != base_str {
        variants.push(nfd.clone());
    }

    // 3) 弯引号替换（U+2018/2019/201B → '，U+201C/201D/201F → "）
    let curly: String = base_str
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201F}' => '"',
            _ => c,
        })
        .collect();

    if curly != base_str {
        variants.push(curly.clone());
    }

    // 4) NFD + 弯引号
    let nfd_curly: String = curly.nfd().collect();
    if nfd_curly != base_str && nfd_curly != nfd && nfd_curly != curly {
        variants.push(nfd_curly);
    }

    for v in variants {
        if ops.exists(&v) {
            return PathBuf::from(v);
        }
    }

    base
}

/// 返回首个「空格 + 两个点」的起始下标（macOS 截图文件名里 AM/PM 前的空格变体）；
/// 调用方据此把该空格换成窄无断空格 U+202F；无匹配返回 None。
fn find_am_pm(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    (0..bytes.len().saturating_sub(2))
        .find(|&i| bytes[i] == b' ' && bytes[i + 1] == b'.' && bytes.get(i + 2) == Some(&b'.'))
}

/// 图片读取结果：缩放/转换 + base64 附件
fn read_image_result(mime: &str, bytes: &[u8], opts: &ReadImageOptions) -> ToolResult {
    let original_size = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok());
    let (_w, _h) = original_size.unwrap_or((0, 0));

    if opts.block_images {
        let mut r = ToolResult::text(format!("Read image file [{}]", mime));
        if let Some((w, h)) = original_size {
            r.details = Some(serde_json::json!({
                "blocked": true,
                "size": { "width": w, "height": h },
                "mimeType": mime,
            }));
        }
        return r;
    }

    let prepared = match prepare_image(bytes, mime, opts.resize_limits, opts.auto_resize) {
        Some(p) => p,
        None => {
            return ToolResult::text(format!(
                "Read image file [{mime}]\n[Image file could not be processed: unsupported or corrupt image.]"
            ));
        }
    };

    let mut text = format!("Read image file [{mime}]");
    if let (Some(original), Some(displayed)) = (prepared.original_size, prepared.displayed_size)
        && let Some(note) = format_dimension_note(original, displayed)
    {
        text.push('\n');
        text.push_str(&note);
    }

    if let Some(note) =
        format_conversion_note(prepared.converted_from.as_deref(), &prepared.mime_type)
    {
        text.push('\n');
        text.push_str(&note);
    }

    if !opts.model_supports_images {
        return ToolResult::text(format!(
            "{text}\n[Image content omitted: image attachments are not supported by this model.]"
        ));
    }

    // 兜底：规范化后仍超限（例如 auto_resize 关闭且原图过大）→ 不发送图片
    if prepared.data_base64.len() > opts.resize_limits.max_bytes {
        return ToolResult::text(format!(
            "Read image file [{mime}]\n[Image content omitted: image too large to send to model.]"
        ));
    }

    ToolResult::text(text).with_attachment(ToolResultAttachment {
        data_base64: prepared.data_base64,
        mime_type: prepared.mime_type,
        original_size: prepared.original_size,
        converted_from: prepared.converted_from,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn make_png(w: u32, h: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        image::RgbaImage::from_pixel(w, h, image::Rgba([10, 20, 30, 255]))
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    fn make_bmp(w: u32, h: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        image::RgbImage::from_pixel(w, h, image::Rgb([200, 100, 50]))
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Bmp)
            .unwrap();
        buf
    }

    #[test]
    fn read_png_attaches_image_block() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pic.png");
        std::fs::write(&p, make_png(64, 32)).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt
            .block_on(execute_read_with_options(
                p.to_str().unwrap(),
                None,
                None,
                dir.path().to_str().unwrap(),
                ReadImageOptions {
                    model_supports_images: true,
                    ..Default::default()
                },
            ))
            .unwrap();
        assert_eq!(r.attachments.len(), 1);
        assert_eq!(r.attachments[0].mime_type, "image/png");
        assert_eq!(r.attachments[0].original_size, Some((64, 32)));
        assert!(r.attachments[0].converted_from.is_none());
        let dec = base64::engine::general_purpose::STANDARD
            .decode(&r.attachments[0].data_base64)
            .unwrap();
        assert_eq!(dec.len(), make_png(64, 32).len()); // 尺寸相同（未缩放）
    }

    #[test]
    fn read_bmp_converts_to_png() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pic.bmp");
        std::fs::write(&p, make_bmp(16, 16)).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt
            .block_on(execute_read_with_options(
                p.to_str().unwrap(),
                None,
                None,
                dir.path().to_str().unwrap(),
                ReadImageOptions {
                    model_supports_images: true,
                    ..Default::default()
                },
            ))
            .unwrap();
        assert_eq!(r.attachments.len(), 1);
        assert_eq!(r.attachments[0].mime_type, "image/png");
        assert_eq!(
            r.attachments[0].converted_from.as_deref(),
            Some("image/bmp")
        );
        assert!(r.text.contains("Read image file [image/bmp]"));
        assert!(
            r.text
                .contains("Image converted from image/bmp to image/png")
        );
    }

    /// 模型目录的 inputLimits 生效：更小的 maxWidth 直接作用于 read 附件。
    #[test]
    fn read_respects_per_model_resize_limits() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("wide.png");
        std::fs::write(&p, make_png(400, 200)).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt
            .block_on(execute_read_with_options(
                p.to_str().unwrap(),
                None,
                None,
                dir.path().to_str().unwrap(),
                ReadImageOptions {
                    model_supports_images: true,
                    resize_limits: ImageResizeLimits {
                        max_width: 64,
                        max_height: 64,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ))
            .unwrap();
        assert_eq!(r.attachments.len(), 1);
        let dec = base64::engine::general_purpose::STANDARD
            .decode(&r.attachments[0].data_base64)
            .unwrap();
        let dims = image::ImageReader::new(std::io::Cursor::new(&dec))
            .with_guessed_format()
            .unwrap()
            .into_dimensions()
            .unwrap();
        assert_eq!(dims, (64, 32), "应按模型 maxWidth=64 等比缩放");
        assert!(r.text.contains("original 400x200, displayed at 64x32"));
    }

    #[test]
    fn read_large_png_resizes_to_2000() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.png");
        std::fs::write(&p, make_png(4000, 2000)).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt
            .block_on(execute_read_with_options(
                p.to_str().unwrap(),
                None,
                None,
                dir.path().to_str().unwrap(),
                ReadImageOptions {
                    model_supports_images: true,
                    ..Default::default()
                },
            ))
            .unwrap();
        assert_eq!(r.attachments.len(), 1);
        let dec = base64::engine::general_purpose::STANDARD
            .decode(&r.attachments[0].data_base64)
            .unwrap();
        let dims = image::ImageReader::new(std::io::Cursor::new(&dec))
            .with_guessed_format()
            .unwrap()
            .into_dimensions()
            .unwrap();
        let limits = ImageResizeLimits::default();
        assert!(
            dims.0 <= limits.max_width && dims.1 <= limits.max_height,
            "{dims:?}"
        );
        assert!(r.text.contains("[Image: original 4000x2000,"), "{}", r.text);
    }

    #[test]
    fn read_block_images_returns_text() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pic.png");
        std::fs::write(&p, make_png(8, 8)).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt
            .block_on(execute_read_with_options(
                p.to_str().unwrap(),
                None,
                None,
                dir.path().to_str().unwrap(),
                ReadImageOptions {
                    model_supports_images: true,
                    block_images: true,
                    ..Default::default()
                },
            ))
            .unwrap();
        assert!(r.attachments.is_empty());
        assert!(r.text.contains("Read image file [image/png]"));
    }

    #[test]
    fn read_non_vision_model_omits_attachment() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pic.png");
        std::fs::write(&p, make_png(8, 8)).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt
            .block_on(execute_read_with_options(
                p.to_str().unwrap(),
                None,
                None,
                dir.path().to_str().unwrap(),
                ReadImageOptions::default(), // model_supports_images=false
            ))
            .unwrap();
        assert!(r.attachments.is_empty());
        assert!(
            r.text
                .contains("image attachments are not supported by this model")
        );
    }

    #[test]
    fn read_gif_stays_inline() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("anim.gif");
        let mut buf = Vec::new();
        let frame = image::RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255]));
        image::codecs::gif::GifEncoder::new(&mut buf)
            .encode_frame(image::Frame::new(frame))
            .unwrap();
        std::fs::write(&p, &buf).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt
            .block_on(execute_read_with_options(
                p.to_str().unwrap(),
                None,
                None,
                dir.path().to_str().unwrap(),
                ReadImageOptions {
                    model_supports_images: true,
                    ..Default::default()
                },
            ))
            .unwrap();
        assert_eq!(r.attachments.len(), 1);
        // GIF 是内联格式（pi 0.87.0 语义）：原样保留，不做 PNG 转换
        assert_eq!(r.attachments[0].mime_type, "image/gif");
        assert_eq!(r.attachments[0].converted_from, None);
    }
}
