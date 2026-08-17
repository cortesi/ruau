//! Structured representation of one serialized Luau bytecode chunk.

use serde::{Deserialize, Serialize};

use crate::opcodes::{CaptureType, FeedbackType, Opcode};

/// One decoded bytecode blob.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum BytecodeChunk {
    /// Valid bytecode chunk.
    Valid {
        /// Serialized bytecode version.
        bytecode_version: u8,
        /// Serialized type-encoding version.
        type_version: u8,
        /// String table in wire order. Entry ids are one-based; id zero is empty.
        strings: Vec<Vec<u8>>,
        /// Userdata type-name mappings in wire order.
        userdata_type_mappings: Vec<UserdataTypeMapping>,
        /// Protos in wire order.
        protos: Vec<Proto>,
        /// Main proto id.
        main_proto: u32,
    },
    /// Non-throwing upstream compile error bytecode.
    Error {
        /// Raw error message bytes after the leading zero marker.
        message: Vec<u8>,
    },
}

/// Userdata type-name remapping entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserdataTypeMapping {
    /// Type tag index.
    pub type_index: u8,
    /// One-based string-table id.
    pub name: u32,
}

/// One decoded proto.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Proto {
    /// Maximum stack size.
    pub max_stack_size: u8,
    /// Fixed parameter count.
    pub num_params: u8,
    /// Upvalue count.
    pub num_upvalues: u8,
    /// Vararg flag.
    pub is_vararg: u8,
    /// Proto flags.
    pub flags: u8,
    /// Type-info section.
    pub type_info: TypeInfo,
    /// Decoded instructions in word order.
    pub code: Vec<Instruction>,
    /// Constant table.
    pub constants: Vec<Constant>,
    /// Child proto ids.
    pub child_protos: Vec<u32>,
    /// Source line where this proto is defined.
    pub line_defined: u32,
    /// One-based string-table id of debug name, or zero.
    pub debug_name: u32,
    /// Encoded line-info block.
    pub line_info: Option<LineInfo>,
    /// Local and upvalue debug info.
    pub debug_info: Option<DebugInfo>,
    /// Feedback slots, present in version 11 and extended-layout chunks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feedback_slots: Vec<FeedbackSlot>,
    /// Estimated execution cost for an inlinable extended-layout proto.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<u64>,
}

/// Raw type-info payload with decoded counts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeInfo {
    /// Encoded type-info payload bytes.
    pub raw: Vec<u8>,
}

/// One instruction header and its optional static AUX word.
///
/// Plain-old-data on purpose: the dispatch loop fetches instructions by copy,
/// so the type must never grow a heap allocation. Every aux-bearing Luau
/// opcode carries exactly one AUX word (`Opcode::instruction_len`).
///
/// # Invariant
///
/// `opcode`, `a`, `b`, `c`, `d`, and `e` are pre-decoded views of `header`;
/// every constructor ([`Instruction::from_words`], [`Instruction::abc`],
/// [`Instruction::abc_with_aux`], [`Instruction::ad`]) derives them together
/// so they always agree. The fields stay public (and mutable) for the VM
/// dispatch loop, so direct mutation of one side can break the invariant —
/// but never silently: [`crate::validate_chunk`] reports the mismatch and
/// [`crate::encode_chunk`] refuses to encode it. To change an instruction,
/// build a replacement through a constructor instead of mutating fields.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Instruction {
    /// Header word.
    pub header: u32,
    /// Decoded opcode.
    pub opcode: Opcode,
    /// ABC A operand.
    pub a: u8,
    /// ABC B operand.
    pub b: u8,
    /// ABC C operand.
    pub c: u8,
    /// AD D operand.
    pub d: i16,
    /// E operand.
    pub e: i32,
    /// The static AUX word following the header, when the opcode has one.
    /// Serialized as a 0/1-element array to keep the committed fixture shape.
    #[serde(with = "aux_serde")]
    pub aux: Option<u32>,
}

