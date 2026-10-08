//! MIME 检测：魔数检测（仅支持可直接 inline 的格式）。

/// 从 `offset` 处读 4 字节大端整数；越界时返回 0。
fn u32_be(data: &[u8], offset: usize) -> u32 {
    data.get(offset..offset + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        .unwrap_or(0)
}

/// 从 `offset` 处读 4 字节小端整数；越界时返回 0。
fn u32_le(data: &[u8], offset: usize) -> u32 {
    data.get(offset..offset + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .unwrap_or(0)
}

/// 从 `offset` 处读 2 字节小端整数；越界时返回 0。
fn u16_le(data: &[u8], offset: usize) -> u16 {
    data.get(offset..offset + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .unwrap_or(0)
}

/// 校验 PNG 签名后第 8 字节起是否为合法的 IHDR 首块（长度 13、类型 "IHDR"）。
fn is_valid_png(data: &[u8]) -> bool {
    data.len() >= 16 && u32_be(data, 8) == 13 && &data[12..16] == b"IHDR"
}

/// 遍历 PNG chunk 判断是否含 acTL（APNG 动画控制块）；遇到 IDAT 即停止。
/// 块结构不合法或未遇到 acTL 时返回 false。
fn is_animated_png(data: &[u8]) -> bool {
    let mut offset = 8usize;
    while offset + 8 <= data.len() {
        let chunk_len = u32_be(data, offset) as usize;
        let chunk_type = offset + 4;
        if data.len() >= chunk_type + 4 && &data[chunk_type..chunk_type + 4] == b"acTL" {
            return true;
        }
        if data.len() >= chunk_type + 4 && &data[chunk_type..chunk_type + 4] == b"IDAT" {
            return false;
        }
        let next = offset.saturating_add(8 + chunk_len + 4);
        if next <= offset || next > data.len() {
            return false;
        }
        offset = next;
    }
    false
}

/// 校验 BMP 头：DIB 头大小、像素数据偏移、位深/调色板等字段需自洽。
fn is_bmp(data: &[u8]) -> bool {
    if data.len() < 26 {
        return false;
    }
    let declared_file_size = u32_le(data, 2);
    let pixel_data_offset = u32_le(data, 10);
    let dib_header_size = u32_le(data, 14);
    if declared_file_size != 0 && declared_file_size < 26 {
        return false;
    }
    if pixel_data_offset < 14 + dib_header_size {
        return false;
    }
    if declared_file_size != 0 && pixel_data_offset >= declared_file_size {
        return false;
    }
    let (planes, bpp) = if dib_header_size == 12 {
        if data.len() < 26 {
            return false;
        }
        (u16_le(data, 22), u16_le(data, 24))
    } else if (40..=124).contains(&dib_header_size) {
        if data.len() < 30 {
            return false;
        }
        (u16_le(data, 26), u16_le(data, 28))
    } else {
        return false;
    };
    planes == 1 && matches!(bpp, 1 | 4 | 8 | 16 | 24 | 32)
}

/// 通过魔数检测图片 MIME 类型；非图片返回 None
pub fn detect_image_mime(data: &[u8]) -> Option<&'static str> {
    /// PNG 文件头的 8 字节魔数签名，供魔数检测识别 PNG 图片。
    const PNG_SIG: &[u8] = &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
    if data.starts_with(&[0xff, 0xd8, 0xff]) {
        return if data.get(3) == Some(&0xf7) {
            None
        } else {
            Some("image/jpeg")
        };
    }
    if data.starts_with(PNG_SIG) {
        return if is_valid_png(data) && !is_animated_png(data) {
            Some("image/png")
        } else {
            None
        };
    }
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if data.starts_with(b"RIFF") && data.len() >= 12 && &data[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if data.starts_with(b"BM") && is_bmp(data) {
        return Some("image/bmp");
    }
    None
}

/// 移除文本开头的 UTF-8 BOM（\u{FEFF}）；无 BOM 时原样返回。
/// frontmatter 与配置文件读取前调用，避免 BOM 破坏 `---` 检测与 JSON 解析。
pub fn strip_bom(content: &str) -> &str {
    content.strip_prefix('\u{FEFF}').unwrap_or(content)
}

#[cfg(test)]
mod image_mime_tests {
    use super::detect_image_mime;

    #[test]
    fn detects_only_complete_gif_signatures() {
        // 回归 #9755：仅 `GIF` 前缀的文本文件不得被当作图片
        assert_eq!(detect_image_mime(b"GIF87a\x00\x01"), Some("image/gif"));
        assert_eq!(detect_image_mime(b"GIF89a\x00\x01"), Some("image/gif"));
        assert_eq!(detect_image_mime(b"GIF is a text prefix"), None);
        assert_eq!(detect_image_mime(b"GIF"), None);
        assert_eq!(detect_image_mime(b"GIF99a"), None);
    }
}

#[cfg(test)]
mod strip_bom_tests {
    #[test]
    fn strips_leading_bom() {
        assert_eq!(super::strip_bom("\u{FEFF}---\nname: x"), "---\nname: x");
        assert_eq!(super::strip_bom("\u{FEFF}{\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn without_bom_unchanged() {
        assert_eq!(super::strip_bom("plain"), "plain");
        assert_eq!(super::strip_bom(""), "");
    }
}
