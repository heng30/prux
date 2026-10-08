//! utils 集成测试：truncate、edit_diff、mime、paths。

use prux::utils::edit_diff::{
    Edit, apply_edits_to_normalized_content, detect_line_ending, generate_diff_string,
    generate_unified_patch, restore_line_endings, strip_bom,
};
use prux::utils::mime::detect_image_mime;
use prux::utils::paths::{expand_tilde, expand_tilde_path, resolve_path, theme_name_from_arg};
use prux::utils::truncate::{
    TruncatedBy, format_size, truncate_head, truncate_line, truncate_tail,
};

// ---------- truncate ----------

#[test]
fn truncate_head_keeps_beginning() {
    let content = (1..=100)
        .map(|i| format!("line{}", i))
        .collect::<Vec<_>>()
        .join("\n");
    let r = truncate_head(&content, (Some(10), None));
    assert!(r.truncated);
    assert_eq!(r.output_lines, 10);
    assert!(r.content.starts_with("line1"));
    assert!(!r.content.contains("line50"));
    assert_eq!(r.truncated_by, TruncatedBy::Lines);
}

#[test]
fn truncate_tail_keeps_end() {
    let content = (1..=100)
        .map(|i| format!("line{}", i))
        .collect::<Vec<_>>()
        .join("\n");
    let r = truncate_tail(&content, (Some(10), None));
    assert!(r.truncated);
    assert_eq!(r.output_lines, 10);
    assert!(r.content.starts_with("line91"));
    assert!(r.content.contains("line100"));
    assert!(
        !r.content.contains("line89"),
        "line89 should be truncated away"
    );
}

#[test]
fn truncate_respects_byte_limit() {
    let content = "x".repeat(200_000);
    let r = truncate_head(&content, (None, Some(50 * 1024)));
    assert!(r.truncated);
    assert!(r.content.len() <= 50 * 1024 + 10);
    assert_eq!(r.truncated_by, TruncatedBy::Bytes);
}

#[test]
fn truncate_does_not_split_multibyte() {
    // emoji 是多字节，不能拦腰切断
    let content = "😀".repeat(10_000);
    let r = truncate_head(&content, (None, Some(1000)));
    assert!(std::str::from_utf8(r.content.as_bytes()).is_ok());
}

#[test]
fn truncate_first_line_exceeds() {
    let content = "y".repeat(200_000);
    let r = truncate_head(&content, (None, None));
    assert!(r.first_line_exceeds_limit);
}

#[test]
fn truncate_line_shortens() {
    let long = "a".repeat(1000);
    let (out, was) = truncate_line(&long, 500);
    assert!(was);
    assert!(out.len() < 1000);
}

#[test]
fn format_size_human() {
    assert_eq!(format_size(500), "500B");
    assert_eq!(format_size(5 * 1024), "5.0KB");
    assert_eq!(format_size(3 * 1024 * 1024), "3.0MB");
}

// ---------- edit_diff ----------

#[test]
fn detect_and_restore_line_endings() {
    assert_eq!(detect_line_ending("a\nb"), "\n");
    assert_eq!(detect_line_ending("a\r\nb"), "\r\n");
    assert_eq!(restore_line_endings("a\nb", "\r\n"), "a\r\nb");
}

#[test]
fn strip_bom_removes_prefix() {
    let (bom, text) = strip_bom("\u{FEFF}hello");
    assert!(!bom.is_empty());
    assert_eq!(text, "hello");
    let (bom2, text2) = strip_bom("no bom");
    assert!(bom2.is_empty());
    assert_eq!(text2, "no bom");
}

#[test]
fn apply_edits_replaces_unique_blocks() {
    let content = "fn main() {\n    let x = 1;\n    let y = 2;\n}";
    let edits = vec![
        Edit {
            old_text: "let x = 1;".into(),
            new_text: "let x = 10;".into(),
        },
        Edit {
            old_text: "let y = 2;".into(),
            new_text: "let y = 20;".into(),
        },
    ];
    let r = apply_edits_to_normalized_content(content, &edits, "test.rs").unwrap();
    assert!(r.new_content.contains("let x = 10;"));
    assert!(r.new_content.contains("let y = 20;"));
}

