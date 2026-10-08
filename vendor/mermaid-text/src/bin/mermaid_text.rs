//! CLI for `mermaid-text`.
//!
//! Reads Mermaid source from stdin or a file path argument and prints the
//! rendered diagram to stdout.  Unicode box-drawing mode is the default;
//! pass `--ascii` to emit plain ASCII characters instead (useful on legacy
//! terminals or in CI logs that strip non-ASCII bytes).
//!
//! The default layout backend is Sugiyama (`ascii-dag`-backed crossing
//! minimisation) since 0.17.0. Pass `--native` to use the previous in-house
//! layered layout. `--sugiyama` is retained as a no-op for backward compat.
//!
//! # Usage
//!
//! ```text
//! # From a file:
//! mermaid-text diagram.mmd
//!
//! # From stdin:
//! echo "graph LR; A-->B-->C" | mermaid-text
//!
//! # With a column budget:
//! mermaid-text --width 80 diagram.mmd
//!
//! # ASCII-only output (no Unicode box-drawing):
//! echo "graph LR; A-->B-->C" | mermaid-text --ascii
//! mermaid-text --ascii --width 60 diagram.mmd
//!
//! # ANSI 24-bit color (honours `style` / `linkStyle` directives):
//! mermaid-text --color diagram.mmd
//! mermaid-text --color --ascii diagram.mmd      # composes with --ascii
//!
//! # Use the legacy in-house layered layout:
//! mermaid-text --native diagram.mmd
//! ```

use std::io::Read;
use std::process;

fn main() {
    let mut args = std::env::args().skip(1).peekable();

    let mut max_width: Option<usize> = None;
    let mut strict_width = false;
    let mut ascii_mode = false;
    let mut color_mode = false;
    // Default is Sugiyama since 0.17.0. `--native` reverts to the in-house
    // layered pipeline; `--sugiyama` is a no-op kept for backward compat.
    let mut backend_mode = mermaid_text::layout::LayoutBackend::default();
    let mut path: Option<String> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--width" | "-w" => {
                let n = args
                    .next()
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or_else(|| {
                        eprintln!("error: --width requires a positive integer argument");
                        process::exit(2);
                    });
                max_width = Some(n);
            }
            "--strict" => {
                strict_width = true;
            }
            "--ascii" => {
                ascii_mode = true;
            }
            "--color" | "-c" => {
                color_mode = true;
            }
            // `--sugiyama` is a no-op since Sugiyama is the default. Kept for
            // backward compat so existing scripts don't break.
            "--sugiyama" => {}
            // `--native` reverts to the pre-0.17.0 in-house layered layout.
            "--native" => {
                backend_mode = mermaid_text::layout::LayoutBackend::Native;
            }
            "--help" | "-h" => {
                println!(
                    "Usage: mermaid-text [--width N] [--strict] [--ascii] [--color] [--native] [FILE]"
                );
                println!();
                println!("Render a Mermaid graph/flowchart diagram as text.");
                println!();
                println!("Arguments:");
                println!("  FILE        Path to a .mmd file (reads stdin if omitted)");
                println!();
                println!("Options:");
                println!("  --width N   Compact output to fit within N terminal columns");
                println!("  --strict    With --width, exit non-zero if the widest line still");
                println!("              exceeds N (a hard budget instead of a soft hint).");
                println!(
                    "  --ascii     Emit plain ASCII characters instead of Unicode box-drawing."
                );
                println!("              Useful for SSH sessions to old hosts, CI log viewers,");
                println!("              or terminals without Unicode fonts.");
                println!("  --color, -c Emit ANSI 24-bit color SGR sequences using the");
                println!("              `style` / `linkStyle` directives in the source.");
                println!("              Off by default; composes with --ascii.");
                println!("  --native    Use the pre-0.17.0 in-house layered layout instead of");
                println!("              the default Sugiyama (ascii-dag) backend.");
                println!("  --help      Print this help message");
                process::exit(0);
            }
            other if !other.starts_with('-') => {
                path = Some(other.to_string());
            }
            other => {
                eprintln!("error: unknown flag '{other}'");
                process::exit(2);
            }
        }
    }

    // Read Mermaid source.
    let source = if let Some(ref file_path) = path {
        std::fs::read_to_string(file_path).unwrap_or_else(|e| {
            eprintln!("error: cannot read '{}': {e}", file_path);
            process::exit(1);
        })
    } else {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .unwrap_or_else(|e| {
                eprintln!("error: failed to read stdin: {e}");
                process::exit(1);
            });
        buf
    };

    // Dispatch to the appropriate renderer.
    let result = mermaid_text::render_with_options(
        &source,
        &mermaid_text::RenderOptions {
            max_width,
            max_width_strict: strict_width,
            ascii: ascii_mode,
            color: color_mode,
            backend: backend_mode,
            gaps_override: None,
        },
    );

    match result {
        Ok(output) => print!("{output}"),
        Err(e) => {
            eprintln!("error: {e}");
            process::exit(1);
        }
    }
}
