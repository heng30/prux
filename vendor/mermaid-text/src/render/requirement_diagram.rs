//! Renderer for [`RequirementDiagram`]. Produces a Unicode text layout
//! with labeled boxes for requirements and elements, and labeled arrows
//! for relationships.
//!
//! ## Layout
//!
//! Each requirement renders as a box with a stereotype header (`<<kind>>`)
//! and a data table below a horizontal divider:
//!
//! ```text
//! ┌──────────────────────────┐
//! │    <<requirement>>       │
//! │        test_req          │
//! ├──────────────────────────┤
//! │ id:           1          │
//! │ text:         the test…  │
//! │ risk:         high       │
//! │ verifymethod: test       │
//! └──────────────────────────┘
//! ```
//!
//! Elements use rounded corners (`╭╮╰╯`) to visually distinguish them:
//!
//! ```text
//! ╭──────────────────────────╮
//! │       test_entity        │
//! ├──────────────────────────┤
//! │ type:   simulation       │
//! ╰──────────────────────────╯
//! ```
//!
//! Relationships are rendered after all boxes as lines of the form:
//!   `<source> --[kind]--> <target>`
//!
//! ## max_width
//!
//! When `max_width` is `Some(n)`, the box content is truncated with `…` so
//! that box lines fit within the budget. Relationship lines are not truncated.
//!
//! ## Phase 1 limitations
//!
//! - Layout is purely vertical (boxes stacked top-to-bottom); no attempt is
//!   made to arrange boxes side-by-side or to draw graphical connection lines.
//! - Relationship arcs are listed as a text summary below the boxes rather than
//!   as drawn lines between boxes (full graph routing is deferred to a later phase).
//! - Long text values are truncated with `…` at `max_width` but values wider
//!   than any reasonable terminal may still overflow when `max_width` is `None`.

use unicode_width::UnicodeWidthStr;

use crate::requirement_diagram::{
    Element, Requirement, RequirementDiagram, RequirementRelationship,
};

/// Minimum useful box width (inner content columns).
const MIN_INNER_WIDTH: usize = 20;

/// Column budget for the box body when no max_width is given.
const DEFAULT_INNER_WIDTH: usize = 30;

/// Render a [`RequirementDiagram`] to a Unicode string.
///
/// # Arguments
///
/// * `diag`      — the parsed diagram
/// * `max_width` — optional column budget; box content is truncated with `…`
///   to keep lines within this budget
///
/// # Returns
///
/// A multi-line string with box-drawing characters representing requirements,
/// elements, and their relationships. Returns an empty string when the diagram
/// has no requirements, elements, or relationships.
pub fn render(diag: &RequirementDiagram, max_width: Option<usize>) -> String {
    if diag.requirements.is_empty() && diag.elements.is_empty() && diag.relationships.is_empty() {
        return String::new();
    }

    // Derive the inner content width from max_width.
    // Box frame overhead: "│ " + " │" = 4 columns, corners = 2 extra cells
    // on top/bottom rows. The inner width controls only the content columns.
    let inner_w = max_width
        .map(|w| w.saturating_sub(4).max(MIN_INNER_WIDTH))
        .unwrap_or(DEFAULT_INNER_WIDTH);

    let mut out = String::new();

    for req in &diag.requirements {
        render_requirement_box(&mut out, req, inner_w);
        out.push('\n');
    }

    for elem in &diag.elements {
        render_element_box(&mut out, elem, inner_w);
        out.push('\n');
    }

    if !diag.relationships.is_empty() {
        out.push_str("Relationships:\n");
        for rel in &diag.relationships {
            render_relationship_line(&mut out, rel);
        }
    }

    // Trim trailing newline to match convention used by other renderers.
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

// ---------------------------------------------------------------------------
// Box rendering helpers
// ---------------------------------------------------------------------------

/// Render a requirement as a box with straight corners (`┌┐└┘`).
fn render_requirement_box(out: &mut String, req: &Requirement, inner_w: usize) {
    // Header rows: `<<kind>>` stereotype + name.
    let stereotype = format!("<<{}>>", req.kind.label());
    let rows: Vec<String> = vec![
        center_text(&stereotype, inner_w),
        center_text(&req.name, inner_w),
    ];

    // Data rows: key-value pairs.
    let data_rows: Vec<(String, String)> = build_req_data_rows(req);
    let key_width = data_rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);

    // Top border.
    push_straight_top(out, inner_w);

    for row in &rows {
        out.push('\u{2502}'); // │
        out.push(' ');
        let w = UnicodeWidthStr::width(row.as_str());
        out.push_str(row);
        // Pad to fill inner_w.
        for _ in w..inner_w {
            out.push(' ');
        }
        out.push(' ');
        out.push('\u{2502}'); // │
        out.push('\n');
    }

    // Divider.
    push_straight_divider(out, inner_w);

    // Data rows.
    for (key, val) in &data_rows {
        let val_display = truncate_value(val, inner_w, key_width + 1);
        let line = format!("{key:<key_width$} {val_display}");
        let line_w = UnicodeWidthStr::width(line.as_str());
        out.push('\u{2502}'); // │
        out.push(' ');
        out.push_str(&line);
        for _ in line_w..inner_w {
            out.push(' ');
        }
        out.push(' ');
        out.push('\u{2502}'); // │
        out.push('\n');
    }

    // Bottom border.
    push_straight_bottom(out, inner_w);
}

