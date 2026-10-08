//! Renderer for [`Packet`] diagrams. Produces a fixed-width table of Unicode
//! box-drawing characters with one row per 32-bit word and field names
//! occupying their bit ranges.
//!
//! ## Layout
//!
//! Rows are 32 bits wide. Each field occupies the columns proportional to its
//! bit width within a row. A bit-number ruler is printed above the first row
//! and at each row boundary so the reader can count bit positions.
//!
//! Fields narrower than 3 characters have their labels truncated with `…` (or
//! omitted when they are 1 bit wide and even the `…` would not fit).
//!
//! Fields spanning more than one row (>32 bits) are split at the 32-bit
//! boundary; the label is printed in the first fragment and the continuation
//! rows are left empty.
//!
//! ## Phase 1 limitations
//!
//! - Row width is fixed at 32 bits. Custom widths are not supported.
//! - Custom colours and `accDescr`/`accTitle` are silently ignored.
//! - Multi-row fields produce a continuation cell rather than a spanning box.

use unicode_width::UnicodeWidthStr;

use crate::packet::{Packet, PacketField};

/// Number of bits per display row.
const BITS_PER_ROW: u32 = 32;

/// Render a [`Packet`] diagram to a Unicode string.
///
/// # Arguments
///
/// * `diag`      — the parsed packet diagram
/// * `max_width` — optional column budget; the inner cell width is scaled down
///   so the total row width stays within the budget (minimum: 1 char per bit)
///
/// # Returns
///
/// A multi-line string. Each row is one 32-bit word with a bit-number ruler
/// above it and the field labels centred inside their cells. Box-drawing
/// characters (`┌ ─ ┐ └ ┘ │ ├ ┤ ┬ ┴ ┼`) are used for borders.
pub fn render(diag: &Packet, max_width: Option<usize>) -> String {
    if diag.fields.is_empty() {
        return "(empty packet diagram)".to_string();
    }

    // Choose cell width (characters per bit) so total width <= max_width.
    // Total row width = 1 (left border) + BITS_PER_ROW * cell_w + BITS_PER_ROW (dividers) + 0
    // = 1 + 32 * cell_w + 32 separator chars
    // Actually: left `│` + for each bit: cell_w chars + `│` = 1 + 32*(cell_w+1) chars
    let cell_w: usize = if let Some(budget) = max_width {
        // 1 + 32*(cell_w+1) <= budget  =>  cell_w <= (budget - 1) / 32 - 1
        let max_cell = budget.saturating_sub(1) / BITS_PER_ROW as usize;
        let max_cell = max_cell.saturating_sub(1);
        max_cell.max(1)
    } else {
        // Default: 2 chars per bit (gives a 97-col row for 32 bits)
        2
    };

    let total_bits = diag.total_bits();
    // Total rows needed (round up to full 32-bit words).
    let total_rows = total_bits.max(1).div_ceil(BITS_PER_ROW);

    let mut out = String::new();

    // Title.
    if let Some(title) = &diag.title {
        out.push_str(title);
        out.push('\n');
        out.push('\n');
    }

    // Build a per-bit slot list: for each bit in the 32*N space, which field
    // (if any) owns it?
    let total_bit_slots = total_rows * BITS_PER_ROW;
    // field_index[bit] = index into diag.fields, or None.
    let mut field_index: Vec<Option<usize>> = vec![None; total_bit_slots as usize];
    for (idx, f) in diag.fields.iter().enumerate() {
        for bit in f.start_bit..=f.end_bit {
            if (bit as usize) < field_index.len() {
                field_index[bit as usize] = Some(idx);
            }
        }
    }

    for row in 0..total_rows {
        let row_start = row * BITS_PER_ROW;
        let row_end = row_start + BITS_PER_ROW - 1;

        // -- Bit ruler above every row --
        out.push_str(&render_ruler(row_start, row_end, cell_w));
        out.push('\n');

        // Collect the field segments for this row.
        let segments = collect_row_segments(&field_index, &diag.fields, row_start, row_end);

        // -- Top border --
        out.push_str(&render_top_border(&segments, cell_w, row == 0));
        out.push('\n');

        // -- Content line --
        out.push_str(&render_content_line(&segments, &diag.fields, cell_w));
        out.push('\n');

        // -- Bottom border (only for the last row) --
        if row + 1 == total_rows {
            out.push_str(&render_bottom_border(&segments, cell_w));
            out.push('\n');
        }
    }

    // Trim trailing newlines.
    while out.ends_with('\n') {
        out.pop();
    }

    out
}

// ---------------------------------------------------------------------------
// Segment helpers
// ---------------------------------------------------------------------------

/// A contiguous run of bits belonging to the same field (or gap) within a row.
#[derive(Debug)]
struct Segment {
    /// Bit index of the first bit in this segment (absolute).
    start_bit: u32,
    /// Bit index of the last bit in this segment (absolute, inclusive).
    end_bit: u32,
    /// `Some(idx)` for a field segment, `None` for an unoccupied gap.
    field_idx: Option<usize>,
    /// `true` when this segment is the first fragment of its field in this row
    /// (used to decide whether to print the label or leave blank).
    is_first_fragment: bool,
}

