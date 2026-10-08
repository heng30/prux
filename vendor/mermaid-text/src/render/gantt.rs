//! Renderer for [`GanttDiagram`]. Produces a Unicode bar-chart string.
//!
//! **Layout** (from left to right):
//!
//! ```text
//! Gantt: Title (2014-01-01 → 2014-03-04, 63 days)
//!
//!                     Jan 01    Jan 15    Feb 01
//! Section A
//!   Design            ████████░░░░░░░░░░░░░░░░  [01-01 → 01-30, 30d]
//!   Implementation    ░░░░░░░░████░░░░░░░░░░░░  [01-31 → 02-19, 20d]
//! Section B
//!   Testing           ░░░░░░░░░░░░░░░░░░██░░░░  [02-15 → 03-01, 15d]
//!   Deployment        ░░░░░░░░░░░░░░░░░░░░░█░░  [03-02 → 03-04, 3d]
//! ```
//!
//! **Bar characters:** `█` (U+2588 FULL BLOCK) for active cells, `░`
//! (U+2591 LIGHT SHADE) for empty cells. Both are geometric symbols, not
//! emoji. The `to_ascii` post-pass maps them to `#` and `.` respectively.
//!
//! **Scaling.** When `max_width` is `Some(N)`, the bar zone is sized to fit
//! within the available columns after the name column and the date annotation.
//! Each task bar occupies `(duration / total_span) * bar_zone` cells (minimum
//! 1 cell per task). When `max_width` is `None`, 1 cell = 1 day.
//!
//! **Axis ticks** are emitted at regular intervals derived from the total span
//! so that tick labels never overlap. The tick format obeys `axis_format`.

use unicode_width::UnicodeWidthStr;

use crate::gantt::{GanttDiagram, GanttTask};

// Bar characters — geometric block elements, not emoji.
const FULL_BLOCK: char = '\u{2588}'; // █
const LIGHT_SHADE: char = '\u{2591}'; // ░

/// Width of the task-name column (chars). Tasks whose names exceed this are
/// right-truncated with `…` so the bar chart column aligns.
const NAME_COL_MIN: usize = 18;

/// Minimum bar zone width in cells when max_width is provided.
const BAR_ZONE_MIN: usize = 20;

/// Default bar zone cell count (1 cell per day) when max_width is None and the
/// span is too large to render 1:1 without clipping. Used as the cap.
const BAR_ZONE_UNCONSTRAINED_CAP: usize = 60;

