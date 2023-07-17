use crate::compiler::ByteMaskCombinator;
use crate::utils::cast;
use regex_syntax::hir::HirKind::Concat;
use regex_syntax::hir::{Class, ClassBytes, ClassBytesRange, Hir, HirKind};
use std::ops::RangeInclusive;
use yara_x_parser::ast::HexByte;

#[derive(Debug, PartialEq)]
pub struct TrailingPattern {
    pub gap: RangeInclusive<u32>,
    pub hir: Hir,
}

/// Pattern chaining is the process of splitting a pattern that contains very
/// large gaps (a.k.a jumps) into multiple pieces that are chained together.
///
/// For example, when matching the pattern `{ 01 02 03 [0-2000] 04 05 06 }` is
/// more efficient if we split it into two patterns `{ 01 02 03}` and
/// `{ 04 05 06 }`, while the latter is chained to the former in order to make
/// sure that `{ 01 02 03}` won't match if `{ 04 05 06 }` doesn't appear at the
/// correct distance after `{ 01 02 03}`.
///
/// Both `{ 01 02 03}` and `{ 04 05 06 }` are handled as if they were separate
/// patterns, except that they are chained together.
///
/// [`PATTERN_CHAINING_THRESHOLD`] controls the how large the gap (or jump)
/// must be in order split the pattern at that point. Gaps shorter than this
/// value don't cause the splitting of the pattern.
const PATTERN_CHAINING_THRESHOLD: u32 = 200;

/// Splits a pattern into multiple pieces if it contains gaps that are larger
/// than [`PATTERN_CHAINING_THRESHOLD`]. Notice that these gaps must be
/// non-greedy, so it doesn't apply to regexps like `/abc.*xyz/s` because `.*`
/// is greedy, but it applies to the non-greedy `/abc.*?xyz/s`. Also notice
/// that only regexps with the `/s` modifier (i.e: `dot_matches_new_line` is
/// true) will be split. In regexps without this modifier `.*?` can not contain
/// newlines, and therefore is not a real gap that contain anything.
///
/// Receives the HIR for the original pattern and returns a tuple where the
/// first item corresponds to the leading piece of the original pattern, and
/// the second item is a vector with zero o more items, corresponding to each
/// remaining piece.
///
/// For example, for pattern `{ 01 02 03 [-] 04 05 06 [-] 07 08 09 }` the
/// first item in the tuple is the HIR for `{ 01 02 03 }`, while the second
/// item is a vector containing two entries, one for `{ 04 05 06 }` and the
/// other for `{ 07 08 09 }`.
///
/// If the pattern doesn't contain any gap that is long enough, the pattern
/// won't be split, and the leading piece will contain the whole pattern
/// while the vector will be empty.
pub(in crate::compiler) fn split_at_large_gaps(
    hir: Hir,
) -> (Hir, Vec<TrailingPattern>) {
    if !matches!(hir.kind(), Concat(_)) {
        return (hir, vec![]);
    }

    let mut heading = None;
    let mut trailing = Vec::new();

    let mut push = |gap: Option<RangeInclusive<u32>>, fragment| {
        if let Some(gap) = gap {
            trailing.push(TrailingPattern { gap, hir: Hir::concat(fragment) });
        } else {
            heading = Some(Hir::concat(fragment));
        }
    };

    let items = cast!(hir.into_kind(), Concat);

    let mut prev_gap = None;
    let mut pattern_chunk = Vec::new();

    for item in items {
        if let HirKind::Repetition(repetition) = item.kind() {
            let max_gap = repetition.max.unwrap_or(u32::MAX) - repetition.min;
            if max_gap > PATTERN_CHAINING_THRESHOLD
                && !repetition.greedy
                && any_byte(repetition.sub.as_ref().kind())
            {
                push(prev_gap, pattern_chunk);
                prev_gap =
                    Some(repetition.min..=repetition.max.unwrap_or(u32::MAX));

                pattern_chunk = Vec::new();
            } else {
                pattern_chunk.push(item);
            }
        } else {
            pattern_chunk.push(item)
        }
    }

    push(prev_gap, pattern_chunk);

    (heading.unwrap(), trailing)
}