impl Segment {
    fn bit_width(&self) -> u32 {
        self.end_bit - self.start_bit + 1
    }

    /// Cell width: number of characters this segment occupies (borders
    /// between cells are shared, so each segment gets `bit_width * cell_w`
    /// inner chars, with one `│` separator between adjacent segments).
    fn inner_width(&self, cell_w: usize) -> usize {
        self.bit_width() as usize * cell_w + self.bit_width() as usize - 1
    }
}

/// Collect the ordered list of segments for the bits `row_start..=row_end`.
fn collect_row_segments(
    field_index: &[Option<usize>],
    fields: &[PacketField],
    row_start: u32,
    row_end: u32,
) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    let mut bit = row_start;

    while bit <= row_end {
        let fi = field_index[bit as usize];
        // Find the run of consecutive bits with the same field.
        let mut end = bit;
        while end < row_end && field_index[(end + 1) as usize] == fi {
            end += 1;
        }

        // Determine if this is the first fragment of this field in any row.
        let is_first_fragment = match fi {
            None => false,
            Some(idx) => fields[idx].start_bit == bit,
        };

        segments.push(Segment {
            start_bit: bit,
            end_bit: end,
            field_idx: fi,
            is_first_fragment,
        });

        bit = end + 1;
    }

    segments
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

/// Render the bit-number ruler line above a row.
///
/// Shows bit numbers at the leftmost and rightmost position of each segment,
/// and at the row start/end. Numbers are printed left-padded in the cell.
fn render_ruler(row_start: u32, row_end: u32, cell_w: usize) -> String {
    // Build a character buffer of width = 1 + 32*(cell_w+1) chars.
    // The `│` borders are not part of the ruler, just spaces.
    // We'll print numbers at certain columns.

    let width = (BITS_PER_ROW as usize) * (cell_w + 1) + 1;
    let mut buf = vec![b' '; width];

    // For each bit in the row, the leftmost character of its cell is at:
    //   col = 1 + (bit - row_start) as usize * (cell_w + 1)
    // Print the bit number at that position when it fits.
    let print_bit_label = |buf: &mut Vec<u8>, bit: u32| {
        let col = 1 + (bit - row_start) as usize * (cell_w + 1);
        let label = bit.to_string();
        for (i, ch) in label.bytes().enumerate() {
            if col + i < buf.len() {
                buf[col + i] = ch;
            }
        }
    };

    // Print start-of-row, mid (bit 16 boundary), and end-of-row numbers.
    print_bit_label(&mut buf, row_start);
    // Mid-ruler: print bit at column 16 of this row if cell_w is wide enough.
    let mid_bit = row_start + BITS_PER_ROW / 2;
    if mid_bit <= row_end && cell_w >= 2 {
        print_bit_label(&mut buf, mid_bit);
    }
    // End of row: right-align the last bit number in the last cell.
    {
        let last_label = row_end.to_string();
        let last_col = 1 + (BITS_PER_ROW - 1) as usize * (cell_w + 1);
        let label_bytes = last_label.as_bytes();
        // Print right-aligned within the last cell.
        let start = last_col + cell_w - label_bytes.len().min(cell_w);
        for (i, &ch) in label_bytes.iter().enumerate() {
            if start + i < buf.len() {
                buf[start + i] = ch;
            }
        }
    }

    String::from_utf8(buf)
        .unwrap_or_default()
        .trim_end()
        .to_string()
}

/// Render the top border line of a row.
///
/// `is_first_row` controls whether we use `┌`/`┐`/`┬` (first row) or
/// `├`/`┤`/`┼`/`┬`/`┴` (continuation rows where top border merges with
/// the previous row's bottom border).
fn render_top_border(segments: &[Segment], cell_w: usize, is_first_row: bool) -> String {
    let mut line = String::new();

    if is_first_row {
        line.push('\u{250C}'); // ┌
    } else {
        line.push('\u{251C}'); // ├
    }

    for (i, seg) in segments.iter().enumerate() {
        let inner = seg.inner_width(cell_w);
        for _ in 0..inner {
            line.push('\u{2500}'); // ─
        }
        if i + 1 < segments.len() {
            if is_first_row {
                line.push('\u{252C}'); // ┬
            } else {
                line.push('\u{253C}'); // ┼
            }
        }
    }

    if is_first_row {
        line.push('\u{2510}'); // ┐
    } else {
        line.push('\u{2524}'); // ┤
    }

    line
}

/// Render the bottom border line of the last row.
fn render_bottom_border(segments: &[Segment], cell_w: usize) -> String {
    let mut line = String::new();
    line.push('\u{2514}'); // └

    for (i, seg) in segments.iter().enumerate() {
        let inner = seg.inner_width(cell_w);
        for _ in 0..inner {
            line.push('\u{2500}'); // ─
        }
        if i + 1 < segments.len() {
            line.push('\u{2534}'); // ┴
        }
    }

    line.push('\u{2518}'); // ┘
    line
}

