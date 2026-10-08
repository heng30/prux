//! Parser for Mermaid `architecture-beta` diagrams.
//!
//! Accepted syntax (subset — Phase 1):
//!
//! ```text
//! architecture-beta
//!     group api(cloud)[API]
//!
//!     service db(database)[Database] in api
//!     service disk1(disk)[Storage] in api
//!     service server(server)[Server] in api
//!
//!     db:L -- R:server
//!     disk1:T -- B:server
//!     server --> db
//! ```
//!
//! Rules:
//! - `architecture-beta` or `architecture` keyword is required as the first
//!   non-blank, non-comment line.
//! - `group <id>(<icon>)[<label>]` — declare a group. Icon and label are optional.
//! - `service <id>(<icon>)[<label>]` — top-level service.
//! - `service <id>(<icon>)[<label>] in <group_id>` — service in a group.
//! - `<src>:<port> -- <port>:<tgt>` — port-to-port edge.
//! - `<src> --> <tgt>` — simple directed edge (no port specifiers).
//! - `junction(id)` lines are silently skipped.
//! - `%%` comment lines, blank lines, `accTitle`, and `accDescr` are silently
//!   skipped.
//! - Malformed lines (unrecognised structure) return [`Error::ParseError`].
//!
//! # Examples
//!
//! ```
//! use mermaid_text::parser::architecture::parse;
//!
//! let src = "architecture-beta\n    group api(cloud)[API]\n    service db(database)[Database] in api";
//! let arch = parse(src).unwrap();
//! assert_eq!(arch.groups.len(), 1);
//! assert_eq!(arch.services.len(), 1);
//! assert_eq!(arch.services[0].group.as_deref(), Some("api"));
//! ```

use crate::Error;
use crate::architecture::{ArchEdge, ArchGroup, ArchService, Architecture, Port};
use crate::parser::common::strip_inline_comment;

/// Parse an `architecture-beta` source string into an [`Architecture`].
///
/// # Errors
///
/// - [`Error::ParseError`] — missing header, malformed group/service/edge
///   syntax, or an unknown structural line.
pub fn parse(src: &str) -> Result<Architecture, Error> {
    let mut header_seen = false;
    let mut arch = Architecture::default();

    for raw in src.lines() {
        let stripped = strip_inline_comment(raw);
        let trimmed = stripped.trim();

        if !header_seen {
            if trimmed.is_empty() || trimmed.starts_with("%%") {
                continue;
            }
            let keyword = trimmed.split_whitespace().next().unwrap_or("");
            if keyword.eq_ignore_ascii_case("architecture-beta")
                || keyword.eq_ignore_ascii_case("architecture")
            {
                header_seen = true;
                continue;
            }
            return Err(Error::ParseError(format!(
                "expected `architecture-beta` or `architecture` header, got {trimmed:?}"
            )));
        }

        // Skip blank and comment lines.
        if trimmed.is_empty() || trimmed.starts_with("%%") {
            continue;
        }

        // Silently skip accessibility metadata.
        if trimmed.starts_with("accTitle") || trimmed.starts_with("accDescr") {
            continue;
        }

        // Dispatch by leading keyword.
        let first = trimmed.split_whitespace().next().unwrap_or("");

        if first.eq_ignore_ascii_case("group") {
            let rest = trimmed["group".len()..].trim();
            let group = parse_group(rest)?;
            arch.groups.push(group);
            continue;
        }

        if first.eq_ignore_ascii_case("service") {
            let rest = trimmed["service".len()..].trim();
            let service = parse_service(rest)?;
            arch.services.push(service);
            continue;
        }

        // Silently skip junction declarations.
        if first.eq_ignore_ascii_case("junction") {
            continue;
        }

        // Try to parse as a port edge: `src:PORT -- PORT:tgt`
        // or a simple directed edge: `src --> tgt`
        if let Some(edge) = try_parse_edge(trimmed)? {
            arch.edges.push(edge);
            continue;
        }

        return Err(Error::ParseError(format!(
            "unrecognised architecture-beta line: {trimmed:?}"
        )));
    }

    if !header_seen {
        return Err(Error::ParseError(
            "missing `architecture-beta` or `architecture` header line".to_string(),
        ));
    }

    Ok(arch)
}

// ---------------------------------------------------------------------------
// Group parsing
// ---------------------------------------------------------------------------

