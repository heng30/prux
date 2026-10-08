//! Parser for Mermaid `packet-beta` (and `packet`) diagrams.
//!
//! Accepted syntax (subset — Phase 1):
//!
//! ```text
//! packet-beta
//!     title TCP Packet
//!     0-15: "Source Port"
//!     16-31: "Destination Port"
//!     32-63: "Sequence Number"
//!     96-99: "Data Offset"
//!     106: "URG"
//! ```
//!
//! Rules:
//! - `packet-beta` or `packet` keyword is required as the first non-blank,
//!   non-comment line.
//! - `title <text>` — optional single-line title. Quoting optional.
//! - Field rows have the form `<bit_range>: "<label>"` where:
//!   - `<bit_range>` is `N-M` (inclusive range, 0-based) or `N` (single bit).
//!   - `<label>` is the field name, typically quoted; both quoted and unquoted
//!     forms are accepted.
//! - `%%` comment lines, blank lines, and `accTitle`/`accDescr` lines are
//!   silently skipped.
//!
//! # Errors
//!
//! - [`Error::ParseError`] — overlapping bit ranges, `end_bit < start_bit`, or
//!   an unclosed quoted label.
//!
//! # Examples
//!
//! ```
//! use mermaid_text::parser::packet::parse;
//!
//! let diag = parse("packet-beta\n    0-15: \"Source Port\"\n    16-31: \"Dest Port\"").unwrap();
//! assert_eq!(diag.fields.len(), 2);
//! assert_eq!(diag.fields[0].label, "Source Port");
//! assert_eq!(diag.fields[1].start_bit, 16);
//! ```

use crate::Error;
use crate::packet::{Packet, PacketField};
use crate::parser::common::strip_inline_comment;

/// Parse a `packet-beta` source string into a [`Packet`].
///
/// # Errors
///
/// - [`Error::ParseError`] — missing header, `end_bit < start_bit`,
///   overlapping ranges, or an unclosed quoted label.
pub fn parse(src: &str) -> Result<Packet, Error> {
    let mut header_seen = false;
    let mut title: Option<String> = None;
    let mut fields: Vec<PacketField> = Vec::new();

    for raw in src.lines() {
        let stripped = strip_inline_comment(raw);
        let trimmed = stripped.trim();

        if !header_seen {
            if trimmed.is_empty() || trimmed.starts_with("%%") {
                continue;
            }
            let keyword = trimmed.split_whitespace().next().unwrap_or("");
            if keyword.eq_ignore_ascii_case("packet-beta") || keyword.eq_ignore_ascii_case("packet")
            {
                header_seen = true;
                continue;
            }
            return Err(Error::ParseError(format!(
                "expected `packet-beta` or `packet` header, got {trimmed:?}"
            )));
        }

        // Skip blank and comment-only lines.
        if trimmed.is_empty() || trimmed.starts_with("%%") {
            continue;
        }

        // Silently skip accessibility metadata.
        if trimmed.starts_with("accTitle") || trimmed.starts_with("accDescr") {
            continue;
        }

        // `title <text>` directive.
        if let Some(rest) = trimmed
            .strip_prefix("title ")
            .or_else(|| trimmed.strip_prefix("title\t"))
        {
            title = Some(rest.trim().to_string());
            continue;
        }

        // Field row: `<bit_range>: <label>`
        if let Some(field) = try_parse_field(trimmed)? {
            // Validate: end_bit >= start_bit.
            if field.end_bit < field.start_bit {
                return Err(Error::ParseError(format!(
                    "end bit {} is less than start bit {} in field {:?}",
                    field.end_bit, field.start_bit, field.label
                )));
            }

            // Validate: no overlap with already-registered fields.
            for existing in &fields {
                if ranges_overlap(
                    existing.start_bit,
                    existing.end_bit,
                    field.start_bit,
                    field.end_bit,
                ) {
                    return Err(Error::ParseError(format!(
                        "field {:?} (bits {}-{}) overlaps with existing field {:?} (bits {}-{})",
                        field.label,
                        field.start_bit,
                        field.end_bit,
                        existing.label,
                        existing.start_bit,
                        existing.end_bit,
                    )));
                }
            }

            fields.push(field);
            continue;
        }

        // Unknown lines are silently ignored for forward compatibility.
    }

    if !header_seen {
        return Err(Error::ParseError(
            "missing `packet-beta` or `packet` header line".to_string(),
        ));
    }

    Ok(Packet { title, fields })
}