/// Render an element as a box with rounded corners (`╭╮╰╯`).
fn render_element_box(out: &mut String, elem: &Element, inner_w: usize) {
    let data_rows: Vec<(String, String)> = build_elem_data_rows(elem);
    let key_width = data_rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);

    // Top border with rounded corners.
    push_rounded_top(out, inner_w);

    // Name header (centered, no stereotype row for elements).
    let name_row = center_text(&elem.name, inner_w);
    let nw = UnicodeWidthStr::width(name_row.as_str());
    out.push('\u{2502}'); // │
    out.push(' ');
    out.push_str(&name_row);
    for _ in nw..inner_w {
        out.push(' ');
    }
    out.push(' ');
    out.push('\u{2502}'); // │
    out.push('\n');

    // Divider (straight even in rounded-corner box — standard convention).
    push_straight_divider(out, inner_w);

    // Data rows.
    for (key, val) in &data_rows {
        let val_display = truncate_value(val, inner_w, key_width + 1);
        let line = format!("{key:<key_width$} {val_display}");
        let line_w = UnicodeWidthStr::width(line.as_str());
        out.push('\u{2502}'); // │
        out.push(' ');
        out.push_str(&line);
        for _ in line_w..inner_w {
            out.push(' ');
        }
        out.push(' ');
        out.push('\u{2502}'); // │
        out.push('\n');
    }

    // Bottom border with rounded corners.
    push_rounded_bottom(out, inner_w);
}

/// Render a relationship as a one-line text summary.
fn render_relationship_line(out: &mut String, rel: &RequirementRelationship) {
    out.push_str(&format!(
        "  {} --[{}]--> {}\n",
        rel.source,
        rel.kind.label(),
        rel.target
    ));
}

// ---------------------------------------------------------------------------
// Data-row builders
// ---------------------------------------------------------------------------

fn build_req_data_rows(req: &Requirement) -> Vec<(String, String)> {
    let mut rows = vec![
        ("id:".to_string(), req.id.clone()),
        ("text:".to_string(), req.text.clone()),
    ];
    if let Some(risk) = req.risk {
        rows.push(("risk:".to_string(), risk_label(risk).to_string()));
    }
    if let Some(vm) = req.verify_method {
        rows.push((
            "verifymethod:".to_string(),
            verify_method_label(vm).to_string(),
        ));
    }
    rows
}