#[test]
fn apply_edits_rejects_missing_old_text() {
    let content = "abc";
    let edits = vec![Edit {
        old_text: "zzz".into(),
        new_text: "x".into(),
    }];
    assert!(apply_edits_to_normalized_content(content, &edits, "t.rs").is_err());
}

#[test]
fn generate_diff_and_patch() {
    let old = "line1\nline2\nline3";
    let new = "line1\nCHANGED\nline3";
    let (diff, first_line) = generate_diff_string(old, new, 4);
    assert!(first_line.is_some());
    assert!(diff.contains("CHANGED"));
    let patch = generate_unified_patch("a.rs", old, new, 4);
    assert!(patch.contains("@@"));
    assert!(patch.contains("CHANGED"));
}

#[test]
fn generate_diff_has_no_spurious_blank_lines() {
    // 多行上下文不应被拆出额外空行（similar 的行切片自带结尾换行）
    let old = "line1\nline2\nline3\nline4\nline5\nline6";
    let new = "line1\nline2\nCHANGED\nline4\nline5\nline6";
    let (diff, _) = generate_diff_string(old, new, 4);
    assert!(
        !diff.contains("\n\n"),
        "diff contains blank line separators: {diff:?}"
    );
    let lines: Vec<&str> = diff.split('\n').collect();
    assert_eq!(lines.len(), 7, "unexpected line count: {lines:?}");

    // 真正的内容空行仍应保留
    let old = "a\n\nb";
    let new = "a\n\nB";
    let (diff, _) = generate_diff_string(old, new, 4);
    let lines: Vec<&str> = diff.split('\n').collect();
    assert!(
        lines.iter().any(|l| l.trim_end().ends_with('2')),
        "blank context line missing: {lines:?}"
    );
}

// ---------- mime ----------

#[test]
fn mime_detection_magic_bytes() {
    // PNG
    let png = [
        0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52,
    ];
    assert_eq!(detect_image_mime(&png), Some("image/png"));
    // JPEG
    let jpeg = [0xFFu8, 0xD8, 0xFF, 0xE0];
    assert_eq!(detect_image_mime(&jpeg), Some("image/jpeg"));
    // GIF
    assert_eq!(detect_image_mime(b"GIF89a..."), Some("image/gif"));
    // WEBP
    let mut webp = b"RIFF\x00\x00\x00\x00WEBP".to_vec();
    webp[4..8].copy_from_slice(&[10, 0, 0, 0]);
    assert_eq!(detect_image_mime(&webp), Some("image/webp"));
    // 非图片
    assert_eq!(detect_image_mime(b"plain text"), None);
}

// ---------- paths ----------

#[test]
fn expand_tilde_handles_home() {
    let home = std::env::var("HOME").unwrap();
    assert_eq!(
        expand_tilde(std::path::PathBuf::from("~")),
        std::path::PathBuf::from(&home)
    );
    assert_eq!(
        expand_tilde(std::path::PathBuf::from("~/x")),
        std::path::PathBuf::from(&home).join("x")
    );
    assert_eq!(
        expand_tilde(std::path::PathBuf::from("/abs")),
        std::path::PathBuf::from("/abs")
    );
    assert_eq!(
        expand_tilde_path("~/y".to_string()),
        std::path::PathBuf::from(&home).join("y")
    );
}

#[test]
fn resolve_path_joins_cwd() {
    assert_eq!(
        resolve_path("rel.txt", "/tmp"),
        std::path::PathBuf::from("/tmp/rel.txt")
    );
    assert_eq!(
        resolve_path("/abs.txt", "/tmp"),
        std::path::PathBuf::from("/abs.txt")
    );
}

#[test]
fn theme_name_from_arg_extracts_stem() {
    assert_eq!(theme_name_from_arg("/x/dark.json"), "dark");
    assert_eq!(theme_name_from_arg("synthwave"), "synthwave");
}
