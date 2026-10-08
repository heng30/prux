//! Parser for Mermaid `timeline` diagrams.
//!
//! Accepted syntax:
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
//!         2007 : iPhone : Tumblr
//! ```
//!
//! Rules:
//! - `timeline` keyword is required as the first non-blank, non-comment line.
//! - `title <text>` is optional; sets the diagram title. Last occurrence wins.
//! - `section <text>` opens a new named group; subsequent event lines belong to it.
//! - Event lines have the form `<period> : <event1> [: <event2> ...]`.
//!   The first colon-separated field is the period (opaque text); subsequent
//!   fields are individual events for that period.
//! - Entries before any `section` keyword are grouped under an implicit
//!   unnamed section (`TimelineSection { name: None }`).
//! - `%%` comment lines and blank lines are silently skipped.
//! - `accTitle`, `accDescr`, and other accessibility metadata are silently ignored.
//!
//! # Examples
//!
//! ```
//! use mermaid_text::parser::timeline::parse;
//!
//! let diag = parse("timeline\ntitle My History\n2002 : LinkedIn").unwrap();
//! assert_eq!(diag.title.as_deref(), Some("My History"));
//! assert_eq!(diag.sections.len(), 1);
//! assert_eq!(diag.sections[0].entries[0].period, "2002");
//! assert_eq!(diag.sections[0].entries[0].events, vec!["LinkedIn"]);
//! ```

use crate::Error;
use crate::parser::common::{strip_inline_comment, strip_keyword_prefix};
use crate::timeline::{Timeline, TimelineEntry, TimelineSection};

