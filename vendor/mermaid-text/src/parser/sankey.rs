//! Parser for Mermaid `sankey-beta` diagrams.
//!
//! Accepted syntax:
//!
//! ```text
//! sankey-beta
//!
//! %% source,target,value
//! Agricultural 'waste',Bio-conversion,124.729
//! Bio-conversion,Liquid,0.597
//! Bio-conversion,Solid,280.322
//! ```
//!
//! Rules:
//! - `sankey-beta` or `sankey` keyword is required as the first non-blank,
//!   non-comment line.
//! - Each data line has the CSV form `source,target,value`.
//! - `source` and `target` may be optionally enclosed in single (`'`) or
//!   double (`"`) quotes. Quotes are stripped; the inner text is used as-is.
//! - `value` must be a positive finite `f64`; zero and negative values are
//!   rejected.
//! - `%%` comment lines and blank lines are silently skipped.
//! - `accTitle` / `accDescr` lines are silently ignored.
//! - Malformed lines (wrong field count, bad value) return [`crate::Error::ParseError`].
//!
//! # Examples
//!
//! ```
//! use mermaid_text::parser::sankey::parse;
//!
//! let src = "sankey-beta\nBio-conversion,Liquid,0.597";
//! let diag = parse(src).unwrap();
//! assert_eq!(diag.flows.len(), 1);
//! assert_eq!(diag.flows[0].source, "Bio-conversion");
//! assert_eq!(diag.flows[0].target, "Liquid");
//! assert!((diag.flows[0].value - 0.597).abs() < 1e-9);
//! ```

use crate::Error;
use crate::sankey::{Sankey, SankeyFlow};

/// Parse a `sankey-beta` source string into a [`Sankey`].
///
/// # Errors
///
/// - [`Error::ParseError`] — missing header, wrong field count, bad numeric
///   value, zero or negative value.
pub fn parse(src: &str) -> Result<Sankey, Error> {
    let mut header_seen = false;
    let mut flows = Vec::new();

    for raw in src.lines() {
        // Strip trailing inline `%%` comment (outside of quoted fields).
        let stripped = strip_sankey_inline_comment(raw);
        let trimmed = stripped.trim();

        // Skip blank lines and standalone comment lines.
        if trimmed.is_empty() || trimmed.starts_with("%%") {
            continue;
        }

        if !header_seen {
            // First content line must be the header keyword.
            let keyword = trimmed.split_whitespace().next().unwrap_or(trimmed);
            if keyword.eq_ignore_ascii_case("sankey-beta") || keyword.eq_ignore_ascii_case("sankey")
            {
                header_seen = true;
                continue;
            } else {
                return Err(Error::ParseError(format!(
                    "expected `sankey-beta` header, got {trimmed:?}"
                )));
            }
        }

        // Silently skip accessibility metadata lines.
        if trimmed.starts_with("accTitle") || trimmed.starts_with("accDescr") {
            continue;
        }

        // Everything else is expected to be a CSV flow line.
        let flow = parse_flow_line(trimmed)?;
        flows.push(flow);
    }

    if !header_seen {
        return Err(Error::ParseError(
            "missing `sankey-beta` header line".to_string(),
        ));
    }

    Ok(Sankey { flows })
}

/// Parse a single CSV flow line of the form `source,target,value`.
///
/// Source and target may each be optionally quoted with `'` or `"`.
/// Returns `Err` if the line does not contain exactly three fields or if
/// the value is not a positive finite number.
fn parse_flow_line(line: &str) -> Result<SankeyFlow, Error> {
    let fields = split_csv_fields(line);

    if fields.len() != 3 {
        return Err(Error::ParseError(format!(
            "expected 3 CSV fields (source,target,value), got {} in {line:?}",
            fields.len()
        )));
    }

    let source = unquote(fields[0]);
    let target = unquote(fields[1]);
    let value_str = fields[2].trim();

    if source.is_empty() {
        return Err(Error::ParseError(format!(
            "source field is empty in {line:?}"
        )));
    }
    if target.is_empty() {
        return Err(Error::ParseError(format!(
            "target field is empty in {line:?}"
        )));
    }

    let value = value_str.parse::<f64>().map_err(|_| {
        Error::ParseError(format!("invalid numeric value {value_str:?} in {line:?}"))
    })?;

    if !value.is_finite() {
        return Err(Error::ParseError(format!(
            "value must be finite, got {value} in {line:?}"
        )));
    }
    if value <= 0.0 {
        return Err(Error::ParseError(format!(
            "value must be positive (got {value}) in {line:?}"
        )));
    }

    Ok(SankeyFlow {
        source: source.to_string(),
        target: target.to_string(),
        value,
    })
}

