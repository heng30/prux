//! 页面快照：注入页面执行的 JS、结果解析与面向模型的排版。
//!
//! 快照给「可交互元素」分配 `e1` / `e2` … 引用，引用表存在页面侧的 `window.__pruxRefs` 里；
//! 之后的 `click` / `type` / `select` / `upload` 用同一个引用取回元素。
//!
//! 引用只对**当次页面上下文**有效：导航或刷新后引用表就没了，旧引用会明确报错
//! （不会静默点到别的元素）——那时重新 `snapshot` 即可。
//!
//! 所有 JS 表达式都返回**对象**（`Runtime.evaluate` 的 `returnByValue` 直接序列化），
//! 失败时返回 `{ error: "..." }`，由 [`error_of`] 统一转成 `ToolError`。

use crate::extensions::util::truncate_chars_with_postfix;
use serde_json::Value;

/// 注入页面收集可交互元素的 JS；`__MAX_ELEMENTS__` 由 [`snapshot_expression`] 替换。
const SNAPSHOT_JS: &str = r#"(() => {
  const MAX = __MAX_ELEMENTS__;
  const SELECTOR = 'a[href],button,input,select,textarea,summary,[role=button],[role=link],[role=checkbox],[role=radio],[role=switch],[role=tab],[role=menuitem],[role=option],[contenteditable=true],[onclick]';
  const label = (el) => {
    const raw = el.getAttribute('aria-label') || el.getAttribute('placeholder') || el.getAttribute('alt') || el.getAttribute('title') || el.innerText || el.value || '';
    const text = String(raw).replace(/\s+/g, ' ').trim();
    return text.length > 80 ? text.slice(0, 77) + '...' : text;
  };
  const visible = (el) => {
    const rect = el.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) return false;
    const style = getComputedStyle(el);
    return style.visibility !== 'hidden' && style.display !== 'none' && style.opacity !== '0';
  };
  const interactable = Array.from(document.querySelectorAll(SELECTOR)).filter(visible);
  const refs = {};
  const lines = interactable.slice(0, MAX).map((el, index) => {
    const ref = 'e' + (index + 1);
    refs[ref] = el;
    const name = el.tagName.toLowerCase();
    const type = el.getAttribute('type');
    const kind = type ? '<' + name + '[' + type + ']>' : '<' + name + '>';
    const value = (typeof el.value === 'string' && el.value !== '') ? ' value=' + JSON.stringify(el.value.slice(0, 60)) : '';
    const href = el.getAttribute('href') ? ' href=' + JSON.stringify(el.getAttribute('href').slice(0, 100)) : '';
    const disabled = el.disabled ? ' disabled' : '';
    return ref + ' ' + kind + ' ' + JSON.stringify(label(el)) + value + href + disabled;
  });
  window.__pruxRefs = refs;
  const body = document.body;
  const text = (body ? (body.innerText || body.textContent || '') : '').replace(/\n{3,}/g, '\n\n').trim();
  return { title: document.title, url: location.href, refs: lines, total: interactable.length, text: text };
})()"#;

/// 取页面可见文本的 JS（`document.body.innerText`，回退 `textContent`）。
pub(crate) const TEXT_JS: &str = "(() => { const body = document.body; return body ? (body.innerText || body.textContent || '') : ''; })()";

/// 取整页 HTML 的 JS（`document.documentElement.outerHTML`）。
pub(crate) const HTML_JS: &str =
    "(() => document.documentElement ? document.documentElement.outerHTML : '')()";

/// 取 `document.readyState` 的 JS（`loading` / `interactive` / `complete`）。
pub(crate) const READY_STATE_JS: &str = "(() => document.readyState)()";

/// 清空当前焦点元素取值的 JS（没给 `ref` / `selector` 的 `type` 用）。
pub(crate) const CLEAR_FOCUSED_JS: &str = r#"(() => {
  const el = document.activeElement;
  if (!el || !('value' in el)) return { error: 'no focused input to clear; pass ref or selector' };
  const descriptor = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(el), 'value');
  if (descriptor && descriptor.set) descriptor.set.call(el, ''); else el.value = '';
  el.dispatchEvent(new Event('input', { bubbles: true }));
  el.dispatchEvent(new Event('change', { bubbles: true }));
  return { ok: true };
})()"#;