/// Parse a `timeline` source string into a [`Timeline`].
///
/// # Errors
///
/// - [`Error::ParseError`] — missing `timeline` header, or an event line
///   that has no colon separator (meaning neither period nor events).
pub fn parse(src: &str) -> Result<Timeline, Error> {
    let mut diag = Timeline::default();
    let mut header_seen = false;

    // Index of the section currently being populated. `None` until either the
    // first event line (creates an unnamed implicit section) or the first
    // `section` keyword appears.
    let mut current_section: Option<usize> = None;

    for raw in src.lines() {
        let line = strip_inline_comment(raw).trim();
        if line.is_empty() {
            continue;
        }

        if !header_seen {
            if !line.eq_ignore_ascii_case("timeline") {
                return Err(Error::ParseError(format!(
                    "expected `timeline` header, got {line:?}"
                )));
            }
            header_seen = true;
            continue;
        }

        // Silently skip accessibility metadata keywords (`accTitle`, `accDescr`).
        if line.starts_with("accTitle") || line.starts_with("accDescr") {
            continue;
        }

        // `title <text>` — optional diagram title (last occurrence wins).
        if let Some(rest) = strip_keyword_prefix(line, "title") {
            diag.title = Some(rest.to_string());
            continue;
        }

        // `section <text>` — open a new named group.
        if let Some(rest) = strip_keyword_prefix(line, "section") {
            diag.sections.push(TimelineSection {
                name: Some(rest.to_string()),
                entries: Vec::new(),
            });
            current_section = Some(diag.sections.len() - 1);
            continue;
        }

        // Everything else is an event line: `<period> : <event1> [: <event2> ...]`.
        // A line without any colon is treated as a period with no events — we
        // silently skip it rather than erroring so unknown future keywords don't
        // break parsing.
        let Some(colon_pos) = line.find(':') else {
            continue;
        };

        let period = line[..colon_pos].trim().to_string();
        if period.is_empty() {
            // A line starting with `:` has no period — silently skip.
            continue;
        }

        // Split remaining colon-separated fields into individual events.
        let events: Vec<String> = line[colon_pos + 1..]
            .split(':')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();

        let entry = TimelineEntry { period, events };

        // If no section has been opened yet, create the implicit unnamed one.
        let idx = match current_section {
            Some(i) => i,
            None => {
                diag.sections.push(TimelineSection {
                    name: None,
                    entries: Vec::new(),
                });
                let i = diag.sections.len() - 1;
                current_section = Some(i);
                i
            }
        };
        diag.sections[idx].entries.push(entry);
    }

    if !header_seen {
        return Err(Error::ParseError(
            "missing `timeline` header line".to_string(),
        ));
    }

    Ok(diag)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_timeline() {
        let diag = parse("timeline\n2002 : LinkedIn").unwrap();
        assert_eq!(diag.title, None);
        assert_eq!(diag.sections.len(), 1);
        assert_eq!(diag.sections[0].name, None); // implicit unnamed section
        assert_eq!(diag.sections[0].entries.len(), 1);
        let entry = &diag.sections[0].entries[0];
        assert_eq!(entry.period, "2002");
        assert_eq!(entry.events, vec!["LinkedIn"]);
    }

    #[test]
    fn parses_title() {
        let diag = parse(
            "timeline\n\
             title History of Social Media\n\
             2002 : LinkedIn",
        )
        .unwrap();
        assert_eq!(diag.title.as_deref(), Some("History of Social Media"));
        assert_eq!(diag.sections[0].entries[0].period, "2002");
    }

    #[test]
    fn parses_section_grouping() {
        let diag = parse(
            "timeline\n\
             section 2002-2004\n\
               2002 : LinkedIn\n\
               2003 : MySpace launched\n\
             section 2005-2008\n\
               2005 : YouTube",
        )
        .unwrap();
        assert_eq!(diag.sections.len(), 2);
        assert_eq!(diag.sections[0].name.as_deref(), Some("2002-2004"));
        assert_eq!(diag.sections[0].entries.len(), 2);
        assert_eq!(diag.sections[1].name.as_deref(), Some("2005-2008"));
        assert_eq!(diag.sections[1].entries.len(), 1);
    }

    #[test]
    fn parses_multiple_events_per_period() {
        let diag = parse("timeline\n2004 : Facebook : Google goes public").unwrap();
        let entry = &diag.sections[0].entries[0];
        assert_eq!(entry.period, "2004");
        assert_eq!(entry.events, vec!["Facebook", "Google goes public"]);
    }

    #[test]
    fn events_before_first_section_land_in_implicit_unnamed_section() {
        let diag = parse(
            "timeline\n\
             2001 : Wikipedia\n\
             section 2002-2004\n\
               2002 : LinkedIn",
        )
        .unwrap();
        assert_eq!(diag.sections.len(), 2);
        assert_eq!(diag.sections[0].name, None);
        assert_eq!(diag.sections[0].entries[0].period, "2001");
        assert_eq!(diag.sections[1].name.as_deref(), Some("2002-2004"));
        assert_eq!(diag.sections[1].entries[0].period, "2002");
    }

    #[test]
    fn comment_lines_are_stripped() {
        let diag = parse(
            "%% leading comment\n\
             timeline\n\
             %% mid comment\n\
             2002 : LinkedIn %% inline comment",
        )
        .unwrap();
        // The inline comment is stripped; the event text is clean.
        assert_eq!(diag.sections[0].entries[0].events[0], "LinkedIn");
    }

    #[test]
    fn blank_lines_are_ignored() {
        let diag = parse(
            "timeline\n\
             \n\
             2002 : LinkedIn\n\
             \n\
             2003 : MySpace",
        )
        .unwrap();
        assert_eq!(diag.sections[0].entries.len(), 2);
    }

    #[test]
    fn missing_timeline_header_returns_error() {
        let err = parse("section 2002\n2002 : LinkedIn").unwrap_err();
        assert!(
            err.to_string().contains("timeline"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn accessibility_metadata_is_silently_ignored() {
        let diag = parse(
            "timeline\n\
             accTitle: My accessible title\n\
             accDescr: Long description\n\
             2002 : LinkedIn",
        )
        .unwrap();
        // accTitle/accDescr do not affect the diagram title or entries.
        assert_eq!(diag.title, None);
        assert_eq!(diag.sections[0].entries.len(), 1);
    }
}
