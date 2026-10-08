//! Parser for Mermaid `xychart-beta` diagrams.
//!
//! Accepted syntax:
//!
//! ```text
//! xychart-beta
//!     title "Sales Revenue"
//!     x-axis [jan, feb, mar, apr, may, jun, jul, aug, sep, oct, nov, dec]
//!     y-axis "Revenue (in $)" 4000 --> 11000
//!     bar [5000, 6000, 7500, 8200, 9500, 10500, 11000, 10200, 9200, 8500, 7000, 6000]
//!     line [5000, 6000, 7500, 8200, 9500, 10500, 11000, 10200, 9200, 8500, 7000, 6000]
//! ```
//!
//! Rules:
//! - `xychart-beta` or `xychart` keyword, optionally followed by `horizontal`,
//!   is required as the first non-blank, non-comment line.
//! - `title "<text>"` — optional; quoted or unquoted text after `title `.
//! - `x-axis [a, b, c, ...]` — categorical axis with N labels.
//! - `x-axis "<label>" <min> --> <max>` — numeric axis.
//! - `y-axis "<label>" <min> --> <max>` — numeric axis, label optional.
//! - `bar [v1, v2, ...]` — bar series values; last definition wins.
//! - `line [v1, v2, ...]` — line series values; last definition wins.
//! - `%%` comment lines, blank lines, and `accTitle`/`accDescr` lines are
//!   silently skipped.
//! - If bar or line series lengths do not match the number of x-axis labels
//!   (for categorical axes), a [`crate::Error::ParseError`] is returned.
//!
//! # Examples
//!
//! ```
//! use mermaid_text::parser::xy_chart::parse;
//!
//! let src = "xychart-beta\n    x-axis [a, b, c]\n    bar [1, 2, 3]";
//! let diag = parse(src).unwrap();
//! assert_eq!(diag.bar_series.len(), 3);
//! assert!((diag.bar_series[0] - 1.0).abs() < 1e-9);
//! ```

use crate::Error;
use crate::parser::common::strip_inline_comment;
use crate::xy_chart::{XAxis, XyChart, XyOrientation, YAxis};