/// 快照文本里的「元素被条数上限截断」标注。
const TRUNCATED_SUFFIX: &str = "\n… [truncated]";

/// 一次快照的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SnapshotData {
    /// 页面标题（`document.title`）。
    pub title: String,
    /// 页面 URL（`location.href`）。
    pub url: String,
    /// 可交互元素行（形如 `e1 <button> "Submit"`），已按上限截断。
    pub refs: Vec<String>,
    /// 页面上实际匹配到的可交互元素总数（可能大于 [`Self::refs`] 的长度）。
    pub total: usize,
    /// 页面可见文本。
    pub text: String,
}

impl SnapshotData {
    /// 元素列表是否因条数上限被截断。
    pub fn refs_truncated(&self) -> bool {
        self.total > self.refs.len()
    }
}

/// 生成收集快照的 JS 表达式（`max_elements` 为可交互元素条数上限）。
pub(crate) fn snapshot_expression(max_elements: usize) -> String {
    SNAPSHOT_JS.replace("__MAX_ELEMENTS__", &max_elements.to_string())
}

/// 解析 `snapshot_expression` 的返回值；形状不对时报错。
pub(crate) fn parse(value: &Value) -> Result<SnapshotData, String> {
    let Some(object) = value.as_object() else {
        return Err("browser: snapshot returned no page data".to_string());
    };

    let refs: Vec<String> = object
        .get("refs")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| item.as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default();

    Ok(SnapshotData {
        title: string_field(object.get("title")),
        url: string_field(object.get("url")),
        total: object
            .get("total")
            .and_then(Value::as_u64)
            .unwrap_or(refs.len() as u64) as usize,
        refs,
        text: string_field(object.get("text")),
    })
}

/// 把快照排版成给模型的文本，并按 `max_chars` 截断。
pub(crate) fn format(data: &SnapshotData, max_chars: usize) -> String {
    let mut out = String::new();
    out.push_str(&format!("Page: {}\n", data.title));
    out.push_str(&format!("URL: {}\n", data.url));

    let header = if data.refs_truncated() {
        format!(
            "Interactables ({} shown of {}):",
            data.refs.len(),
            data.total
        )
    } else {
        format!("Interactables ({}):", data.refs.len())
    };
    out.push_str(&header);
    out.push('\n');
    for line in &data.refs {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    if data.refs.is_empty() {
        out.push_str("  (none)\n");
    }

    out.push_str("Visible text:\n");
    out.push_str(&data.text);

    truncate_chars_with_postfix(&out, max_chars, TRUNCATED_SUFFIX)
}

/// 取元素引用的 JS 表达式：优先 `ref`（快照分配的 `e1` 引用），否则用 CSS `selector`。
///
/// `ref` 必须形如 `e<数字>`（引用表就是按这个键建的），非法值直接报错，
/// 也就不会把模型给的任意字符串拼进页面里执行的 JS。
pub(crate) fn element_expression(
    ref_id: Option<&str>,
    selector: Option<&str>,
) -> Result<String, String> {
    if let Some(ref_id) = ref_id {
        let digits = ref_id.trim().strip_prefix('e').unwrap_or("");
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!(
                "browser: `ref` must look like `e1` (a reference from snapshot), got `{ref_id}`"
            ));
        }
        return Ok(format!(
            "(window.__pruxRefs && window.__pruxRefs[{}]) || null",
            json_literal(ref_id.trim())
        ));
    }

    if let Some(selector) = selector {
        return Ok(format!(
            "document.querySelector({})",
            json_literal(selector)
        ));
    }

    Err("browser: pass `ref` (from snapshot) or `selector`".to_string())
}

