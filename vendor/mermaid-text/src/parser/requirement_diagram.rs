//! Parser for Mermaid `requirementDiagram` diagrams.
//!
//! Accepted syntax:
//!
//! ```text
//! requirementDiagram
//!
//!     requirement test_req {
//!         id: 1
//!         text: the test text.
//!         risk: high
//!         verifymethod: test
//!     }
//!
//!     element test_entity {
//!         type: simulation
//!         docref: reqs/test_entity
//!     }
//!
//!     test_entity - satisfies -> test_req
//! ```
//!
//! Rules:
//! - `requirementDiagram` keyword is required as the first non-blank,
//!   non-comment line (case-sensitive; Mermaid spells it camelCase).
//! - Requirement blocks: `<kind> <name> { id: … text: … risk: … verifymethod: … }`.
//!   `id:` and `text:` are required; `risk:` and `verifymethod:` are optional.
//! - Element blocks: `element <name> { type: … docref: … }`.
//!   `type:` is required; `docref:` is optional.
//! - Relationship lines: `<source> - <kind> -> <target>`.
//! - `%%` comment lines, blank lines, and `accTitle`/`accDescr` lines are
//!   silently skipped.
//! - Unknown lines outside blocks are silently ignored for forward compatibility.
//!
//! # Examples
//!
//! ```
//! use mermaid_text::parser::requirement_diagram::parse;
//!
//! let src = "requirementDiagram\n    requirement r1 {\n        id: 1\n        text: some text.\n    }";
//! let diag = parse(src).unwrap();
//! assert_eq!(diag.requirements.len(), 1);
//! assert_eq!(diag.requirements[0].name, "r1");
//! assert_eq!(diag.requirements[0].id, "1");
//! assert_eq!(diag.requirements[0].text, "some text.");
//! ```

use crate::Error;
use crate::parser::common::strip_inline_comment;
use crate::requirement_diagram::{
    Element, RelationshipKind, Requirement, RequirementDiagram, RequirementKind,
    RequirementRelationship, Risk, VerifyMethod,
};

