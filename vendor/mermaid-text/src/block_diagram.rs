//! Data model for Mermaid `block-beta` diagrams.
//!
//! A block-beta diagram is a fixed-width grid of rectangular blocks with
//! optional directed edges between them. Blocks occupy one or more columns
//! (column span). Rows are implicit: blocks fill the current row left-to-right
//! according to the declared column count, wrapping when the row is full.
//!
//! Example source:
//!
//! ```text
//! block-beta
//!     columns 3
//!     a["A label"] b:2 c
//!     d e f
//!     g["spans across"]:3
//! ```
//!
//! With directed edges:
//!
//! ```text
//! block-beta
//!     A
//!     B
//!     C
//!     A --> B
//!     B --> C
//! ```
//!
//! Constructed by [`crate::parser::block_diagram::parse`] and consumed by
//! [`crate::render::block_diagram::render`].
//!
//! ## Phase 1 limitations
//!
//! - Only rectangle-shaped blocks are supported. All block shapes (rounded,
//!   stadium, cylinder, etc.) are normalised to plain rectangular boxes with
//!   the inner text displayed as-is.
//! - Nested blocks are not supported; nested block declarations are silently
//!   skipped.
//! - Vertical spans (multi-row blocks) are not supported.
//! - Custom block colours and `accDescr`/`accTitle` are silently ignored.
//! - Edge labels are parsed but rendered as plain text in the edge summary
//!   (no inline label decoration on arrows).

/// A single block in a `block-beta` diagram.
///
/// A block occupies `col_span` columns in the grid. When `text` is empty
/// the block `id` is used as the display label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// The identifier used to reference this block in edges.
    pub id: String,
    /// The display text (from `id["text"]`); empty string means use `id` as label.
    pub text: String,
    /// How many columns this block spans (≥ 1).
    pub col_span: usize,
}

impl Block {
    /// The label to display inside the block's box.
    ///
    /// Returns `text` when non-empty, otherwise falls back to `id`.
    pub fn display_text(&self) -> &str {
        if self.text.is_empty() {
            &self.id
        } else {
            &self.text
        }
    }
}

/// A directed edge between two blocks in a `block-beta` diagram.
///
/// In Phase 1 only `-->` directed edges are parsed. Edge labels are
/// captured but rendered only in the edge summary below the grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockEdge {
    /// The identifier of the source block.
    pub source: String,
    /// The identifier of the target block.
    pub target: String,
    /// An optional edge label (from `source -->|label| target` syntax).
    pub label: Option<String>,
}

/// A parsed `block-beta` diagram.
///
/// Constructed by [`crate::parser::block_diagram::parse`] and consumed by
/// [`crate::render::block_diagram::render`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlockDiagram {
    /// Number of columns in the grid (set by `columns N`; default 1).
    pub columns: usize,
    /// Ordered list of blocks in the diagram (declaration order).
    pub blocks: Vec<Block>,
    /// Directed edges between blocks.
    pub edges: Vec<BlockEdge>,
}

impl BlockDiagram {
    /// Total number of blocks in the diagram.
    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    /// Total number of edges in the diagram.
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Look up a block by its `id`. Returns `None` if no block has that id.
    pub fn find_block(&self, id: &str) -> Option<&Block> {
        self.blocks.iter().find(|b| b.id == id)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_diagram_is_empty() {
        let d = BlockDiagram::default();
        assert_eq!(d.columns, 0);
        assert_eq!(d.block_count(), 0);
        assert_eq!(d.edge_count(), 0);
    }

    #[test]
    fn block_display_text_falls_back_to_id() {
        let b_with_text = Block {
            id: "myid".to_string(),
            text: "My Label".to_string(),
            col_span: 1,
        };
        assert_eq!(b_with_text.display_text(), "My Label");

        let b_bare = Block {
            id: "bare".to_string(),
            text: String::new(),
            col_span: 1,
        };
        assert_eq!(b_bare.display_text(), "bare");
    }

    #[test]
    fn find_block_returns_correct_block() {
        let diag = BlockDiagram {
            columns: 2,
            blocks: vec![
                Block {
                    id: "A".to_string(),
                    text: "Alpha".to_string(),
                    col_span: 1,
                },
                Block {
                    id: "B".to_string(),
                    text: String::new(),
                    col_span: 2,
                },
            ],
            edges: vec![],
        };

        let found = diag.find_block("A").expect("block A must exist");
        assert_eq!(found.id, "A");
        assert_eq!(found.text, "Alpha");

        assert!(diag.find_block("Z").is_none(), "Z does not exist");
    }

    #[test]
    fn equality_holds_for_identical_diagrams() {
        let a = BlockDiagram {
            columns: 3,
            blocks: vec![Block {
                id: "X".to_string(),
                text: "Ex".to_string(),
                col_span: 2,
            }],
            edges: vec![BlockEdge {
                source: "X".to_string(),
                target: "Y".to_string(),
                label: None,
            }],
        };
        let b = a.clone();
        assert_eq!(a, b);

        let c = BlockDiagram {
            columns: 1,
            ..Default::default()
        };
        assert_ne!(a, c);
    }

    #[test]
    fn block_edge_label_is_optional() {
        let with_label = BlockEdge {
            source: "A".to_string(),
            target: "B".to_string(),
            label: Some("calls".to_string()),
        };
        let without_label = BlockEdge {
            source: "A".to_string(),
            target: "B".to_string(),
            label: None,
        };
        assert_ne!(with_label, without_label);
        assert!(with_label.label.is_some());
        assert!(without_label.label.is_none());
    }
}
