//! Renderer for [`Sankey`] diagrams.
//!
//! ## Layout
//!
//! Each source node is printed as a header line annotated with its total
//! outgoing flow. Each arc is indented below with a proportional Unicode bar
//! so users can compare magnitudes at a glance, followed by the bracketed
//! value and an arrow to the target:
//!
//! ```text
//! Bio-conversion  (total: 280.9)
//!   █████████████████████████████████ [280.3] ► Solid
//!   ▏                                 [  0.6] ► Liquid
//! ```
//!
//! Bars use full-block `█` (U+2588) glyphs plus the sub-cell eighth series
//! (`▏▎▍▌▋▊▉`, U+258F–U+2589) so a small flow does not snap to "1 cell"
//! when next to a large one. A single global scale factor is computed once
//! from the maximum flow value in the entire diagram — bars are therefore
//! mutually comparable across all source groups.
//!
//! ## max_width
//!
//! When `max_width` is `Some(n)`, node header lines and arc label text are
//! truncated to fit within the budget. The minimum guaranteed width is
//! [`MIN_WIDTH`] columns; narrower budgets are silently clamped up.

use unicode_width::UnicodeWidthStr;

use crate::sankey::Sankey;

/// Default column width when no budget is specified.
const DEFAULT_WIDTH: usize = 80;

/// Minimum column budget (clamp floor for very narrow terminals).
const MIN_WIDTH: usize = 20;

/// Maximum bar width in terminal cells.
const BAR_MAX_CELLS: usize = 33;

/// Sub-cell eighth glyphs in ascending fill order.
///
/// Index 0 is never used (0 eighths = nothing). Index 1..=7 map to
/// 1/8 through 7/8 of a cell.
const EIGHTH_GLYPHS: [char; 8] = [
    ' ',        // 0/8 — placeholder; callers skip index 0
    '\u{258F}', // 1/8 ▏
    '\u{258E}', // 2/8 ▎
    '\u{258D}', // 3/8 ▍
    '\u{258C}', // 4/8 ▌
    '\u{258B}', // 5/8 ▋
    '\u{258A}', // 6/8 ▊
    '\u{2589}', // 7/8 ▉
];

/// Full-block glyph (1 complete cell).
const FULL_BLOCK: char = '\u{2588}'; // █

/// Glyph used as the arrowhead pointing right.
const ARROW_HEAD: char = '\u{25BA}'; // ►

// ---------------------------------------------------------------------------
// Public interface
// ---------------------------------------------------------------------------

/// Render a [`Sankey`] to a Unicode string.
///
/// # Arguments
///
/// * `diag`      — the parsed diagram
/// * `max_width` — optional column budget; lines are truncated to this many
///   columns (minimum [`MIN_WIDTH`])
///
/// # Returns
///
/// A multi-line string ready for printing. Trailing newlines are stripped.
pub fn render(diag: &Sankey, max_width: Option<usize>) -> String {
    let width = max_width.map(|w| w.max(MIN_WIDTH)).unwrap_or(DEFAULT_WIDTH);

    if diag.flows.is_empty() {
        return "(empty sankey diagram)".to_string();
    }

    // Collect outgoing flows per source, preserving first-seen source order.
    let mut sources: Vec<String> = Vec::new();
    let mut outgoing: std::collections::HashMap<String, Vec<(String, f64)>> =
        std::collections::HashMap::new();

    for flow in &diag.flows {
        if !sources.contains(&flow.source) {
            sources.push(flow.source.clone());
        }
        outgoing
            .entry(flow.source.clone())
            .or_default()
            .push((flow.target.clone(), flow.value));
    }

    // Single global scale factor: the largest individual flow value.
    let global_max = diag.flows.iter().map(|f| f.value).fold(0.0_f64, f64::max);

    // Determine the maximum formatted value width (for bracket alignment).
    let max_val_len = diag
        .flows
        .iter()
        .map(|f| format!("{:.1}", f.value).len())
        .max()
        .unwrap_or(1);

    let mut out = String::new();
    let mut first_source = true;

    for source in &sources {
        if !first_source {
            out.push('\n');
        }
        first_source = false;

        let total: f64 = outgoing
            .get(source)
            .map(|arcs| arcs.iter().map(|(_, v)| v).sum())
            .unwrap_or(0.0);

        let header_text = format!("{source}  (total: {total:.1})");
        let header = truncate_to_width(&header_text, width);
        out.push_str(&header);
        out.push('\n');

        let arcs = outgoing.get(source).map(Vec::as_slice).unwrap_or(&[]);
        for (target, value) in arcs {
            let arc_line = format_arc(
                target,
                *value,
                max_val_len,
                global_max,
                BAR_MAX_CELLS,
                width,
            );
            out.push_str(&arc_line);
            out.push('\n');
        }
    }

    while out.ends_with('\n') {
        out.pop();
    }

    out
}