/// Serializes the optional AUX word as the 0/1-element array the committed
/// fixture corpus already uses.
mod aux_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(aux: &Option<u32>, serializer: S) -> Result<S::Ok, S::Error> {
        let words: &[u32] = match aux {
            Some(word) => std::slice::from_ref(word),
            None => &[],
        };
        words.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<u32>, D::Error> {
        let words = Vec::<u32>::deserialize(deserializer)?;
        match words.as_slice() {
            [] => Ok(None),
            [word] => Ok(Some(*word)),
            _ => Err(serde::de::Error::custom(
                "an instruction has at most one AUX word",
            )),
        }
    }
}

impl Instruction {
    /// Creates an instruction from its raw words.
    #[must_use]
    pub fn from_words(header: u32, aux: Option<u32>) -> Option<Self> {
        let opcode = Opcode::from_byte((header & 0xff) as u8)?;
        Some(Self {
            header,
            opcode,
            a: ((header >> 8) & 0xff) as u8,
            b: ((header >> 16) & 0xff) as u8,
            c: ((header >> 24) & 0xff) as u8,
            d: (header >> 16) as u16 as i16,
            e: (header as i32) >> 8,
            aux,
        })
    }

    /// Encodes ABC instruction fields.
    #[must_use]
    pub fn abc(opcode: Opcode, a: u8, b: u8, c: u8) -> Self {
        let header =
            opcode.byte() as u32 | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24);
        Self::from_words(header, None).expect("opcode bytes from the Opcode enum always decode")
    }

    /// Encodes ABC instruction fields followed by an optional static AUX word.
    #[must_use]
    pub fn abc_with_aux(opcode: Opcode, a: u8, b: u8, c: u8, aux: Option<u32>) -> Self {
        let header =
            opcode.byte() as u32 | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24);
        Self::from_words(header, aux).expect("opcode bytes from the Opcode enum always decode")
    }

    /// Encodes AD instruction fields.
    #[must_use]
    pub fn ad(opcode: Opcode, a: u8, d: i16) -> Self {
        let header = opcode.byte() as u32 | ((a as u32) << 8) | ((d as u16 as u32) << 16);
        Self::from_words(header, None).expect("opcode bytes from the Opcode enum always decode")
    }

    /// Returns the encoded jump offset in instruction words, if this opcode jumps.
    ///
    /// Luau jump offsets are relative to the word after the instruction header.
    /// For instructions with AUX words, the AUX words are part of the encoded
    /// offset span; for example, comparison jumps use `D = 1` for the next
    /// decoded instruction because their AUX word occupies the intervening word.
    #[must_use]
    pub fn jump_offset_words(&self) -> Option<i32> {
        match self.opcode {
            Opcode::Jump
            | Opcode::JumpBack
            | Opcode::JumpIf
            | Opcode::JumpIfNot
            | Opcode::JumpIfEq
            | Opcode::JumpIfLe
            | Opcode::JumpIfLt
            | Opcode::JumpIfNotEq
            | Opcode::JumpIfNotLe
            | Opcode::JumpIfNotLt
            | Opcode::ForNPrep
            | Opcode::ForNLoop
            | Opcode::ForGLoop
            | Opcode::ForGPrepInext
            | Opcode::ForGPrepNext
            | Opcode::ForGPrep
            | Opcode::JumpXEqKNil
            | Opcode::JumpXEqKB
            | Opcode::JumpXEqKN
            | Opcode::JumpXEqKS
            | Opcode::CmpProto => Some(i32::from(self.d)),
            Opcode::JumpX => Some(self.e),
            Opcode::LoadB
            | Opcode::FastCall
            | Opcode::FastCall1
            | Opcode::FastCall2
            | Opcode::FastCall2K
            | Opcode::FastCall3 => Some(i32::from(self.c)),
            _ => None,
        }
    }

    /// Resolves this instruction's jump target as an absolute instruction-word offset.
    #[must_use]
    pub fn jump_target_word(&self, instruction_word: u32) -> Option<i32> {
        Some(instruction_word as i32 + 1 + self.jump_offset_words()?)
    }

    /// Returns the `CAPTURE` operand kind, if this is a valid capture instruction.
    #[must_use]
    pub fn capture_type(&self) -> Option<CaptureType> {
        if self.opcode == Opcode::Capture {
            CaptureType::from_byte(self.a)
        } else {
            None
        }
    }

    /// Returns the `CAPTURE` source register or upvalue index.
    #[must_use]
    pub fn capture_source(&self) -> Option<u8> {
        self.capture_type().map(|_| self.b)
    }

    /// Returns the number of instruction words this instruction occupies,
    /// counting its header word plus any AUX words.
    #[must_use]
    pub fn word_len(&self) -> u32 {
        1 + u32::from(self.aux.is_some())
    }

    /// Returns `true` when the pre-decoded operand fields match a fresh
    /// re-decode of `header`, i.e. the instruction upholds the type's
    /// header/field invariant.
    #[must_use]
    pub fn is_header_consistent(&self) -> bool {
        Self::from_words(self.header, self.aux).is_some_and(|decoded| decoded == *self)
    }
}

