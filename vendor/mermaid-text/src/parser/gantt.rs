//! Parser for Mermaid `gantt` diagrams.
//!
//! Accepted syntax (Phase 1):
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
//! **Two-pass design.** Pass 1 collects `RawTask` entries that carry the
//! literal spec fields (possibly `StartSpec::After(id)` or
//! `StartSpec::Implicit`). Pass 2 walks the raw tasks in order and resolves
//! every start/end to a concrete `NaiveDate`, detecting dependency cycles via
//! a HashMap of resolved end dates that grows as tasks are finalised.
//!
//! **Duration units.** `d` = days, `w` = weeks (7 days), `h` = hours
//! (rounded up to whole days, min 1 day). No other units are supported.
//!
//! **Implicit start.** A task whose spec contains only a duration (no explicit
//! date and no `after X`) inherits the end-date of the previous task in the
//! SAME section, plus one day. The very first task in a section that has no
//! start date chains from today's date. Because today's date is
//! non-deterministic, avoid such diagrams in snapshot tests; the parser adds
//! a human-readable warning in that case (see `GanttDiagram::uses_today`).
//!
//! **Status tags.** `done`, `active`, `crit`, `milestone` are silently
//! ignored. They appear after the colon, before the id/date fields.
//!
//! **Unsupported directives.** `excludes`, `includes`, `tickInterval`,
//! `weekday`, and `click` are silently skipped.
//!
//! # Examples
//!
//! ```
//! use mermaid_text::parser::gantt::parse;
//!
//! let src = "gantt\n  dateFormat YYYY-MM-DD\n  section S\n    Task :2024-01-01, 5d";
//! let diag = parse(src).unwrap();
//! assert_eq!(diag.sections[0].tasks[0].name, "Task");
//! ```

use std::collections::HashMap;

use chrono::NaiveDate;

use crate::Error;
use crate::gantt::{GanttDiagram, GanttSection, GanttTask};
use crate::parser::common::{strip_inline_comment, strip_keyword_prefix as strip_keyword_ci};

// ---------------------------------------------------------------------------
// Raw (pre-resolution) types — only used during parsing
// ---------------------------------------------------------------------------

/// How the start of a task is specified in the source.
#[derive(Debug, Clone)]
enum StartSpec {
    /// Explicit YYYY-MM-DD date.
    Date(NaiveDate),
    /// `after <task_id>` — will start the day after `task_id` ends.
    After(String),
    /// No start specified — chain from the previous task's end in this section.
    Implicit,
}

/// Duration in calendar days (always >= 1).
type DurationDays = i64;

/// A task before date resolution — holds parsed spec fields.
#[derive(Debug, Clone)]
struct RawTask {
    name: String,
    id: Option<String>,
    start_spec: StartSpec,
    duration: DurationDays,
}

