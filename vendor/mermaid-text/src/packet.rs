//! Data model for Mermaid `packet-beta` diagrams.
//!
//! A packet diagram maps bit ranges to named fields, visualising the layout of
//! a network packet header (or any fixed-width binary structure). Mermaid renders
//! these as 32-bit-wide rows with bit numbers across the top and field labels
//! occupying their bit ranges.
//!
//! Example source:
//!
//! ```text
//! packet-beta
//!     title TCP Packet
//!     0-15: "Source Port"
//!     16-31: "Destination Port"
//!     32-63: "Sequence Number"
//! ```
//!
//! Constructed by [`crate::parser::packet::parse`] and consumed by
//! [`crate::render::packet::render`].
//!
//! ## Phase 1 limitations
//!
//! - Row width is always 32 bits. Custom widths are not supported.
//! - Custom colours and `accDescr`/`accTitle` are silently ignored.
//! - No custom bit-numbering direction or endianness selection.

/// A single field in a packet diagram.
///
/// Both `start_bit` and `end_bit` are inclusive bit indices (0-based).
/// For single-bit fields, `end_bit == start_bit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacketField {
    /// The inclusive start bit (0-based).
    pub start_bit: u32,
    /// The inclusive end bit (0-based). Equal to `start_bit` for single-bit fields.
    pub end_bit: u32,
    /// The display label for this field.
    pub label: String,
}

impl PacketField {
    /// The number of bits this field spans.
    ///
    /// Always at least 1 (single-bit fields return 1).
    pub fn bit_width(&self) -> u32 {
        self.end_bit - self.start_bit + 1
    }
}

/// A parsed `packet-beta` diagram.
///
/// Constructed by [`crate::parser::packet::parse`] and consumed by
/// [`crate::render::packet::render`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    /// An optional title displayed above the diagram.
    pub title: Option<String>,
    /// Ordered list of fields in the diagram (declaration order).
    pub fields: Vec<PacketField>,
}

impl Packet {
    /// The total number of bits spanned by this packet.
    ///
    /// Returns `highest_end_bit + 1`, or `0` when there are no fields.
    pub fn total_bits(&self) -> u32 {
        self.fields.iter().map(|f| f.end_bit + 1).max().unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_field_bit_width_single() {
        let f = PacketField {
            start_bit: 5,
            end_bit: 5,
            label: "FLAG".to_string(),
        };
        assert_eq!(f.bit_width(), 1);
    }

    #[test]
    fn packet_field_bit_width_range() {
        let f = PacketField {
            start_bit: 0,
            end_bit: 15,
            label: "Source Port".to_string(),
        };
        assert_eq!(f.bit_width(), 16);
    }

    #[test]
    fn packet_total_bits_empty() {
        let p = Packet {
            title: None,
            fields: vec![],
        };
        assert_eq!(p.total_bits(), 0);
    }

    #[test]
    fn packet_total_bits_multiple_fields() {
        let p = Packet {
            title: Some("TCP".to_string()),
            fields: vec![
                PacketField {
                    start_bit: 0,
                    end_bit: 15,
                    label: "Source Port".to_string(),
                },
                PacketField {
                    start_bit: 16,
                    end_bit: 31,
                    label: "Dest Port".to_string(),
                },
            ],
        };
        // highest end_bit = 31, so total = 32
        assert_eq!(p.total_bits(), 32);
    }

    #[test]
    fn packet_equality_and_clone() {
        let a = Packet {
            title: Some("IP".to_string()),
            fields: vec![PacketField {
                start_bit: 0,
                end_bit: 3,
                label: "Version".to_string(),
            }],
        };
        let b = a.clone();
        assert_eq!(a, b);

        let c = Packet {
            title: None,
            fields: vec![],
        };
        assert_ne!(a, c);
    }
}
