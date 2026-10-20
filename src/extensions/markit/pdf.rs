//! PDF → 纯文本：只抽**文本层**，不渲染版面、不识别图片。
//!
//! 扫描件（图片型 PDF）没有文本层，抽出来是空串——由上层给出「没有可提取文本」的提示。
//! `catch_unwind` 的兜底在上层 [`super::parse`] 统一做。

/// 从 PDF 字节里抽取文本层。
pub(super) fn extract(bytes: &[u8]) -> Result<String, String> {
    pdf_extract::extract_text_from_mem(bytes)
        .map_err(|e| format!("failed to extract PDF text: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn garbage_bytes_report_error() {
        let err = extract(b"not a pdf at all").unwrap_err();
        assert!(!err.is_empty(), "{err}");
    }
}
