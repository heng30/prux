//! Data model for Mermaid `sankey-beta` diagrams.
//!
//! A sankey diagram encodes directed flows between named nodes. Each flow has
//! a source, a target, and a non-negative numeric value representing the
//! volume of material, energy, or cost moving along that arc.
//!
//! Example source:
//!
//! ```text
//! sankey-beta
//!
//! %% source,target,value
//! Agricultural 'waste',Bio-conversion,124.729
//! Bio-conversion,Liquid,0.597
//! Bio-conversion,Solid,280.322
//! Coal imports,Coal,11.606
//! Coal,Solid,75.571
//! ```
//!
//! Constructed by [`crate::parser::sankey::parse`] and consumed by
//! [`crate::render::sankey::render`].
//!
//! ## Phase 1 limitations
//!
//! - True proportional sankey rendering (i.e., node heights scaled to flow
//!   volume and curvilinear bands joining them) requires Sugiyama / Sankey
//!   layout which is out of scope for Phase 1. Instead the renderer uses a
//!   grouped-arrow list: nodes are printed as headers; each outgoing arc is
//!   indented below with `──[value]──►`.
//! - Custom node ordering is not supported; nodes are listed in first-seen
//!   source order from the flow list.
//! - Colour / theming directives are silently ignored.
//! - `accDescr` / `accTitle` accessibility metadata is silently ignored.
//! - Cyclic flows (e.g. A → B → A) are rejected with [`crate::Error::ParseError`].

/// A single directed flow between two named nodes.
///
/// `source` and `target` are arbitrary Unicode strings (single- or double-
/// quoted strings in the CSV source are unquoted before storage). `value`
/// must be strictly positive; the parser rejects zero or negative values.
#[derive(Debug, Clone, PartialEq)]
pub struct SankeyFlow {
    /// Origin node name (already unquoted).
    pub source: String,
    /// Destination node name (already unquoted).
    pub target: String,
    /// Flow volume — strictly positive.
    pub value: f64,
}

/// A parsed `sankey-beta` diagram.
///
/// Constructed by [`crate::parser::sankey::parse`] and consumed by
/// [`crate::render::sankey::render`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sankey {
    /// Flows in the order they appeared in the source.
    pub flows: Vec<SankeyFlow>,
}

impl Sankey {
    /// Sum of all flow values in the diagram.
    ///
    /// Returns `0.0` for an empty diagram.
    ///
    /// ```
    /// use mermaid_text::sankey::{Sankey, SankeyFlow};
    ///
    /// let s = Sankey {
    ///     flows: vec![
    ///         SankeyFlow { source: "A".into(), target: "B".into(), value: 10.0 },
    ///         SankeyFlow { source: "B".into(), target: "C".into(), value: 5.0 },
    ///     ],
    /// };
    /// assert!((s.total_volume() - 15.0).abs() < 1e-9);
    /// ```
    pub fn total_volume(&self) -> f64 {
        self.flows.iter().map(|f| f.value).sum()
    }

    /// All unique node names referenced by any flow, in first-seen order.
    ///
    /// A node may appear as both a source and a target; it is included only
    /// once in the returned list (first occurrence wins).
    ///
    /// ```
    /// use mermaid_text::sankey::{Sankey, SankeyFlow};
    ///
    /// let s = Sankey {
    ///     flows: vec![
    ///         SankeyFlow { source: "A".into(), target: "B".into(), value: 1.0 },
    ///         SankeyFlow { source: "B".into(), target: "C".into(), value: 2.0 },
    ///     ],
    /// };
    /// assert_eq!(s.unique_node_names(), vec!["A", "B", "C"]);
    /// ```
    pub fn unique_node_names(&self) -> Vec<String> {
        let mut seen = Vec::new();
        for flow in &self.flows {
            if !seen.contains(&flow.source) {
                seen.push(flow.source.clone());
            }
            if !seen.contains(&flow.target) {
                seen.push(flow.target.clone());
            }
        }
        seen
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_flow(src: &str, tgt: &str, val: f64) -> SankeyFlow {
        SankeyFlow {
            source: src.to_string(),
            target: tgt.to_string(),
            value: val,
        }
    }

    #[test]
    fn total_volume_empty_is_zero() {
        let s = Sankey::default();
        assert!((s.total_volume() - 0.0).abs() < 1e-9);
    }

    #[test]
    fn total_volume_sums_all_flows() {
        let s = Sankey {
            flows: vec![
                make_flow("A", "B", 10.0),
                make_flow("B", "C", 5.5),
                make_flow("A", "C", 2.25),
            ],
        };
        assert!((s.total_volume() - 17.75).abs() < 1e-9);
    }

    #[test]
    fn unique_node_names_empty() {
        let s = Sankey::default();
        assert!(s.unique_node_names().is_empty());
    }

    #[test]
    fn unique_node_names_deduplicates_and_preserves_order() {
        let s = Sankey {
            flows: vec![
                make_flow("Coal", "Solid", 75.0),
                make_flow("Coal", "Gas", 10.0),
                make_flow("Gas", "Solid", 5.0),
            ],
        };
        assert_eq!(s.unique_node_names(), vec!["Coal", "Solid", "Gas"]);
    }

    #[test]
    fn unique_node_names_single_flow() {
        let s = Sankey {
            flows: vec![make_flow("Source", "Target", 42.0)],
        };
        assert_eq!(s.unique_node_names(), vec!["Source", "Target"]);
    }

    #[test]
    fn partial_eq_holds_for_identical_sankeys() {
        let a = Sankey {
            flows: vec![make_flow("A", "B", 1.0)],
        };
        let b = a.clone();
        assert_eq!(a, b);

        let c = Sankey {
            flows: vec![make_flow("A", "B", 2.0)],
        };
        assert_ne!(a, c);
    }
}
