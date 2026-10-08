//! Regression corpus snapshot harness.
//!
//! This test suite walks every `.mmd` file under
//! `tests/regression_corpus/sources/`, renders it twice (unconstrained and
//! width-constrained to 80 columns), and compares each result against a
//! committed snapshot under `tests/regression_corpus/snapshots/`.
//!
//! # Why it exists
//!
//! Bugs B3, B9, and B12 all live in the routing-attach path that was
//! stabilised across three iterations in 1.22.x. Before any surgeon touches
//! that code, this harness must be green. Any post-surgery diff surfaces
//! immediately via `cargo insta review`, where a human classifies each change
//! as Improvement / Neutral / Regression.
//!
//! # Running
//!
//! ```shell
//! cargo test --test regression_corpus
//! ```
//!
//! # Updating snapshots after an intentional change
//!
//! ```shell
//! INSTA_UPDATE=always cargo test --test regression_corpus
//! cargo insta review
//! ```

// Insta manages snapshots; it may generate review artifacts that clippy
// would otherwise flag as unused imports.
#![allow(clippy::items_after_test_module)]

use std::fs;
use std::path::Path;

use insta::assert_snapshot;

/// Render a `.mmd` source file and snapshot both the natural-size and the
/// 80-column-constrained outputs.
///
/// The test name (used as the insta snapshot key) is derived from the
/// filename stem so it is stable across machine and OS.
fn render_and_snapshot(path: &Path) {
    let stem = path
        .file_stem()
        .expect("path has stem")
        .to_str()
        .expect("stem is valid UTF-8");

    let src = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read corpus source {}: {e}", path.display()));

    // Natural-size render — no width constraint.
    let natural =
        mermaid_text::render(&src).unwrap_or_else(|e| panic!("render failed for {stem}: {e}"));

    // Width-constrained render — 80 columns. Bug B3 only manifests here.
    let constrained = mermaid_text::render_with_width(&src, Some(80))
        .unwrap_or_else(|e| panic!("render_with_width(80) failed for {stem}: {e}"));

    // Snapshot names must be static string literals for insta's macro, so we
    // use the `with_settings!` + `assert_snapshot!` pattern that insta
    // supports for dynamic snapshot names via the `snapshot_name` setting.
    //
    // We override the snapshot path so the corpus snapshots are isolated
    // under `tests/regression_corpus/snapshots/` rather than mixed into the
    // main `tests/snapshots/` directory.
    insta::with_settings!({
        snapshot_path => "regression_corpus/snapshots",
        prepend_module_to_snapshot => false,
    }, {
        assert_snapshot!(stem, natural);
    });

    let width_key = format!("{stem}.width80");
    insta::with_settings!({
        snapshot_path => "regression_corpus/snapshots",
        prepend_module_to_snapshot => false,
    }, {
        assert_snapshot!(width_key, constrained);
    });
}

/// Walk the corpus sources directory in deterministic alphabetical order and
/// snapshot-test every `.mmd` file.
///
/// Alphabetical ordering is critical: it makes test output and snapshot diffs
/// reproducible regardless of filesystem traversal order (which varies across
/// OS and hardware).
#[test]
fn corpus_snapshot_all() {
    // Locate the sources directory relative to the manifest so the test works
    // from any working directory, including `cargo test --workspace`.
    //
    // `CARGO_MANIFEST_DIR` is set by Cargo to the crate root at compile time,
    // which is the stable anchor for locating test fixtures.
    let sources_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("regression_corpus")
        .join("sources");

    let mut entries: Vec<_> = fs::read_dir(&sources_dir)
        .unwrap_or_else(|e| {
            panic!(
                "cannot read corpus sources directory {}: {e}",
                sources_dir.display()
            )
        })
        .filter_map(|entry| {
            let entry = entry.expect("read_dir entry");
            let path = entry.path();
            // Only process `.mmd` files.
            if path.extension().and_then(|s| s.to_str()) == Some("mmd") {
                Some(path)
            } else {
                None
            }
        })
        .collect();

    // Sort alphabetically by filename for deterministic ordering.
    // Using the full path sort is equivalent to filename sort when all files
    // share the same parent directory.
    entries.sort();

    assert!(
        !entries.is_empty(),
        "no .mmd files found in {}",
        sources_dir.display()
    );

    for path in &entries {
        render_and_snapshot(path);
    }
}
