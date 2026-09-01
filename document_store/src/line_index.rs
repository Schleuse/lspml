use lsp_types::Position;

/// Translates between byte offsets into a document's text and lsp positions,
/// whose character component counts utf-16 code units. Built once per document
/// version, because it describes one exact text.
///
/// Line extents include their terminator, which makes the translation a
/// bijection over every character boundary of the text: no offset is
/// unaddressable. Positions the client sends that lie beyond the end of their
/// line are clamped to it, as the lsp specification prescribes.
#[derive(Clone, Debug, PartialEq)]
pub struct LineIndex {
    lines: Vec<Line>,
    /// Length of the text in bytes, which bounds the last line.
    length: usize,
}

#[derive(Clone, Debug, PartialEq)]
struct Line {
    /// Byte offset of the line's first character.
    start: usize,
    /// The line's characters that are wider than a single byte, in order.
    /// Absent for the overwhelmingly common ascii-only line, where byte offsets
    /// and utf-16 code units coincide.
    wide_characters: Vec<WideCharacter>,
}

#[derive(Clone, Debug, PartialEq)]
struct WideCharacter {
    /// Byte offset of the character, relative to the start of its line.
    start: usize,
    /// Width of the character in bytes.
    bytes: usize,
    /// Width of the character in utf-16 code units: two for anything outside
    /// the basic multilingual plane, one otherwise.
    code_units: usize,
}

impl LineIndex {
    pub fn new(text: &str) -> LineIndex {
        let mut lines = vec![Line {
            start: 0,
            wide_characters: Vec::new(),
        }];
        for (offset, character) in text.char_indices() {
            if character == '\n' {
                lines.push(Line {
                    start: offset + 1,
                    wide_characters: Vec::new(),
                });
                continue;
            }
            let bytes = character.len_utf8();
            if bytes > 1 {
                let line = lines.last_mut().expect("there always is a current line");
                line.wide_characters.push(WideCharacter {
                    start: offset - line.start,
                    bytes,
                    code_units: character.len_utf16(),
                });
            }
        }
        return LineIndex {
            lines,
            length: text.len(),
        };
    }

    /// The position of `offset`, whose character component counts utf-16 code
    /// units. An offset beyond the end of the text yields the position of its
    /// end; an offset that is not a character boundary yields the position of
    /// the character containing it.
    pub fn utf16_position(&self, offset: usize) -> Position {
        let offset = offset.min(self.length);
        let line_number = self
            .lines
            .partition_point(|line| line.start <= offset)
            .saturating_sub(1);
        let line = &self.lines[line_number];
        let target = offset - line.start;
        let mut byte = 0;
        let mut code_unit = 0;
        for character in &line.wide_characters {
            if target <= character.start {
                break;
            }
            code_unit += character.start - byte;
            byte = character.start;
            if target < character.start + character.bytes {
                return position(line_number, code_unit);
            }
            code_unit += character.code_units;
            byte += character.bytes;
        }
        return position(line_number, code_unit + (target - byte));
    }

    /// The byte offset of `position`, whose character component is read as
    /// utf-16 code units. `None` when the position names a line the text does
    /// not have; a character beyond the end of its line is clamped to it, as is
    /// one that would split a surrogate pair.
    pub fn byte_offset(&self, position: Position) -> Option<usize> {
        let line_number = position.line as usize;
        let line = self.lines.get(line_number)?;
        let end = self.line_end(line_number);
        let target = position.character as usize;
        let mut byte = 0;
        let mut code_unit = 0;
        for character in &line.wide_characters {
            let ascii = character.start - byte;
            if target < code_unit + ascii {
                return Some(line.start + byte + (target - code_unit));
            }
            code_unit += ascii;
            byte = character.start;
            if target < code_unit + character.code_units {
                return Some(line.start + byte);
            }
            code_unit += character.code_units;
            byte += character.bytes;
        }
        return Some((line.start + byte + (target - code_unit)).min(end));
    }

    /// The last byte offset the line can address. The next line's start belongs
    /// to that line, so a line that has a successor ends one byte before it -
    /// on its own terminating newline.
    fn line_end(&self, line_number: usize) -> usize {
        return match self.lines.get(line_number + 1) {
            Some(line) => line.start - 1,
            None => self.length,
        };
    }
}