/// 返回元素视口矩形（CSS 像素，`x` / `y` 为左上角）的 JS。
///
/// 先 `scrollIntoView` 滚到视口中央，再读 `getBoundingClientRect`：点击用它的中心点，
/// 截图用它的尺寸。
pub(crate) fn element_rect_expression(element: &str) -> String {
    format!(
        r#"(() => {{
  const el = {element};
  if (!el) return {{ error: 'element not found; references go stale after a navigation - call snapshot again' }};
  if (el.scrollIntoView) el.scrollIntoView({{ block: 'center', inline: 'center' }});
  const rect = el.getBoundingClientRect();
  return {{ x: rect.left, y: rect.top, width: rect.width, height: rect.height }};
}})()"#
    )
}

/// 聚焦元素（可选先清空）的 JS。
pub(crate) fn focus_expression(element: &str, clear: bool) -> String {
    format!(
        r#"(() => {{
  const el = {element};
  if (!el) return {{ error: 'element not found; references go stale after a navigation - call snapshot again' }};
  if (el.focus) el.focus();
  if ({clear} && 'value' in el) {{
    const descriptor = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(el), 'value');
    if (descriptor && descriptor.set) descriptor.set.call(el, ''); else el.value = '';
    el.dispatchEvent(new Event('input', {{ bubbles: true }}));
    el.dispatchEvent(new Event('change', {{ bubbles: true }}));
  }}
  return {{ ok: true }};
}})()"#
    )
}

/// 设置 `<select>` 取值的 JS：取值不存在时报错（而不是静默什么都不做）。
pub(crate) fn select_expression(element: &str, value: &str) -> String {
    let literal = json_literal(value);
    format!(
        r#"(() => {{
  const el = {element};
  if (!el) return {{ error: 'element not found; references go stale after a navigation - call snapshot again' }};
  if (el.tagName !== 'SELECT') return {{ error: 'element is <' + el.tagName.toLowerCase() + '>, not a <select>' }};
  el.value = {literal};
  if (el.value !== {literal}) return {{ error: 'no <option> with value ' + {literal} }};
  el.dispatchEvent(new Event('input', {{ bubbles: true }}));
  el.dispatchEvent(new Event('change', {{ bubbles: true }}));
  return {{ ok: true, value: el.value }};
}})()"#
    )
}

/// 滚动的 JS：给了元素就 `scrollIntoView`，给了 `to` 就滚到顶/底，否则按增量滚。
pub(crate) fn scroll_expression(
    element: Option<&str>,
    to: Option<&str>,
    dx: f64,
    dy: f64,
) -> String {
    if let Some(element) = element {
        return format!(
            "(() => {{ const el = {element}; if (!el) return {{ error: 'element not found; references go stale after a navigation - call snapshot again' }}; el.scrollIntoView({{ block: 'center', inline: 'center' }}); return {{ ok: true, y: window.scrollY }}; }})()"
        );
    }
    if let Some(to) = to {
        let y = if to == "bottom" {
            "Math.max(document.body ? document.body.scrollHeight : 0, document.documentElement ? document.documentElement.scrollHeight : 0)".to_string()
        } else {
            "0".to_string()
        };
        return format!(
            "(() => {{ window.scrollTo(0, {y}); return {{ ok: true, y: window.scrollY }}; }})()"
        );
    }
    format!(
        "(() => {{ window.scrollBy({dx}, {dy}); return {{ ok: true, x: window.scrollX, y: window.scrollY }}; }})()"
    )
}

