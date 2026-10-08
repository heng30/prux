//! Data model for Mermaid `timeline` diagrams.
//!
//! A timeline diagram records events that occurred during labelled time
//! periods. Periods are grouped into named sections; each period may have
//! one or more event strings.
//!
//! Example source:
//!
//! ```text
//! timeline
//!     title History of Social Media
//!     section 2002-2004
//!         2002 : LinkedIn
//!         2003 : MySpace launched
//!         2004 : Facebook : Google goes public
//!     section 2005-2008
//!         2005 : YouTube
//!         2006 : Twitter
//! ```
//!
//! Constructed by [`crate::parser::timeline::parse`] and consumed by
//! [`crate::render::timeline::render`].

/// A single time-period row in a [`TimelineSection`].
///
/// `period` is the free-text label for the time period (year, decade, or any
/// text); `events` lists each event that occurred during that period. At least
/// one event is expected per entry, but the data model allows an empty list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TimelineEntry {
    pub period: String,
    pub events: Vec<String>,
}

/// A named group of [`TimelineEntry`] rows within a [`Timeline`].
///
/// `name` is `None` for entries that appear before any `section` keyword
/// (Mermaid silently accepts these and groups them under an implicit unnamed
/// section).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TimelineSection {
    pub name: Option<String>,
    pub entries: Vec<TimelineEntry>,
}

/// A parsed `timeline` diagram.
///
/// Constructed by [`crate::parser::timeline::parse`] and consumed by
/// [`crate::render::timeline::render`]. `title` is the optional diagram title
/// declared with the `title` keyword; `sections` is the ordered list of
/// sections (each containing its time-period entries).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Timeline {
    pub title: Option<String>,
    pub sections: Vec<TimelineSection>,
}

impl Timeline {
    /// Total number of entries (time-period rows) across all sections.
    ///
    /// Useful for validation and summary display. Returns 0 when the diagram
    /// has no sections or all sections are empty.
    pub fn total_entries(&self) -> usize {
        self.sections.iter().map(|s| s.entries.len()).sum()
    }

    /// Total number of individual events across all entries in all sections.
    ///
    /// Returns 0 for an empty diagram.
    pub fn total_events(&self) -> usize {
        self.sections
            .iter()
            .flat_map(|s| s.entries.iter())
            .map(|e| e.events.len())
            .sum()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn total_entries_across_sections() {
        let diag = Timeline {
            title: Some("Test".to_string()),
            sections: vec![
                TimelineSection {
                    name: Some("Early".to_string()),
                    entries: vec![
                        TimelineEntry {
                            period: "2002".to_string(),
                            events: vec!["LinkedIn".to_string()],
                        },
                        TimelineEntry {
                            period: "2003".to_string(),
                            events: vec!["MySpace".to_string()],
                        },
                    ],
                },
                TimelineSection {
                    name: Some("Later".to_string()),
                    entries: vec![TimelineEntry {
                        period: "2005".to_string(),
                        events: vec!["YouTube".to_string()],
                    }],
                },
            ],
        };
        assert_eq!(diag.total_entries(), 3);
    }

    #[test]
    fn total_events_counts_all_events_across_multi_event_entries() {
        let diag = Timeline {
            title: None,
            sections: vec![TimelineSection {
                name: None,
                entries: vec![
                    TimelineEntry {
                        period: "2004".to_string(),
                        // Two events in one period.
                        events: vec!["Facebook".to_string(), "Google IPO".to_string()],
                    },
                    TimelineEntry {
                        period: "2005".to_string(),
                        events: vec!["YouTube".to_string()],
                    },
                ],
            }],
        };
        assert_eq!(diag.total_entries(), 2);
        assert_eq!(diag.total_events(), 3);
    }

    #[test]
    fn total_entries_empty_diagram() {
        assert_eq!(Timeline::default().total_entries(), 0);
        assert_eq!(Timeline::default().total_events(), 0);
    }
}
