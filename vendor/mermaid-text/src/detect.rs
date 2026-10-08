//! Diagram-type detection from the first non-blank line of Mermaid source.

use crate::Error;

/// The diagram types that this crate can handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagramKind {
    /// `graph <direction>` or `flowchart <direction>` diagrams.
    Flowchart,
    /// `sequenceDiagram` diagrams.
    Sequence,
    /// `stateDiagram` and `stateDiagram-v2` diagrams. Both keywords share the
    /// same grammar in upstream Mermaid; only the JS renderer differs.
    State,
    /// `pie` charts. Render as a horizontal bar chart in text mode.
    Pie,
    /// `erDiagram` (entity-relationship). Render as labelled entity
    /// boxes joined by cardinality-tagged relationship lines.
    Er,
    /// `classDiagram` (UML class diagram). Render as a layered box-table
    /// diagram with boxes per class and labelled relationship lines.
    Class,
    /// `journey` (user-journey) diagrams. Render as a section/task tree
    /// with filled-star satisfaction scores per step.
    Journey,
    /// `gantt` diagrams. Render as a horizontal bar chart with one row
    /// per task aligned to a date axis (Phase 1 — no status tags,
    /// excludes, or milestones).
    Gantt,
    /// `timeline` diagrams. Render as a vertical bullet-on-a-wire flow with
    /// one row per time period and event text hanging off bullet connectors
    /// (Phase 1 — no custom themes or colour, no `&` relationship links).
    Timeline,
    /// `gitGraph` diagrams. Render as a lane-based commit graph with
    /// branches as vertical columns and commits flowing top-to-bottom
    /// (Phase 1 — no custom themes, orientation, or extended commit types).
    GitGraph,
    /// `mindmap` diagrams. Render as a vertical tree with the root in a
    /// rounded box at the top and children branching below with `├──` / `└──`
    /// connectors (Phase 1 — no shape variants or icons).
    Mindmap,
    /// `quadrantChart` diagrams. Render as a 2x2 priority matrix with labeled
    /// quadrants and proportionally-placed data points (Phase 1 — no custom
    /// point styling or background colours).
    QuadrantChart,
    /// `requirementDiagram` diagrams. Render as labeled requirement and element
    /// boxes with relationship arcs listed below (Phase 1 — vertical stacking,
    /// no graphical relationship lines).
    RequirementDiagram,
    /// `sankey-beta` (and `sankey`) diagrams. Render as a grouped-arrow list
    /// with source nodes as headers and indented arcs showing flow values
    /// (Phase 1 — grouped list layout; true proportional sankey with
    /// Sugiyama band routing is planned for a future phase).
    Sankey,
    /// `xychart-beta` (and `xychart`) diagrams. Render as a bar/line chart
    /// with a categorical or numeric x-axis and a numeric y-axis
    /// (Phase 1 — last bar and last line series only; horizontal orientation
    /// is parsed but rendered vertically; no custom colours or point styling).
    XyChart,
    /// `block-beta` (or `block`) diagrams. Render as a fixed-width grid of
    /// rectangle blocks with an edge summary below (Phase 1 — rectangles only,
    /// no nested blocks, no vertical spanning, no custom colours).
    BlockDiagram,
    /// `architecture-beta` (or `architecture`) diagrams. Render as labeled
    /// group boxes containing service boxes with a connection summary below
    /// (Phase 1 — no icon rendering, no spatial edge routing, no junction nodes).
    Architecture,
    /// `packet-beta` (or `packet`) diagrams. Render as a 32-bit-wide row table
    /// with field labels occupying their bit ranges and a bit-number ruler above
    /// each row (Phase 1 — fixed 32-bit row width; no custom colours).
    Packet,
}