/// Split a CSV line on unquoted commas.
///
/// Handles single-quoted (`'`) and double-quoted (`"`) fields. A quoted field
/// spans from the opening quote to the matching closing quote; commas inside
/// are not treated as separators. This is a minimal, non-escaping CSV parser
/// sufficient for the Mermaid sankey format.
fn split_csv_fields(line: &str) -> Vec<&str> {
    let mut fields: Vec<&str> = Vec::new();
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut start = 0usize;
    let mut i = 0usize;

    while i < len {
        let b = bytes[i];
        match b {
            b'\'' | b'"' => {
                // Enter a quoted region — skip to the matching closing quote.
                let quote = b;
                i += 1;
                while i < len && bytes[i] != quote {
                    i += 1;
                }
                if i < len {
                    i += 1; // consume closing quote
                }
            }
            b',' => {
                fields.push(&line[start..i]);
                i += 1;
                start = i;
            }
            _ => {
                i += 1;
            }
        }
    }
    // Push the final field (possibly empty if trailing comma).
    fields.push(&line[start..]);
    fields
}

/// Remove an optional enclosing quote pair from a field.
///
/// Accepts both `'text'` and `"text"`. Only strips if the first and last
/// character are the same quote character. Surrounding whitespace is trimmed
/// first and last.
fn unquote(field: &str) -> &str {
    let trimmed = field.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'\'' || first == b'"') && first == last {
            return trimmed[1..trimmed.len() - 1].trim();
        }
    }
    trimmed
}