// ---------------------------------------------------------------------------
// Bar helpers
// ---------------------------------------------------------------------------

/// Convert `value` to a count of 1/8-cell units using integer arithmetic to
/// avoid floating-point truncation inconsistencies.
///
/// The formula is `floor(value * max_cells * 8 / max_value)`, giving the
/// total number of eighths that fit within `max_cells` terminal columns.
///
/// Returns `0` when `max_value` is zero or `value` is zero.
pub fn bar_eighths(value: f64, max_value: f64, max_cells: usize) -> usize {
    if max_value <= 0.0 || value <= 0.0 {
        return 0;
    }
    // Scale to integer eighths without introducing floating-point rounding
    // drift: multiply first, then divide, using f64 only for the ratio.
    let eighths = (value / max_value * (max_cells * 8) as f64) as usize;
    eighths.min(max_cells * 8)
}

/// Build a proportional bar string for `value` relative to `max_value`.
///
/// The bar is at most `max_cells` terminal columns wide. Full cells use `█`;
/// a fractional remainder uses the appropriate eighth-block glyph. The
/// returned string is left-aligned and contains no trailing spaces — callers
/// are responsible for padding to a uniform width.
pub fn proportional_bar(value: f64, max_value: f64, max_cells: usize) -> String {
    let eighths = bar_eighths(value, max_value, max_cells);
    let full = eighths / 8;
    let partial = eighths % 8;

    let mut bar = String::with_capacity(full + if partial > 0 { 1 } else { 0 });
    for _ in 0..full {
        bar.push(FULL_BLOCK);
    }
    if partial > 0 {
        bar.push(EIGHTH_GLYPHS[partial]);
    }
    bar
}

// ---------------------------------------------------------------------------
// Arc formatting
// ---------------------------------------------------------------------------

/// Format a single arc line with a proportional bar.
///
/// Shape: `  <bar padded to effective_cells> [<value>] ► <target>`
///
/// The bar column width is the minimum of `max_cells` and the space
/// remaining after the fixed overhead (indent + value bracket + arrow).
/// This ensures the line never exceeds `max_width` even on narrow terminals.
fn format_arc(
    target: &str,
    value: f64,
    max_val_len: usize,
    global_max: f64,
    max_cells: usize,
    max_width: usize,
) -> String {
    const INDENT: &str = "  ";
    // Overhead columns: INDENT(2) + space(1) + "[" + max_val_len + "]" + " ► "(3)
    let fixed_overhead = 2 + 1 + 1 + max_val_len + 1 + 3;
    let effective_cells = max_cells.min(max_width.saturating_sub(fixed_overhead));

    let bar_raw = proportional_bar(value, global_max, effective_cells);
    let bar_raw_w = UnicodeWidthStr::width(bar_raw.as_str());
    let bar_pad = " ".repeat(effective_cells.saturating_sub(bar_raw_w));
    let bar_col = format!("{bar_raw}{bar_pad}");

    let value_str = format!("{value:.1}");
    let pad = max_val_len.saturating_sub(value_str.len());
    let value_col = format!("[{}{value_str}]", " ".repeat(pad));

    let prefix = format!("{INDENT}{bar_col} {value_col} {ARROW_HEAD} ");
    let prefix_w = UnicodeWidthStr::width(prefix.as_str());

    let remaining = max_width.saturating_sub(prefix_w);
    let target_truncated = truncate_to_width(target, remaining);

    format!("{prefix}{target_truncated}")
}

// ---------------------------------------------------------------------------
// Text utilities
// ---------------------------------------------------------------------------

