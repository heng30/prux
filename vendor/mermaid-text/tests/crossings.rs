//! Crossing-counter regression harness for A\* edge routing.
//!
//! For each flowchart fixture in the snapshot corpus, this file counts
//! cells whose character is a true cross junction (`┼` = UP+DOWN+LEFT+RIGHT,
//! or `╋` for the thick variant). Crossing counts are snapshotted so that
//! future routing changes that _increase_ crossings fail the test.
//!
//! The counts were established after Phase 4 (pure A\* routing) landed.
//! A regression is defined as `new_count > snapshotted_count`.
//!
//! To regenerate snapshots after an intentional improvement:
//!   INSTA_UPDATE=always cargo test -p mermaid-text --test crossings
//! then commit the updated `.snap` files.

use insta::assert_snapshot;

/// Count the number of cells in a rendered string that contain a true
/// cross junction glyph.
///
/// A "true cross" requires line segments in all four cardinal directions:
/// - `┼` — thin-line cross (direction-bit mask 0b1111 in `DIR_TO_CHAR`)
/// - `╋` — thick-line cross (direction-bit mask 0b1111 in `THICK_DIR_TO_CHAR`)
///
/// T-junctions (`├`, `┤`, `┬`, `┴`) are not counted; they are 3-way splits,
/// not crossings.
fn count_crossings(rendered: &str) -> usize {
    rendered.chars().filter(|&c| c == '┼' || c == '╋').count()
}

/// Format a crossing count as a snapshot string for insta.
///
/// Using a labelled format rather than a bare integer makes `.snap` files
/// self-documenting and easier to review in diffs.
fn fmt_count(name: &str, n: usize) -> String {
    format!("{name}: {n} crossing(s)")
}

// ---------------------------------------------------------------------------
// Fixtures — each fixture must correspond to a flowchart that goes through
// the A\* router (i.e. `graph` or `stateDiagram-v2` sources, not pie/ER/
// sequence which use their own renderers and never call route_all).
// ---------------------------------------------------------------------------

#[test]
fn crossings_simple_chain_lr() {
    let out = mermaid_text::render("graph LR; A-->B-->C").unwrap();
    let n = count_crossings(&out);
    assert_snapshot!(fmt_count("simple_chain_lr", n));
}

#[test]
fn crossings_simple_chain_td() {
    let out = mermaid_text::render("graph TD; A-->B-->C").unwrap();
    let n = count_crossings(&out);
    assert_snapshot!(fmt_count("simple_chain_td", n));
}

#[test]
fn crossings_diamond_with_branches() {
    let src = "graph TD
        A[Start]-->B{Ok?}
        B-->|Yes|C[Go]
        B-->|No|D[Stop]";
    let out = mermaid_text::render(src).unwrap();
    let n = count_crossings(&out);
    assert_snapshot!(fmt_count("diamond_with_branches", n));
}

#[test]
fn crossings_single_subgraph_lr() {
    let src = "graph LR
        subgraph SG[My Group]
            A-->B
        end
        B-->C";
    let out = mermaid_text::render(src).unwrap();
    let n = count_crossings(&out);
    assert_snapshot!(fmt_count("single_subgraph_lr", n));
}

#[test]
fn crossings_three_sibling_subgraphs_lr() {
    let src = "graph LR
        subgraph Alpha
            A1-->A2
        end
        subgraph Beta
            B1-->B2
        end
        subgraph Gamma
            G1-->G2
        end
        A2-->B1
        B2-->G1";
    let out = mermaid_text::render(src).unwrap();
    let n = count_crossings(&out);
    assert_snapshot!(fmt_count("three_sibling_subgraphs_lr", n));
}

#[test]
fn crossings_perpendicular_subgraph_direction() {
    let src = "graph LR
        subgraph Sub
            direction TD
            X-->Y-->Z
        end
        A-->Sub";
    let out = mermaid_text::render(src).unwrap();
    let n = count_crossings(&out);
    assert_snapshot!(fmt_count("perpendicular_subgraph_direction", n));
}

#[test]
fn crossings_crossing_edges_with_cross_junction() {
    // This fixture is specifically designed to produce cross junctions:
    // four edges arranged so two pairs must cross.
    let src = "graph LR
        A-->C
        B-->D
        A-->D
        B-->C";
    let out = mermaid_text::render(src).unwrap();
    let n = count_crossings(&out);
    assert_snapshot!(fmt_count("crossing_edges_with_cross_junction", n));
}

#[test]
fn crossings_back_edge_lr() {
    let out = mermaid_text::render("graph LR; A-->B-->C; C-->A").unwrap();
    let n = count_crossings(&out);
    assert_snapshot!(fmt_count("back_edge_lr", n));
}

#[test]
fn crossings_back_edge_td_cycle() {
    // Back-edge routes around the diagram exterior — zero interior crossings expected.
    let src = "graph TD
        A-->B
        B-->C
        C-->A";
    let out = mermaid_text::render(src).unwrap();
    let n = count_crossings(&out);
    assert_snapshot!(fmt_count(
        "back_edge_avoids_diagram_interior_in_td_cycle",
        n
    ));
}

