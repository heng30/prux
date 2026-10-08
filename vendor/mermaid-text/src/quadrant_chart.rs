//! Data model for Mermaid `quadrantChart` diagrams.
//!
//! A quadrant chart is a 2x2 priority matrix with labeled quadrants and
//! plotted points on a unit-square coordinate system ([0, 1] x [0, 1]).
//! Quadrant numbering follows Mermaid convention: Q1 = top-right,
//! Q2 = top-left, Q3 = bottom-left, Q4 = bottom-right.
//!
//! Example source:
//!
//! ```text
//! quadrantChart
//!     title Reach and engagement of campaigns
//!     x-axis Low Reach --> High Reach
//!     y-axis Low Engagement --> High Engagement
//!     quadrant-1 We should expand
//!     quadrant-2 Need to promote
//!     quadrant-3 Re-evaluate
//!     quadrant-4 May be improved
//!     Campaign A: [0.3, 0.6]
//!     Campaign B: [0.45, 0.23]
//! ```
//!
//! Constructed by [`crate::parser::quadrant_chart::parse`] and consumed by
//! [`crate::render::quadrant_chart::render`].
//!
//! ## Phase 1 limitations
//!
//! - Custom point styling (colour, radius) is not supported; all points render
//!   as a `·` marker followed by the point name and coordinates.
//! - Background quadrant colours/gradients are not rendered.
//! - `accDescr` / `accTitle` accessibility metadata is silently ignored.
//! - Points that are close together may overlap in the text output.

/// Axis label pair for one dimension of the quadrant chart.
///
/// `low` is the label at the origin-side of the axis;
/// `high` is the label at the far end.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AxisLabels {
    pub low: String,
    pub high: String,
}

/// Labels for each of the four quadrants.
///
/// Mermaid numbers quadrants starting from top-right and going
/// counter-clockwise: Q1 = top-right, Q2 = top-left,
/// Q3 = bottom-left, Q4 = bottom-right.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QuadrantLabels {
    pub q1: Option<String>,
    pub q2: Option<String>,
    pub q3: Option<String>,
    pub q4: Option<String>,
}

/// A single plotted data point in the quadrant chart.
///
/// Coordinates `x` and `y` must be in [0, 1]; the parser rejects values
/// outside this range. Overlapping points are possible in Phase 1 when two
/// points map to the same terminal cell.
#[derive(Debug, Clone, PartialEq)]
pub struct QuadrantPoint {
    pub name: String,
    /// Horizontal position in [0, 1]; 0 = left edge, 1 = right edge.
    pub x: f64,
    /// Vertical position in [0, 1]; 0 = bottom edge, 1 = top edge.
    pub y: f64,
}

/// A parsed `quadrantChart` diagram.
///
/// Constructed by [`crate::parser::quadrant_chart::parse`] and consumed by
/// [`crate::render::quadrant_chart::render`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QuadrantChart {
    /// Optional diagram title.
    pub title: Option<String>,
    /// Optional x-axis labels (low end and high end).
    pub x_axis: Option<AxisLabels>,
    /// Optional y-axis labels (low end and high end).
    pub y_axis: Option<AxisLabels>,
    /// Labels for each of the four quadrants (all optional).
    pub quadrants: QuadrantLabels,
    /// Data points to plot on the chart.
    pub points: Vec<QuadrantPoint>,
}

impl QuadrantChart {
    /// Total number of data points in the chart.
    pub fn point_count(&self) -> usize {
        self.points.len()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_chart_has_zero_points() {
        let chart = QuadrantChart::default();
        assert_eq!(chart.point_count(), 0);
        assert!(chart.title.is_none());
        assert!(chart.x_axis.is_none());
        assert!(chart.y_axis.is_none());
        assert!(chart.quadrants.q1.is_none());
        assert!(chart.quadrants.q4.is_none());
        assert!(chart.points.is_empty());
    }

    #[test]
    fn point_count_reflects_number_of_points() {
        let chart = QuadrantChart {
            points: vec![
                QuadrantPoint {
                    name: "A".to_string(),
                    x: 0.3,
                    y: 0.6,
                },
                QuadrantPoint {
                    name: "B".to_string(),
                    x: 0.7,
                    y: 0.2,
                },
                QuadrantPoint {
                    name: "C".to_string(),
                    x: 0.5,
                    y: 0.5,
                },
            ],
            ..Default::default()
        };
        assert_eq!(chart.point_count(), 3);
    }

    #[test]
    fn equality_holds_for_identical_charts() {
        let a = QuadrantChart {
            title: Some("My Chart".to_string()),
            x_axis: Some(AxisLabels {
                low: "Low".to_string(),
                high: "High".to_string(),
            }),
            points: vec![QuadrantPoint {
                name: "P".to_string(),
                x: 0.5,
                y: 0.5,
            }],
            ..Default::default()
        };
        let b = a.clone();
        assert_eq!(a, b);

        let c = QuadrantChart {
            title: Some("Other".to_string()),
            ..Default::default()
        };
        assert_ne!(a, c);
    }

    #[test]
    fn partial_eq_works_for_f64_coordinates() {
        let p1 = QuadrantPoint {
            name: "X".to_string(),
            x: 0.123_456_789,
            y: 0.987_654_321,
        };
        let p2 = p1.clone();
        assert_eq!(p1, p2);

        let p3 = QuadrantPoint {
            name: "X".to_string(),
            x: 0.123_456_789,
            y: 0.1,
        };
        assert_ne!(p1, p3);
    }
}