/// 从 JS 返回的对象里取 `error` 字段；有错就是失败。
pub(crate) fn error_of(value: &Value) -> Option<String> {
    value
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// JSON 值的 JSON 字面量（安全嵌入 JS 源码；`to_string` 对字符串总会成功）。
pub(crate) fn json_literal(value: &str) -> String {
    Value::String(value.to_string()).to_string()
}

/// 取对象里的字符串字段（缺键 / 非字符串回落空串）。
fn string_field(value: Option<&Value>) -> String {
    value.and_then(Value::as_str).unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 快照排版包含标题、URL、元素行与正文；元素被截断时给出总数。
    #[test]
    fn format_includes_structure_and_marks_truncation() {
        let data = SnapshotData {
            title: "Example".to_string(),
            url: "https://example.test/".to_string(),
            refs: vec!["e1 <button> \"Submit\"".to_string()],
            total: 3,
            text: "hello".to_string(),
        };
        let text = format(&data, 10_000);
        assert!(text.contains("Page: Example"));
        assert!(text.contains("URL: https://example.test/"));
        assert!(text.contains("Interactables (1 shown of 3):"));
        assert!(text.contains("  e1 <button> \"Submit\""));
        assert!(text.ends_with("Visible text:\nhello"));
    }

    /// 没有可交互元素时明确写 `(none)`，而不是留一片空白。
    #[test]
    fn format_marks_an_empty_element_list() {
        let data = SnapshotData {
            title: "t".to_string(),
            url: "u".to_string(),
            refs: Vec::new(),
            total: 0,
            text: String::new(),
        };
        let text = format(&data, 10_000);
        assert!(text.contains("Interactables (0):\n  (none)"));
        assert!(!data.refs_truncated());
    }

    /// 超长快照按字符截断并标注（不切坏 UTF-8）。
    #[test]
    fn format_truncates_by_characters() {
        let data = SnapshotData {
            title: "标题".to_string(),
            url: "u".to_string(),
            refs: Vec::new(),
            total: 0,
            text: "正文".repeat(100),
        };
        let text = format(&data, 20);
        assert!(text.contains("[truncated]"));
        assert!(text.chars().count() <= 20 + TRUNCATED_SUFFIX.chars().count());
    }

    /// 解析 JS 返回的对象；形状不对时报错。
    #[test]
    fn parse_reads_snapshot_fields() {
        let value =
            json!({ "title": "t", "url": "u", "refs": ["e1 <a>"], "total": 5, "text": "body" });
        let data = parse(&value).unwrap();
        assert_eq!(data.title, "t");
        assert_eq!(data.refs, vec!["e1 <a>".to_string()]);
        assert_eq!(data.total, 5);
        assert!(data.refs_truncated());

        assert!(parse(&json!("nope")).is_err());
    }

    /// `total` 缺失时退化成元素条数（老页面 / 部分实现）。
    #[test]
    fn parse_defaults_total_to_element_count() {
        let value = json!({ "refs": ["e1 <a>", "e2 <a>"] });
        let data = parse(&value).unwrap();
        assert_eq!(data.total, 2);
        assert!(!data.refs_truncated());
    }

    /// 引用必须是 `e<数字>`：非法值直接报错，不会被拼进页面 JS。
    #[test]
    fn element_expression_rejects_bad_refs() {
        let error = element_expression(Some("e1); alert(1"), None).unwrap_err();
        assert!(error.contains("must look like `e1`"), "{error}");
        assert_eq!(
            element_expression(Some("e12"), None).unwrap(),
            "(window.__pruxRefs && window.__pruxRefs[\"e12\"]) || null"
        );
    }

    /// 没有引用时用选择器，两者都缺时报错。
    #[test]
    fn element_expression_prefers_ref_then_selector() {
        assert_eq!(
            element_expression(None, Some("#submit")).unwrap(),
            "document.querySelector(\"#submit\")"
        );
        assert!(element_expression(None, None).is_err());
    }

    /// 选择器里的引号被 JSON 转义，不会破坏 JS 字符串。
    #[test]
    fn element_expression_escapes_selectors() {
        let expression = element_expression(None, Some("a[href=\"x\"]")).unwrap();
        assert_eq!(expression, "document.querySelector(\"a[href=\\\"x\\\"]\")");
    }

    /// `error_of` 只认对象里的 `error` 字符串。
    #[test]
    fn error_of_reads_only_the_error_field() {
        assert_eq!(error_of(&json!({ "error": "boom" })), Some("boom".into()));
        assert_eq!(error_of(&json!({ "ok": true })), None);
        assert_eq!(error_of(&json!(null)), None);
    }

    /// 元素上限替换进 JS；其余部分不被动过。
    #[test]
    fn snapshot_expression_substitutes_the_element_limit() {
        let expression = snapshot_expression(42);
        assert!(expression.contains("const MAX = 42;"));
        assert!(!expression.contains("__MAX_ELEMENTS__"));
    }
}