/// Detect the kind of Mermaid diagram described by `input`.
///
/// Only the first non-blank, non-comment line is examined.  Lines beginning
/// with `%%` are treated as comments and skipped.
///
/// # Arguments
///
/// * `input` — the Mermaid source string (need not be fully valid)
///
/// # Returns
///
/// The detected [`DiagramKind`] on success.
///
/// # Errors
///
/// Returns [`Error::EmptyInput`] if `input` contains no non-blank lines.
/// Returns [`Error::UnsupportedDiagram`] if the diagram type is not supported.
///
/// # Examples
///
/// ```
/// use mermaid_text::detect::{detect, DiagramKind};
///
/// assert_eq!(detect("graph LR\nA-->B").unwrap(), DiagramKind::Flowchart);
/// assert_eq!(detect("flowchart TD\nA-->B").unwrap(), DiagramKind::Flowchart);
/// assert_eq!(detect("sequenceDiagram\nA->>B: hi").unwrap(), DiagramKind::Sequence);
/// assert_eq!(detect("pie title Pets").unwrap(), DiagramKind::Pie);
/// assert_eq!(detect("gantt\ntitle Roadmap").unwrap(), DiagramKind::Gantt);
/// assert_eq!(detect("timeline\n2002 : LinkedIn").unwrap(), DiagramKind::Timeline);
/// assert_eq!(detect("gitGraph\ncommit").unwrap(), DiagramKind::GitGraph);
/// assert_eq!(detect("mindmap\n  root").unwrap(), DiagramKind::Mindmap);
/// assert_eq!(detect("quadrantChart\nA: [0.5, 0.5]").unwrap(), DiagramKind::QuadrantChart);
/// assert_eq!(detect("requirementDiagram\n").unwrap(), DiagramKind::RequirementDiagram);
/// assert_eq!(detect("sankey-beta\nA,B,1.0").unwrap(), DiagramKind::Sankey);
/// assert_eq!(detect("xychart-beta\nbar [1,2,3]").unwrap(), DiagramKind::XyChart);
/// assert_eq!(detect("xychart\nbar [1,2,3]").unwrap(), DiagramKind::XyChart);
/// assert_eq!(detect("block-beta\n    A B").unwrap(), DiagramKind::BlockDiagram);
/// assert_eq!(detect("block\n    A B").unwrap(), DiagramKind::BlockDiagram);
/// assert_eq!(detect("architecture-beta\n    service s(server)[S]").unwrap(), DiagramKind::Architecture);
/// assert_eq!(detect("architecture\n    service s(server)[S]").unwrap(), DiagramKind::Architecture);
/// assert_eq!(detect("packet-beta\n    0-15: \"Src\"").unwrap(), DiagramKind::Packet);
/// assert_eq!(detect("packet\n    0-15: \"Src\"").unwrap(), DiagramKind::Packet);
/// assert!(detect("").is_err());
/// ```
pub fn detect(input: &str) -> Result<DiagramKind, Error> {
    let first = input
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("%%"))
        .ok_or(Error::EmptyInput)?;

    // The first token (before any whitespace) determines the diagram type.
    let keyword = first.split_whitespace().next().unwrap_or(first);

    match keyword.to_lowercase().as_str() {
        "graph" | "flowchart" => Ok(DiagramKind::Flowchart),
        "sequencediagram" => Ok(DiagramKind::Sequence),
        "statediagram" | "statediagram-v2" => Ok(DiagramKind::State),
        "pie" => Ok(DiagramKind::Pie),
        "erdiagram" => Ok(DiagramKind::Er),
        "classdiagram" => Ok(DiagramKind::Class),
        "journey" => Ok(DiagramKind::Journey),
        "gantt" => Ok(DiagramKind::Gantt),
        "timeline" => Ok(DiagramKind::Timeline),
        // gitGraph is camelCase in Mermaid spec; match case-insensitively for
        // resilience against linters/formatters that normalise the keyword.
        "gitgraph" => Ok(DiagramKind::GitGraph),
        "mindmap" => Ok(DiagramKind::Mindmap),
        "quadrantchart" => Ok(DiagramKind::QuadrantChart),
        // requirementDiagram is camelCase in the Mermaid spec; match
        // case-insensitively for robustness against formatting tools.
        "requirementdiagram" => Ok(DiagramKind::RequirementDiagram),
        // `sankey-beta` is the official Mermaid keyword; `sankey` is also
        // accepted as a relaxed alias for resilience against linters.
        "sankey-beta" | "sankey" => Ok(DiagramKind::Sankey),
        // `xychart-beta` is the official Mermaid keyword; `xychart` is also
        // accepted as a relaxed alias for resilience against linters.
        "xychart-beta" | "xychart" => Ok(DiagramKind::XyChart),
        // `block-beta` is the official Mermaid keyword; `block` is accepted
        // as a relaxed alias for resilience against linters.
        "block-beta" | "block" => Ok(DiagramKind::BlockDiagram),
        // `architecture-beta` is the official Mermaid keyword; `architecture`
        // is accepted as a relaxed alias for resilience against linters.
        "architecture-beta" | "architecture" => Ok(DiagramKind::Architecture),
        // `packet-beta` is the official Mermaid keyword; `packet` is accepted
        // as a relaxed alias for resilience against linters.
        "packet-beta" | "packet" => Ok(DiagramKind::Packet),
        other => Err(Error::UnsupportedDiagram(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_graph_keyword() {
        assert_eq!(detect("graph LR\nA-->B").unwrap(), DiagramKind::Flowchart);
    }

    #[test]
    fn detects_flowchart_keyword() {
        assert_eq!(
            detect("flowchart TD\nA-->B").unwrap(),
            DiagramKind::Flowchart
        );
    }

    #[test]
    fn empty_returns_error() {
        assert!(matches!(detect(""), Err(Error::EmptyInput)));
        assert!(matches!(detect("   \n  "), Err(Error::EmptyInput)));
    }

    #[test]
    fn unknown_type_returns_error() {
        // A truly unsupported type returns UnsupportedDiagram.
        assert!(matches!(
            detect("unknownDiagram title Roadmap"),
            Err(Error::UnsupportedDiagram(_))
        ));
    }

    #[test]
    fn detects_gantt_keyword() {
        assert_eq!(detect("gantt\ntitle A Plan").unwrap(), DiagramKind::Gantt);
        // Case-insensitive.
        assert_eq!(detect("Gantt").unwrap(), DiagramKind::Gantt);
    }

    #[test]
    fn detects_pie_keyword() {
        assert_eq!(
            detect("pie title Pets\n\"Dogs\" : 5").unwrap(),
            DiagramKind::Pie
        );
        assert_eq!(detect("PIE\n\"X\" : 1").unwrap(), DiagramKind::Pie);
    }

    #[test]
    fn skips_comment_lines() {
        assert_eq!(
            detect("%% This is a comment\ngraph LR\nA-->B").unwrap(),
            DiagramKind::Flowchart
        );
    }

    #[test]
    fn detects_state_diagram_keyword() {
        assert_eq!(
            detect("stateDiagram\n[*] --> A").unwrap(),
            DiagramKind::State
        );
    }

    #[test]
    fn detects_state_diagram_v2_keyword() {
        assert_eq!(
            detect("stateDiagram-v2\n[*] --> A").unwrap(),
            DiagramKind::State
        );
    }

    #[test]
    fn state_diagram_keyword_is_case_insensitive() {
        assert_eq!(
            detect("StateDiagram-V2\n[*] --> A").unwrap(),
            DiagramKind::State
        );
    }

    #[test]
    fn detects_class_diagram_keyword() {
        assert_eq!(
            detect("classDiagram\nclass Animal").unwrap(),
            DiagramKind::Class
        );
        // Case-insensitive.
        assert_eq!(detect("ClassDiagram").unwrap(), DiagramKind::Class);
    }

    #[test]
    fn detects_journey_keyword() {
        assert_eq!(
            detect("journey\ntitle My Day").unwrap(),
            DiagramKind::Journey
        );
        // Case-insensitive.
        assert_eq!(detect("Journey").unwrap(), DiagramKind::Journey);
    }

    #[test]
    fn detects_timeline_keyword() {
        assert_eq!(
            detect("timeline\n2002 : LinkedIn").unwrap(),
            DiagramKind::Timeline
        );
        // Case-insensitive.
        assert_eq!(detect("Timeline").unwrap(), DiagramKind::Timeline);
    }

    #[test]
    fn detects_git_graph_keyword() {
        assert_eq!(detect("gitGraph\ncommit").unwrap(), DiagramKind::GitGraph);
        // Case-insensitive match: "gitgraph" lowercased.
        assert_eq!(detect("GitGraph").unwrap(), DiagramKind::GitGraph);
    }

    #[test]
    fn detects_mindmap_keyword() {
        assert_eq!(detect("mindmap\n  root").unwrap(), DiagramKind::Mindmap);
        // Case-insensitive.
        assert_eq!(detect("Mindmap").unwrap(), DiagramKind::Mindmap);
    }

    #[test]
    fn detects_quadrant_chart_keyword() {
        assert_eq!(
            detect("quadrantChart\nA: [0.5, 0.5]").unwrap(),
            DiagramKind::QuadrantChart
        );
        // Case-insensitive.
        assert_eq!(detect("QuadrantChart").unwrap(), DiagramKind::QuadrantChart);
    }

    #[test]
    fn detects_requirement_diagram_keyword() {
        assert_eq!(
            detect("requirementDiagram\n").unwrap(),
            DiagramKind::RequirementDiagram
        );
        // Case-insensitive.
        assert_eq!(
            detect("RequirementDiagram").unwrap(),
            DiagramKind::RequirementDiagram
        );
    }

    #[test]
    fn detects_sankey_beta_keyword() {
        assert_eq!(detect("sankey-beta\nA,B,1.0").unwrap(), DiagramKind::Sankey);
        // Relaxed alias.
        assert_eq!(detect("sankey\nA,B,1.0").unwrap(), DiagramKind::Sankey);
    }

    #[test]
    fn detects_sankey_keyword_case_insensitive() {
        // The keyword contains a hyphen; case-insensitive matching is via
        // `to_lowercase()` on the split token, so test both.
        assert_eq!(detect("Sankey-Beta\nA,B,1.0").unwrap(), DiagramKind::Sankey);
        assert_eq!(detect("Sankey\nA,B,1.0").unwrap(), DiagramKind::Sankey);
    }

    #[test]
    fn detects_xychart_beta_keyword() {
        assert_eq!(
            detect("xychart-beta\nbar [1,2,3]").unwrap(),
            DiagramKind::XyChart
        );
        // Alias without `-beta`.
        assert_eq!(detect("xychart\nbar [1]").unwrap(), DiagramKind::XyChart);
    }

    #[test]
    fn detects_xychart_keyword_case_insensitive() {
        // Case-insensitive matching via `to_lowercase()` on the split token.
        assert_eq!(
            detect("XyChart-Beta\nbar [1]").unwrap(),
            DiagramKind::XyChart
        );
        assert_eq!(detect("XYCHART\nbar [1]").unwrap(), DiagramKind::XyChart);
    }

    #[test]
    fn detects_block_beta_keyword() {
        assert_eq!(
            detect("block-beta\n    A B").unwrap(),
            DiagramKind::BlockDiagram
        );
        // Relaxed alias without `-beta`.
        assert_eq!(detect("block\n    A B").unwrap(), DiagramKind::BlockDiagram);
    }

    #[test]
    fn detects_block_beta_keyword_case_insensitive() {
        assert_eq!(
            detect("Block-Beta\n    A").unwrap(),
            DiagramKind::BlockDiagram
        );
        assert_eq!(detect("BLOCK\n    A").unwrap(), DiagramKind::BlockDiagram);
    }

    #[test]
    fn detects_architecture_beta_keyword() {
        assert_eq!(
            detect("architecture-beta\n    service s(server)[S]").unwrap(),
            DiagramKind::Architecture
        );
        // Relaxed alias.
        assert_eq!(
            detect("architecture\n    service s(server)[S]").unwrap(),
            DiagramKind::Architecture
        );
    }

    #[test]
    fn detects_architecture_beta_keyword_case_insensitive() {
        assert_eq!(
            detect("Architecture-Beta\n    service s(server)[S]").unwrap(),
            DiagramKind::Architecture
        );
        assert_eq!(
            detect("ARCHITECTURE\n    service s(server)[S]").unwrap(),
            DiagramKind::Architecture
        );
    }

    #[test]
    fn detects_packet_beta_keyword() {
        assert_eq!(
            detect("packet-beta\n    0-15: \"Src\"").unwrap(),
            DiagramKind::Packet
        );
        // Relaxed alias without `-beta`.
        assert_eq!(
            detect("packet\n    0-15: \"Src\"").unwrap(),
            DiagramKind::Packet
        );
    }

    #[test]
    fn detects_packet_beta_keyword_case_insensitive() {
        assert_eq!(
            detect("Packet-Beta\n    0-15: \"Src\"").unwrap(),
            DiagramKind::Packet
        );
        assert_eq!(
            detect("PACKET\n    0-15: \"Src\"").unwrap(),
            DiagramKind::Packet
        );
    }
}