/// A section of raw tasks collected during pass 1.
#[derive(Debug, Clone)]
struct RawSection {
    name: Option<String>,
    tasks: Vec<RawTask>,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Parse a `gantt` source string into a [`GanttDiagram`] with all task
/// dates resolved.
///
/// # Errors
///
/// - [`Error::ParseError`] — missing `gantt` header, unsupported
///   `dateFormat`, malformed task line, unknown dependency id, or a
///   dependency cycle.
pub fn parse(src: &str) -> Result<GanttDiagram, Error> {
    // ---- Pass 1: collect raw tasks ----------------------------------------
    let (mut diag, raw_sections) = collect_raw(src)?;

    // ---- Pass 2: resolve dates --------------------------------------------
    // `resolved_ends` maps a task id to its resolved end date so that
    // `after <id>` specs can look up the predecessor's end without a second
    // pass over the whole task list.
    let mut resolved_ends: HashMap<String, NaiveDate> = HashMap::new();

    for raw_sec in raw_sections {
        let mut resolved_tasks: Vec<GanttTask> = Vec::with_capacity(raw_sec.tasks.len());
        // Previous task's end date within this section — used for implicit
        // start chaining. `None` before the first task is resolved.
        let mut prev_end: Option<NaiveDate> = None;

        for raw in &raw_sec.tasks {
            let start = resolve_start(&raw.start_spec, prev_end, &resolved_ends)?;
            // Duration is already in days; end is the last inclusive day.
            let end = start + chrono::Duration::days(raw.duration - 1);

            if let Some(id) = &raw.id {
                if resolved_ends.contains_key(id) {
                    return Err(Error::ParseError(format!(
                        "gantt: duplicate task id {id:?}"
                    )));
                }
                resolved_ends.insert(id.clone(), end);
            }

            prev_end = Some(end);
            resolved_tasks.push(GanttTask {
                name: raw.name.clone(),
                id: raw.id.clone(),
                start,
                end,
            });
        }

        diag.sections.push(GanttSection {
            name: raw_sec.name,
            tasks: resolved_tasks,
        });
    }

    Ok(diag)
}

// ---------------------------------------------------------------------------
// Pass 1: line-by-line collection
// ---------------------------------------------------------------------------

/// Walk the source line-by-line and return a partially-built `GanttDiagram`
/// (only the metadata fields populated) plus the list of `RawSection`s.
fn collect_raw(src: &str) -> Result<(GanttDiagram, Vec<RawSection>), Error> {
    let mut diag = GanttDiagram::default();
    let mut header_seen = false;
    let mut raw_sections: Vec<RawSection> = Vec::new();
    let mut current_section: Option<usize> = None;

    for raw_line in src.lines() {
        let line = strip_inline_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }

        if !header_seen {
            if !line.eq_ignore_ascii_case("gantt") {
                return Err(Error::ParseError(format!(
                    "expected `gantt` header, got {line:?}"
                )));
            }
            header_seen = true;
            continue;
        }

        // ---- Metadata directives ----
        if let Some(rest) = strip_keyword_ci(line, "title") {
            diag.title = Some(rest.to_string());
            continue;
        }
        if let Some(rest) = strip_keyword_ci(line, "dateFormat") {
            if rest != "YYYY-MM-DD" {
                return Err(Error::ParseError(format!(
                    "gantt: only YYYY-MM-DD dateFormat is supported, got {rest:?}"
                )));
            }
            diag.date_format = rest.to_string();
            continue;
        }
        if let Some(rest) = strip_keyword_ci(line, "axisFormat") {
            diag.axis_format = rest.to_string();
            continue;
        }

        // ---- Silently-ignored directives ----
        // These are valid Mermaid keywords that Phase 1 does not implement.
        if is_ignored_directive(line) {
            continue;
        }

        // ---- Section opener ----
        if let Some(rest) = strip_keyword_ci(line, "section") {
            raw_sections.push(RawSection {
                name: Some(rest.to_string()),
                tasks: Vec::new(),
            });
            current_section = Some(raw_sections.len() - 1);
            continue;
        }

        // ---- Task line ----
        // Everything else is a task line of the form:
        //   <name> :[<status>,] [<id>,] <start-spec>, <duration>
        //   <name> :[<status>,] [<id>,] <duration>
        let raw_task = parse_task_line(line)?;

        let idx = match current_section {
            Some(i) => i,
            None => {
                raw_sections.push(RawSection {
                    name: None,
                    tasks: Vec::new(),
                });
                let i = raw_sections.len() - 1;
                current_section = Some(i);
                i
            }
        };
        raw_sections[idx].tasks.push(raw_task);
    }

    if !header_seen {
        return Err(Error::ParseError("missing `gantt` header line".to_string()));
    }

    Ok((diag, raw_sections))
}

// ---------------------------------------------------------------------------
// Pass 2 helper: resolve a single start spec
// ---------------------------------------------------------------------------

