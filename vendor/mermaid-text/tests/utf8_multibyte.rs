//! Regression tests for issue #29 — byte offsets used where char offsets are
//! expected, causing panics and silent label corruption on multi-byte
//! (non-ASCII) diagram text.
//!
//! https://github.com/leboiko/markdown-reader/issues/29

/// Bug 1 — sequence/state parsers must not panic on non-ASCII keyword-prefixed
/// lines. `strip_keyword_prefix` sliced at `keyword.len()` (a byte index)
/// without a char-boundary check, so any line whose byte at that index landed
/// inside a multi-byte char panicked.
#[test]
fn sequence_cyrillic_participant_does_not_panic() {
    // Byte 8 of "Сервер->>Сервер: ok" lands inside a Cyrillic char while the
    // parser probes the 8-byte keyword "activate".
    let out =
        mermaid_text::render("sequenceDiagram\n    participant Сервер\n    Сервер->>Сервер: ok\n")
            .expect("Cyrillic sequence diagram must render, not error");
    assert!(
        out.contains("Сервер"),
        "participant label must survive:\n{out}"
    );
}

#[test]
fn sequence_cyrillic_loop_and_alt_do_not_panic() {
    // Each of these lands a keyword-probe byte index inside a multi-byte char.
    for src in [
        "sequenceDiagram\n    participant A\n    loop до готовности\n    A->>A: x\n    end\n",
        "sequenceDiagram\n    participant A\n    alt если готов\n    A->>A: x\n    else иначе\n    A->>A: y\n    end\n",
        "sequenceDiagram\n    participant Оркестратор\n    Оркестратор->>Оркестратор: x\n",
    ] {
        let r = std::panic::catch_unwind(|| mermaid_text::render(src));
        assert!(r.is_ok(), "render panicked on non-ASCII input:\n{src}");
        assert!(
            r.unwrap().is_ok(),
            "render errored on valid non-ASCII input:\n{src}"
        );
    }
}

/// Bug 2 — a multi-byte `|label|` edge label must not corrupt the *following*
/// node. `try_consume_pipe_label` returned a byte length that the char-indexed
/// tokenizer added to its cursor, over-advancing by `byte_len - char_len` and
/// eating part of the next node token (leaking its `[` bracket into the label).
#[test]
fn flowchart_cyrillic_edge_label_does_not_corrupt_next_node() {
    // Single space after the label: the pre-fix over-advance of 2 (the byte/char
    // delta of "да") eats " B", leaving "[End]" as the node token so a literal
    // bracket leaks. A correct render shows "End" boxed with NO stray bracket.
    let out = mermaid_text::render("flowchart LR\n    A[Start] -->|да| B[End]\n")
        .expect("flowchart must render");
    assert!(
        out.contains("End"),
        "second node label must be present:\n{out}"
    );
    assert!(
        out.contains("Start"),
        "first node label must be present:\n{out}"
    );
    assert!(out.contains("да"), "edge label must be present:\n{out}");
    assert!(
        !out.contains('[') && !out.contains(']'),
        "no raw bracket must leak into the rendered diagram:\n{out}"
    );
}

#[test]
fn flowchart_multi_node_cyrillic_keeps_all_nodes() {
    // From the issue: the 4th node ("Форма входа") was dropped entirely and a
    // stray "Форма входа]" was drawn. Assert all four labels survive intact and
    // no bracket leaks.
    let src = "flowchart TD\n    A[Пользователь] --> B{Есть токен?}\n    B -->|Да| C[Доступ разрешён]\n    B -->|Нет| D[Форма входа]\n";
    let out = mermaid_text::render(src).expect("flowchart must render");
    for label in [
        "Пользователь",
        "Есть токен?",
        "Доступ разрешён",
        "Форма входа",
    ] {
        assert!(
            out.contains(label),
            "label {label:?} must render intact:\n{out}"
        );
    }
    assert!(!out.contains('['), "no raw '[' bracket must leak:\n{out}");
    assert!(!out.contains(']'), "no raw ']' bracket must leak:\n{out}");
}