/// Render a [`GanttDiagram`] to a Unicode string.
///
/// # Arguments
///
/// * `diag`      — the parsed diagram
/// * `max_width` — optional column budget; when `Some(N)` the bar zone is
///   scaled to fit within N columns; when `None` each cell represents 1 day
///   (capped at 60 cells to keep the output manageable).
///
/// # Returns
///
/// A multi-line string ready for printing. Sections are separated by blank
/// lines; the header line (with title and date range) appears first.
pub fn render(diag: &GanttDiagram, max_width: Option<usize>) -> String {
    let (min_date, max_date) = match (diag.min_date(), diag.max_date()) {
        (Some(lo), Some(hi)) => (lo, hi),
        _ => return render_empty(diag),
    };

    let span_days = diag.span_days().max(1);

    // Compute column widths.
    let name_col = diag
        .sections
        .iter()
        .flat_map(|s| s.tasks.iter())
        .map(|t| t.name.len() + 2) // 2 spaces of indent
        .max()
        .unwrap_or(NAME_COL_MIN)
        .max(NAME_COL_MIN);

    // Date annotation column: "[MM-DD → MM-DD, NNd]" — up to ~22 chars.
    let annot_col = 24usize;

    // Bar zone: how many cells to allocate for the horizontal bars.
    let bar_zone = compute_bar_zone(max_width, name_col, annot_col, span_days);

    let mut out = String::new();

    // ---- Header line -------------------------------------------------------
    let lo_str = min_date.format("%Y-%m-%d").to_string();
    let hi_str = max_date.format("%Y-%m-%d").to_string();
    if let Some(title) = &diag.title {
        out.push_str(&format!(
            "Gantt: {title} ({lo_str} \u{2192} {hi_str}, {span_days} days)\n"
        ));
    } else {
        out.push_str(&format!(
            "Gantt ({lo_str} \u{2192} {hi_str}, {span_days} days)\n"
        ));
    }

    // ---- Axis line ---------------------------------------------------------
    out.push('\n');
    let axis_line = build_axis_line(
        &diag.axis_format,
        min_date,
        span_days,
        bar_zone,
        name_col,
        annot_col,
    );
    out.push_str(&axis_line);
    out.push('\n');

    // ---- Sections and tasks ------------------------------------------------
    for section in &diag.sections {
        out.push('\n');

        if let Some(name) = &section.name {
            out.push_str(name);
            out.push('\n');
        }

        for task in &section.tasks {
            let row = render_task_row(
                task,
                min_date,
                span_days,
                bar_zone,
                name_col,
                &diag.axis_format,
            );
            out.push_str(&row);
            out.push('\n');
        }
    }

    // Trim the trailing newline.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

// ---------------------------------------------------------------------------
// Bar zone sizing
// ---------------------------------------------------------------------------

/// Compute how many bar cells to allocate.
///
/// When `max_width` is given, the bar zone fills the remaining space after the
/// name column and annotation column. When `None`, each cell represents 1 day
/// capped at `BAR_ZONE_UNCONSTRAINED_CAP`.
fn compute_bar_zone(
    max_width: Option<usize>,
    name_col: usize,
    annot_col: usize,
    span_days: i64,
) -> usize {
    match max_width {
        Some(budget) => {
            // Layout: <name_col> <bar_zone> <2-space gap> <annot_col>
            let overhead = name_col + 2 + annot_col;
            budget.saturating_sub(overhead).max(BAR_ZONE_MIN)
        }
        None => (span_days as usize).min(BAR_ZONE_UNCONSTRAINED_CAP),
    }
}

// ---------------------------------------------------------------------------
// Task row
// ---------------------------------------------------------------------------

/// Render one task row: name column + bar + annotation.
fn render_task_row(
    task: &GanttTask,
    min_date: chrono::NaiveDate,
    span_days: i64,
    bar_zone: usize,
    name_col: usize,
    axis_format: &str,
) -> String {
    // Name column — indent two spaces, pad/truncate to name_col width.
    let display_name = format!("  {}", task.name);
    let col_out = pad_or_truncate(&display_name, name_col);

    // Bar
    let bar = build_bar(task, min_date, span_days, bar_zone);

    // Annotation: "[start → end, Nd]" using axis_format date style.
    let start_str = format_date_axis(&task.start, axis_format);
    let end_str = format_date_axis(&task.end, axis_format);
    let dur = task.duration_days();
    let annot = format!("  [{start_str} \u{2192} {end_str}, {dur}d]");

    format!("{col_out}{bar}{annot}")
}

// ---------------------------------------------------------------------------
// Bar building
// ---------------------------------------------------------------------------

/// Build a bar string of `bar_zone` cells where cells within [start, end] are
/// `FULL_BLOCK` and all others are `LIGHT_SHADE`.
fn build_bar(
    task: &GanttTask,
    min_date: chrono::NaiveDate,
    span_days: i64,
    bar_zone: usize,
) -> String {
    let task_start_offset = (task.start - min_date).num_days();
    let task_end_offset = (task.end - min_date).num_days();

    let mut bar = String::with_capacity(bar_zone * 3); // each char is up to 3 UTF-8 bytes
    for cell in 0..bar_zone {
        // Map cell index to a day offset within the span. Use floating-point
        // to avoid cumulative integer rounding drift.
        let day_lo = (cell as f64 * span_days as f64) / bar_zone as f64;
        let day_hi = ((cell + 1) as f64 * span_days as f64) / bar_zone as f64 - 1.0;

        // Cell is "active" if the task overlaps with this day range.
        let active = (task_end_offset as f64) >= day_lo && (task_start_offset as f64) <= day_hi;
        bar.push(if active { FULL_BLOCK } else { LIGHT_SHADE });
    }
    bar
}

// ---------------------------------------------------------------------------
// Axis line
// ---------------------------------------------------------------------------

/// Build the date axis header line.
///
/// Tick interval is chosen so that ticks are spaced at least 8 cells apart
/// (to avoid label overlap). Labels use the diagram's `axis_format`.
fn build_axis_line(
    axis_format: &str,
    min_date: chrono::NaiveDate,
    span_days: i64,
    bar_zone: usize,
    name_col: usize,
    _annot_col: usize,
) -> String {
    // The axis starts at the same column as the bar zone.
    let prefix = " ".repeat(name_col);

    // Determine tick interval in days so tick labels don't overlap.
    // Each label is at most 8 chars wide; require at least 8 cells between ticks.
    let min_tick_cells = 8usize;
    let cells_per_day = bar_zone as f64 / span_days as f64;
    let min_days_between_ticks = (min_tick_cells as f64 / cells_per_day).ceil() as i64;

    // Round to a "nice" interval: 1, 2, 5, 7, 10, 14, 21, 30, 60, 90, 180, 365.
    let tick_interval_days = nice_interval(min_days_between_ticks.max(1));

    // Build the axis row as an array of chars (one per bar cell), then
    // overwrite tick-label positions.
    let mut row: Vec<char> = vec![' '; bar_zone];

    let mut tick_day: i64 = 0;
    while tick_day < span_days {
        let cell = ((tick_day as f64 * bar_zone as f64) / span_days as f64) as usize;
        let tick_date = min_date + chrono::Duration::days(tick_day);
        let label = format_date_axis(&tick_date, axis_format);

        // Write the label characters into `row` starting at `cell`.
        for (j, ch) in label.chars().enumerate() {
            if cell + j < bar_zone {
                row[cell + j] = ch;
            }
        }

        tick_day += tick_interval_days;
    }

    let axis_str: String = row.into_iter().collect();
    format!("{prefix}{axis_str}")
}

/// Round `n` up to the nearest "nice" interval.
fn nice_interval(n: i64) -> i64 {
    const NICE: &[i64] = &[1, 2, 5, 7, 10, 14, 21, 30, 60, 90, 180, 365];
    NICE.iter().copied().find(|&v| v >= n).unwrap_or(365)
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

/// Format a date using the Mermaid axis format pattern.
///
/// Supported patterns: `%b %d`, `%Y-%m-%d`, `%m/%d`, `%d`, `%m-%d`
/// (the default). Unrecognised patterns fall back to `%m-%d`.
fn format_date_axis(date: &chrono::NaiveDate, axis_format: &str) -> String {
    match axis_format {
        "%b %d" => {
            // "%b" is the abbreviated month name (e.g. "Jan"); chrono
            // formats it correctly via NaiveDate::format.
            date.format("%b %d").to_string()
        }
        "%Y-%m-%d" => date.format("%Y-%m-%d").to_string(),
        "%m/%d" => date.format("%m/%d").to_string(),
        "%d" => date.format("%d").to_string(),
        // Default (and explicit "%m-%d")
        _ => date.format("%m-%d").to_string(),
    }
}

/// Pad `s` to `width` display cells, or truncate with `…` if it exceeds it.
fn pad_or_truncate(s: &str, width: usize) -> String {
    let w = UnicodeWidthStr::width(s as &str);
    if w >= width {
        // Truncate: gather chars until we'd exceed width-1, then append '…'.
        let mut out = String::new();
        let mut used = 0;
        for ch in s.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
            if used + cw + 1 > width {
                break;
            }
            out.push(ch);
            used += cw;
        }
        out.push('\u{2026}'); // …
        // Pad any remaining space.
        let final_w = UnicodeWidthStr::width(out.as_str());
        for _ in final_w..width {
            out.push(' ');
        }
        out
    } else {
        let mut out = s.to_string();
        for _ in w..width {
            out.push(' ');
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Edge case: diagram with no tasks
// ---------------------------------------------------------------------------

fn render_empty(diag: &GanttDiagram) -> String {
    if let Some(title) = &diag.title {
        format!("Gantt: {title} (no tasks)")
    } else {
        "Gantt (no tasks)".to_string()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::gantt::parse;

    fn date(y: i32, m: u32, d: u32) -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    // ---- (1) title appears in output ---------------------------------------

    #[test]
    fn title_appears_in_output() {
        let src =
            "gantt\n  title My Project\n  dateFormat YYYY-MM-DD\n  section S\n  T :2024-01-01, 5d";
        let diag = parse(src).unwrap();
        let out = render(&diag, None);
        assert!(
            out.contains("My Project"),
            "title not found in output:\n{out}"
        );
    }

    // ---- (2) date labels appear in output ----------------------------------

    #[test]
    fn date_labels_appear_in_output() {
        let src = "gantt\n\
            dateFormat YYYY-MM-DD\n\
            axisFormat %Y-%m-%d\n\
            section S\n\
              Task :2024-03-01, 30d";
        let diag = parse(src).unwrap();
        let out = render(&diag, Some(100));
        // The axis line must contain some date label starting with "2024-".
        assert!(
            out.contains("2024-"),
            "date label not found in output:\n{out}"
        );
    }

    // ---- (3) bar widths roughly proportional to task durations -------------

    #[test]
    fn bar_widths_proportional_to_durations() {
        // Task A: 10 days; Task B: 30 days. B should have a longer bar.
        let src = "gantt\n\
            dateFormat YYYY-MM-DD\n\
            section S\n\
              Short :2024-01-01, 10d\n\
              Long  :2024-01-11, 30d";
        let diag = parse(src).unwrap();
        let out = render(&diag, Some(120));

        // Count FULL_BLOCK chars per task line.
        let mut lines = out
            .lines()
            .filter(|l| l.contains("Short") || l.contains("Long"));
        let short_line = lines.next().unwrap_or("");
        let long_line = lines.next().unwrap_or("");
        let short_blocks = short_line.chars().filter(|&c| c == FULL_BLOCK).count();
        let long_blocks = long_line.chars().filter(|&c| c == FULL_BLOCK).count();
        assert!(
            long_blocks > short_blocks,
            "long task bar ({long_blocks}) not wider than short ({short_blocks})"
        );
    }

    // ---- (4) ASCII fallback substitutes block characters -------------------

    #[test]
    fn ascii_fallback_substitutes_block_chars() {
        let src = "gantt\n  dateFormat YYYY-MM-DD\n  section S\n  T :2024-01-01, 5d";
        let diag = parse(src).unwrap();
        let unicode_out = render(&diag, None);
        // Confirm block chars are present in unicode output.
        assert!(unicode_out.contains(FULL_BLOCK) || unicode_out.contains(LIGHT_SHADE));
        // Apply the same to_ascii transform the library uses.
        let ascii_out = crate::to_ascii(&unicode_out);
        assert!(
            ascii_out.is_ascii(),
            "non-ASCII chars remain after to_ascii:\n{ascii_out}"
        );
        // '#' replaces FULL_BLOCK; '.' replaces LIGHT_SHADE.
        assert!(ascii_out.contains('#') || ascii_out.contains('.'));
    }

    // ---- (5) render empty diagram ------------------------------------------

    #[test]
    fn render_empty_diagram_returns_placeholder() {
        let diag = GanttDiagram {
            title: Some("Empty".to_string()),
            ..Default::default()
        };
        let out = render(&diag, None);
        assert!(out.contains("Empty"));
        assert!(out.contains("no tasks"));
    }

    // ---- (6) bar positions don't bleed outside task window -----------------

    #[test]
    fn bar_starts_and_ends_at_correct_positions() {
        // Single task spans the entire diagram — all cells should be FULL_BLOCK.
        // We use a large bar_zone equal to the span_days so there is a 1:1 mapping
        // and every cell falls cleanly inside the task window.
        let src = "gantt\n  dateFormat YYYY-MM-DD\n  section S\n  Only :2024-01-01, 20d";
        let diag = parse(src).unwrap();
        let task = &diag.sections[0].tasks[0];
        let span = diag.span_days(); // 20
        let bar_zone = span as usize; // 1 cell per day
        let bar = build_bar(task, date(2024, 1, 1), span, bar_zone);
        assert_eq!(
            bar.chars().filter(|&c| c == FULL_BLOCK).count(),
            bar_zone,
            "all cells should be active when task spans full diagram (1 cell per day)"
        );
    }
}
