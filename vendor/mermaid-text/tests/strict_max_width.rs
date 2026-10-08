//! Tests for the opt-in hard `max_width` budget (issue #32).
//!
//! `RenderOptions::max_width_strict` turns `max_width` from a soft hint into a
//! hard budget: a render whose widest line still exceeds the budget returns
//! `Error::TooWide { requested, actual }` instead of an over-wide string.
//!
//! https://github.com/leboiko/markdown-reader/issues/32

use mermaid_text::{Error, RenderOptions, render_with_options};
use unicode_width::UnicodeWidthStr;

/// Widest line of a render, in display columns — the ground-truth measure the
/// assertions compare against (mirrors what the crate does internally). The
/// non-colored renders used here contain no ANSI escapes, so this is exact.
fn widest(s: &str) -> usize {
    s.lines().map(UnicodeWidthStr::width).max().unwrap_or(0)
}

fn opts(max_width: Option<usize>, strict: bool) -> RenderOptions {
    RenderOptions {
        max_width,
        max_width_strict: strict,
        ..Default::default()
    }
}

// A sequence diagram: its layout ignores `max_width` entirely, so its natural
// width is a stable, comfortably-wide value to test the budget against.
const SEQ: &str = "sequenceDiagram\n    participant Alice\n    participant Bob\n    Alice->>Bob: hello there\n    Bob-->>Alice: hi back\n";

#[test]
fn strict_over_budget_returns_too_wide_with_exact_values() {
    // Measure the natural width, then demand one column less under strict mode.
    let natural = widest(&render_with_options(SEQ, &opts(None, false)).unwrap());
    let budget = natural - 1;

    let err = render_with_options(SEQ, &opts(Some(budget), true)).unwrap_err();
    assert_eq!(
        err,
        Error::TooWide {
            requested: budget,
            actual: natural
        },
        "must report the exact requested budget and the true widest width"
    );
}

#[test]
fn strict_within_budget_returns_ok() {
    let natural = widest(&render_with_options(SEQ, &opts(None, false)).unwrap());
    // A budget the render already fits within must pass strict mode untouched.
    let out = render_with_options(SEQ, &opts(Some(natural + 20), true)).unwrap();
    assert!(widest(&out) <= natural + 20);
    assert!(out.contains("Alice") && out.contains("Bob"));
}

#[test]
fn strict_exactly_at_budget_is_ok() {
    // Boundary: widest == budget must NOT error (only strictly-greater does).
    let natural = widest(&render_with_options(SEQ, &opts(None, false)).unwrap());
    let out = render_with_options(SEQ, &opts(Some(natural), true)).expect("== budget must pass");
    assert_eq!(widest(&out), natural);
}

#[test]
fn non_strict_over_budget_still_returns_the_wide_string() {
    // Default behaviour must be unchanged: an over-wide render is returned as a
    // string, never an error, when strict mode is off.
    let natural = widest(&render_with_options(SEQ, &opts(None, false)).unwrap());
    let out = render_with_options(SEQ, &opts(Some(natural - 5), false))
        .expect("soft mode must never error on overflow");
    assert_eq!(
        widest(&out),
        natural,
        "soft mode leaves the render untouched"
    );
}

#[test]
fn strict_with_no_budget_is_a_noop() {
    // strict = true but max_width = None: nothing to enforce, must be Ok.
    let out = render_with_options(SEQ, &opts(None, true)).expect("no budget → no enforcement");
    assert!(out.contains("Alice"));
}

#[test]
fn strict_measures_display_columns_not_ansi_bytes() {
    // With color on, the render carries ANSI escape bytes. The budget check must
    // measure *display columns*, ignoring escapes — otherwise a diagram that
    // visually fits would be falsely rejected.
    let src = "flowchart LR\n    A[Start] --> B[End]\n    style A fill:#336,color:#fff\n";
    let colored = RenderOptions {
        color: true,
        ..Default::default()
    };
    let out = render_with_options(src, &colored).unwrap();
    assert!(
        out.contains("\x1b["),
        "sanity: color mode must emit ANSI escapes"
    );

    // The visible width fits this budget, but the raw byte length is far larger
    // (escapes inflate it). A naive byte/char measure would wrongly trip here.
    let visible = out
        .lines()
        .map(|l| {
            // strip a superset of escape-ish bytes for the ground-truth compare
            UnicodeWidthStr::width(l.replace('\u{1b}', "").as_str())
        })
        .max()
        .unwrap_or(0);
    assert!(
        out.len() > visible,
        "sanity: escape bytes make the raw string longer than its display width"
    );

    let strict = RenderOptions {
        color: true,
        max_width: Some(visible),
        max_width_strict: true,
        ..Default::default()
    };
    render_with_options(src, &strict)
        .expect("a render whose *display* width fits must pass strict mode even with color on");
}

#[test]
fn strict_measures_display_width_of_multibyte_labels() {
    // Cyrillic labels: display width (1 col/char) is far below the byte length.
    // The budget check must use display columns, so a diagram that visually
    // fits its budget must not be rejected because its UTF-8 bytes exceed it.
    let src = "flowchart LR\n    A[Разработка] --> B[Готово]\n";
    let out = render_with_options(src, &opts(None, false)).unwrap();
    let visible = widest(&out);
    assert!(
        out.len() > visible,
        "sanity: multibyte labels make bytes exceed display width"
    );
    // Budget == visible width must pass (would fail if measured in bytes).
    render_with_options(src, &opts(Some(visible), true))
        .expect("multibyte diagram fitting its display budget must pass strict mode");
}

#[test]
fn strict_flowchart_too_narrow_for_min_layout_returns_too_wide() {
    // Flowchart DOES honour max_width (progressive compaction), but a node box
    // can't shrink below its label. With a budget far under the label width,
    // even the tightest layout overflows — the real case from #29/#32 (a
    // classDiagram at 203 cols vs a Some(90) budget). Strict mode must surface
    // TooWide rather than a silently-over-wide string.
    let src = "flowchart LR\n    A[a node label that is far too wide to fit] --> B[end]\n";
    let budget = 10;
    match render_with_options(src, &opts(Some(budget), true)) {
        Err(Error::TooWide { requested, actual }) => {
            assert_eq!(requested, budget);
            assert!(
                actual > budget,
                "actual {actual} must exceed budget {budget}"
            );
        }
        other => panic!("expected TooWide, got {other:?}"),
    }
    // Same input in soft mode returns the (over-wide) render, unchanged.
    let soft = render_with_options(src, &opts(Some(budget), false)).unwrap();
    assert!(widest(&soft) > budget);
}

#[test]
fn too_wide_error_message_names_both_widths() {
    let err = Error::TooWide {
        requested: 40,
        actual: 57,
    };
    let msg = err.to_string();
    assert!(msg.contains("57") && msg.contains("40"), "message: {msg}");
}