/// Returns true if `hir_kind` is a byte class containing all possible bytes.
///
/// For example `??` in an hex pattern, or `.` in a regexp that uses the `/s`
/// modifier (i.e: `dot_matches_new_line` is true).
pub fn any_byte(hir_kind: &HirKind) -> bool {
    match hir_kind {
        HirKind::Class(Class::Bytes(class)) => {
            if let Some(range) = class.ranges().first() {
                range.start() == 0 && range.end() == u8::MAX
            } else {
                false
            }
        }
        HirKind::Class(Class::Unicode(class)) => {
            if let Some(range) = class.ranges().first() {
                range.start() == 0 as char && range.end() == char::MAX
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Returns [`Some(HexByte)`] if the given [`ClassBytes`] represents a
/// masked byte.
///
/// This function basically does the opposite than [`hex_byte_to_class`].
/// However, not all the classes represent a masked byte, in such cases
/// this function returns [`None`].
pub fn class_to_hex_byte(c: &ClassBytes) -> Option<HexByte> {
    if c.ranges().is_empty() {
        return None;
    }

    // Get the smallest and largest bytes in the class. The ranges are
    // guaranteed to be sorted, so the smallest byte is the one that
    // starts the first range and the largest byte is the one that ends
    // the last range.
    let smallest_byte = c.ranges().first().unwrap().start();
    let largest_byte = c.ranges().last().unwrap().end();

    // In a class that represents a masked hex byte, we can compute the mask
    // by XORing the largest byte and the smallest one. The smallest byte
    // corresponds to the byte with all the masked bits set to 0, and the
    // largest byte is the byte with all the masked bits set to 1. For
    // example, in `3?`, the smallest byte is `30` and the largest one is
    // `3F`, by xoring the two we get `0x0F`.
    let neg_mask = largest_byte ^ smallest_byte;

    // Make sure that a bitwise and between the largest and the smallest
    // bytes is equal to the smallest one.
    if largest_byte & smallest_byte != smallest_byte {
        return None;
    }

    // Compute the number of bytes in the class.
    let mut num_bytes: u32 = 0;

    for range in c.ranges().iter() {
        num_bytes += (range.end() - range.start()) as u32 + 1;
    }

    // The class must have 2^N bytes, where N is the number of 1s in the
    // negated mask, if not, this is not a masked byte. For instance, if the
    // negated mask is `0000 1111`, it means that the bits that are set to 1
    // can have an arbitrary value in the byte, so possible bytes are
    // `0000 0001`, `0000 0010`, `0000 0011`, up to `0000 1111`. Therefore the
    // number of possible bytes is 2^4 (16).
    if 1 << neg_mask.count_ones() != num_bytes {
        return None;
    }

    Some(HexByte { value: smallest_byte, mask: !neg_mask })
}

pub fn hex_byte_to_class(b: HexByte) -> ClassBytes {
    // A zero bit in the mask indicates that the corresponding bit in the value
    // must will be ignored, but those ignored bits should be set to 0.
    assert_eq!(b.value & !b.mask, 0);

    let mut class = ClassBytes::empty();
    for b in ByteMaskCombinator::new(b.value, b.mask) {
        class.push(ClassBytesRange::new(b, b));
    }

    class
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use regex_syntax::hir::{
        Class, ClassBytes, ClassBytesRange, Dot, Hir, HirKind, Repetition,
    };
    use yara_x_parser::ast::HexByte;

    use crate::compiler::ir::utils::{
        class_to_hex_byte, hex_byte_to_class, TrailingPattern,
        PATTERN_CHAINING_THRESHOLD,
    };

    #[test]
    fn split() {
        assert_eq!(
            super::split_at_large_gaps(Hir::literal([0x01, 0x02, 0x03])),
            (Hir::literal([0x01, 0x02, 0x03]), vec![])
        );

        assert_eq!(
            super::split_at_large_gaps(Hir::concat(vec![
                // Input
                Hir::literal([0x01, 0x02, 0x03]),
                Hir::literal([0x06, 0x07]),
            ])),
            (
                // Output
                Hir::concat(vec![
                    Hir::literal([0x01, 0x02, 0x03]),
                    Hir::literal([0x06, 0x07])
                ]),
                vec![]
            )
        );

        // Check that the pattern is not split when the jump is small.
        assert_eq!(
            super::split_at_large_gaps(Hir::concat(vec![
                // Input
                Hir::literal([0x01]),
                Hir::repetition(Repetition {
                    min: 0,
                    max: Some(PATTERN_CHAINING_THRESHOLD),
                    greedy: false,
                    sub: Box::new(Hir::dot(Dot::AnyByte)),
                }),
                Hir::literal([0x02, 0x03]),
            ])),
            (
                // Output
                Hir::concat(vec![
                    // Input
                    Hir::literal([0x01]),
                    Hir::repetition(Repetition {
                        min: 0,
                        max: Some(PATTERN_CHAINING_THRESHOLD),
                        greedy: false,
                        sub: Box::new(Hir::dot(Dot::AnyByte)),
                    }),
                    Hir::literal([0x02, 0x03]),
                ]),
                vec![]
            )
        );

        // Check that the pattern is not split when the jump is greedy.
        assert_eq!(
            super::split_at_large_gaps(Hir::concat(vec![
                // Input
                Hir::literal([0x01]),
                Hir::repetition(Repetition {
                    min: 0,
                    max: Some(2 * PATTERN_CHAINING_THRESHOLD),
                    greedy: true,
                    sub: Box::new(Hir::dot(Dot::AnyByte)),
                }),
                Hir::literal([0x02, 0x03]),
            ])),
            (
                // Output
                Hir::concat(vec![
                    // Input
                    Hir::literal([0x01]),
                    Hir::repetition(Repetition {
                        min: 0,
                        max: Some(2 * PATTERN_CHAINING_THRESHOLD),
                        greedy: true,
                        sub: Box::new(Hir::dot(Dot::AnyByte)),
                    }),
                    Hir::literal([0x02, 0x03]),
                ]),
                vec![]
            )
        );

        // Check that the pattern is split when the jump is large.
        assert_eq!(
            super::split_at_large_gaps(Hir::concat(vec![
                // Input
                Hir::literal([0x01, 0x02, 0x03]),
                Hir::repetition(Repetition {
                    min: 0,
                    max: None,
                    greedy: false,
                    sub: Box::new(Hir::dot(Dot::AnyByte)),
                }),
                Hir::literal([0x05]),
                Hir::repetition(Repetition {
                    min: 10,
                    max: Some(11 + PATTERN_CHAINING_THRESHOLD),
                    greedy: false,
                    sub: Box::new(Hir::dot(Dot::AnyByte)),
                }),
                Hir::literal([0x06, 0x07]),
            ])),
            (
                // Output
                Hir::literal([0x01, 0x02, 0x03]),
                vec![
                    TrailingPattern {
                        gap: 0..=u32::MAX,
                        hir: Hir::literal([0x05])
                    },
                    TrailingPattern {
                        gap: 10..=11 + PATTERN_CHAINING_THRESHOLD,
                        hir: Hir::literal([0x06, 0x07])
                    }
                ]
            )
        );
    }

    #[test]
    fn mask() {
        assert_eq!(
            class_to_hex_byte(&hex_byte_to_class(HexByte {
                value: 0x30,
                mask: 0xF0
            })),
            Some(HexByte { value: 0x30, mask: 0xF0 })
        );

        assert_eq!(
            class_to_hex_byte(&hex_byte_to_class(HexByte {
                value: 0x05,
                mask: 0x0F
            })),
            Some(HexByte { value: 0x05, mask: 0x0F })
        );

        assert_eq!(
            class_to_hex_byte(&hex_byte_to_class(HexByte {
                value: 0x08,
                mask: 0xAA
            })),
            Some(HexByte { value: 0x08, mask: 0xAA })
        );

        assert_eq!(
            class_to_hex_byte(&ClassBytes::new(vec![
                ClassBytesRange::new(3, 4),
                ClassBytesRange::new(8, 8),
            ])),
            None,
        );

        assert_eq!(
            class_to_hex_byte(&ClassBytes::new(vec![
                ClassBytesRange::new(0, 0),
                ClassBytesRange::new(2, 2),
                ClassBytesRange::new(4, 4),
            ])),
            None,
        );

        let hir = Hir::dot(Dot::AnyByte);

        if let HirKind::Class(Class::Bytes(class)) = hir.kind() {
            assert_eq!(
                class_to_hex_byte(class),
                Some(HexByte { value: 0x00, mask: 0x00 })
            );
        } else {
            unreachable!()
        }
    }
}