fn position(line: usize, character: usize) -> Position {
    return Position {
        line: line as u32,
        character: character as u32,
    };
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{LineIndex, position};

    #[test]
    fn test_ascii_offsets_are_their_own_columns() {
        let index = LineIndex::new("abc\ndef");

        assert_eq!(index.utf16_position(0), position(0, 0));
        assert_eq!(index.utf16_position(2), position(0, 2));
        assert_eq!(index.utf16_position(4), position(1, 0));
        assert_eq!(index.utf16_position(6), position(1, 2));
        assert_eq!(index.byte_offset(position(1, 1)), Some(5));
    }

    #[test]
    fn test_umlauts_count_as_a_single_code_unit() {
        // "ü" and "ä" take two bytes each but one utf-16 code unit each.
        let index = LineIndex::new("üä\nx");

        assert_eq!(index.utf16_position(0), position(0, 0));
        assert_eq!(index.utf16_position(2), position(0, 1));
        assert_eq!(index.utf16_position(4), position(0, 2));
        assert_eq!(index.utf16_position(5), position(1, 0));
        assert_eq!(index.byte_offset(position(0, 1)), Some(2));
        assert_eq!(index.byte_offset(position(0, 2)), Some(4));
        assert_eq!(index.byte_offset(position(1, 0)), Some(5));
    }

    #[test]
    fn test_characters_outside_the_basic_multilingual_plane_count_as_two_code_units() {
        // "𝄞" is U+1D11E: four bytes, and a surrogate pair in utf-16.
        let index = LineIndex::new("a𝄞b");

        assert_eq!(index.utf16_position(1), position(0, 1));
        assert_eq!(index.utf16_position(5), position(0, 3));
        assert_eq!(index.utf16_position(6), position(0, 4));
        assert_eq!(index.byte_offset(position(0, 1)), Some(1));
        assert_eq!(index.byte_offset(position(0, 3)), Some(5));
        assert_eq!(index.byte_offset(position(0, 4)), Some(6));
    }

    #[test]
    fn test_a_column_splitting_a_surrogate_pair_clamps_to_the_character() {
        let index = LineIndex::new("a𝄞b");

        assert_eq!(index.byte_offset(position(0, 2)), Some(1));
    }

    #[test]
    fn test_an_offset_inside_a_character_yields_that_characters_position() {
        let index = LineIndex::new("a𝄞b");

        assert_eq!(index.utf16_position(3), position(0, 1));
    }

    #[test]
    fn test_carriage_returns_belong_to_the_line_they_terminate() {
        let index = LineIndex::new("a\r\nb");

        assert_eq!(index.utf16_position(1), position(0, 1));
        assert_eq!(index.utf16_position(2), position(0, 2));
        assert_eq!(index.utf16_position(3), position(1, 0));
        assert_eq!(index.byte_offset(position(1, 0)), Some(3));
    }

    #[test]
    fn test_a_column_beyond_the_end_of_its_line_clamps_to_it() {
        let index = LineIndex::new("äö\nx");

        assert_eq!(index.byte_offset(position(0, 99)), Some(4));
        assert_eq!(index.byte_offset(position(1, 99)), Some(6));
    }

    #[test]
    fn test_an_offset_beyond_the_end_of_the_text_clamps_to_it() {
        let index = LineIndex::new("äö");

        assert_eq!(index.utf16_position(99), position(0, 2));
    }

    #[test]
    fn test_a_line_the_text_does_not_have_has_no_offset() {
        let index = LineIndex::new("a\nb");

        assert_eq!(index.byte_offset(position(2, 0)), None);
    }

    #[test]
    fn test_the_empty_text_has_one_empty_line() {
        let index = LineIndex::new("");

        assert_eq!(index.utf16_position(0), position(0, 0));
        assert_eq!(index.byte_offset(position(0, 0)), Some(0));
        assert_eq!(index.byte_offset(position(1, 0)), None);
    }

    fn arbitrary_text() -> impl Strategy<Value = String> {
        return prop::collection::vec(any::<char>(), 0..40)
            .prop_map(|characters| characters.into_iter().collect::<String>());
    }

    proptest! {
        /// Every character boundary of an arbitrary text survives the
        /// translation into a position and back.
        #[test]
        fn test_offsets_round_trip_through_positions(text in arbitrary_text()) {
            let index = LineIndex::new(&text);
            for offset in (0..=text.len()).filter(|offset| text.is_char_boundary(*offset)) {
                prop_assert_eq!(index.byte_offset(index.utf16_position(offset)), Some(offset));
            }
        }

        /// An arbitrary position - one the index did not itself produce, as
        /// anything a client sends - translates into a character boundary whose
        /// own position translates back to it.
        #[test]
        fn test_positions_round_trip_through_offsets(
            text in arbitrary_text(),
            line in 0u32..8,
            character in 0u32..48,
        ) {
            let index = LineIndex::new(&text);
            let Some(offset) = index.byte_offset(position(line as usize, character as usize)) else {
                return Ok(());
            };
            prop_assert!(text.is_char_boundary(offset));
            prop_assert_eq!(index.byte_offset(index.utf16_position(offset)), Some(offset));
        }
    }
}