/// Returns the total number of instruction words spanned by `code`.
#[must_use]
pub fn code_word_count(code: &[Instruction]) -> u32 {
    code.iter().map(Instruction::word_len).sum()
}

/// Returns the absolute instruction-word offset for each decoded instruction.
#[must_use]
pub fn instruction_word_offsets(code: &[Instruction]) -> Vec<u32> {
    let mut next = 0u32;
    code.iter()
        .map(|instruction| {
            let current = next;
            next += instruction.word_len();
            current
        })
        .collect()
}

/// Resolves one decoded instruction's jump target to another decoded instruction index.
#[must_use]
pub fn jump_target_instruction_index(
    code: &[Instruction],
    instruction_index: usize,
) -> Option<usize> {
    let offsets = instruction_word_offsets(code);
    let instruction = code.get(instruction_index)?;
    let source_word = *offsets.get(instruction_index)?;
    let target = instruction.jump_target_word(source_word)?;
    offsets.iter().position(|word| *word as i32 == target)
}

/// Constant table entry.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Constant {
    /// Nil.
    Nil,
    /// Boolean.
    Boolean {
        /// Boolean value.
        value: bool,
    },
    /// Floating-point number, stored by exact bits.
    Number {
        /// Little-endian f64 bit pattern interpreted numerically by upstream.
        bits: u64,
    },
    /// One-based string-table id.
    String {
        /// String table id.
        string: u32,
    },
    /// Packed import id.
    Import {
        /// Import id.
        import_id: u32,
    },
    /// Table shape.
    Table {
        /// Constant ids used as keys.
        keys: Vec<u32>,
    },
    /// Closure proto reference.
    Closure {
        /// Proto id.
        proto: u32,
    },
    /// Vector, stored by exact f32 bits.
    Vector {
        /// Four f32 bit patterns.
        bits: [u32; 4],
    },
    /// Double-precision vector, stored by exact f64 bits.
    VectorDouble {
        /// Four f64 bit patterns.
        bits: [u64; 4],
    },
    /// Table shape with constant payloads.
    TableWithConstants {
        /// Key/value entries.
        entries: Vec<TableEntry>,
    },
    /// 64-bit integer.
    Integer {
        /// Integer value.
        value: i64,
    },
    /// Class shape.
    ClassShape {
        /// Shape payload.
        shape: ClassShape,
    },
}

/// One table shape entry with a pre-filled constant.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableEntry {
    /// Key constant id.
    pub key: u32,
    /// Value constant id, or -1 sentinel.
    pub value: i32,
}

/// Class-shape constant payload.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassShape {
    /// Class-name string id.
    pub class_name: u32,
    /// Property-name string ids.
    pub property_names: Vec<u32>,
    /// Method-name string ids.
    pub method_names: Vec<u32>,
}

/// Wire-preserving line info.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LineInfo {
    /// Log2 of encoded line span.
    pub log2_span: u8,
    /// Per-instruction unsigned delta differences.
    pub delta_bytes: Vec<u8>,
    /// Per-span signed baseline differences.
    pub baseline_deltas: Vec<i32>,
}