/// Bug 2b — same byte-as-char defect in the compact inline arrow form
/// (`A -. label .-> B` / `A == label ==> B`).
#[test]
fn flowchart_compact_inline_cyrillic_label_does_not_corrupt_next_node() {
    // Compact inline form `-.label.->`. Bracketed target node so the pre-fix
    // over-advance leaks a '[' the same way as the pipe-label case.
    let out = mermaid_text::render("flowchart LR\n    A[Start] -.да.-> B[End]\n")
        .expect("flowchart must render");
    assert!(
        out.contains("да"),
        "inline compact label must be present:\n{out}"
    );
    assert!(
        out.contains("End"),
        "target node label must survive:\n{out}"
    );
    assert!(
        !out.contains('[') && !out.contains(']'),
        "no raw bracket must leak into the rendered diagram:\n{out}"
    );
}

/// Bug 2c — the inline *quoted* arrow form (`-. "label" .->` / `== "label" ==>`)
/// had the same defect: it measured the consumed slice (which includes the
/// multi-byte quoted label) with `s.len() - tail.len()` — a byte length used as
/// a char advance. Not in the original report; found while auditing the sibling
/// consumers.
#[test]
fn flowchart_inline_quoted_cyrillic_label_does_not_corrupt_next_node() {
    let out = mermaid_text::render("flowchart LR\n    A[Start] -. \"да\" .-> B[End]\n")
        .expect("flowchart must render");
    assert!(
        out.contains("да"),
        "quoted inline label must be present:\n{out}"
    );
    assert!(
        out.contains("End"),
        "target node label must survive:\n{out}"
    );
    assert!(
        !out.contains('[') && !out.contains(']'),
        "no raw bracket must leak into the rendered diagram:\n{out}"
    );
}

// ---- Audit findings (same byte-vs-char class, found sweeping other parsers) --

/// Audit F1 — sequence participant `<Id> as <Alias>`. `parse_participant_decl`
/// searched for " as " in a `to_lowercase()` copy of the line, then applied that
/// byte offset to the ORIGINAL. When case-folding changes byte length (e.g.
/// `İ` U+0130 lowercases to a 3-byte `i̇`), the offset diverges and the slice
/// panics or mis-splits.
#[test]
fn sequence_participant_case_fold_alias_does_not_panic() {
    for src in [
        "sequenceDiagram\n    participant İd as Сервер\n    İd->>İd: ok\n",
        "sequenceDiagram\n    participant Straße as Backend\n    Straße->>Straße: ok\n",
    ] {
        let r = std::panic::catch_unwind(|| mermaid_text::render(src));
        assert!(
            r.is_ok(),
            "render panicked on case-folding participant:\n{src}"
        );
        assert!(r.unwrap().is_ok(), "render errored:\n{src}");
    }
    // And the alias must be split correctly, not corrupted by a shifted offset.
    let out =
        mermaid_text::render("sequenceDiagram\n    participant İd as Сервер\n    İd->>İd: ok\n")
            .expect("must render");
    assert!(out.contains("Сервер"), "alias label must be intact:\n{out}");
}

/// Audit F2 — gantt/timeline/journey each had a local copy of the keyword-strip
/// helper still slicing at a byte index (`line[..keyword.len()]`), so a line
/// whose byte at that index falls mid-char panics. Exercise each parser with
/// non-ASCII lines that land a multi-byte char across a probed keyword length.
#[test]
fn gantt_non_ascii_does_not_panic() {
    let src =
        "gantt\n    title Планирование\n    section Этап\n    Задача разработки: 2026-01-01, 3d\n";
    let r = std::panic::catch_unwind(|| mermaid_text::render(src));
    assert!(r.is_ok(), "gantt render panicked on non-ASCII:\n{src}");
    assert!(r.unwrap().is_ok(), "gantt render errored:\n{src}");
}

#[test]
fn timeline_non_ascii_does_not_panic() {
    let src = "timeline\n    title История\n    section Эпоха\n    Событие первое\n";
    let r = std::panic::catch_unwind(|| mermaid_text::render(src));
    assert!(r.is_ok(), "timeline render panicked on non-ASCII:\n{src}");
    assert!(r.unwrap().is_ok(), "timeline render errored:\n{src}");
}

#[test]
fn journey_non_ascii_does_not_panic() {
    let src = "journey\n    title Мой день\n    section Утро\n      Проснуться: 3: Я\n";
    let r = std::panic::catch_unwind(|| mermaid_text::render(src));
    assert!(r.is_ok(), "journey render panicked on non-ASCII:\n{src}");
    assert!(r.unwrap().is_ok(), "journey render errored:\n{src}");
}
