//! 文件参数处理：
//!
//! 把 @path 参数读入，文本拼为 <file> 块、图片 base64 编码。

use crate::{
    core::{
        self,
        provider::{AgentMessage, ContentBlock},
        settings_manager,
    },
    error::{Error, Result},
    utils::{
        image::{ImageResizeLimits, prepare_image},
        mime::detect_image_mime,
        paths::resolve_path,
    },
};

/// 处理 @文件参数，返回 (文本拼接, 图片列表[(base64, mime)])
///
/// 图片按 `limits`（模型目录 `inputLimits.images.resize`）压到内联限制内；无法处理/压不进去的
/// 图片被跳过并在文本里给出提示（不让整次提交失败）。
/// `images.blockImages` 打开时一律不附图片，只留文本提示。
pub fn process_file_arguments(
    file_args: &[String],
    cwd: &str,
    limits: ImageResizeLimits,
) -> Result<(String, Vec<(String, String)>)> {
    let auto_resize = settings_manager::read_settings_images_auto_resize();
    let block_images = settings_manager::read_settings_images_block_images();
    let mut text = String::new();
    let mut images: Vec<(String, String)> = Vec::new();

    for file_arg in file_args {
        let path = resolve_path(file_arg, cwd);
        let metadata = std::fs::metadata(&path)
            .map_err(|_| Error::msg(format!("File not found: {}", path.display())))?;
        if metadata.len() == 0 {
            continue;
        }
        let bytes = std::fs::read(&path).map_err(|_| {
            Error::msg(format!(
                "Could not read file {}: permission denied",
                path.display()
            ))
        })?;

        if let Some(mime) = detect_image_mime(&bytes) {
            if block_images {
                text.push_str(&format!(
                    "<file name=\"{}\"></file>\n[Image omitted: images are blocked by settings (images.blockImages).]\n",
                    path.display()
                ));
                continue;
            }
            match prepare_image(&bytes, mime, limits, auto_resize) {
                Some(p) => {
                    images.push((p.data_base64, p.mime_type));
                    text.push_str(&format!("<file name=\"{}\"></file>\n", path.display()));
                }
                None => {
                    text.push_str(&format!(
                        "<file name=\"{}\"></file>\n[Image omitted: could not be resized below the inline image size limit.]\n",
                        path.display()
                    ));
                }
            }
        } else {
            let content = std::fs::read_to_string(&path).map_err(|e| Error::Io {
                context: format!("Could not read file {}", path.display()),
                source: e,
            })?;
            text.push_str(&format!(
                "<file name=\"{}\">\n{}\n</file>\n",
                path.display(),
                content
            ));
        }
    }
    Ok((text, images))
}

/// 展开文本中的 @路径 引用：图片文件 → 收集图片附件列表，文本保留 @路径 原样；
/// 图片按 `limits` 压到模型内联限制内（压不进去则跳过该图）；
/// `images.blockImages` 打开时不收集任何图片（@路径 文本保持不变）。
pub fn expand_at_image_refs(
    text: &str,
    cwd: &str,
    limits: ImageResizeLimits,
) -> (String, Vec<(String, String)>) {
    let auto_resize = settings_manager::read_settings_images_auto_resize();
    let block_images = settings_manager::read_settings_images_block_images();
    let mut images: Vec<(String, String)> = Vec::new();
    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(at) = rest.find('@') {
        out.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        // 提取 token：引号包裹则到结束引号，否则到空白
        let (token, consumed) = if let Some(stripped) = rest.strip_prefix('"') {
            match stripped.find('"') {
                Some(i) => (&rest[..i + 2], i + 2),
                None => (rest, rest.len()),
            }
        } else {
            let e = rest.find(char::is_whitespace).unwrap_or(rest.len());
            (&rest[..e], e)
        };

        let inner = token.trim_matches('"').trim();
        if !inner.is_empty() && !block_images {
            let path = resolve_path(inner, cwd);
            if let Ok(bytes) = std::fs::read(&path)
                && let Some(mime) = detect_image_mime(&bytes)
                && let Some(p) = prepare_image(&bytes, mime, limits, auto_resize)
            {
                images.push((p.data_base64, p.mime_type));
            }
        }

        out.push('@');
        out.push_str(token);
        rest = &rest[consumed..];
    }

    out.push_str(rest);
    (out, images)
}