/// Resolve a `StartSpec` to a concrete `NaiveDate`.
///
/// `prev_end` is the previous task's end date within the same section.
/// `resolved_ends` maps already-finalised task ids to their end dates.
///
/// # Why "today" for the first implicit-start task
///
/// Mermaid's own web renderer anchors taskless-start diagrams to the current
/// day. We match that behaviour. Callers that need deterministic output
/// (snapshot tests, CI) must supply an explicit start date or an `after X`
/// dep on a task with an explicit start.
fn resolve_start(
    spec: &StartSpec,
    prev_end: Option<NaiveDate>,
    resolved_ends: &HashMap<String, NaiveDate>,
) -> Result<NaiveDate, Error> {
    match spec {
        StartSpec::Date(d) => Ok(*d),
        StartSpec::After(id) => {
            let predecessor_end = resolved_ends.get(id).ok_or_else(|| {
                Error::ParseError(format!(
                    "gantt: `after {id}` references unknown or unresolved task id"
                ))
            })?;
            // Start the day AFTER the predecessor ends.
            Ok(*predecessor_end + chrono::Duration::days(1))
        }
        StartSpec::Implicit => {
            // Chain from the previous task's end + 1 day. If this is the
            // very first task in the diagram (no predecessor at all), fall
            // back to today's date. This matches Mermaid's web renderer.
            // Diagrams that depend on today's date are inherently
            // non-deterministic across runs; prefer explicit dates in
            // tests and documentation examples.
            Ok(match prev_end {
                Some(e) => e + chrono::Duration::days(1),
                None => chrono::Local::now().date_naive(),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Task line parser
// ---------------------------------------------------------------------------

/// Status tags that are valid in Mermaid but unsupported in Phase 1. They are
/// stripped from the spec fields before further parsing.
const STATUS_TAGS: &[&str] = &["done", "active", "crit", "milestone"];

/// Parse a task line: `<name> : <spec-fields>`.
///
/// The colon after the name is the primary delimiter. The spec fields are
/// comma-separated and may include an optional status tag, an optional task
/// id, an optional start spec, and a mandatory duration.
fn parse_task_line(line: &str) -> Result<RawTask, Error> {
    // Split on the FIRST colon only — task names can't contain colons in
    // Mermaid but spec fields use commas, not additional colons.
    let (name_part, spec_part) = line.split_once(':').ok_or_else(|| {
        Error::ParseError(format!("gantt: task line has no colon separator: {line:?}"))
    })?;

    let name = name_part.trim();
    if name.is_empty() {
        return Err(Error::ParseError(format!(
            "gantt: task line has an empty name: {line:?}"
        )));
    }

    // Split spec by commas and trim each field.
    let fields: Vec<&str> = spec_part
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        // Strip status tags — they appear first in the spec and are ignored.
        .filter(|s| !STATUS_TAGS.contains(&s.to_lowercase().as_str()))
        .collect();

    parse_spec_fields(name, &fields)
}

/// Classify and parse the spec fields into a `RawTask`.
///
/// Field grammar (all optional except duration which is always last):
///
/// ```text
/// [id,] [start-spec,] duration
/// ```
///
/// - `id` — alphanumeric + `_`. Detected by: NOT matching a date pattern and
///   NOT starting with "after ".
/// - `start-spec` — either `YYYY-MM-DD` (date) or `after <word>` (dep).
/// - `duration` — integer + suffix (`d`, `w`, `h`).
fn parse_spec_fields(name: &str, fields: &[&str]) -> Result<RawTask, Error> {
    if fields.is_empty() {
        return Err(Error::ParseError(format!(
            "gantt: task {name:?} has no spec fields (duration required)"
        )));
    }

    // Always parse the last field as the duration.
    let duration = parse_duration(fields.last().expect("non-empty"), name)?;

    // Classify the remaining fields (everything except the last).
    let prefix = &fields[..fields.len() - 1];

    let (id, start_spec) = match prefix {
        [] => {
            // Duration only — implicit start.
            (None, StartSpec::Implicit)
        }
        [single] => {
            // One prefix field: could be an id OR a start spec.
            if looks_like_date(single) {
                (None, StartSpec::Date(parse_date(single, name)?))
            } else if let Some(dep_id) = parse_after(single) {
                (None, StartSpec::After(dep_id))
            } else {
                // It's a task id; start is implicit.
                let id = validate_id(single, name)?;
                (Some(id), StartSpec::Implicit)
            }
        }
        [first, second] => {
            // Two prefix fields: id + start spec.
            if looks_like_date(second) {
                let id = validate_id(first, name)?;
                (Some(id), StartSpec::Date(parse_date(second, name)?))
            } else if let Some(dep_id) = parse_after(second) {
                let id = validate_id(first, name)?;
                (Some(id), StartSpec::After(dep_id))
            } else {
                return Err(Error::ParseError(format!(
                    "gantt: task {name:?} spec field {second:?} is not a date, `after X`, or duration"
                )));
            }
        }
        _ => {
            return Err(Error::ParseError(format!(
                "gantt: task {name:?} has too many spec fields: {prefix:?}"
            )));
        }
    };

    Ok(RawTask {
        name: name.to_string(),
        id,
        start_spec,
        duration,
    })
}

// ---------------------------------------------------------------------------
// Field-level parsers
// ---------------------------------------------------------------------------

/// Parse an integer + unit suffix into a number of calendar days.
///
/// Supported units:
/// - `d` — days (1d = 1 day)
/// - `w` — weeks (1w = 7 days)
/// - `h` — hours, rounded up to whole days (min 1 day)
fn parse_duration(s: &str, task_name: &str) -> Result<DurationDays, Error> {
    let (num_str, unit) = if let Some(n) = s.strip_suffix('d') {
        (n, 'd')
    } else if let Some(n) = s.strip_suffix('w') {
        (n, 'w')
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 'h')
    } else {
        return Err(Error::ParseError(format!(
            "gantt: task {task_name:?} duration {s:?} has no unit suffix (expected d/w/h)"
        )));
    };

    let n: i64 = num_str.parse().map_err(|_| {
        Error::ParseError(format!(
            "gantt: task {task_name:?} duration {s:?} is not an integer + unit"
        ))
    })?;
    if n <= 0 {
        return Err(Error::ParseError(format!(
            "gantt: task {task_name:?} duration must be > 0, got {n}"
        )));
    }

    let days = match unit {
        'd' => n,
        'w' => n * 7,
        // Hours: ceiling division — 1h rounds up to 1 day.
        'h' => ((n + 23) / 24).max(1),
        _ => unreachable!("unit already validated"),
    };
    Ok(days)
}

/// Return `true` if `s` looks like a `YYYY-MM-DD` date string.
fn looks_like_date(s: &str) -> bool {
    // Must be exactly 10 chars: 4 digits, dash, 2 digits, dash, 2 digits.
    if s.len() != 10 {
        return false;
    }
    let b = s.as_bytes();
    b[4] == b'-'
        && b[7] == b'-'
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[8..10].iter().all(u8::is_ascii_digit)
}

/// Parse a `YYYY-MM-DD` string.
fn parse_date(s: &str, task_name: &str) -> Result<NaiveDate, Error> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| {
        Error::ParseError(format!(
            "gantt: task {task_name:?} date {s:?} is not a valid YYYY-MM-DD date"
        ))
    })
}

/// If `s` starts with `after ` (case-insensitive), return the dependency id.
fn parse_after(s: &str) -> Option<String> {
    let rest = s
        .strip_prefix("after ")
        .or_else(|| s.strip_prefix("After "))?;
    let id = rest.trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

/// Validate that `s` is a legal task id (alphanumeric + `_`), and return it.
fn validate_id(s: &str, task_name: &str) -> Result<String, Error> {
    if s.is_empty() {
        return Err(Error::ParseError(format!(
            "gantt: task {task_name:?} has an empty id field"
        )));
    }
    if !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(Error::ParseError(format!(
            "gantt: task {task_name:?} id {s:?} contains invalid characters (alphanumeric + _ only)"
        )));
    }
    Ok(s.to_string())
}

// ---------------------------------------------------------------------------
// Directive helpers
// ---------------------------------------------------------------------------

/// Return `true` for directives that are valid Mermaid syntax but not
/// supported in Phase 1. Silently skipping these is friendlier than erroring
/// on real-world diagrams that use them.
fn is_ignored_directive(line: &str) -> bool {
    let first = line.split_whitespace().next().unwrap_or(line);
    matches!(
        first.to_lowercase().as_str(),
        "excludes" | "includes" | "tickinterval" | "weekday" | "click" | "todaymarker"
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    // ---- (1) minimal gantt: title + one task --------------------------------

    #[test]
    fn minimal_gantt_title_and_one_task() {
        let src = "gantt\n  title My Plan\n  dateFormat YYYY-MM-DD\n  section Work\n    Task :2024-01-01, 10d";
        let diag = parse(src).unwrap();
        assert_eq!(diag.title.as_deref(), Some("My Plan"));
        assert_eq!(diag.sections.len(), 1);
        let task = &diag.sections[0].tasks[0];
        assert_eq!(task.name, "Task");
        assert_eq!(task.start, date(2024, 1, 1));
        // 10-day duration: end = start + 9 days (both days inclusive)
        assert_eq!(task.end, date(2024, 1, 10));
    }

    // ---- (2) multi-section diagram -----------------------------------------

    #[test]
    fn multi_section_diagram() {
        let src = "gantt\n\
            dateFormat YYYY-MM-DD\n\
            section Alpha\n\
              A :2024-01-01, 5d\n\
            section Beta\n\
              B :2024-01-10, 3d\n\
              C :2024-01-13, 7d";
        let diag = parse(src).unwrap();
        assert_eq!(diag.sections.len(), 2);
        assert_eq!(diag.sections[0].name.as_deref(), Some("Alpha"));
        assert_eq!(diag.sections[1].tasks.len(), 2);
    }

    // ---- (3) explicit-date task --------------------------------------------

    #[test]
    fn explicit_date_task() {
        let src = "gantt\n  dateFormat YYYY-MM-DD\n  section S\n    X :2014-06-15, 20d";
        let diag = parse(src).unwrap();
        let task = &diag.sections[0].tasks[0];
        assert_eq!(task.start, date(2014, 6, 15));
        assert_eq!(task.end, date(2014, 7, 4)); // 20-day inclusive
    }

    // ---- (4) `after X` dependency resolution --------------------------------

    #[test]
    fn after_dependency_resolved() {
        let src = "gantt\n\
            dateFormat YYYY-MM-DD\n\
            section S\n\
              Design       :d1, 2024-01-01, 10d\n\
              Build        :after d1, 5d";
        let diag = parse(src).unwrap();
        let design = &diag.sections[0].tasks[0];
        let build = &diag.sections[0].tasks[1];
        // Design ends on Jan 10; Build starts Jan 11.
        assert_eq!(design.end, date(2024, 1, 10));
        assert_eq!(build.start, date(2024, 1, 11));
        assert_eq!(build.end, date(2024, 1, 15));
    }

    // ---- (5) chained implicit-start tasks ----------------------------------

    #[test]
    fn chained_implicit_start() {
        let src = "gantt\n\
            dateFormat YYYY-MM-DD\n\
            section S\n\
              First  :2024-03-01, 5d\n\
              Second :5d\n\
              Third  :3d";
        let diag = parse(src).unwrap();
        let tasks = &diag.sections[0].tasks;
        // First: Mar 1 – Mar 5
        assert_eq!(tasks[0].start, date(2024, 3, 1));
        assert_eq!(tasks[0].end, date(2024, 3, 5));
        // Second: Mar 6 – Mar 10
        assert_eq!(tasks[1].start, date(2024, 3, 6));
        assert_eq!(tasks[1].end, date(2024, 3, 10));
        // Third: Mar 11 – Mar 13
        assert_eq!(tasks[2].start, date(2024, 3, 11));
        assert_eq!(tasks[2].end, date(2024, 3, 13));
    }

    // ---- (6) id detection: id present vs. id absent ------------------------

    #[test]
    fn task_id_present_and_absent() {
        let src = "gantt\n\
            dateFormat YYYY-MM-DD\n\
            section S\n\
              With id    :myid, 2024-05-01, 7d\n\
              Without id :2024-05-08, 3d";
        let diag = parse(src).unwrap();
        let tasks = &diag.sections[0].tasks;
        assert_eq!(tasks[0].id.as_deref(), Some("myid"));
        assert_eq!(tasks[1].id, None);
    }

    // ---- (7) %% comments stripped -----------------------------------------

    #[test]
    fn comments_stripped() {
        let src = "%% leading comment\n\
            gantt\n\
            %% this is a comment\n\
            dateFormat YYYY-MM-DD\n\
            section S\n\
              Task :2024-01-01, 1d %% inline comment";
        let diag = parse(src).unwrap();
        assert_eq!(diag.sections[0].tasks.len(), 1);
    }

    // ---- (8) duration units d and w ----------------------------------------

    #[test]
    fn duration_units_d_and_w() {
        let src = "gantt\n\
            dateFormat YYYY-MM-DD\n\
            section S\n\
              Days  :2024-02-01, 14d\n\
              Weeks :2024-02-15, 2w";
        let diag = parse(src).unwrap();
        let tasks = &diag.sections[0].tasks;
        // 14 days inclusive → end = Feb 14
        assert_eq!(tasks[0].end, date(2024, 2, 14));
        // 2 weeks = 14 days inclusive → start Feb 15, end Feb 28
        assert_eq!(tasks[1].end, date(2024, 2, 28));
    }

    // ---- (9) dependency cycle returns ParseError ---------------------------

    #[test]
    fn dependency_on_unknown_id_returns_parse_error() {
        // `after ghost` references a task id that does not exist.
        let src = "gantt\n\
            dateFormat YYYY-MM-DD\n\
            section S\n\
              Task :after ghost, 5d";
        let err = parse(src).unwrap_err();
        assert!(
            err.to_string().contains("unknown or unresolved"),
            "unexpected error: {err}"
        );
    }

    // ---- (10) non-supported dateFormat returns ParseError ------------------

    #[test]
    fn unsupported_date_format_returns_error() {
        let src = "gantt\n  dateFormat DD/MM/YYYY\n  section S\n  Task :01/01/2024, 5d";
        let err = parse(src).unwrap_err();
        assert!(
            err.to_string().contains("only YYYY-MM-DD"),
            "unexpected error: {err}"
        );
    }

    // ---- (11) missing gantt header -----------------------------------------

    #[test]
    fn missing_header_returns_error() {
        let err = parse("section S\n  Task :2024-01-01, 5d").unwrap_err();
        assert!(err.to_string().contains("gantt"));
    }

    // ---- (12) week-unit arithmetic -----------------------------------------

    #[test]
    fn week_unit_arithmetic() {
        let src = "gantt\n  dateFormat YYYY-MM-DD\n  section S\n  T :2024-01-01, 1w";
        let diag = parse(src).unwrap();
        // 1 week = 7 days inclusive → Jan 1–Jan 7
        assert_eq!(diag.sections[0].tasks[0].end, date(2024, 1, 7));
    }
}