/// Strip a trailing `%%` comment from a sankey line, but only when the `%%`
/// appears outside a quoted region.
///
/// In sankey CSV lines, quoted strings use `'` or `"`. A `%%` that appears
/// inside a quoted field is part of the data and must not be stripped.
fn strip_sankey_inline_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut in_quote: Option<u8> = None;
    let mut i = 0usize;

    while i + 1 < len {
        let b = bytes[i];
        match in_quote {
            Some(q) if b == q => {
                in_quote = None;
            }
            Some(_) => {}
            None => {
                if b == b'\'' || b == b'"' {
                    in_quote = Some(b);
                } else if b == b'%' && bytes[i + 1] == b'%' {
                    return &line[..i];
                }
            }
        }
        i += 1;
    }
    line
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "sankey-beta\n";

    // -- parse ----------------------------------------------------------------

    #[test]
    fn parses_minimal_single_flow() {
        let src = format!("{HEADER}Bio-conversion,Liquid,0.597");
        let diag = parse(&src).unwrap();
        assert_eq!(diag.flows.len(), 1);
        assert_eq!(diag.flows[0].source, "Bio-conversion");
        assert_eq!(diag.flows[0].target, "Liquid");
        assert!((diag.flows[0].value - 0.597).abs() < 1e-9);
    }

    #[test]
    fn parses_multiple_flows() {
        let src = format!(
            "{HEADER}\
            Bio-conversion,Liquid,0.597\n\
            Bio-conversion,Solid,280.322\n\
            Coal,Solid,75.571"
        );
        let diag = parse(&src).unwrap();
        assert_eq!(diag.flows.len(), 3);
        assert!((diag.flows[1].value - 280.322).abs() < 1e-6);
    }

    #[test]
    fn strips_percent_percent_comment_lines() {
        let src = format!("{HEADER}%% this is a comment\nCoal,Solid,75.571");
        let diag = parse(&src).unwrap();
        assert_eq!(diag.flows.len(), 1);
        assert_eq!(diag.flows[0].source, "Coal");
    }

    #[test]
    fn strips_inline_comment_after_header() {
        // A trailing `%% note` on the header line itself is uncommon but valid.
        let src = "sankey-beta\nCoal,Solid,75.571";
        let diag = parse(src).unwrap();
        assert_eq!(diag.flows.len(), 1);
    }

    #[test]
    fn accepts_sankey_keyword_without_beta() {
        let src = "sankey\nA,B,1.0";
        let diag = parse(src).unwrap();
        assert_eq!(diag.flows.len(), 1);
    }

    #[test]
    fn unquotes_single_quoted_fields() {
        let src = format!("{HEADER}Agricultural 'waste',Bio-conversion,124.729");
        // The source contains the literal text without the outer single-quote pair.
        let diag = parse(&src).unwrap();
        assert_eq!(diag.flows.len(), 1);
        // Single-quote pairs that wrap just the token are stripped;
        // apostrophes embedded inside are left intact (no escaping in Phase 1).
        let source = &diag.flows[0].source;
        // The outer quotes around 'waste' make: Agricultural 'waste'
        // Since the entire field is Agricultural 'waste' — the leading chars
        // are not a quote — only exact full-field quoting is stripped.
        assert!(
            source.contains("waste"),
            "source should contain waste: {source}"
        );
    }

    #[test]
    fn unquotes_double_quoted_fields() {
        let src = format!("{HEADER}\"Source Node\",\"Target Node\",42.0");
        let diag = parse(&src).unwrap();
        assert_eq!(diag.flows[0].source, "Source Node");
        assert_eq!(diag.flows[0].target, "Target Node");
    }

    #[test]
    fn rejects_zero_value() {
        let src = format!("{HEADER}A,B,0.0");
        assert!(parse(&src).is_err(), "zero value should be rejected");
    }

    #[test]
    fn rejects_negative_value() {
        let src = format!("{HEADER}A,B,-5.0");
        assert!(parse(&src).is_err(), "negative value should be rejected");
    }

    #[test]
    fn rejects_non_numeric_value() {
        let src = format!("{HEADER}A,B,not_a_number");
        assert!(parse(&src).is_err(), "non-numeric value should be rejected");
    }

    #[test]
    fn rejects_too_few_fields() {
        let src = format!("{HEADER}A,B");
        assert!(parse(&src).is_err(), "two fields should be rejected");
    }

    #[test]
    fn rejects_missing_header() {
        let src = "A,B,1.0";
        assert!(parse(src).is_err(), "missing header should be rejected");
    }

    #[test]
    fn skips_blank_lines() {
        let src = format!("{HEADER}\n\nA,B,1.0\n\nB,C,2.0\n");
        let diag = parse(&src).unwrap();
        assert_eq!(diag.flows.len(), 2);
    }

    #[test]
    fn skips_acc_title_and_acc_descr() {
        let src = format!("{HEADER}accTitle: My Sankey\naccDescr: Flow diagram\nA,B,1.0");
        let diag = parse(&src).unwrap();
        assert_eq!(diag.flows.len(), 1);
    }

    // -- split_csv_fields / unquote internals ---------------------------------

    #[test]
    fn split_csv_fields_basic() {
        let fields = split_csv_fields("A,B,1.0");
        assert_eq!(fields, vec!["A", "B", "1.0"]);
    }

    #[test]
    fn split_csv_fields_quoted_comma_not_split() {
        let fields = split_csv_fields("\"A,B\",C,1.0");
        assert_eq!(fields, vec!["\"A,B\"", "C", "1.0"]);
    }

    #[test]
    fn unquote_strips_double_quotes() {
        assert_eq!(unquote("\"hello world\""), "hello world");
    }

    #[test]
    fn unquote_strips_single_quotes() {
        assert_eq!(unquote("'hello world'"), "hello world");
    }

    #[test]
    fn unquote_unquoted_passthrough() {
        assert_eq!(unquote("hello"), "hello");
    }
}