/// 构建初始消息列表（初始文本/图片 + 剩余 CLI 消息）
pub fn build_initial_messages(
    initial_text: &str,
    initial_images: &[(String, String)],
    remaining_messages: &[String],
) -> Vec<AgentMessage> {
    let mut result = Vec::new();
    if !initial_text.trim().is_empty() || !initial_images.is_empty() {
        let mut msg = core::provider::AgentMessage::user_text(initial_text);
        for (data, mime) in initial_images {
            msg.content.push(ContentBlock::Image {
                data: data.clone(),
                mime_type: mime.clone(),
            });
        }
        result.push(msg);
    }
    result.extend(
        remaining_messages
            .iter()
            .map(|t| core::provider::AgentMessage::user_text(t)),
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::image::ImageResizeLimits;
    use base64::Engine as _;

    fn write_png(path: &std::path::Path, w: u32, h: u32) {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([1, 2, 3, 255]));
        let mut buf = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        std::fs::write(path, buf).unwrap();
    }

    /// `@图片` 引用按模型内联限制缩放（pi 0.87.0 #9631：文件附件同样受限）。
    #[test]
    fn at_image_refs_apply_model_resize_limits() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("shot.png");
        write_png(&p, 400, 200);
        let cwd = dir.path().to_str().unwrap();

        let limits = ImageResizeLimits {
            max_width: 50,
            max_height: 50,
            ..Default::default()
        };
        let (text, images) = expand_at_image_refs("look @shot.png", cwd, limits);
        assert_eq!(text, "look @shot.png", "文本里的 @路径 保持原样");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].1, "image/png");
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&images[0].0)
            .unwrap();
        let dims = image::ImageReader::new(std::io::Cursor::new(&raw))
            .with_guessed_format()
            .unwrap()
            .into_dimensions()
            .unwrap();
        assert_eq!(dims, (50, 25), "应按 maxWidth=50 等比缩放");
    }

    /// 压不进上限的图片被跳过（不产生附件），其余引用不受影响。
    #[test]
    fn process_file_arguments_skip_unresizable_image() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.png");
        write_png(&p, 200, 200);
        let cwd = dir.path().to_str().unwrap();

        // max_bytes 极小 → 无论如何编码都压不进去 → 省略图片并给出提示
        let limits = ImageResizeLimits {
            max_width: 4,
            max_height: 4,
            max_bytes: 1,
            ..Default::default()
        };
        let (text, images) =
            process_file_arguments(&[p.to_string_lossy().to_string()], cwd, limits).unwrap();
        assert!(images.is_empty());
        assert!(text.contains("<file name="), "{text}");
        assert!(text.contains("Image omitted"), "{text}");
    }

    /// `images.blockImages` 打开时：@图片 引用与 CLI 文件参数都不再附图片（只留文本提示）。
    #[test]
    fn block_images_drops_every_file_image() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("shot.png");
        write_png(&p, 40, 20);
        let cwd = dir.path().to_str().unwrap();
        let path = p.to_string_lossy().to_string();

        settings_manager::write_settings_images_block_images(true).unwrap();

        let (text, images) =
            expand_at_image_refs(&format!("look @{path}"), cwd, Default::default());
        assert!(images.is_empty(), "@引用 不应附图片");
        assert!(
            text.contains(&format!("@{path}")),
            "@路径 文本保持原样: {text}"
        );

        let (text, images) = process_file_arguments(
            std::slice::from_ref(&path),
            cwd,
            ImageResizeLimits::default(),
        )
        .unwrap();
        assert!(images.is_empty(), "CLI 参数不应附图片");
        assert!(
            text.contains("images.blockImages"),
            "应给出被阻止的提示: {text}"
        );

        // 关掉后恢复
        settings_manager::write_settings_images_block_images(false).unwrap();
        let (_, images) = expand_at_image_refs(&format!("look @{path}"), cwd, Default::default());
        assert_eq!(images.len(), 1, "关闭后应恢复附图片");
    }

    /// 文本文件参数不受图片限制影响。
    #[test]
    fn process_file_arguments_reads_text_files() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("note.txt");
        std::fs::write(&p, "hello").unwrap();
        let (text, images) = process_file_arguments(
            &[p.to_string_lossy().to_string()],
            dir.path().to_str().unwrap(),
            ImageResizeLimits::default(),
        )
        .unwrap();
        assert!(images.is_empty());
        assert!(text.contains("hello"), "{text}");
    }
}