/// Returns `true` when two inclusive bit ranges overlap.
fn ranges_overlap(a_start: u32, a_end: u32, b_start: u32, b_end: u32) -> bool {
    a_start <= b_end && b_start <= a_end
}

/// Try to parse a field line of the form `<bit_range>: "<label>"`.
///
/// Returns `Ok(Some(field))` when the line matches, `Ok(None)` when it
/// does not look like a field line, and `Err(...)` when it structurally
/// matches (has a colon) but the label is malformed (e.g. unclosed quote).
fn try_parse_field(line: &str) -> Result<Option<PacketField>, Error> {
    // The line must contain a `:` that separates the bit range from the label.
    let Some(colon_pos) = line.find(':') else {
        return Ok(None);
    };

    let range_str = line[..colon_pos].trim();
    let label_str = line[colon_pos + 1..].trim();

    // The range part must be purely numeric (digits and an optional '-').
    // Quick guard: if it is empty or starts with a non-digit, it's not a field row.
    if range_str.is_empty() {
        return Ok(None);
    }
    if !range_str.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return Ok(None);
    }

    // Parse `N` or `N-M`.
    let (start_bit, end_bit) = parse_bit_range(range_str)?;

    // Parse the label (quoted or unquoted).
    let label = parse_label(label_str, line)?;

    Ok(Some(PacketField {
        start_bit,
        end_bit,
        label,
    }))
}

/// Parse `N` (single bit) or `N-M` (inclusive range).
fn parse_bit_range(s: &str) -> Result<(u32, u32), Error> {
    if let Some(dash_pos) = s.find('-') {
        let start_str = s[..dash_pos].trim();
        let end_str = s[dash_pos + 1..].trim();
        let start = start_str.parse::<u32>().map_err(|_| {
            Error::ParseError(format!("invalid bit range start {start_str:?} in {s:?}"))
        })?;
        let end = end_str.parse::<u32>().map_err(|_| {
            Error::ParseError(format!("invalid bit range end {end_str:?} in {s:?}"))
        })?;
        Ok((start, end))
    } else {
        let bit = s
            .trim()
            .parse::<u32>()
            .map_err(|_| Error::ParseError(format!("invalid bit index {s:?}")))?;
        Ok((bit, bit))
    }
}