/// Render the content line: `│ label │ label │ …`.
fn render_content_line(segments: &[Segment], fields: &[PacketField], cell_w: usize) -> String {
    let mut line = String::new();
    line.push('\u{2502}'); // │

    for seg in segments {
        let inner = seg.inner_width(cell_w);
        let label = match seg.field_idx {
            None => String::new(),
            Some(idx) if seg.is_first_fragment => fields[idx].label.clone(),
            Some(_) => String::new(), // continuation fragment — blank
        };

        let label = fit_label(&label, inner);
        let label_w = UnicodeWidthStr::width(label.as_str());
        let total_pad = inner.saturating_sub(label_w);
        let left_pad = total_pad / 2;
        let right_pad = total_pad - left_pad;

        for _ in 0..left_pad {
            line.push(' ');
        }
        line.push_str(&label);
        for _ in 0..right_pad {
            line.push(' ');
        }
        line.push('\u{2502}'); // │
    }

    line
}

/// Fit a label into `max_w` display columns, truncating with `…` if needed.
///
/// When `max_w` is 0, returns an empty string.
/// When `max_w` is 1, returns `…` if the label is non-empty, otherwise `""`.
fn fit_label(label: &str, max_w: usize) -> String {
    if max_w == 0 || label.is_empty() {
        return String::new();
    }
    let w = UnicodeWidthStr::width(label);
    if w <= max_w {
        return label.to_string();
    }
    // Truncate to max_w - 1 columns, then append `…`.
    let target = max_w.saturating_sub(1);
    if target == 0 {
        return "\u{2026}".to_string(); // …
    }
    let mut result = String::new();
    let mut used = 0usize;
    for ch in label.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
        if used + cw > target {
            break;
        }
        result.push(ch);
        used += cw;
    }
    result.push('\u{2026}'); // …
    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::packet::parse;

    fn parsed(src: &str) -> Packet {
        parse(src).expect("parse should succeed")
    }

    #[test]
    fn title_appears_in_output() {
        let diag = parsed("packet-beta\n    title My Header\n    0-31: \"Data\"");
        let out = render(&diag, None);
        assert!(
            out.contains("My Header"),
            "title must appear in output:\n{out}"
        );
    }

    #[test]
    fn single_row_32_bit_fields_render() {
        // Two fields that together fill exactly 32 bits.
        let diag =
            parsed("packet-beta\n    0-15: \"Source Port\"\n    16-31: \"Destination Port\"");
        let out = render(&diag, None);

        // Both labels must appear.
        assert!(out.contains("Source Port"), "Source Port missing:\n{out}");
        assert!(
            out.contains("Destination Port"),
            "Destination Port missing:\n{out}"
        );

        // Box-drawing corners must be present.
        assert!(
            out.contains('\u{250C}'),
            "top-left corner ┌ missing:\n{out}"
        );
        assert!(
            out.contains('\u{2510}'),
            "top-right corner ┐ missing:\n{out}"
        );
        assert!(
            out.contains('\u{2514}'),
            "bottom-left corner └ missing:\n{out}"
        );
        assert!(
            out.contains('\u{2518}'),
            "bottom-right corner ┘ missing:\n{out}"
        );

        // A mid-row divider ┬ must be present (between the two fields).
        assert!(out.contains('\u{252C}'), "top divider ┬ missing:\n{out}");
    }

    #[test]
    fn multi_row_field_label_appears_in_first_row_only() {
        // A 64-bit field spans two rows. The label appears in row 0 only.
        let diag = parsed("packet-beta\n    0-63: \"Sequence Number\"");
        let out = render(&diag, None);

        assert!(out.contains("Sequence Number"), "label must appear:\n{out}");

        // Count occurrences of "Sequence Number" — should be exactly 1.
        let occurrences = out.matches("Sequence Number").count();
        assert_eq!(
            occurrences, 1,
            "label should appear exactly once (first fragment only):\n{out}"
        );

        // Two rows means the bottom-left └ comes after a mid-row ├ border.
        assert!(
            out.contains('\u{251C}'),
            "row continuation ├ missing:\n{out}"
        );
    }

    #[test]
    fn empty_diagram_renders_placeholder() {
        let diag = Packet {
            title: None,
            fields: vec![],
        };
        let out = render(&diag, None);
        assert!(
            out.contains("empty packet diagram"),
            "placeholder missing:\n{out}"
        );
    }

    #[test]
    fn single_bit_field_renders_without_panic() {
        let diag = parsed("packet-beta\n    0-30: \"Data\"\n    31: \"Flag\"");
        let out = render(&diag, None);
        // Both fields must appear in some form.
        assert!(out.contains("Data"), "Data field missing:\n{out}");
        // Flag or its truncated form must be somewhere.
        let has_flag = out.contains("Flag") || out.contains('\u{2026}');
        assert!(has_flag, "Flag or ellipsis missing:\n{out}");
    }

    #[test]
    fn max_width_does_not_panic() {
        let diag = parsed("packet-beta\n    0-31: \"Header\"");
        // Very narrow budget — must not panic.
        let out = render(&diag, Some(40));
        assert!(out.contains('\u{250C}'), "box must still render:\n{out}");
    }
}
