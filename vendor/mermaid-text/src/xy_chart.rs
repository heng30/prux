//! Data model for Mermaid `xychart-beta` diagrams.
//!
//! An XY chart is a line / bar chart with a categorical or numeric x-axis and
//! a numeric y-axis. Up to one bar series and one line series can be rendered
//! simultaneously (Phase 1 — last definition wins for each series type).
//!
//! Example source:
//!
//! ```text
//! xychart-beta
//!     title "Sales Revenue"
//!     x-axis [jan, feb, mar, apr, may, jun, jul, aug, sep, oct, nov, dec]
//!     y-axis "Revenue (in $)" 4000 --> 11000
//!     bar [5000, 6000, 7500, 8200, 9500, 10500, 11000, 10200, 9200, 8500, 7000, 6000]
//!     line [5000, 6000, 7500, 8200, 9500, 10500, 11000, 10200, 9200, 8500, 7000, 6000]
//! ```
//!
//! Constructed by [`crate::parser::xy_chart::parse`] and consumed by
//! [`crate::render::xy_chart::render`].
//!
//! ## Phase 1 limitations
//!
//! - Multiple overlapping series of the same kind are not supported; only the
//!   last `bar` and the last `line` definition are kept.
//! - Custom point styling and colours are not supported.
//! - `accDescr` / `accTitle` accessibility metadata is silently ignored.
//! - Horizontal orientation (`xychart-beta horizontal`) is parsed but rendered
//!   vertically. This is a known Phase 1 limitation.

/// Orientation of the XY chart.
///
/// Mermaid supports `xychart-beta horizontal` for a transposed layout.
/// Phase 1 always renders vertically regardless of this setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum XyOrientation {
    #[default]
    Vertical,
    Horizontal,
}

/// The x-axis of an XY chart.
///
/// Either a categorical axis with N string labels or a numeric axis
/// with a min/max range and an optional label.
#[derive(Debug, Clone, PartialEq)]
pub enum XAxis {
    /// Categorical axis: `x-axis [jan, feb, mar, ...]`
    Categorical { labels: Vec<String> },
    /// Numeric axis: `x-axis "Label" 0 --> 100`
    Numeric {
        label: Option<String>,
        min: f64,
        max: f64,
    },
}

impl Default for XAxis {
    fn default() -> Self {
        XAxis::Categorical { labels: Vec::new() }
    }
}

/// The y-axis of an XY chart.
///
/// Always numeric with a min/max range and an optional label.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct YAxis {
    /// Optional axis label (e.g. `"Revenue (in $)"`).
    pub label: Option<String>,
    /// Minimum value on the y-axis.
    pub min: f64,
    /// Maximum value on the y-axis.
    pub max: f64,
}

/// A parsed `xychart-beta` diagram.
///
/// Constructed by [`crate::parser::xy_chart::parse`] and consumed by
/// [`crate::render::xy_chart::render`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct XyChart {
    /// Optional diagram title.
    pub title: Option<String>,
    /// Orientation (Vertical or Horizontal). Phase 1 always renders vertically.
    pub orientation: XyOrientation,
    /// The x-axis definition.
    pub x_axis: XAxis,
    /// The y-axis definition.
    pub y_axis: YAxis,
    /// Bar series data values. Empty when no `bar` line is present.
    pub bar_series: Vec<f64>,
    /// Line series data values. Empty when no `line` line is present.
    pub line_series: Vec<f64>,
}

impl XyChart {
    /// Returns the number of data points (from whichever series is non-empty,
    /// or the x-axis label count for categorical axes).
    pub fn data_count(&self) -> usize {
        if !self.bar_series.is_empty() {
            self.bar_series.len()
        } else if !self.line_series.is_empty() {
            self.line_series.len()
        } else {
            match &self.x_axis {
                XAxis::Categorical { labels } => labels.len(),
                XAxis::Numeric { .. } => 0,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_chart_has_empty_series() {
        let chart = XyChart::default();
        assert!(chart.bar_series.is_empty());
        assert!(chart.line_series.is_empty());
        assert!(chart.title.is_none());
        assert_eq!(chart.orientation, XyOrientation::Vertical);
    }

    #[test]
    fn data_count_from_bar_series() {
        let chart = XyChart {
            bar_series: vec![1.0, 2.0, 3.0],
            ..Default::default()
        };
        assert_eq!(chart.data_count(), 3);
    }

    #[test]
    fn data_count_from_line_series_when_bar_empty() {
        let chart = XyChart {
            line_series: vec![1.0, 2.0, 3.0, 4.0],
            ..Default::default()
        };
        assert_eq!(chart.data_count(), 4);
    }

    #[test]
    fn data_count_prefers_bar_over_line() {
        let chart = XyChart {
            bar_series: vec![1.0, 2.0],
            line_series: vec![1.0, 2.0, 3.0],
            ..Default::default()
        };
        // bar_series wins when non-empty
        assert_eq!(chart.data_count(), 2);
    }

    #[test]
    fn equality_holds_for_identical_charts() {
        let a = XyChart {
            title: Some("My Chart".to_string()),
            bar_series: vec![1.0, 2.0, 3.0],
            ..Default::default()
        };
        let b = a.clone();
        assert_eq!(a, b);

        let c = XyChart {
            title: Some("Other".to_string()),
            ..Default::default()
        };
        assert_ne!(a, c);
    }

    #[test]
    fn x_axis_default_is_categorical_empty() {
        let ax = XAxis::default();
        match ax {
            XAxis::Categorical { labels } => assert!(labels.is_empty()),
            XAxis::Numeric { .. } => panic!("expected Categorical default"),
        }
    }

    #[test]
    fn y_axis_default_has_zero_range() {
        let y = YAxis::default();
        assert!(y.label.is_none());
        assert!((y.min - 0.0).abs() < f64::EPSILON);
        assert!((y.max - 0.0).abs() < f64::EPSILON);
    }
}
