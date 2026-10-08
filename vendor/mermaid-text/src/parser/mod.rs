//! Mermaid diagram parsers.
//!
//! Supports `graph`/`flowchart`, `sequenceDiagram`, `stateDiagram` /
//! `stateDiagram-v2`, `erDiagram`, `classDiagram`, `journey`, `gantt`,
//! `timeline`, `gitGraph`, `mindmap`, `quadrantChart`, `requirementDiagram`,
//! `sankey-beta`, `xychart-beta`, `block-beta`, `architecture-beta`, and
//! `packet-beta` syntax.

pub mod architecture;
pub mod block_diagram;
pub mod class;
pub(crate) mod common;
pub mod er;
pub mod flowchart;
pub mod gantt;
pub mod git_graph;
pub mod journey;
pub mod mindmap;
pub mod packet;
pub mod pie;
pub mod quadrant_chart;
pub mod requirement_diagram;
pub mod sankey;
pub mod sequence;
pub mod state;
pub mod timeline;
pub mod xy_chart;

pub use flowchart::parse;