impl LineInfo {
    /// Encodes absolute source lines using upstream's line-info span packing.
    #[must_use]
    pub(crate) fn from_line_numbers(lines: &[i32]) -> Option<Self> {
        if lines.is_empty() {
            return None;
        }

        let mut span = 1usize << 24;
        let mut offset = 0usize;
        while offset < lines.len() {
            let mut next = offset;
            let mut min = lines[offset];
            let mut max = lines[offset];

            while next < lines.len() && next < offset + span {
                min = min.min(lines[next]);
                max = max.max(lines[next]);
                if max - min > 255 {
                    break;
                }
                next += 1;
            }

            if next < lines.len() && next - offset < span {
                span = 1usize << line_info_log2(next - offset);
            }

            offset += span;
        }

        let log2_span = line_info_log2(span) as u8;
        let baseline_size = (lines.len() - 1) / span + 1;
        let mut baselines = Vec::with_capacity(baseline_size);
        for chunk in lines.chunks(span) {
            baselines.push(*chunk.iter().min().expect("non-empty chunk"));
        }

        let mut delta_bytes = Vec::with_capacity(lines.len());
        let mut last_offset = 0u8;
        for (index, line) in lines.iter().enumerate() {
            let baseline = baselines[index >> usize::from(log2_span)];
            let delta = *line - baseline;
            debug_assert!((0..=255).contains(&delta));
            let delta = delta as u8;
            delta_bytes.push(delta.wrapping_sub(last_offset));
            last_offset = delta;
        }

        let mut baseline_deltas = Vec::with_capacity(baseline_size);
        let mut last_line = 0;
        for baseline in baselines {
            baseline_deltas.push(baseline - last_line);
            last_line = baseline;
        }

        Some(Self {
            log2_span,
            delta_bytes,
            baseline_deltas,
        })
    }

    /// Decodes wire line-info payloads into absolute source lines.
    #[must_use]
    pub fn to_line_numbers(&self) -> Option<Vec<i32>> {
        let mut baselines = Vec::with_capacity(self.baseline_deltas.len());
        let mut baseline = 0i32;
        for delta in &self.baseline_deltas {
            // Line numbers wrap rather than panic on a hostile delta stream.
            baseline = baseline.wrapping_add(*delta);
            baselines.push(baseline);
        }

        let mut lines = Vec::with_capacity(self.delta_bytes.len());
        let mut last_offset = 0u8;
        for (index, byte) in self.delta_bytes.iter().enumerate() {
            let baseline = *baselines.get(index >> usize::from(self.log2_span))?;
            let offset = last_offset.wrapping_add(*byte);
            lines.push(baseline.wrapping_add(i32::from(offset)));
            last_offset = offset;
        }

        Some(lines)
    }
}

fn line_info_log2(value: usize) -> usize {
    debug_assert!(value > 0);

    let mut result = 0;
    while value >= (2usize << result) {
        result += 1;
    }
    result
}

/// Debug local and upvalue names.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugInfo {
    /// Local debug entries.
    pub locals: Vec<DebugLocal>,
    /// One-based string-table ids for upvalue names.
    pub upvalues: Vec<u32>,
}

/// One local debug-info entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugLocal {
    /// One-based string-table id.
    pub name: u32,
    /// Inclusive start pc.
    pub start_pc: u32,
    /// Exclusive end pc.
    pub end_pc: u32,
    /// Register.
    pub register: u8,
}

/// Feedback slot metadata.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackSlot {
    /// Slot type.
    pub kind: FeedbackType,
    /// Instruction pc associated with the slot.
    pub pc: u32,
}

#[cfg(test)]
mod tests {
    use super::{
        CaptureType, Instruction, LineInfo, Opcode, instruction_word_offsets,
        jump_target_instruction_index,
    };

    #[test]
    fn jump_targets_are_word_relative() {
        let jump_next = Instruction::ad(Opcode::Jump, 0, 0);
        assert_eq!(jump_next.jump_offset_words(), Some(0));
        assert_eq!(jump_next.jump_target_word(10), Some(11));

        let jump_back = Instruction::ad(Opcode::JumpBack, 0, -3);
        assert_eq!(jump_back.jump_target_word(10), Some(8));

        let jump_x = Instruction::from_words(
            Opcode::JumpX.byte() as u32 | ((-5_i32 as u32 & 0x00ff_ffff) << 8),
            None,
        )
        .expect("jumpx");
        assert_eq!(jump_x.jump_offset_words(), Some(-5));
        assert_eq!(jump_x.jump_target_word(10), Some(6));
    }