/// Parse a `xychart-beta` source string into an [`XyChart`].
///
/// # Errors
///
/// - [`Error::ParseError`] — missing header, malformed axis/series syntax,
///   or series length mismatch with categorical x-axis labels.
pub fn parse(src: &str) -> Result<XyChart, Error> {
    let mut header_seen = false;
    let mut chart = XyChart::default();

    for raw in src.lines() {
        let stripped = strip_inline_comment(raw);
        let trimmed = stripped.trim();

        if !header_seen {
            if trimmed.is_empty() || trimmed.starts_with("%%") {
                continue;
            }
            // First keyword token determines chart type; rest may be modifiers.
            let keyword = trimmed.split_whitespace().next().unwrap_or("");
            if !keyword.eq_ignore_ascii_case("xychart-beta")
                && !keyword.eq_ignore_ascii_case("xychart")
            {
                return Err(Error::ParseError(format!(
                    "expected `xychart-beta` header, got {trimmed:?}"
                )));
            }
            // Check for optional `horizontal` modifier.
            let rest_of_header = trimmed[keyword.len()..].trim();
            if rest_of_header.eq_ignore_ascii_case("horizontal") {
                chart.orientation = XyOrientation::Horizontal;
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

        if let Some(rest) = trimmed.strip_prefix("title ") {
            chart.title = Some(strip_quotes(rest.trim()).to_string());
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("x-axis ") {
            chart.x_axis = parse_x_axis(rest.trim())?;
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("y-axis ") {
            chart.y_axis = parse_y_axis(rest.trim())?;
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("bar ") {
            chart.bar_series = parse_value_list(rest.trim())?;
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("line ") {
            chart.line_series = parse_value_list(rest.trim())?;
            continue;
        }

        // Silently ignore unrecognised lines for forward-compatibility.
    }

    if !header_seen {
        return Err(Error::ParseError(
            "missing `xychart-beta` header line".to_string(),
        ));
    }

    // Validate: for categorical x-axes, series lengths must match label count.
    if let XAxis::Categorical { labels } = &chart.x_axis {
        let n = labels.len();
        if n > 0 {
            if !chart.bar_series.is_empty() && chart.bar_series.len() != n {
                return Err(Error::ParseError(format!(
                    "bar series has {} values but x-axis has {} labels",
                    chart.bar_series.len(),
                    n
                )));
            }
            if !chart.line_series.is_empty() && chart.line_series.len() != n {
                return Err(Error::ParseError(format!(
                    "line series has {} values but x-axis has {} labels",
                    chart.line_series.len(),
                    n
                )));
            }
        }
    }

    Ok(chart)
}

/// Strip surrounding double-quotes from a string if present.
fn strip_quotes(s: &str) -> &str {
    if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Parse the x-axis definition.
///
/// Two forms:
/// - `[a, b, c, ...]` — categorical
/// - `"<label>" <min> --> <max>` or `<min> --> <max>` — numeric
fn parse_x_axis(s: &str) -> Result<XAxis, Error> {
    if s.starts_with('[') {
        let labels = parse_label_list(s)?;
        return Ok(XAxis::Categorical { labels });
    }

    // Numeric form: optional quoted label then `min --> max`.
    parse_numeric_axis(s).map(|(label, min, max)| XAxis::Numeric { label, min, max })
}

/// Parse the y-axis definition.
///
/// Form: `"<label>" <min> --> <max>` or `<min> --> <max>`.
fn parse_y_axis(s: &str) -> Result<YAxis, Error> {
    let (label, min, max) = parse_numeric_axis(s)?;
    Ok(YAxis { label, min, max })
}

/// Parse a numeric axis spec: optional `"label"` then `min --> max`.
fn parse_numeric_axis(s: &str) -> Result<(Option<String>, f64, f64), Error> {
    let (label, remainder) = if let Some(after_open) = s.strip_prefix('"') {
        // Consume up to the closing quote.
        let close = after_open
            .find('"')
            .ok_or_else(|| Error::ParseError(format!("unclosed quote in axis definition {s:?}")))?;
        let lbl = after_open[..close].to_string();
        (Some(lbl), after_open[close + 1..].trim())
    } else {
        (None, s)
    };

    // Now expect `min --> max`.
    let Some((min_str, max_str)) = remainder.split_once(" --> ") else {
        return Err(Error::ParseError(format!(
            "expected `<min> --> <max>` in axis definition, got {remainder:?}"
        )));
    };

    let min = min_str
        .trim()
        .parse::<f64>()
        .map_err(|_| Error::ParseError(format!("invalid axis min value {min_str:?}")))?;
    let max = max_str
        .trim()
        .parse::<f64>()
        .map_err(|_| Error::ParseError(format!("invalid axis max value {max_str:?}")))?;

    if min >= max {
        return Err(Error::ParseError(format!(
            "axis min ({min}) must be less than max ({max})"
        )));
    }

    Ok((label, min, max))
}

/// Parse a categorical label list: `[a, b, c, ...]`.
fn parse_label_list(s: &str) -> Result<Vec<String>, Error> {
    let inner = extract_bracket_body(s)?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    let labels = inner
        .split(',')
        .map(|l| strip_quotes(l.trim()).to_string())
        .collect();
    Ok(labels)
}

/// Parse a numeric value list: `[1.0, 2.5, 3.0, ...]`.
fn parse_value_list(s: &str) -> Result<Vec<f64>, Error> {
    let inner = extract_bracket_body(s)?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .enumerate()
        .map(|(i, v)| {
            v.trim().parse::<f64>().map_err(|_| {
                Error::ParseError(format!(
                    "invalid numeric value at index {i}: {:?}",
                    v.trim()
                ))
            })
        })
        .collect()
}

/// Extract the body between the first `[` and the last `]`.
fn extract_bracket_body(s: &str) -> Result<&str, Error> {
    let open = s
        .find('[')
        .ok_or_else(|| Error::ParseError(format!("expected `[` in list {s:?}")))?;
    let close = s
        .rfind(']')
        .ok_or_else(|| Error::ParseError(format!("expected `]` in list {s:?}")))?;
    if close <= open {
        return Err(Error::ParseError(format!("malformed bracket list {s:?}")));
    }
    Ok(&s[open + 1..close])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xy_chart::XAxis;

    const HEADER: &str = "xychart-beta\n";

    // --- header detection ---------------------------------------------------

    #[test]
    fn parses_xychart_beta_header() {
        let chart = parse("xychart-beta\n    bar [1, 2, 3]").unwrap();
        assert_eq!(chart.bar_series.len(), 3);
    }

    #[test]
    fn parses_xychart_alias_header() {
        let chart = parse("xychart\n    bar [1, 2]").unwrap();
        assert_eq!(chart.bar_series.len(), 2);
    }

    #[test]
    fn missing_header_returns_error() {
        assert!(parse("bar [1, 2, 3]").is_err());
        assert!(parse("").is_err());
        assert!(parse("   \n").is_err());
    }

    #[test]
    fn horizontal_modifier_sets_orientation() {
        let chart = parse("xychart-beta horizontal\n    bar [1, 2]").unwrap();
        assert_eq!(chart.orientation, XyOrientation::Horizontal);
    }

    #[test]
    fn default_orientation_is_vertical() {
        let chart = parse(HEADER).unwrap();
        assert_eq!(chart.orientation, XyOrientation::Vertical);
    }

    // --- title --------------------------------------------------------------

    #[test]
    fn parses_quoted_title() {
        let src = format!("{HEADER}    title \"Sales Revenue\"");
        let chart = parse(&src).unwrap();
        assert_eq!(chart.title, Some("Sales Revenue".to_string()));
    }

    #[test]
    fn parses_unquoted_title() {
        let src = format!("{HEADER}    title My Chart");
        let chart = parse(&src).unwrap();
        assert_eq!(chart.title, Some("My Chart".to_string()));
    }

    // --- x-axis -------------------------------------------------------------

    #[test]
    fn parses_categorical_x_axis() {
        let src = format!("{HEADER}    x-axis [jan, feb, mar]");
        let chart = parse(&src).unwrap();
        match &chart.x_axis {
            XAxis::Categorical { labels } => {
                assert_eq!(labels, &["jan", "feb", "mar"]);
            }
            XAxis::Numeric { .. } => panic!("expected Categorical"),
        }
    }

    #[test]
    fn parses_numeric_x_axis() {
        let src = format!("{HEADER}    x-axis \"X Label\" 0 --> 100");
        let chart = parse(&src).unwrap();
        match &chart.x_axis {
            XAxis::Numeric { label, min, max } => {
                assert_eq!(label, &Some("X Label".to_string()));
                assert!((min - 0.0).abs() < 1e-9);
                assert!((max - 100.0).abs() < 1e-9);
            }
            XAxis::Categorical { .. } => panic!("expected Numeric"),
        }
    }

    // --- y-axis -------------------------------------------------------------

    #[test]
    fn parses_y_axis_with_label_and_range() {
        let src = format!("{HEADER}    y-axis \"Revenue (in $)\" 4000 --> 11000");
        let chart = parse(&src).unwrap();
        assert_eq!(chart.y_axis.label, Some("Revenue (in $)".to_string()));
        assert!((chart.y_axis.min - 4000.0).abs() < 1e-9);
        assert!((chart.y_axis.max - 11000.0).abs() < 1e-9);
    }

    #[test]
    fn parses_y_axis_without_label() {
        let src = format!("{HEADER}    y-axis 0 --> 100");
        let chart = parse(&src).unwrap();
        assert!(chart.y_axis.label.is_none());
        assert!((chart.y_axis.min - 0.0).abs() < 1e-9);
        assert!((chart.y_axis.max - 100.0).abs() < 1e-9);
    }

    // --- bar / line series --------------------------------------------------

    #[test]
    fn parses_bar_series() {
        let src = format!("{HEADER}    bar [1, 2, 3]");
        let chart = parse(&src).unwrap();
        assert_eq!(chart.bar_series.len(), 3);
        assert!((chart.bar_series[1] - 2.0).abs() < 1e-9);
    }

    #[test]
    fn parses_line_series() {
        let src = format!("{HEADER}    line [10.5, 20.0]");
        let chart = parse(&src).unwrap();
        assert_eq!(chart.line_series.len(), 2);
        assert!((chart.line_series[0] - 10.5).abs() < 1e-9);
    }

    #[test]
    fn last_bar_definition_wins() {
        let src = format!("{HEADER}    bar [1, 2]\n    bar [3, 4, 5]");
        let chart = parse(&src).unwrap();
        assert_eq!(chart.bar_series.len(), 3);
        assert!((chart.bar_series[2] - 5.0).abs() < 1e-9);
    }

    // --- validation ---------------------------------------------------------

    #[test]
    fn series_length_mismatch_returns_error() {
        let src = format!("{HEADER}    x-axis [a, b, c]\n    bar [1, 2]");
        assert!(parse(&src).is_err(), "length mismatch should fail");
    }

    #[test]
    fn series_length_match_succeeds() {
        let src = format!("{HEADER}    x-axis [a, b, c]\n    bar [1, 2, 3]\n    line [4, 5, 6]");
        let chart = parse(&src).unwrap();
        assert_eq!(chart.bar_series.len(), 3);
        assert_eq!(chart.line_series.len(), 3);
    }

    #[test]
    fn invalid_numeric_value_returns_error() {
        let src = format!("{HEADER}    bar [1, abc, 3]");
        assert!(
            parse(&src).is_err(),
            "non-numeric value in bar list should fail"
        );
    }

    #[test]
    fn comment_lines_are_skipped() {
        let src = "%% preamble\nxychart-beta\n%% inner\n    bar [1, 2] %% trailing";
        let chart = parse(src).unwrap();
        assert_eq!(chart.bar_series.len(), 2);
    }

    #[test]
    fn acc_title_and_acc_descr_are_silently_ignored() {
        let src = format!("{HEADER}    accTitle: My Title\n    accDescr: description\n    bar [1]");
        let chart = parse(&src).unwrap();
        assert_eq!(chart.bar_series.len(), 1);
    }

    // --- full canonical example ---------------------------------------------

    #[test]
    fn parses_canonical_sales_example() {
        let src = "xychart-beta
    title \"Sales Revenue\"
    x-axis [jan, feb, mar, apr, may, jun, jul, aug, sep, oct, nov, dec]
    y-axis \"Revenue (in $)\" 4000 --> 11000
    bar [5000, 6000, 7500, 8200, 9500, 10500, 11000, 10200, 9200, 8500, 7000, 6000]
    line [5000, 6000, 7500, 8200, 9500, 10500, 11000, 10200, 9200, 8500, 7000, 6000]";

        let chart = parse(src).unwrap();
        assert_eq!(chart.title, Some("Sales Revenue".to_string()));
        assert_eq!(chart.bar_series.len(), 12);
        assert_eq!(chart.line_series.len(), 12);
        assert_eq!(chart.y_axis.label, Some("Revenue (in $)".to_string()));
        assert!((chart.y_axis.min - 4000.0).abs() < 1e-9);
        assert!((chart.y_axis.max - 11000.0).abs() < 1e-9);
        match &chart.x_axis {
            XAxis::Categorical { labels } => assert_eq!(labels.len(), 12),
            _ => panic!("expected Categorical x-axis"),
        }
    }
}
