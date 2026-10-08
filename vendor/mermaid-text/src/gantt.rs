//! Data model for Mermaid `gantt` diagrams.
//!
//! A Gantt diagram represents a project schedule as a horizontal bar chart.
//! Tasks are grouped into named sections, each with an explicit or derived
//! start date and a duration. Dates are resolved at parse time so that
//! consumers receive fully-concrete `NaiveDate` values, not raw spec strings.
//!
//! Example source:
//!
//! ```text
//! gantt
//!     title A Gantt Diagram
//!     dateFormat YYYY-MM-DD
//!     axisFormat %b %d
//!     section Section A
//!         Design        :a1, 2014-01-01, 30d
//!         Implementation:after a1, 20d
//!     section Section B
//!         Testing       :2014-02-15, 15d
//!         Deployment    :3d
//! ```
//!
//! Constructed by [`crate::parser::gantt::parse`] and consumed by
//! [`crate::render::gantt::render`].

use chrono::NaiveDate;

/// A single task in a Gantt diagram with fully-resolved dates.
///
/// `name` is the display label of the task. `id` is an optional alphanumeric
/// identifier used as a dependency target in `after <id>` specs. `start` is
/// the resolved first calendar day of the task; `end` is the last calendar day
/// (inclusive, so `end - start + 1 == duration_days`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GanttTask {
    pub name: String,
    /// Optional identifier, alphanumeric plus `_`. Used by `after <id>` deps.
    pub id: Option<String>,
    /// First calendar day of this task (inclusive).
    pub start: NaiveDate,
    /// Last calendar day of this task (inclusive).
    pub end: NaiveDate,
}

impl GanttTask {
    /// Duration of this task in whole days (always >= 1).
    pub fn duration_days(&self) -> i64 {
        // +1 because both start and end are inclusive days.
        (self.end - self.start).num_days() + 1
    }
}

/// A named group of [`GanttTask`]s within a [`GanttDiagram`].
///
/// `name` is `None` for tasks that appear before any `section` keyword.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GanttSection {
    pub name: Option<String>,
    pub tasks: Vec<GanttTask>,
}

/// A parsed `gantt` diagram with all task dates resolved.
///
/// Constructed by [`crate::parser::gantt::parse`] and consumed by
/// [`crate::render::gantt::render`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GanttDiagram {
    /// Optional diagram title (from the `title` directive).
    pub title: Option<String>,
    /// Date format pattern. Currently only `"YYYY-MM-DD"` is accepted by the
    /// parser; stored for round-trip fidelity and future extension.
    pub date_format: String,
    /// Axis format pattern, e.g. `"%b %d"`, `"%Y-%m-%d"`, `"%m/%d"`, `"%d"`.
    /// Defaults to `"%m-%d"` when the source omits `axisFormat`.
    pub axis_format: String,
    pub sections: Vec<GanttSection>,
}

impl Default for GanttDiagram {
    fn default() -> Self {
        Self {
            title: None,
            date_format: "YYYY-MM-DD".to_string(),
            axis_format: "%m-%d".to_string(),
            sections: Vec::new(),
        }
    }
}

impl GanttDiagram {
    /// Earliest start date across all tasks, or `None` when the diagram has no
    /// tasks.
    pub fn min_date(&self) -> Option<NaiveDate> {
        self.all_tasks().map(|t| t.start).min()
    }

    /// Latest end date across all tasks, or `None` when the diagram has no
    /// tasks.
    pub fn max_date(&self) -> Option<NaiveDate> {
        self.all_tasks().map(|t| t.end).max()
    }

    /// Total number of tasks across all sections.
    pub fn total_tasks(&self) -> usize {
        self.sections.iter().map(|s| s.tasks.len()).sum()
    }

    /// Total calendar span in days (max_date − min_date + 1), or 0 when
    /// the diagram has no tasks.
    pub fn span_days(&self) -> i64 {
        match (self.min_date(), self.max_date()) {
            (Some(lo), Some(hi)) => (hi - lo).num_days() + 1,
            _ => 0,
        }
    }

    /// Iterator over every task in section order.
    fn all_tasks(&self) -> impl Iterator<Item = &GanttTask> {
        self.sections.iter().flat_map(|s| s.tasks.iter())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn make_task(name: &str, start: NaiveDate, end: NaiveDate) -> GanttTask {
        GanttTask {
            name: name.to_string(),
            id: None,
            start,
            end,
        }
    }

    #[test]
    fn total_tasks_across_sections() {
        let diag = GanttDiagram {
            sections: vec![
                GanttSection {
                    name: Some("A".to_string()),
                    tasks: vec![
                        make_task("T1", make_date(2024, 1, 1), make_date(2024, 1, 10)),
                        make_task("T2", make_date(2024, 1, 11), make_date(2024, 1, 20)),
                    ],
                },
                GanttSection {
                    name: Some("B".to_string()),
                    tasks: vec![make_task(
                        "T3",
                        make_date(2024, 2, 1),
                        make_date(2024, 2, 7),
                    )],
                },
            ],
            ..Default::default()
        };
        assert_eq!(diag.total_tasks(), 3);
    }

    #[test]
    fn min_max_date_helpers() {
        let diag = GanttDiagram {
            sections: vec![GanttSection {
                name: None,
                tasks: vec![
                    make_task("A", make_date(2024, 3, 5), make_date(2024, 3, 15)),
                    make_task("B", make_date(2024, 3, 1), make_date(2024, 3, 10)),
                    make_task("C", make_date(2024, 3, 12), make_date(2024, 4, 1)),
                ],
            }],
            ..Default::default()
        };
        assert_eq!(diag.min_date(), Some(make_date(2024, 3, 1)));
        assert_eq!(diag.max_date(), Some(make_date(2024, 4, 1)));
    }

    #[test]
    fn empty_diagram_has_no_dates() {
        let diag = GanttDiagram::default();
        assert_eq!(diag.min_date(), None);
        assert_eq!(diag.max_date(), None);
        assert_eq!(diag.total_tasks(), 0);
        assert_eq!(diag.span_days(), 0);
    }

    #[test]
    fn duration_days_single_day_task() {
        let t = make_task("X", make_date(2024, 6, 15), make_date(2024, 6, 15));
        assert_eq!(t.duration_days(), 1);
    }

    #[test]
    fn span_days_multi_task() {
        let diag = GanttDiagram {
            sections: vec![GanttSection {
                name: None,
                tasks: vec![
                    make_task("A", make_date(2024, 1, 1), make_date(2024, 1, 10)),
                    make_task("B", make_date(2024, 1, 11), make_date(2024, 1, 30)),
                ],
            }],
            ..Default::default()
        };
        // min = Jan 1, max = Jan 30 → 30 days
        assert_eq!(diag.span_days(), 30);
    }
}