    #[test]
    fn jump_targets_count_aux_words() {
        let compare = Instruction::abc_with_aux(Opcode::JumpIfEq, 0, 1, 0, Some(0));
        assert_eq!(compare.jump_offset_words(), Some(1));
        assert_eq!(compare.jump_target_word(4), Some(6));

        let code = vec![
            Instruction::ad(Opcode::LoadN, 0, 1),
            compare,
            Instruction::ad(Opcode::LoadN, 1, 2),
            Instruction::abc(Opcode::Return, 0, 1, 0),
        ];
        assert_eq!(instruction_word_offsets(&code), vec![0, 1, 3, 4]);
        assert_eq!(jump_target_instruction_index(&code, 1), Some(2));

        let compare_to_return = Instruction::abc_with_aux(Opcode::JumpIfEq, 0, 2, 0, Some(0));
        let code = vec![
            Instruction::ad(Opcode::LoadN, 0, 1),
            compare_to_return,
            Instruction::ad(Opcode::LoadN, 1, 2),
            Instruction::abc(Opcode::Return, 0, 1, 0),
        ];
        assert_eq!(jump_target_instruction_index(&code, 1), Some(3));
    }

    #[test]
    fn short_jump_operands_are_exposed() {
        let loadb = Instruction::abc(Opcode::LoadB, 0, 1, 2);
        assert_eq!(loadb.jump_offset_words(), Some(2));
        assert_eq!(loadb.jump_target_word(7), Some(10));

        let fastcall = Instruction::abc(Opcode::FastCall2K, 18, 1, 3);
        assert_eq!(fastcall.jump_offset_words(), Some(3));
        assert_eq!(fastcall.jump_target_word(7), Some(11));

        let not_a_jump = Instruction::ad(Opcode::LoadN, 0, 5);
        assert_eq!(not_a_jump.jump_offset_words(), None);
        assert_eq!(not_a_jump.jump_target_word(0), None);
    }

    #[test]
    fn capture_operands_are_exposed() {
        let by_value = Instruction::abc(Opcode::Capture, CaptureType::Val as u8, 3, 0);
        assert_eq!(by_value.capture_type(), Some(CaptureType::Val));
        assert_eq!(by_value.capture_source(), Some(3));

        let by_reference = Instruction::abc(Opcode::Capture, CaptureType::Ref as u8, 4, 0);
        assert_eq!(by_reference.capture_type(), Some(CaptureType::Ref));
        assert_eq!(by_reference.capture_source(), Some(4));

        let upvalue = Instruction::abc(Opcode::Capture, CaptureType::Upval as u8, 1, 0);
        assert_eq!(upvalue.capture_type(), Some(CaptureType::Upval));
        assert_eq!(upvalue.capture_source(), Some(1));

        let invalid = Instruction::abc(Opcode::Capture, 99, 1, 0);
        assert_eq!(invalid.capture_type(), None);
        assert_eq!(invalid.capture_source(), None);

        let not_capture = Instruction::ad(Opcode::LoadN, 0, 5);
        assert_eq!(not_capture.capture_type(), None);
        assert_eq!(not_capture.capture_source(), None);
    }

    #[test]
    fn line_info_uses_large_span_when_deltas_fit() {
        let line_info = LineInfo::from_line_numbers(&[10, 10, 20]).expect("line info");
        assert_eq!(line_info.log2_span, 24);
        assert_eq!(line_info.delta_bytes, vec![0, 0, 10]);
        assert_eq!(line_info.baseline_deltas, vec![10]);
        assert_eq!(
            line_info.to_line_numbers().expect("line numbers"),
            vec![10, 10, 20]
        );
    }

    #[test]
    fn line_info_shrinks_span_before_8_bit_delta_overflows() {
        let lines = [1, 2, 300, 301, 302];
        let line_info = LineInfo::from_line_numbers(&lines).expect("line info");

        assert_eq!(line_info.log2_span, 1);
        assert_eq!(line_info.delta_bytes, vec![0, 1, 255, 1, 255]);
        assert_eq!(line_info.baseline_deltas, vec![1, 299, 2]);
        assert_eq!(line_info.to_line_numbers().expect("line numbers"), lines);
    }
}
