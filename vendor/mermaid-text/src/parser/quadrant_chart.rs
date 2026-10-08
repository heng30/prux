//! Parser for Mermaid `quadrantChart` diagrams.
//!
//! Accepted syntax:
//!
//! ```text
//! quadrantChart
//!     title Reach and engagement of campaigns
//!     x-axis Low Reach --> High Reach
//!     y-axis Low Engagement --> High Engagement
//!     quadrant-1 We should expand
//!     quadrant-2 Need to promote
//!     quadrant-3 Re-evaluate
//!     quadrant-4 May be improved
//!     Campaign A: [0.3, 0.6]
//!     Campaign B: [0.45, 0.23]
//! ```
//!
//! Rules:
//! - `quadrantChart` keyword is required as the first non-blank, non-comment line.
//! - `title <text>` — optional; the text after `title ` is the diagram title.
//! - `x-axis <low> --> <high>` — optional x-axis labels split on ` --> `.
//! - `y-axis <low> --> <high>` — optional y-axis labels split on ` --> `.
//! - `quadrant-N <text>` — optional label for each quadrant (N = 1..4).
//! - `<name>: [<x>, <y>]` — a data point. Coordinates must be in [0, 1].
//! - `%%` comment lines, blank lines, and `accTitle`/`accDescr` lines are
//!   silently skipped.
//!
//! # Examples
//!
//! ```
//! use mermaid_text::parser::quadrant_chart::parse;
//!
//! let diag = parse("quadrantChart\n    Campaign A: [0.3, 0.6]").unwrap();
//! assert_eq!(diag.points.len(), 1);
//! assert_eq!(diag.points[0].name, "Campaign A");
//! assert!((diag.points[0].x - 0.3).abs() < 1e-9);
//! assert!((diag.points[0].y - 0.6).abs() < 1e-9);
//! ```

use crate::Error;
use crate::parser::common::strip_inline_comment;
use crate::quadrant_chart::{AxisLabels, QuadrantChart, QuadrantPoint};