/// Parse the body of a `group` line: `<id>(<icon>)[<label>]`.
///
/// Both `(<icon>)` and `[<label>]` are optional. The id is the first token
/// before any `(` or `[`.
fn parse_group(rest: &str) -> Result<ArchGroup, Error> {
    let (id, after_id) = parse_id_token(rest)?;
    let (icon, after_icon) = parse_optional_paren(after_id);
    let (label, _after_label) = parse_optional_bracket(after_icon);

    Ok(ArchGroup { id, icon, label })
}

// ---------------------------------------------------------------------------
// Service parsing
// ---------------------------------------------------------------------------

/// Parse the body of a `service` line:
/// `<id>(<icon>)[<label>]` or `<id>(<icon>)[<label>] in <group_id>`.
fn parse_service(rest: &str) -> Result<ArchService, Error> {
    let (id, after_id) = parse_id_token(rest)?;
    let (icon, after_icon) = parse_optional_paren(after_id);
    let (label, after_label) = parse_optional_bracket(after_icon);

    // Check for optional `in <group_id>` suffix.
    let group = parse_in_clause(after_label.trim());

    Ok(ArchService {
        id,
        icon,
        label,
        group,
    })
}

/// Parse an optional `in <group_id>` clause from the remaining text.
///
/// Returns `Some(group_id)` if the text starts with `in `, otherwise `None`.
fn parse_in_clause(rest: &str) -> Option<String> {
    let rest = rest.trim();
    if let Some(after_in) = rest.strip_prefix("in ").or_else(|| {
        // also accept `in\t`
        rest.strip_prefix("in\t")
    }) {
        let gid = after_in.split_whitespace().next().unwrap_or("");
        if gid.is_empty() {
            None
        } else {
            Some(gid.to_string())
        }
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Edge parsing
// ---------------------------------------------------------------------------

/// Try to parse a line as an architecture edge.
///
/// Recognises two forms:
/// 1. Port edge: `src:PORT -- PORT:tgt` (double-dash, no arrow head)
/// 2. Simple directed edge: `src --> tgt`
///
/// Returns `Ok(Some(edge))` when the line matches either form,
/// `Ok(None)` when the line is not an edge at all, and `Err(...)` when
/// the line structurally looks like an edge but is malformed.
fn try_parse_edge(line: &str) -> Result<Option<ArchEdge>, Error> {
    // Detect port-style edge: must contain ` -- ` (space-dash-dash-space).
    if line.contains(" -- ") {
        return parse_port_edge(line).map(Some);
    }

    // Detect simple directed edge: contains `-->`.
    if line.contains("-->") {
        return parse_simple_edge(line).map(Some);
    }

    Ok(None)
}

/// Parse a port edge of the form `src:PORT -- PORT:tgt`.
///
/// The port characters are `L`/`R`/`T`/`B`. Either or both ports may be
/// absent — e.g. `src -- tgt` is treated as a port-less undirected edge.
fn parse_port_edge(line: &str) -> Result<ArchEdge, Error> {
    // Split on ` -- `.
    let Some((left, right)) = line.split_once(" -- ") else {
        return Err(Error::ParseError(format!(
            "expected ` -- ` in port edge: {line:?}"
        )));
    };
    let left = left.trim();
    let right = right.trim();

    // Left side: `src:PORT` or just `src`.
    let (source, source_port) = parse_side_with_port(left)
        .map_err(|e| Error::ParseError(format!("malformed source in port edge {line:?}: {e}")))?;

    // Right side: `PORT:tgt` or just `tgt`.
    let (target_port, target) = parse_target_side(right)
        .map_err(|e| Error::ParseError(format!("malformed target in port edge {line:?}: {e}")))?;

    if source.is_empty() {
        return Err(Error::ParseError(format!(
            "empty source in port edge: {line:?}"
        )));
    }
    if target.is_empty() {
        return Err(Error::ParseError(format!(
            "empty target in port edge: {line:?}"
        )));
    }

    Ok(ArchEdge {
        source,
        source_port,
        target,
        target_port,
        label: None,
    })
}

/// Parse `src:PORT` from the left side of a port edge.
///
/// Returns `(id, port)`. Port is `None` when no `:PORT` suffix is present.
fn parse_side_with_port(s: &str) -> Result<(String, Option<Port>), String> {
    if let Some((id, port_char)) = s.rsplit_once(':') {
        let id = id.trim().to_string();
        let port = parse_port_char(port_char.trim())?;
        Ok((id, Some(port)))
    } else {
        Ok((s.trim().to_string(), None))
    }
}

/// Parse `PORT:tgt` from the right side of a port edge.
///
/// Returns `(port, id)`. Port is `None` when no `PORT:` prefix is present.
fn parse_target_side(s: &str) -> Result<(Option<Port>, String), String> {
    // Check if the string starts with a single port character followed by `:`.
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        // First byte is the port char, rest is id.
        let port_char = &s[..1];
        let id = s[2..].trim().to_string();
        let port = parse_port_char(port_char)?;
        return Ok((Some(port), id));
    }
    // No port prefix — entire string is the id.
    Ok((None, s.trim().to_string()))
}

/// Parse a single port character (`L`, `R`, `T`, `B`) — case-insensitive.
fn parse_port_char(c: &str) -> Result<Port, String> {
    match c.to_uppercase().as_str() {
        "L" => Ok(Port::Left),
        "R" => Ok(Port::Right),
        "T" => Ok(Port::Top),
        "B" => Ok(Port::Bottom),
        other => Err(format!("unknown port {other:?}: must be L, R, T, or B")),
    }
}

/// Parse a simple directed edge: `src --> tgt`.
fn parse_simple_edge(line: &str) -> Result<ArchEdge, Error> {
    let Some(arrow_pos) = line.find("-->") else {
        return Err(Error::ParseError(format!(
            "expected `-->` in simple edge: {line:?}"
        )));
    };
    let source = line[..arrow_pos].trim().to_string();
    let target = line[arrow_pos + 3..].trim().to_string();

    if source.is_empty() {
        return Err(Error::ParseError(format!(
            "empty source in simple edge: {line:?}"
        )));
    }
    if target.is_empty() {
        return Err(Error::ParseError(format!(
            "empty target in simple edge: {line:?}"
        )));
    }

    Ok(ArchEdge {
        source,
        source_port: None,
        target,
        target_port: None,
        label: None,
    })
}

// ---------------------------------------------------------------------------
// Token helpers
// ---------------------------------------------------------------------------

/// Extract a bare identifier from the start of `s`.
///
/// The id ends at the first `(`, `[`, or ASCII whitespace. Returns
/// `(id, remainder)` where `remainder` is the slice after the id.
fn parse_id_token(s: &str) -> Result<(String, &str), Error> {
    let end = s
        .find(|c: char| c == '(' || c == '[' || c.is_ascii_whitespace())
        .unwrap_or(s.len());
    let id = s[..end].trim().to_string();
    if id.is_empty() {
        return Err(Error::ParseError(format!("missing identifier in: {s:?}")));
    }
    Ok((id, &s[end..]))
}

/// Parse an optional `(<content>)` token from the start of `s`.
///
/// Returns `(Some(content), remainder)` when the text starts with `(`,
/// and `(None, s)` otherwise.
fn parse_optional_paren(s: &str) -> (Option<String>, &str) {
    let s = s.trim_start();
    if s.starts_with('(')
        && let Some(close) = s.find(')')
    {
        let content = s[1..close].trim().to_string();
        let rest = &s[close + 1..];
        let val = if content.is_empty() {
            None
        } else {
            Some(content)
        };
        return (val, rest);
    }
    (None, s)
}

/// Parse an optional `[<content>]` token from the start of `s`.
///
/// Returns `(Some(content), remainder)` when the text starts with `[`,
/// and `(None, s)` otherwise.
fn parse_optional_bracket(s: &str) -> (Option<String>, &str) {
    let s = s.trim_start();
    if s.starts_with('[')
        && let Some(close) = s.find(']')
    {
        let content = s[1..close].trim().to_string();
        let rest = &s[close + 1..];
        let val = if content.is_empty() {
            None
        } else {
            Some(content)
        };
        return (val, rest);
    }
    (None, s)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- header detection ---------------------------------------------------

    #[test]
    fn parses_architecture_beta_header() {
        let src = "architecture-beta\n    group g(cloud)[G]";
        let arch = parse(src).unwrap();
        assert_eq!(arch.groups.len(), 1);
    }

    #[test]
    fn parses_architecture_alias_header() {
        let src = "architecture\n    service s(server)[S]";
        let arch = parse(src).unwrap();
        assert_eq!(arch.services.len(), 1);
    }

    #[test]
    fn missing_header_returns_error() {
        assert!(parse("group api(cloud)[API]").is_err());
        assert!(parse("").is_err());
    }

    // --- group parsing -------------------------------------------------------

    #[test]
    fn parses_group_with_icon_and_label() {
        let src = "architecture-beta\n    group api(cloud)[API]";
        let arch = parse(src).unwrap();
        assert_eq!(arch.groups.len(), 1);
        let g = &arch.groups[0];
        assert_eq!(g.id, "api");
        assert_eq!(g.icon.as_deref(), Some("cloud"));
        assert_eq!(g.label.as_deref(), Some("API"));
    }

    #[test]
    fn parses_group_without_icon_or_label() {
        let src = "architecture-beta\n    group bare";
        let arch = parse(src).unwrap();
        assert_eq!(arch.groups.len(), 1);
        let g = &arch.groups[0];
        assert_eq!(g.id, "bare");
        assert!(g.icon.is_none());
        assert!(g.label.is_none());
    }

    // --- service parsing -----------------------------------------------------

    #[test]
    fn parses_service_in_group() {
        let src = "architecture-beta\n    group api(cloud)[API]\n    service db(database)[Database] in api";
        let arch = parse(src).unwrap();
        assert_eq!(arch.services.len(), 1);
        let s = &arch.services[0];
        assert_eq!(s.id, "db");
        assert_eq!(s.icon.as_deref(), Some("database"));
        assert_eq!(s.label.as_deref(), Some("Database"));
        assert_eq!(s.group.as_deref(), Some("api"));
    }

    #[test]
    fn parses_top_level_service() {
        let src = "architecture-beta\n    service ext(internet)[External]";
        let arch = parse(src).unwrap();
        let s = &arch.services[0];
        assert_eq!(s.id, "ext");
        assert!(s.group.is_none());
    }

    // --- edge parsing --------------------------------------------------------

    #[test]
    fn parses_port_edge() {
        let src = "architecture-beta\n    db:L -- R:server";
        let arch = parse(src).unwrap();
        assert_eq!(arch.edges.len(), 1);
        let e = &arch.edges[0];
        assert_eq!(e.source, "db");
        assert_eq!(e.source_port, Some(Port::Left));
        assert_eq!(e.target, "server");
        assert_eq!(e.target_port, Some(Port::Right));
    }

    #[test]
    fn parses_simple_directed_edge() {
        let src = "architecture-beta\n    server --> db";
        let arch = parse(src).unwrap();
        assert_eq!(arch.edges.len(), 1);
        let e = &arch.edges[0];
        assert_eq!(e.source, "server");
        assert_eq!(e.source_port, None);
        assert_eq!(e.target, "db");
        assert_eq!(e.target_port, None);
    }

    #[test]
    fn skips_comment_and_blank_lines() {
        let src = "%% preamble\narchitecture-beta\n%% inner\n\n    service s(server)[S]";
        let arch = parse(src).unwrap();
        assert_eq!(arch.services.len(), 1);
    }

    #[test]
    fn malformed_line_returns_error() {
        // A line that is not a group, service, junction, or edge.
        let src = "architecture-beta\n    unknown_directive foo bar";
        assert!(parse(src).is_err());
    }

    #[test]
    fn skips_junction_declaration() {
        // Junction nodes are silently skipped in Phase 1.
        let src = "architecture-beta\n    junction jct\n    service s(server)[S]";
        let arch = parse(src).unwrap();
        assert_eq!(arch.services.len(), 1);
        assert_eq!(arch.groups.len(), 0);
        assert_eq!(arch.edges.len(), 0);
    }

    #[test]
    fn parses_canonical_example() {
        let src = "architecture-beta
    group api(cloud)[API]

    service db(database)[Database] in api
    service disk1(disk)[Storage] in api
    service disk2(disk)[Storage] in api
    service server(server)[Server] in api

    db:L -- R:server
    disk1:T -- B:server
    disk2:T -- B:db";

        let arch = parse(src).unwrap();
        assert_eq!(arch.groups.len(), 1);
        assert_eq!(arch.services.len(), 4);
        assert_eq!(arch.edges.len(), 3);

        assert_eq!(arch.groups[0].id, "api");
        assert_eq!(arch.groups[0].icon.as_deref(), Some("cloud"));
        assert_eq!(arch.groups[0].label.as_deref(), Some("API"));

        let db = arch.find_service("db").expect("db must exist");
        assert_eq!(db.group.as_deref(), Some("api"));

        let e0 = &arch.edges[0];
        assert_eq!(e0.source, "db");
        assert_eq!(e0.source_port, Some(Port::Left));
        assert_eq!(e0.target, "server");
        assert_eq!(e0.target_port, Some(Port::Right));
    }
}
