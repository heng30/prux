//! Rendering pipeline: graph + positions → Unicode string.

pub mod architecture;
pub mod block_diagram;
pub mod box_table;
pub mod class;
pub mod er;
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
pub mod timeline;
pub mod unicode;
pub mod xy_chart;

pub use unicode::{render, render_color};