fn build_elem_data_rows(elem: &Element) -> Vec<(String, String)> {
    let mut rows = vec![("type:".to_string(), elem.kind.clone())];
    if let Some(dr) = &elem.docref {
        rows.push(("docref:".to_string(), dr.clone()));
    }
    rows
}

fn risk_label(r: crate::requirement_diagram::Risk) -> &'static str {
    use crate::requirement_diagram::Risk;
    match r {
        Risk::Low => "low",
        Risk::Medium => "medium",
        Risk::High => "high",
    }
}

fn verify_method_label(v: crate::requirement_diagram::VerifyMethod) -> &'static str {
    use crate::requirement_diagram::VerifyMethod;
    match v {
        VerifyMethod::Analysis => "analysis",
        VerifyMethod::Inspection => "inspection",
        VerifyMethod::Test => "test",
        VerifyMethod::Demonstration => "demonstration",
    }
}

// ---------------------------------------------------------------------------
// Border helpers
// ---------------------------------------------------------------------------

fn push_straight_top(out: &mut String, inner_w: usize) {
    out.push('\u{250C}'); // ┌
    for _ in 0..inner_w + 2 {
        out.push('\u{2500}'); // ─
    }
    out.push('\u{2510}'); // ┐
    out.push('\n');
}

fn push_straight_divider(out: &mut String, inner_w: usize) {
    out.push('\u{251C}'); // ├
    for _ in 0..inner_w + 2 {
        out.push('\u{2500}'); // ─
    }
    out.push('\u{2524}'); // ┤
    out.push('\n');
}

fn push_straight_bottom(out: &mut String, inner_w: usize) {
    out.push('\u{2514}'); // └
    for _ in 0..inner_w + 2 {
        out.push('\u{2500}'); // ─
    }
    out.push('\u{2518}'); // ┘
    out.push('\n');
}

fn push_rounded_top(out: &mut String, inner_w: usize) {
    out.push('\u{256D}'); // ╭
    for _ in 0..inner_w + 2 {
        out.push('\u{2500}'); // ─
    }
    out.push('\u{256E}'); // ╮
    out.push('\n');
}

fn push_rounded_bottom(out: &mut String, inner_w: usize) {
    out.push('\u{2570}'); // ╰
    for _ in 0..inner_w + 2 {
        out.push('\u{2500}'); // ─
    }
    out.push('\u{256F}'); // ╯
    out.push('\n');
}

// ---------------------------------------------------------------------------
// Text utilities
// ---------------------------------------------------------------------------

/// Centre `text` within a field of `width` columns, padding with spaces.
/// If `text` is wider than `width`, it is truncated with `…`.
fn center_text(text: &str, width: usize) -> String {
    let tw = UnicodeWidthStr::width(text);
    if tw >= width {
        return truncate_str(text, width.saturating_sub(1));
    }
    let pad_total = width - tw;
    let pad_left = pad_total / 2;
    let pad_right = pad_total - pad_left;
    format!(
        "{}{}{text}{}{}",
        " ".repeat(pad_left),
        "",
        "",
        " ".repeat(pad_right)
    )
}

/// Truncate `val` so that `key_width + 1 + display_width(val)` fits within
/// `inner_w`. Appends `…` when truncation is applied.
fn truncate_value(val: &str, inner_w: usize, key_col_w: usize) -> String {
    let available = inner_w.saturating_sub(key_col_w);
    let vw = UnicodeWidthStr::width(val);
    if vw <= available {
        return val.to_string();
    }
    truncate_str(val, available.saturating_sub(1))
}

