//! edit 工具：精确文本替换 + fuzzy 匹配 + 统一补丁。

use crate::{
    core::tools::index::{ToolError, ToolResult, err_code, resolve_tool_path},
    utils::edit_diff,
};
use serde_json::{Value, json};

/// 对文件执行一处或多处精确文本替换（含 fuzzy 匹配与重叠检测），成功时写盘。
///
/// `edits` 为含 `oldText`/`newText` 的对象或对象数组，缺失/为空时返回错误；
/// 返回 `(工具结果, 供 UI 展示的 diff+patch 文本, 首个变更行号)`。
pub async fn execute_edit(
    path: &str,
    edits: &Value,
    cwd: &str,
) -> Result<(ToolResult, String, Option<usize>), ToolError> {
    let edits = normalize_edit_args(edits);
    if !edits.is_array() || edits.as_array().map(|a| a.is_empty()).unwrap_or(true) {
        return Err(ToolError(
            "Edit tool input is invalid. edits must contain at least one replacement.".to_string(),
        ));
    }
    let edits_arr = edits.as_array().unwrap();
    let mut parsed: Vec<edit_diff::Edit> = Vec::new();
    for e in edits_arr {
        let old_text = e.get("oldText").and_then(|v| v.as_str()).unwrap_or("");
        let new_text = e.get("newText").and_then(|v| v.as_str()).unwrap_or("");
        parsed.push(edit_diff::Edit {
            old_text: old_text.to_string(),
            new_text: new_text.to_string(),
        });
    }

    let absolute_path = resolve_tool_path(path, cwd);
    let is_file = std::fs::metadata(&absolute_path)
        .map(|m| m.is_file() || m.file_type().is_symlink())
        .unwrap_or(false);
    if !is_file {
        return Err(ToolError(format!(
            "Could not edit file: {}. Path is not a file.",
            path
        )));
    }
    let content = match std::fs::read_to_string(&absolute_path) {
        Ok(c) => c,
        Err(e) => {
            return Err(ToolError(format!(
                "Could not edit file: {}. Error code: {}. {}",
                path,
                err_code(&e),
                e
            )));
        }
    };

    let (bom, text) = edit_diff::strip_bom(&content);
    let original_ending = edit_diff::detect_line_ending(&text);
    let normalized_content = edit_diff::normalize_to_lf(&text);
    let result = edit_diff::apply_edits_to_normalized_content(&normalized_content, &parsed, path)
        .map_err(ToolError)?;
    let final_content = format!(
        "{}{}",
        bom,
        edit_diff::restore_line_endings(&result.new_content, original_ending)
    );
    if let Err(e) = std::fs::write(&absolute_path, &final_content) {
        return Err(ToolError(format!(
            "Could not edit file: {}. Error code: {}. {}",
            path,
            err_code(&e),
            e
        )));
    }

    let (diff, first_changed_line) =
        edit_diff::generate_diff_string(&result.base_content, &result.new_content, 4);
    let patch =
        edit_diff::generate_unified_patch(path, &result.base_content, &result.new_content, 4);
    let mut result = ToolResult::text(format!(
        "Successfully replaced {} block(s) in {}.",
        parsed.len(),
        path
    ));
    result.details = Some(json!({
        "diff": diff,
        "patch": patch,
        "firstChangedLine": first_changed_line,
    }));

    Ok((
        result,
        format!("{}\n\nPATCH:\n{}", diff, patch),
        first_changed_line,
    ))
}

/// 预览 edit 的显示 diff：只读文件、apply 替换但不写盘，返回 generate_diff_string(4)
/// 的 diff 字符串（供 UI 在工具尚未返回结果时展示 call 阶段预览）。
/// 任何失败（路径不可读 / oldText 不匹配 / 重叠 / 无变化）都返回 None，调用方静默降级。
pub fn preview_edit_diff(path: &str, edits: &Value, cwd: &str) -> Option<String> {
    let edits = normalize_edit_args(edits);
    let edits_arr = edits.as_array()?;
    if edits_arr.is_empty() {
        return None;
    }

    let mut parsed: Vec<edit_diff::Edit> = Vec::new();
    for e in edits_arr {
        let old_text = e.get("oldText").and_then(|v| v.as_str()).unwrap_or("");
        let new_text = e.get("newText").and_then(|v| v.as_str()).unwrap_or("");
        if old_text.is_empty() {
            return None;
        }
        parsed.push(edit_diff::Edit {
            old_text: old_text.to_string(),
            new_text: new_text.to_string(),
        });
    }

    let absolute_path = resolve_tool_path(path, cwd);
    let content = std::fs::read_to_string(&absolute_path).ok()?;
    let (_, text) = edit_diff::strip_bom(&content);
    let normalized = edit_diff::normalize_to_lf(&text);
    let result = edit_diff::apply_edits_to_normalized_content(&normalized, &parsed, path).ok()?;

    if result.base_content == result.new_content {
        return None;
    }

    let (diff, _) = edit_diff::generate_diff_string(&result.base_content, &result.new_content, 4);
    Some(diff)
}

/// 兼容旧式输入：edits 可能是字符串 JSON；或平铺 oldText/newText
pub(crate) fn normalize_edit_args(input: &Value) -> Value {
    let mut input = input.clone();

    // edits 为 JSON 字符串：数组或单对象
    if let Some(edits) = input.get("edits")
        && let Some(s) = edits.as_str()
        && let Ok(parsed) = serde_json::from_str::<Value>(s)
    {
        if parsed.is_array() {
            input["edits"] = parsed;
        } else if parsed.is_object() {
            input["edits"] = Value::Array(vec![parsed]);
        }
    }

    // edits 为单对象（非数组）：包成数组
    if let Some(edits) = input.get("edits")
        && let Some(obj) = edits.as_object()
    {
        input["edits"] = Value::Array(vec![Value::Object(obj.clone())]);
    }

    let legacy_old = input.get("oldText").and_then(|v| v.as_str());
    let legacy_new = input.get("newText").and_then(|v| v.as_str());
    if let (Some(old), Some(new)) = (legacy_old, legacy_new) {
        let mut edits = input
            .get("edits")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        edits.push(json!({ "oldText": old, "newText": new }));
        input["edits"] = Value::Array(edits);
        if let Some(obj) = input.as_object_mut() {
            obj.remove("oldText");
            obj.remove("newText");
        }
    }
    input
}
