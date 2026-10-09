//! Layout algorithms and the character grid canvas.

pub mod grid;
pub mod layered;
pub(crate) mod nudge;
pub(crate) mod router;
pub mod subgraph;
pub mod sugiyama;

pub use grid::Grid;
pub use layered::{LayoutBackend, LayoutConfig, layout};
pub use subgraph::{SubgraphBounds, compute_subgraph_bounds};
pub use sugiyama::sugiyama_layout;