/// Parse a `quadrantChart` source string into a [`QuadrantChart`].
///
/// # Errors
///
/// - [`Error::ParseError`] — missing `quadrantChart` header, malformed
///   `[x, y]` syntax, or coordinates outside [0, 1].
pub fn parse(src: &str) -> Result<QuadrantChart, Error> {
    let mut header_seen = false;
    let mut chart = QuadrantChart::default();

    for raw in src.lines() {
        let stripped = strip_inline_comment(raw);
        let trimmed = stripped.trim();

        if !header_seen {
            if trimmed.is_empty() || trimmed.starts_with("%%") {
                continue;
            }
            if !trimmed.eq_ignore_ascii_case("quadrantChart") {
                return Err(Error::ParseError(format!(
                    "expected `quadrantChart` header, got {trimmed:?}"
                )));
            }
            header_seen = true;
            continue;
        }

        // Skip blank and comment lines.
        if trimmed.is_empty() || trimmed.starts_with("%%") {
            continue;
        }

        // Silently skip accessibility metadata.
        if trimmed.starts_with("accTitle") || trimmed.starts_with("accDescr") {
            continue;
        }

        if let Some(text) = trimmed.strip_prefix("title ") {
            chart.title = Some(text.trim().to_string());
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("x-axis ") {
            chart.x_axis = Some(parse_axis_labels(rest)?);
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("y-axis ") {
            chart.y_axis = Some(parse_axis_labels(rest)?);
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("quadrant-1 ") {
            chart.quadrants.q1 = Some(rest.trim().to_string());
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("quadrant-2 ") {
            chart.quadrants.q2 = Some(rest.trim().to_string());
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("quadrant-3 ") {
            chart.quadrants.q3 = Some(rest.trim().to_string());
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("quadrant-4 ") {
            chart.quadrants.q4 = Some(rest.trim().to_string());
            continue;
        }

        // A data point line has the form: `Name: [x, y]`
        if let Some(point) = try_parse_point(trimmed)? {
            chart.points.push(point);
            continue;
        }

        // Silently ignore unrecognised lines to stay forward-compatible with
        // future Mermaid directives (e.g. colour, radius).
    }

    if !header_seen {
        return Err(Error::ParseError(
            "missing `quadrantChart` header line".to_string(),
        ));
    }

    Ok(chart)
}

/// Parse an axis label pair from `<low> --> <high>`.
///
/// If the ` --> ` separator is absent the whole string is used as the `high`
/// label and `low` is left empty — this is permissive to match Mermaid's own
/// lenient parser behaviour.
fn parse_axis_labels(s: &str) -> Result<AxisLabels, Error> {
    if let Some((low, high)) = s.split_once(" --> ") {
        Ok(AxisLabels {
            low: low.trim().to_string(),
            high: high.trim().to_string(),
        })
    } else {
        // No separator — treat entire string as the high label (permissive).
        Ok(AxisLabels {
            low: String::new(),
            high: s.trim().to_string(),
        })
    }
}

/// Try to parse a data point from a line of the form `Name: [x, y]`.
///
/// Returns `Ok(Some(point))` on success, `Ok(None)` if the line is not a
/// point syntax, and `Err(...)` on a structurally recognisable but invalid
/// point (e.g. coordinates out of range, malformed brackets).
fn try_parse_point(line: &str) -> Result<Option<QuadrantPoint>, Error> {
    // A point line must contain `: [` to be recognisable.
    let Some(colon_pos) = line.find(": [") else {
        return Ok(None);
    };

    let name = line[..colon_pos].trim().to_string();
    if name.is_empty() {
        return Err(Error::ParseError(format!(
            "point name is empty in {line:?}"
        )));
    }

    // The bracket payload starts after `: [` and must end with `]`.
    let bracket_start = colon_pos + 3; // skip `: [`
    let bracket_body = &line[bracket_start..];
    let Some(bracket_end) = bracket_body.find(']') else {
        return Err(Error::ParseError(format!(
            "malformed point syntax — missing `]` in {line:?}"
        )));
    };

    let inner = &bracket_body[..bracket_end];
    let Some((x_str, y_str)) = inner.split_once(',') else {
        return Err(Error::ParseError(format!(
            "malformed point syntax — expected `x, y` inside brackets in {line:?}"
        )));
    };

    let x = x_str
        .trim()
        .parse::<f64>()
        .map_err(|_| Error::ParseError(format!("invalid x coordinate {x_str:?} in {line:?}")))?;
    let y = y_str
        .trim()
        .parse::<f64>()
        .map_err(|_| Error::ParseError(format!("invalid y coordinate {y_str:?} in {line:?}")))?;

    if !(0.0..=1.0).contains(&x) {
        return Err(Error::ParseError(format!(
            "x coordinate {x} is outside [0, 1] in {line:?}"
        )));
    }
    if !(0.0..=1.0).contains(&y) {
        return Err(Error::ParseError(format!(
            "y coordinate {y} is outside [0, 1] in {line:?}"
        )));
    }

    Ok(Some(QuadrantPoint { name, x, y }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "quadrantChart\n";

    #[test]
    fn parses_minimal_quadrant_chart() {
        let src = "quadrantChart\n    Campaign A: [0.3, 0.6]";
        let chart = parse(src).unwrap();
        assert_eq!(chart.points.len(), 1);
        assert_eq!(chart.points[0].name, "Campaign A");
        assert!((chart.points[0].x - 0.3).abs() < 1e-9);
        assert!((chart.points[0].y - 0.6).abs() < 1e-9);
    }

    #[test]
    fn parses_title() {
        let src = format!("{HEADER}    title My Chart");
        let chart = parse(&src).unwrap();
        assert_eq!(chart.title, Some("My Chart".to_string()));
    }

    #[test]
    fn parses_axis_labels() {
        let src =
            format!("{HEADER}    x-axis Low Reach --> High Reach\n    y-axis Low Eng --> High Eng");
        let chart = parse(&src).unwrap();
        let x = chart.x_axis.as_ref().unwrap();
        assert_eq!(x.low, "Low Reach");
        assert_eq!(x.high, "High Reach");
        let y = chart.y_axis.as_ref().unwrap();
        assert_eq!(y.low, "Low Eng");
        assert_eq!(y.high, "High Eng");
    }

    #[test]
    fn parses_all_four_quadrant_labels() {
        let src = format!(
            "{HEADER}\
            quadrant-1 Q1 label\n\
            quadrant-2 Q2 label\n\
            quadrant-3 Q3 label\n\
            quadrant-4 Q4 label"
        );
        let chart = parse(&src).unwrap();
        assert_eq!(chart.quadrants.q1, Some("Q1 label".to_string()));
        assert_eq!(chart.quadrants.q2, Some("Q2 label".to_string()));
        assert_eq!(chart.quadrants.q3, Some("Q3 label".to_string()));
        assert_eq!(chart.quadrants.q4, Some("Q4 label".to_string()));
    }

    #[test]
    fn parses_multiple_points() {
        let src = format!(
            "{HEADER}\
            A: [0.1, 0.2]\n\
            B: [0.9, 0.8]\n\
            C: [0.5, 0.5]"
        );
        let chart = parse(&src).unwrap();
        assert_eq!(chart.points.len(), 3);
        assert_eq!(chart.points[0].name, "A");
        assert_eq!(chart.points[1].name, "B");
        assert_eq!(chart.points[2].name, "C");
        assert!((chart.points[2].x - 0.5).abs() < 1e-9);
        assert!((chart.points[2].y - 0.5).abs() < 1e-9);
    }

    #[test]
    fn point_outside_unit_square_returns_error() {
        let x_over = format!("{HEADER}    Bad: [1.5, 0.5]");
        assert!(parse(&x_over).is_err(), "x > 1 should fail");

        let x_under = format!("{HEADER}    Bad: [-0.1, 0.5]");
        assert!(parse(&x_under).is_err(), "x < 0 should fail");

        let y_over = format!("{HEADER}    Bad: [0.5, 1.1]");
        assert!(parse(&y_over).is_err(), "y > 1 should fail");

        let y_under = format!("{HEADER}    Bad: [0.5, -0.01]");
        assert!(parse(&y_under).is_err(), "y < 0 should fail");
    }

    #[test]
    fn malformed_point_syntax_returns_error() {
        // Missing closing bracket.
        let no_close = format!("{HEADER}    P: [0.5, 0.5");
        assert!(parse(&no_close).is_err(), "missing ] should fail");

        // Missing comma inside brackets.
        let no_comma = format!("{HEADER}    P: [0.5 0.5]");
        assert!(parse(&no_comma).is_err(), "missing comma should fail");

        // Non-numeric x coordinate.
        let bad_x = format!("{HEADER}    P: [abc, 0.5]");
        assert!(parse(&bad_x).is_err(), "non-numeric x should fail");
    }

    #[test]
    fn comment_lines_skipped() {
        let src = "%% preamble\nquadrantChart\n%% inner\n    A: [0.5, 0.5] %% trailing";
        let chart = parse(src).unwrap();
        assert_eq!(chart.points.len(), 1);
        assert_eq!(chart.points[0].name, "A");
    }

    #[test]
    fn boundary_coordinates_are_accepted() {
        // Exactly 0.0 and 1.0 are valid — they sit on the axis.
        let src = format!("{HEADER}    P: [0.0, 1.0]");
        let chart = parse(&src).unwrap();
        assert_eq!(chart.points.len(), 1);
        assert!((chart.points[0].x - 0.0).abs() < 1e-9);
        assert!((chart.points[0].y - 1.0).abs() < 1e-9);
    }
}