/// Parse a field label from the right-hand side of a field line.
///
/// Accepts:
/// - `"text"` — double-quoted (quotes stripped)
/// - `'text'` — single-quoted (quotes stripped)
/// - `text` — bare (used as-is, trimmed)
///
/// Returns `Err(ParseError)` when a quoted label has no closing quote.
fn parse_label(s: &str, source_line: &str) -> Result<String, Error> {
    if let Some(rest) = s.strip_prefix('"') {
        // Double-quoted label.
        if let Some(close) = rest.find('"') {
            Ok(rest[..close].to_string())
        } else {
            Err(Error::ParseError(format!(
                "unclosed double-quoted label in {source_line:?}"
            )))
        }
    } else if let Some(rest) = s.strip_prefix('\'') {
        // Single-quoted label.
        if let Some(close) = rest.find('\'') {
            Ok(rest[..close].to_string())
        } else {
            Err(Error::ParseError(format!(
                "unclosed single-quoted label in {source_line:?}"
            )))
        }
    } else {
        // Unquoted — use trimmed as-is.
        Ok(s.trim().to_string())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- header detection ---------------------------------------------------

    #[test]
    fn parses_packet_beta_header() {
        let src = "packet-beta\n    0-7: \"Type\"";
        let diag = parse(src).unwrap();
        assert_eq!(diag.fields.len(), 1);
    }

    #[test]
    fn parses_packet_header_alias() {
        let src = "packet\n    0-7: \"Type\"";
        let diag = parse(src).unwrap();
        assert_eq!(diag.fields.len(), 1);
    }

    #[test]
    fn missing_header_returns_error() {
        assert!(parse("0-7: \"Type\"").is_err(), "no header should fail");
        assert!(parse("").is_err(), "empty input should fail");
    }

    // --- title --------------------------------------------------------------

    #[test]
    fn parses_title() {
        let src = "packet-beta\n    title My Packet\n    0-7: \"Type\"";
        let diag = parse(src).unwrap();
        assert_eq!(diag.title, Some("My Packet".to_string()));
    }

    #[test]
    fn no_title_is_none() {
        let src = "packet-beta\n    0-7: \"Type\"";
        let diag = parse(src).unwrap();
        assert!(diag.title.is_none());
    }

    // --- field parsing ------------------------------------------------------

    #[test]
    fn parses_minimal_single_field() {
        let src = "packet-beta\n    0-15: \"Source Port\"";
        let diag = parse(src).unwrap();
        assert_eq!(diag.fields.len(), 1);
        assert_eq!(diag.fields[0].start_bit, 0);
        assert_eq!(diag.fields[0].end_bit, 15);
        assert_eq!(diag.fields[0].label, "Source Port");
    }

    #[test]
    fn parses_single_bit_field() {
        let src = "packet-beta\n    7: \"Flag\"";
        let diag = parse(src).unwrap();
        assert_eq!(diag.fields.len(), 1);
        assert_eq!(diag.fields[0].start_bit, 7);
        assert_eq!(diag.fields[0].end_bit, 7);
    }

    #[test]
    fn parses_multiple_fields_across_rows() {
        let src = "packet-beta\n    0-15: \"Source Port\"\n    16-31: \"Dest Port\"\n    32-63: \"Seq Num\"";
        let diag = parse(src).unwrap();
        assert_eq!(diag.fields.len(), 3);
        assert_eq!(diag.fields[2].start_bit, 32);
        assert_eq!(diag.fields[2].end_bit, 63);
    }

    #[test]
    fn parses_unquoted_label() {
        let src = "packet-beta\n    0-7: Type";
        let diag = parse(src).unwrap();
        assert_eq!(diag.fields[0].label, "Type");
    }

    // --- comment stripping --------------------------------------------------

    #[test]
    fn skips_comment_and_blank_lines() {
        let src = "%% preamble\npacket-beta\n%% inner comment\n\n    0-7: \"Type\"";
        let diag = parse(src).unwrap();
        assert_eq!(diag.fields.len(), 1);
    }

    // --- error cases --------------------------------------------------------

    #[test]
    fn overlapping_ranges_returns_error() {
        let src = "packet-beta\n    0-15: \"A\"\n    8-23: \"B\"";
        let err = parse(src).unwrap_err();
        assert!(
            matches!(err, Error::ParseError(_)),
            "expected ParseError for overlap"
        );
    }

    #[test]
    fn end_less_than_start_returns_error() {
        let src = "packet-beta\n    15-0: \"Backwards\"";
        let err = parse(src).unwrap_err();
        assert!(
            matches!(err, Error::ParseError(_)),
            "expected ParseError for reversed range"
        );
    }

    #[test]
    fn unclosed_quote_returns_error() {
        let src = "packet-beta\n    0-7: \"unclosed";
        let err = parse(src).unwrap_err();
        assert!(
            matches!(err, Error::ParseError(_)),
            "expected ParseError for unclosed quote"
        );
    }

    // --- full TCP example ---------------------------------------------------

    #[test]
    fn parses_tcp_header_subset() {
        let src = "packet-beta
    title TCP Packet
    0-15: \"Source Port\"
    16-31: \"Destination Port\"
    32-63: \"Sequence Number\"
    64-95: \"Acknowledgment Number\"
    96-99: \"Data Offset\"
    100-105: \"Reserved\"
    106: \"URG\"
    107: \"ACK\"";
        let diag = parse(src).unwrap();
        assert_eq!(diag.title, Some("TCP Packet".to_string()));
        assert_eq!(diag.fields.len(), 8);
        // Single-bit URG field
        let urg = &diag.fields[6];
        assert_eq!(urg.start_bit, 106);
        assert_eq!(urg.end_bit, 106);
        assert_eq!(urg.label, "URG");
    }
}