/// Truncate `s` to `max_cols` display columns and append `…`.
fn truncate_str(s: &str, max_cols: usize) -> String {
    let mut result = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
        if used + w > max_cols {
            break;
        }
        result.push(ch);
        used += w;
    }
    result.push('\u{2026}'); // …
    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::requirement_diagram::parse;
    use crate::requirement_diagram::{
        Element, Requirement, RequirementDiagram, RequirementKind, Risk, VerifyMethod,
    };

    fn single_req_diagram() -> RequirementDiagram {
        RequirementDiagram {
            requirements: vec![Requirement {
                kind: RequirementKind::Functional,
                name: "test_req".to_string(),
                id: "1".to_string(),
                text: "do the thing".to_string(),
                risk: Some(Risk::High),
                verify_method: Some(VerifyMethod::Test),
            }],
            elements: vec![],
            relationships: vec![],
        }
    }

    // 1. Requirement box has stereotype header + name
    #[test]
    fn requirement_box_has_type_tag_and_name_in_header() {
        let diag = single_req_diagram();
        let out = render(&diag, None);

        assert!(
            out.contains("<<functionalRequirement>>"),
            "stereotype tag missing:\n{out}"
        );
        assert!(out.contains("test_req"), "requirement name missing:\n{out}");
        // Straight corners must be used for requirements.
        assert!(
            out.contains('\u{250C}'),
            "top-left straight corner (┌) missing:\n{out}"
        );
        assert!(
            out.contains('\u{2510}'),
            "top-right straight corner (┐) missing:\n{out}"
        );
    }

    // 2. Element box uses rounded border style
    #[test]
    fn element_box_uses_rounded_border_style() {
        let diag = RequirementDiagram {
            requirements: vec![],
            elements: vec![Element {
                name: "my_entity".to_string(),
                kind: "simulation".to_string(),
                docref: None,
            }],
            relationships: vec![],
        };
        let out = render(&diag, None);

        assert!(out.contains("my_entity"), "element name missing:\n{out}");
        // Rounded corners must be used for elements.
        assert!(
            out.contains('\u{256D}'),
            "rounded top-left corner (╭) missing:\n{out}"
        );
        assert!(
            out.contains('\u{256F}'),
            "rounded bottom-right corner (╯) missing:\n{out}"
        );
        // Straight corners must NOT appear in element boxes.
        assert!(
            !out.contains('\u{250C}'),
            "straight corner (┌) must not appear in element box:\n{out}"
        );
    }

    // 3. Relationships render as labeled arrows
    #[test]
    fn relationships_render_as_labeled_arrows() {
        let src = "requirementDiagram
    requirement r1 {
        id: 1
        text: some text.
    }
    element e1 {
        type: simulation
    }
    e1 - satisfies -> r1
    r1 - traces -> r1";

        let diag = parse(src).unwrap();
        let out = render(&diag, None);

        assert!(
            out.contains("satisfies"),
            "relationship kind missing:\n{out}"
        );
        assert!(out.contains("e1"), "source missing:\n{out}");
        assert!(out.contains("r1"), "target missing:\n{out}");
        assert!(
            out.contains("traces"),
            "second relationship missing:\n{out}"
        );
        assert!(
            out.contains("Relationships:"),
            "relationships section header missing:\n{out}"
        );
    }

    // 4. Empty diagram renders gracefully
    #[test]
    fn empty_diagram_renders_gracefully() {
        let diag = RequirementDiagram::default();
        let out = render(&diag, None);
        assert!(out.is_empty(), "empty diagram should produce empty output");
    }

    // 5. Data fields appear in requirement boxes
    #[test]
    fn requirement_box_shows_data_fields() {
        let diag = single_req_diagram();
        let out = render(&diag, None);

        assert!(out.contains("id:"), "id field missing:\n{out}");
        assert!(out.contains("text:"), "text field missing:\n{out}");
        assert!(out.contains("risk:"), "risk field missing:\n{out}");
        assert!(
            out.contains("verifymethod:"),
            "verifymethod field missing:\n{out}"
        );
        assert!(out.contains("high"), "risk value missing:\n{out}");
        assert!(out.contains("test"), "verifymethod value missing:\n{out}");
    }
}