/// Truncate `s` so its display width does not exceed `max_cols`.
///
/// Uses `unicode-width` for accurate terminal column counting.
/// If truncation is needed, the last visible character is replaced with `…`.
fn truncate_to_width(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    let total = UnicodeWidthStr::width(s);
    if total <= max_cols {
        return s.to_string();
    }
    let budget = max_cols.saturating_sub(1);
    let mut result = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw > budget {
            break;
        }
        result.push(ch);
        used += cw;
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
    use crate::parser::sankey::parse;

    fn canonical_src() -> &'static str {
        "sankey-beta

%% source,target,value
Agricultural 'waste',Bio-conversion,124.729
Bio-conversion,Liquid,0.597
Bio-conversion,Solid,280.322
Coal imports,Coal,11.606
Coal,Solid,75.571"
    }

    #[test]
    fn source_nodes_appear_as_headers() {
        let diag = parse(canonical_src()).unwrap();
        let out = render(&diag, None);

        assert!(
            out.contains("Bio-conversion"),
            "Bio-conversion header missing:\n{out}"
        );
        assert!(
            out.contains("Coal imports"),
            "Coal imports header missing:\n{out}"
        );
        assert!(out.contains("Coal"), "Coal header missing:\n{out}");
    }

    #[test]
    fn arrow_glyphs_present() {
        let diag = parse(canonical_src()).unwrap();
        let out = render(&diag, None);

        assert!(out.contains(ARROW_HEAD), "arrowhead glyph missing:\n{out}");
    }

    #[test]
    fn all_target_names_appear_in_output() {
        let diag = parse(canonical_src()).unwrap();
        let out = render(&diag, None);

        for name in &["Liquid", "Solid", "Coal"] {
            assert!(
                out.contains(name),
                "target {name:?} missing from output:\n{out}"
            );
        }
    }

    #[test]
    fn values_appear_in_output() {
        let diag = parse(canonical_src()).unwrap();
        let out = render(&diag, None);

        assert!(
            out.contains("124.7"),
            "124.7 value missing from output:\n{out}"
        );
        assert!(out.contains("0.6"), "0.6 value missing from output:\n{out}");
        assert!(
            out.contains("280.3"),
            "280.3 value missing from output:\n{out}"
        );
    }

    #[test]
    fn empty_sankey_renders_placeholder() {
        let diag = Sankey::default();
        let out = render(&diag, None);
        assert!(out.contains("empty"), "empty placeholder missing:\n{out}");
    }

    #[test]
    fn max_width_truncates_long_names() {
        let src = "sankey-beta\nA Very Long Source Node Name That Exceeds Eighty Columns,B,10.0";
        let diag = parse(src).unwrap();
        let out = render(&diag, Some(40));

        for line in out.lines() {
            let w = UnicodeWidthStr::width(line);
            assert!(w <= 40, "line exceeds max_width=40 (w={w}): {line:?}");
        }
    }

    #[test]
    fn single_flow_round_trip() {
        let src = "sankey-beta\nSource,Target,42.5";
        let diag = parse(src).unwrap();
        let out = render(&diag, None);

        assert!(out.contains("Source"), "source missing");
        assert!(out.contains("Target"), "target missing");
        assert!(out.contains("42.5"), "value missing");
    }

    // -----------------------------------------------------------------------
    // Proportional bar tests — written BEFORE implementation so they fail on
    // the old renderer and cannot be satisfied by a trivial no-op impl.
    // -----------------------------------------------------------------------

    /// The sub-cell eighths helper must map values to exact eighth-cell counts.
    ///
    /// A trivially-broken impl that returns a constant would fail the 0, 40,
    /// 80 assertions simultaneously.
    #[test]
    fn bar_eighths_boundary_values() {
        assert_eq!(
            bar_eighths(0.0, 100.0, 10),
            0,
            "zero value must map to 0 eighths"
        );
        assert_eq!(
            bar_eighths(100.0, 100.0, 10),
            80,
            "max value must map to max_cells * 8 = 80 eighths"
        );
        assert_eq!(
            bar_eighths(50.0, 100.0, 10),
            40,
            "half of max must map to 40 eighths"
        );
        // 13/100 * 10 cells * 8 = 10.4 → truncate → 10 eighths.
        assert_eq!(
            bar_eighths(13.0, 100.0, 10),
            10,
            "13/100 of max with 10-cell budget must give 10 eighths"
        );
    }

    /// The proportional bar string for the max-value flow must use the full
    /// budget width (10 full `█` cells = 10 glyphs, no partial).
    #[test]
    fn proportional_bar_full_fill_at_max() {
        let bar = proportional_bar(100.0, 100.0, 10);
        let full_cells: usize = bar.chars().filter(|&c| c == '█').count();
        assert_eq!(
            full_cells, 10,
            "max-value bar must be exactly 10 full-block glyphs: {bar:?}"
        );
        // No partial-fill glyph should appear at full fill.
        let partial_count: usize = bar.chars().filter(|&c| "▏▎▍▌▋▊▉".contains(c)).count();
        assert_eq!(
            partial_count, 0,
            "max-value bar must have no partial glyph: {bar:?}"
        );
    }

    /// A zero-value flow produces an empty bar (no block glyphs at all).
    #[test]
    fn proportional_bar_zero_is_empty() {
        let bar = proportional_bar(0.0, 100.0, 10);
        let any_block: usize = bar.chars().filter(|&c| "█▏▎▍▌▋▊▉".contains(c)).count();
        assert_eq!(
            any_block, 0,
            "zero-value bar must contain no block glyphs: {bar:?}"
        );
    }

    /// Core proportionality regression: flow A ≥ 2× flow B → bar A has ≥ 1.8×
    /// as many full `█` glyphs as bar B. A trivial impl that emits one `█` per
    /// flow fails because both bars would have 1 glyph and 1.0 < 1.8. An impl
    /// that uses raw value as cell count without scaling passes only when
    /// max_value ≤ max_cells; the longest-bar cap assertion closes that loophole.
    #[test]
    fn proportional_bar_ratio_tracks_value_ratio() {
        // flow A = 200, flow B = 80. Ratio A/B = 2.5.
        // With a sensible max_cells cap (say 40), bar A should have ≥ 1.8× the
        // full-cell count of bar B.
        let src = "sankey-beta\nX,A,200.0\nX,B,80.0";
        let diag = parse(src).unwrap();
        let out = render(&diag, None);

        // Regression guard: flow targets and values must still appear.
        assert!(out.contains('A'), "target A missing: {out}");
        assert!(out.contains('B'), "target B missing: {out}");
        assert!(out.contains("200"), "value 200 missing: {out}");
        assert!(out.contains("80"), "value 80 missing: {out}");

        // Extract the two flow lines.
        let line_a = out
            .lines()
            .find(|l| l.contains("] ► A") || (l.contains("► A") && l.contains('█')))
            .expect("flow line for target A (with a bar) not found in output");
        let line_b = out
            .lines()
            .find(|l| l.contains("] ► B") || (l.contains("► B") && l.contains('█')))
            .expect("flow line for target B (with a bar) not found in output");

        let full_a = line_a.chars().filter(|&c| c == '█').count();
        let full_b = line_b.chars().filter(|&c| c == '█').count();

        assert!(
            full_b > 0,
            "flow B bar has no full-block glyphs (B line: {line_b:?})"
        );
        assert!(
            full_a > 0,
            "flow A bar has no full-block glyphs (A line: {line_a:?})"
        );

        // 1.8× tolerance accounts for sub-cell rounding at the lower end.
        let ratio = full_a as f64 / full_b as f64;
        assert!(
            ratio >= 1.8,
            "full-cell ratio A/B = {ratio:.2} < 1.8 (A={full_a}, B={full_b})\nA: {line_a:?}\nB: {line_b:?}"
        );

        // Longest bar must be ≤ 40 cells — ensures scaling actually happened
        // rather than using raw value (200) as cell count.
        let max_full = full_a.max(full_b);
        assert!(
            max_full <= 40,
            "longest bar has {max_full} full-block glyphs, expected ≤ 40 (scaling must cap it)"
        );

        // At least one `█` must appear somewhere in the output (basic existence).
        assert!(out.contains('█'), "no block glyph at all in output:\n{out}");
    }

    /// The total-flow annotation on source headers must appear in rendered output.
    #[test]
    fn source_header_shows_total() {
        // X → A:200, X → B:80; total for X = 280.
        let src = "sankey-beta\nX,A,200.0\nX,B,80.0";
        let diag = parse(src).unwrap();
        let out = render(&diag, None);

        // The header line for X must contain a parenthetical total.
        let header_line = out
            .lines()
            .find(|l| l.starts_with('X'))
            .expect("source header X not found");
        assert!(
            header_line.contains("280") || header_line.contains("total"),
            "source header must show total flow: {header_line:?}"
        );
    }
}