#[test]
fn crossings_edge_crosses_subgraph_boundary() {
    let src = "graph LR
        subgraph Infra
            DB[(Postgres)]
            Cache[(Redis)]
        end
        subgraph Services
            API[API Server]
            Worker[Worker]
        end
        API-->DB
        API-->Cache
        Worker-->DB
        Worker-->Cache
        LB[Load Balancer]-->API
        LB-->Worker";
    let out = mermaid_text::render(src).unwrap();
    let n = count_crossings(&out);
    assert_snapshot!(fmt_count("edge_crosses_subgraph_boundary", n));
}

#[test]
fn crossings_supervisor_bidirectional_in_subgraph() {
    let src = "graph LR
    subgraph Supervisor
        direction TB
        F[Factory] -->|creates| W[Worker]
        W -->|panics| F
    end
    W -->|beat| HB[Heartbeat]
    HB --> WD[Watchdog]";
    let out = mermaid_text::render(src).unwrap();
    let n = count_crossings(&out);
    assert_snapshot!(fmt_count("supervisor_bidirectional_in_subgraph", n));
}

#[test]
fn crossings_architecture_sugiyama() {
    let src = "graph LR
    App --> DB[(PostgreSQL)]
    App --> Cache[(Redis)]
    App --> Queue[(RabbitMQ)]
    Queue --> Worker[Worker]
    Worker --> DB";
    let opts = mermaid_text::RenderOptions {
        backend: mermaid_text::layout::LayoutBackend::Sugiyama,
        ..Default::default()
    };
    let out = mermaid_text::render_with_options(src, &opts).unwrap();
    let n = count_crossings(&out);
    assert_snapshot!(fmt_count("architecture_diagram_with_sugiyama_backend", n));
}

#[test]
fn crossings_nested_subgraphs_td() {
    let src = "graph TD
        subgraph Outer
            subgraph Inner
                A-->B
            end
            B-->C
        end
        C-->D";
    let out = mermaid_text::render(src).unwrap();
    let n = count_crossings(&out);
    assert_snapshot!(fmt_count("nested_subgraphs_td", n));
}

#[test]
fn crossings_cylinder_in_flow() {
    let src = "graph LR
        A[App]-->DB[(Database)]-->B[Cache]-->C[Output]";
    let out = mermaid_text::render(src).unwrap();
    let n = count_crossings(&out);
    assert_snapshot!(fmt_count("cylinder_in_flow", n));
}

// ---------------------------------------------------------------------------
// Dense-graph fixtures (Step 8) — 6+ nodes per layer, fan-in/fan-out,
// back-edge cases, and self-loop cases added for maximum router stress.
// ---------------------------------------------------------------------------

// Dense fan-out: one source feeding six targets — wide LR layout stress test.
#[test]
fn crossings_dense_fan_out() {
    let src = "graph LR
        Src-->T1
        Src-->T2
        Src-->T3
        Src-->T4
        Src-->T5
        Src-->T6";
    let out = mermaid_text::render(src).unwrap();
    let n = count_crossings(&out);
    assert_snapshot!(fmt_count("dense_fan_out", n));
}

// Dense fan-in: six sources converging to one sink.
#[test]
fn crossings_dense_fan_in() {
    let src = "graph LR
        S1-->Sink
        S2-->Sink
        S3-->Sink
        S4-->Sink
        S5-->Sink
        S6-->Sink";
    let out = mermaid_text::render(src).unwrap();
    let n = count_crossings(&out);
    assert_snapshot!(fmt_count("dense_fan_in", n));
}

// Dense fully-connected bipartite graph: 3 left nodes each connected to
// 3 right nodes — forces the router to handle 9 edges in a tight column.
#[test]
fn crossings_dense_bipartite() {
    let src = "graph LR
        L1-->R1
        L1-->R2
        L1-->R3
        L2-->R1
        L2-->R2
        L2-->R3
        L3-->R1
        L3-->R2
        L3-->R3";
    let out = mermaid_text::render(src).unwrap();
    let n = count_crossings(&out);
    assert_snapshot!(fmt_count("dense_bipartite", n));
}

// Dense graph with multiple back-edges: A→B→C→D→E + back-edges D→B and E→A.
// Tests that the perimeter router handles multiple simultaneous back-edges
// without crossings in the interior.
#[test]
fn crossings_dense_multiple_back_edges() {
    let src = "graph LR
        A-->B-->C-->D-->E
        D-->B
        E-->A";
    let out = mermaid_text::render(src).unwrap();
    let n = count_crossings(&out);
    assert_snapshot!(fmt_count("dense_multiple_back_edges", n));
}

// Dense TD graph: two-layer pipeline with 4 sources and 4 sinks arranged
// so some crossing is unavoidable (topological crossing number > 0).
#[test]
fn crossings_dense_td_crossing() {
    let src = "graph TD
        A-->W
        A-->X
        B-->W
        B-->X
        B-->Y
        C-->X
        C-->Y
        C-->Z
        D-->Y
        D-->Z";
    let out = mermaid_text::render(src).unwrap();
    let n = count_crossings(&out);
    assert_snapshot!(fmt_count("dense_td_crossing", n));
}