/// Parse a `requirementDiagram` source string into a [`RequirementDiagram`].
///
/// # Errors
///
/// - [`Error::ParseError`] — missing `requirementDiagram` header, missing
///   required `id:` or `text:` field inside a requirement block, or a
///   malformed relationship arrow.
pub fn parse(src: &str) -> Result<RequirementDiagram, Error> {
    let mut header_seen = false;
    let mut diag = RequirementDiagram::default();

    // Block parsing state: we accumulate lines inside `{ … }` until we
    // hit the matching `}` at the same or lower indent level.
    let mut in_block = false;
    let mut block_kind: BlockKind = BlockKind::Requirement(RequirementKind::default());
    let mut block_name = String::new();
    let mut block_lines: Vec<String> = Vec::new();

    for raw in src.lines() {
        let stripped = strip_inline_comment(raw);
        let trimmed = stripped.trim();

        if !header_seen {
            if trimmed.is_empty() || trimmed.starts_with("%%") {
                continue;
            }
            // Case-sensitive per spec; Mermaid's detector is camelCase-exact
            // for requirementDiagram. We accept it case-insensitively for
            // robustness (matches how the detector module handles it).
            if !trimmed.eq_ignore_ascii_case("requirementDiagram") {
                return Err(Error::ParseError(format!(
                    "expected `requirementDiagram` header, got {trimmed:?}"
                )));
            }
            header_seen = true;
            continue;
        }

        // Skip blank lines and full-line comments.
        if trimmed.is_empty() || trimmed.starts_with("%%") {
            continue;
        }

        // Silently skip accessibility metadata.
        if trimmed.starts_with("accTitle") || trimmed.starts_with("accDescr") {
            continue;
        }

        if in_block {
            if trimmed == "}" {
                // Close the current block and parse its accumulated lines.
                in_block = false;
                match block_kind {
                    BlockKind::Requirement(kind) => {
                        let req = parse_requirement_block(kind, &block_name, &block_lines)?;
                        diag.requirements.push(req);
                    }
                    BlockKind::Element => {
                        let elem = parse_element_block(&block_name, &block_lines)?;
                        diag.elements.push(elem);
                    }
                }
                block_lines.clear();
                block_name.clear();
            } else {
                block_lines.push(trimmed.to_string());
            }
            continue;
        }

        // Try to open a new block.
        if let Some((kind, name)) = try_parse_block_header(trimmed) {
            in_block = true;
            block_kind = kind;
            block_name = name;
            block_lines.clear();
            continue;
        }

        // Try to parse a relationship line: `source - kind -> target`
        if let Some(rel) = try_parse_relationship(trimmed)? {
            diag.relationships.push(rel);
            continue;
        }

        // Unknown top-level line: silently ignore for forward compatibility.
    }

    if !header_seen {
        return Err(Error::ParseError(
            "missing `requirementDiagram` header line".to_string(),
        ));
    }

    Ok(diag)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Discriminant for block types during multi-line parsing.
#[derive(Debug, Clone, Copy)]
enum BlockKind {
    Requirement(RequirementKind),
    Element,
}

/// Match a line like `requirement foo {` or `element bar {`.
///
/// Returns `(BlockKind, name)` if the line opens a block, otherwise `None`.
fn try_parse_block_header(line: &str) -> Option<(BlockKind, String)> {
    // All block headers end with ` {` or just `{`.
    // We need to strip the trailing `{` and optional whitespace.
    let body = line.strip_suffix('{')?.trim();

    // Split on the first whitespace: keyword + name.
    let (keyword, rest) = body.split_once(char::is_whitespace)?;
    let name = rest.trim().to_string();
    if name.is_empty() {
        return None;
    }

    let kind = match keyword.to_lowercase().as_str() {
        "requirement" => BlockKind::Requirement(RequirementKind::Requirement),
        "functionalrequirement" => BlockKind::Requirement(RequirementKind::Functional),
        "interfacerequirement" => BlockKind::Requirement(RequirementKind::Interface),
        "performancerequirement" => BlockKind::Requirement(RequirementKind::Performance),
        "physicalrequirement" => BlockKind::Requirement(RequirementKind::Physical),
        "designconstraint" => BlockKind::Requirement(RequirementKind::DesignConstraint),
        "element" => BlockKind::Element,
        _ => return None,
    };
    Some((kind, name))
}

/// Parse the key-value fields of a requirement block.
///
/// `id:` and `text:` are required; missing either returns [`Error::ParseError`].
fn parse_requirement_block(
    kind: RequirementKind,
    name: &str,
    lines: &[String],
) -> Result<Requirement, Error> {
    let mut id: Option<String> = None;
    let mut text: Option<String> = None;
    let mut risk: Option<Risk> = None;
    let mut verify_method: Option<VerifyMethod> = None;

    for line in lines {
        let trimmed = line.trim();
        if let Some(val) = trimmed.strip_prefix("id:") {
            id = Some(val.trim().to_string());
        } else if let Some(val) = trimmed.strip_prefix("text:") {
            text = Some(val.trim().to_string());
        } else if let Some(val) = trimmed.strip_prefix("risk:") {
            risk = parse_risk(val.trim());
        } else if let Some(val) = trimmed.strip_prefix("verifymethod:") {
            verify_method = parse_verify_method(val.trim());
        }
        // Unknown fields are silently ignored.
    }

    let id = id.ok_or_else(|| {
        Error::ParseError(format!(
            "requirement {name:?} is missing required `id:` field"
        ))
    })?;
    let text = text.ok_or_else(|| {
        Error::ParseError(format!(
            "requirement {name:?} is missing required `text:` field"
        ))
    })?;

    Ok(Requirement {
        kind,
        name: name.to_string(),
        id,
        text,
        risk,
        verify_method,
    })
}

/// Parse the key-value fields of an element block.
///
/// `type:` is required; missing it returns [`Error::ParseError`].
fn parse_element_block(name: &str, lines: &[String]) -> Result<Element, Error> {
    let mut kind: Option<String> = None;
    let mut docref: Option<String> = None;

    for line in lines {
        let trimmed = line.trim();
        if let Some(val) = trimmed.strip_prefix("type:") {
            kind = Some(val.trim().to_string());
        } else if let Some(val) = trimmed.strip_prefix("docref:") {
            docref = Some(val.trim().to_string());
        }
        // Unknown fields are silently ignored.
    }

    let kind = kind.ok_or_else(|| {
        Error::ParseError(format!(
            "element {name:?} is missing required `type:` field"
        ))
    })?;

    Ok(Element {
        name: name.to_string(),
        kind,
        docref,
    })
}

/// Try to parse a relationship line of the form `source - kind -> target`.
///
/// Returns `Ok(Some(rel))` on success, `Ok(None)` if the line does not look
/// like a relationship at all, and `Err(...)` when the line contains ` - `
/// but has malformed arrow syntax.
fn try_parse_relationship(line: &str) -> Result<Option<RequirementRelationship>, Error> {
    // A relationship line must contain both ` - ` and ` -> `.
    let Some(dash_pos) = line.find(" - ") else {
        return Ok(None);
    };

    let source = line[..dash_pos].trim().to_string();
    let after_dash = &line[dash_pos + 3..]; // skip " - "

    let Some(arrow_pos) = after_dash.find(" -> ") else {
        return Err(Error::ParseError(format!(
            "malformed relationship — expected `source - kind -> target`, got {line:?}"
        )));
    };

    let kind_str = after_dash[..arrow_pos].trim();
    let target = after_dash[arrow_pos + 4..].trim().to_string(); // skip " -> "

    if source.is_empty() || target.is_empty() {
        return Err(Error::ParseError(format!(
            "malformed relationship — source or target is empty in {line:?}"
        )));
    }

    let kind = match kind_str.to_lowercase().as_str() {
        "contains" => RelationshipKind::Contains,
        "copies" => RelationshipKind::Copies,
        "derives" => RelationshipKind::Derives,
        "satisfies" => RelationshipKind::Satisfies,
        "verifies" => RelationshipKind::Verifies,
        "refines" => RelationshipKind::Refines,
        "traces" => RelationshipKind::Traces,
        other => {
            return Err(Error::ParseError(format!(
                "unknown relationship kind {other:?} in {line:?}"
            )));
        }
    };

    Ok(Some(RequirementRelationship {
        source,
        target,
        kind,
    }))
}

fn parse_risk(s: &str) -> Option<Risk> {
    match s.to_lowercase().as_str() {
        "low" => Some(Risk::Low),
        "medium" => Some(Risk::Medium),
        "high" => Some(Risk::High),
        _ => None,
    }
}

fn parse_verify_method(s: &str) -> Option<VerifyMethod> {
    match s.to_lowercase().as_str() {
        "analysis" => Some(VerifyMethod::Analysis),
        "inspection" => Some(VerifyMethod::Inspection),
        "test" => Some(VerifyMethod::Test),
        "demonstration" => Some(VerifyMethod::Demonstration),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "requirementDiagram\n";

    fn minimal_req(name: &str) -> String {
        format!(
            "{HEADER}    requirement {name} {{\n        id: 1\n        text: some text.\n    }}"
        )
    }

    // 1. Minimal: header + 1 requirement
    #[test]
    fn parses_minimal_requirement_diagram() {
        let src = minimal_req("r1");
        let diag = parse(&src).unwrap();
        assert_eq!(diag.requirements.len(), 1);
        assert_eq!(diag.requirements[0].name, "r1");
        assert_eq!(diag.requirements[0].id, "1");
        assert_eq!(diag.requirements[0].text, "some text.");
        assert_eq!(diag.requirements[0].kind, RequirementKind::Requirement);
    }

    // 2. All 6 requirement kinds
    #[test]
    fn parses_all_six_requirement_kinds() {
        let src = format!(
            "{HEADER}\
            requirement r1 {{\n    id: 1\n    text: t.\n}}\n\
            functionalRequirement r2 {{\n    id: 2\n    text: t.\n}}\n\
            interfaceRequirement r3 {{\n    id: 3\n    text: t.\n}}\n\
            performanceRequirement r4 {{\n    id: 4\n    text: t.\n}}\n\
            physicalRequirement r5 {{\n    id: 5\n    text: t.\n}}\n\
            designConstraint r6 {{\n    id: 6\n    text: t.\n}}"
        );
        let diag = parse(&src).unwrap();
        assert_eq!(diag.requirements.len(), 6);
        assert_eq!(diag.requirements[0].kind, RequirementKind::Requirement);
        assert_eq!(diag.requirements[1].kind, RequirementKind::Functional);
        assert_eq!(diag.requirements[2].kind, RequirementKind::Interface);
        assert_eq!(diag.requirements[3].kind, RequirementKind::Performance);
        assert_eq!(diag.requirements[4].kind, RequirementKind::Physical);
        assert_eq!(diag.requirements[5].kind, RequirementKind::DesignConstraint);
    }

    // 3. Element with type only
    #[test]
    fn parses_element_with_type_only() {
        let src = format!("{HEADER}element e1 {{\n    type: simulation\n}}");
        let diag = parse(&src).unwrap();
        assert_eq!(diag.elements.len(), 1);
        assert_eq!(diag.elements[0].name, "e1");
        assert_eq!(diag.elements[0].kind, "simulation");
        assert!(diag.elements[0].docref.is_none());
    }

    // 4. Element with type + docref
    #[test]
    fn parses_element_with_type_and_docref() {
        let src =
            format!("{HEADER}element e2 {{\n    type: word doc\n    docref: reqs/test_entity\n}}");
        let diag = parse(&src).unwrap();
        assert_eq!(diag.elements.len(), 1);
        assert_eq!(diag.elements[0].kind, "word doc");
        assert_eq!(
            diag.elements[0].docref,
            Some("reqs/test_entity".to_string())
        );
    }

    // 5. All 7 relationship kinds
    #[test]
    fn parses_all_seven_relationship_kinds() {
        let src = format!(
            "{HEADER}\
            a - contains -> b\n\
            a - copies -> b\n\
            a - derives -> b\n\
            a - satisfies -> b\n\
            a - verifies -> b\n\
            a - refines -> b\n\
            a - traces -> b"
        );
        let diag = parse(&src).unwrap();
        assert_eq!(diag.relationships.len(), 7);
        assert_eq!(diag.relationships[0].kind, RelationshipKind::Contains);
        assert_eq!(diag.relationships[1].kind, RelationshipKind::Copies);
        assert_eq!(diag.relationships[2].kind, RelationshipKind::Derives);
        assert_eq!(diag.relationships[3].kind, RelationshipKind::Satisfies);
        assert_eq!(diag.relationships[4].kind, RelationshipKind::Verifies);
        assert_eq!(diag.relationships[5].kind, RelationshipKind::Refines);
        assert_eq!(diag.relationships[6].kind, RelationshipKind::Traces);
    }

    // 6. Risk + verifymethod fields
    #[test]
    fn parses_risk_and_verifymethod_fields() {
        let src = format!(
            "{HEADER}requirement r1 {{\n\
                id: 1\n\
                text: t.\n\
                risk: medium\n\
                verifymethod: inspection\n\
            }}"
        );
        let diag = parse(&src).unwrap();
        let req = &diag.requirements[0];
        assert_eq!(req.risk, Some(Risk::Medium));
        assert_eq!(req.verify_method, Some(VerifyMethod::Inspection));
    }

    // 7. Missing required id: returns error
    #[test]
    fn missing_id_returns_error() {
        let src = format!("{HEADER}requirement r1 {{\n    text: some text.\n}}");
        let err = parse(&src).unwrap_err();
        assert!(
            err.to_string().contains("id"),
            "error message should mention missing id: {err}"
        );
    }

    // 8. Missing required text: returns error
    #[test]
    fn missing_text_returns_error() {
        let src = format!("{HEADER}requirement r1 {{\n    id: 1\n}}");
        let err = parse(&src).unwrap_err();
        assert!(
            err.to_string().contains("text"),
            "error message should mention missing text: {err}"
        );
    }

    // 9. Comment lines are skipped
    #[test]
    fn comment_lines_skipped() {
        let src = format!(
            "%% preamble\n{HEADER}\
            %% inner comment\n\
            requirement r1 {{\n\
                id: 1\n\
                text: some text. %% trailing\n\
            }}"
        );
        let diag = parse(&src).unwrap();
        assert_eq!(diag.requirements.len(), 1);
    }

    // 10. Malformed relationship arrow returns error
    #[test]
    fn malformed_relationship_arrow_returns_error() {
        // Contains ` - ` but missing ` -> `.
        let src = format!("{HEADER}a - satisfies b");
        let err = parse(&src).unwrap_err();
        assert!(
            err.to_string().contains("malformed"),
            "expected malformed error, got: {err}"
        );
    }

    // 11. Full canonical example parses completely
    #[test]
    fn parses_canonical_example() {
        let src = "requirementDiagram

    requirement test_req {
        id: 1
        text: the test text.
        risk: high
        verifymethod: test
    }

    functionalRequirement test_req2 {
        id: 1.1
        text: the second test text.
        risk: low
        verifymethod: inspection
    }

    element test_entity {
        type: simulation
    }

    element test_entity2 {
        type: word doc
        docref: reqs/test_entity
    }

    test_entity - satisfies -> test_req2
    test_req - traces -> test_req2
    test_req - contains -> test_req";

        let diag = parse(src).unwrap();
        assert_eq!(diag.requirements.len(), 2);
        assert_eq!(diag.elements.len(), 2);
        assert_eq!(diag.relationships.len(), 3);

        let req = &diag.requirements[0];
        assert_eq!(req.name, "test_req");
        assert_eq!(req.id, "1");
        assert_eq!(req.risk, Some(Risk::High));
        assert_eq!(req.verify_method, Some(VerifyMethod::Test));

        let elem = &diag.elements[1];
        assert_eq!(elem.kind, "word doc");
        assert_eq!(elem.docref, Some("reqs/test_entity".to_string()));

        assert_eq!(diag.relationships[0].kind, RelationshipKind::Satisfies);
        assert_eq!(diag.relationships[0].source, "test_entity");
        assert_eq!(diag.relationships[0].target, "test_req2");
    }

    // 12. All verify methods are parsed
    #[test]
    fn parses_all_verify_methods() {
        for (kw, expected) in &[
            ("analysis", VerifyMethod::Analysis),
            ("inspection", VerifyMethod::Inspection),
            ("test", VerifyMethod::Test),
            ("demonstration", VerifyMethod::Demonstration),
        ] {
            let src = format!(
                "{HEADER}requirement r1 {{\n    id: 1\n    text: t.\n    verifymethod: {kw}\n}}"
            );
            let diag = parse(&src).unwrap();
            assert_eq!(
                diag.requirements[0].verify_method,
                Some(*expected),
                "failed for verifymethod={kw}"
            );
        }
    }
}
