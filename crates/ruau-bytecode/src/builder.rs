//! Small order-preserving chunk builder used by the first compiler slice.

use crate::{
    BytecodeChunk, ClassShape, Constant, DebugInfo, DebugLocal, Instruction, LineInfo, Proto,
    TableEntry, TypeInfo, codec::write_varint, instruction_word_offsets, opcodes::Opcode,
    types::code_word_count,
};

/// Version emitted by the pinned upstream compiler without bytecode flags.
pub const DEFAULT_VERSION: u8 = 6;
/// Type encoding version emitted by the pinned upstream compiler.
pub const DEFAULT_TYPE_VERSION: u8 = 3;

#[derive(Clone, Copy)]
struct TypedLocal {
    type_tag: u8,
    reg: u8,
    startpc: u32,
    endpc: u32,
}

#[derive(Default)]
pub struct ProtoWork {
    child_protos: Vec<u32>,
    constants: Vec<Constant>,
    code: Vec<Instruction>,
    code_lines: Vec<i32>,
    feedback_slots: Vec<crate::FeedbackSlot>,
    max_stack_size: u8,
    current_line: i32,
    implicit_return_line_delta: u8,
    implicit_return_line_base: Option<i32>,
    proto_flags: u8,
    implicit_prepvarargs: bool,
    function_type_info: Vec<u8>,
    typed_upvalues: Vec<u8>,
    typed_locals: Vec<TypedLocal>,
    debug_locals: Vec<DebugLocal>,
    debug_upvalues: Vec<u32>,
}

pub struct ProtoMetadata {
    pub(crate) num_params: u8,
    pub(crate) num_upvalues: u8,
    pub(crate) is_vararg: bool,
    pub(crate) flags: u8,
    pub(crate) line_defined: u32,
    pub(crate) debug_name: u32,
    pub(crate) debug_level: u8,
}

impl ProtoWork {
    fn new() -> Self {
        Self {
            current_line: 1,
            proto_flags: 2,
            implicit_prepvarargs: true,
            ..Self::default()
        }
    }

    fn new_function() -> Self {
        Self {
            current_line: 1,
            proto_flags: 2,
            ..Self::default()
        }
    }

    pub(crate) fn fold_jumps(&mut self) {
        let offsets = instruction_word_offsets(&self.code);

        for index in 0..self.code.len() {
            if !is_foldable_jump_d(self.code[index].opcode) {
                continue;
            }

            let source_word = offsets[index];
            let Some(mut target_word) = self.code[index].jump_target_word(source_word) else {
                continue;
            };
            if target_word < 0 {
                continue;
            }

            let Some(mut target_index) = instruction_index_for_word(&offsets, target_word as u32)
            else {
                continue;
            };

            while self.code[target_index].opcode == Opcode::Jump && self.code[target_index].d >= 0 {
                let Some(next_target) =
                    self.code[target_index].jump_target_word(offsets[target_index])
                else {
                    break;
                };
                if next_target < 0 {
                    break;
                }
                let Some(next_index) = instruction_index_for_word(&offsets, next_target as u32)
                else {
                    break;
                };
                target_word = next_target;
                target_index = next_index;
            }
            if self.code[index].opcode != Opcode::Jump
                && self.code[target_index].opcode == Opcode::Return
            {
                while target_index + 1 < self.code.len()
                    && self.code[target_index + 1] == self.code[target_index]
                {
                    target_index += 1;
                    target_word = offsets[target_index] as i32;
                }
            }

            let offset = target_word - source_word as i32 - 1;
            if self.code[index].opcode == Opcode::Jump
                && self.code[target_index].opcode == Opcode::Return
            {
                self.code[index] = self.code[target_index];
            } else if let Ok(offset) = i16::try_from(offset) {
                patch_jump_d_instruction(&mut self.code[index], offset);
            }
        }
    }

    pub(crate) fn into_proto(self, metadata: &ProtoMetadata) -> Proto {
        let line_info = if metadata.debug_level >= 1 {
            LineInfo::from_line_numbers(&self.code_lines)
        } else {
            None
        };
        let debug_info = (metadata.debug_level >= 2
            && (!self.debug_locals.is_empty() || !self.debug_upvalues.is_empty()))
        .then_some(DebugInfo {
            locals: self.debug_locals,
            upvalues: self.debug_upvalues,
        });

        Proto {
            max_stack_size: self.max_stack_size,
            num_params: metadata.num_params,
            num_upvalues: metadata.num_upvalues,
            is_vararg: u8::from(metadata.is_vararg),
            flags: metadata.flags,
            type_info: TypeInfo {
                raw: type_info_payload(
                    &self.function_type_info,
                    &self.typed_upvalues,
                    &self.typed_locals,
                ),
            },
            code: self.code,
            constants: self.constants,
            child_protos: self.child_protos,
            line_defined: metadata.line_defined,
            debug_name: metadata.debug_name,
            line_info,
            debug_info,
            feedback_slots: self.feedback_slots,
        }
    }
}

pub struct ChunkBuilder {
    strings: Vec<Vec<u8>>,
    protos: Vec<Proto>,
    current: ProtoWork,
    bytecode_version: u8,
}

impl ChunkBuilder {
    pub(crate) fn new() -> Self {
        Self {
            bytecode_version: DEFAULT_VERSION,
            current: ProtoWork::new(),
            strings: Vec::new(),
            protos: Vec::new(),
        }
    }

    pub(crate) fn begin_proto(&mut self) -> ProtoWork {
        std::mem::replace(&mut self.current, ProtoWork::new_function())
    }

    pub(crate) fn end_proto(&mut self, previous: ProtoWork) -> ProtoWork {
        std::mem::replace(&mut self.current, previous)
    }

    pub(crate) fn set_bytecode_version(&mut self, version: u8) {
        self.bytecode_version = version;
    }

    pub(crate) fn set_max_stack_size(&mut self, value: u8) {
        self.current.max_stack_size = self.current.max_stack_size.max(value);
    }

    pub(crate) fn set_proto_flags(&mut self, value: u8) {
        self.current.proto_flags = value;
    }

    pub(crate) fn set_implicit_return_line_delta(&mut self, value: u8) {
        self.current.implicit_return_line_delta = value;
    }

    pub(crate) fn set_implicit_return_line_base(&mut self, line: u32) {
        self.current.implicit_return_line_base = Some(line as i32);
    }

    pub(crate) fn set_debug_line(&mut self, line: u32) {
        self.current.current_line = line as i32;
    }

    pub(crate) fn add_number(&mut self, value: f64) -> u32 {
        let bits = value.to_bits();
        self.add_constant(Constant::Number { bits })
    }

    pub(crate) fn add_string(&mut self, value: &str) -> u32 {
        let bytes = decode_ast_string_bytes(value);
        if let Some(index) = self
            .strings
            .iter()
            .position(|string| string.as_slice() == bytes)
        {
            return (index + 1) as u32;
        }
        self.strings.push(bytes);
        self.strings.len() as u32
    }

    pub(crate) fn add_string_constant(&mut self, value: &str) -> u32 {
        let string = self.add_string(value);
        self.add_constant(Constant::String { string })
    }

    pub(crate) fn add_nil(&mut self) -> u32 {
        self.add_constant(Constant::Nil)
    }

    pub(crate) fn add_boolean(&mut self, value: bool) -> u32 {
        self.add_constant(Constant::Boolean { value })
    }

    pub(crate) fn add_vector_bits(&mut self, bits: [u32; 4]) -> u32 {
        self.add_constant(Constant::Vector { bits })
    }

    pub(crate) fn add_integer(&mut self, value: i64) -> u32 {
        self.add_constant(Constant::Integer { value })
    }

    pub(crate) fn add_import(&mut self, import_id: u32) -> u32 {
        self.add_constant(Constant::Import { import_id })
    }

    pub(crate) fn add_table_shape(&mut self, keys: Vec<u32>) -> u32 {
        self.add_constant(Constant::Table { keys })
    }

    pub(crate) fn add_table_with_constants(&mut self, entries: Vec<TableEntry>) -> u32 {
        self.add_constant(Constant::TableWithConstants { entries })
    }

    pub(crate) fn add_class_shape(&mut self, shape: ClassShape) -> u32 {
        self.add_constant(Constant::ClassShape { shape })
    }

    pub(crate) fn add_closure(&mut self, proto: u32) -> u32 {
        self.add_constant(Constant::Closure { proto })
    }

    pub(crate) fn add_child_proto(&mut self, proto: u32) -> i16 {
        if let Some(index) = self
            .current
            .child_protos
            .iter()
            .position(|existing| *existing == proto)
        {
            return index as i16;
        }
        let index = self.current.child_protos.len();
        self.current.child_protos.push(proto);
        index as i16
    }

    pub(crate) fn add_proto(&mut self, proto: Proto) -> u32 {
        let id = self.protos.len() as u32;
        self.protos.push(proto);
        id
    }

    pub(crate) fn emit(&mut self, instruction: Instruction) -> usize {
        let line = self.current.current_line;
        self.emit_at_line(instruction, line)
    }

    pub(crate) fn emit_implicit_return(&mut self) -> usize {
        self.emit_at_line(
            Instruction::abc(Opcode::Return, 0, 1, 0),
            self.implicit_return_line(),
        )
    }

    pub(crate) fn patch_ad(&mut self, index: usize, opcode: Opcode, a: u8, d: i16) {
        self.current.code[index] = Instruction::ad(opcode, a, d);
    }

    pub(crate) fn patch_ad_with_aux(
        &mut self,
        index: usize,
        opcode: Opcode,
        a: u8,
        d: i16,
        aux: Option<u32>,
    ) {
        let b = (d as u16 & 0xff) as u8;
        let c = ((d as u16 >> 8) & 0xff) as u8;
        self.current.code[index] = Instruction::abc_with_aux(opcode, a, b, c, aux);
    }

    pub(crate) fn patch_skip_c_to_current(&mut self, index: usize) -> bool {
        let source = self.instruction_word_offset(index);
        let target = self.current_word_offset();
        let Some(offset) = target.checked_sub(source + 1) else {
            return false;
        };
        let Ok(offset) = u8::try_from(offset) else {
            return false;
        };
        let instruction = &self.current.code[index];
        debug_assert!(matches!(
            instruction.opcode,
            Opcode::FastCall
                | Opcode::FastCall1
                | Opcode::FastCall2
                | Opcode::FastCall2K
                | Opcode::FastCall3
        ));
        self.current.code[index] = Instruction::abc_with_aux(
            instruction.opcode,
            instruction.a,
            instruction.b,
            offset,
            instruction.aux,
        );
        true
    }

    pub(crate) fn current_word_offset(&self) -> u32 {
        code_word_count(&self.current.code)
    }

    pub(crate) fn current_type_info_pc(&self) -> u32 {
        self.current_word_offset()
            + u32::from(self.current.implicit_prepvarargs && self.needs_implicit_prepvarargs())
    }

    pub(crate) fn instruction_word_offset(&self, index: usize) -> u32 {
        code_word_count(&self.current.code[..index])
    }

    pub(crate) fn push_local_type_info(&mut self, type_tag: u8, reg: u8, startpc: u32, endpc: u32) {
        self.current.typed_locals.push(TypedLocal {
            type_tag,
            reg,
            startpc,
            endpc,
        });
    }

    pub(crate) fn set_function_type_info(&mut self, type_info: Vec<u8>) {
        self.current.function_type_info = type_info;
    }

    pub(crate) fn push_upvalue_type_info(&mut self, type_tag: u8) {
        self.current.typed_upvalues.push(type_tag);
    }

    pub(crate) fn push_debug_local(&mut self, name: u32, start_pc: u32, end_pc: u32, register: u8) {
        self.current.debug_locals.push(DebugLocal {
            name,
            start_pc,
            end_pc,
            register,
        });
    }

    pub(crate) fn push_debug_upvalue(&mut self, name: u32) {
        self.current.debug_upvalues.push(name);
    }

    pub(crate) fn current_code(&self) -> &[Instruction] {
        &self.current.code
    }

    pub(crate) fn push_feedback_slot(&mut self, slot: crate::FeedbackSlot) -> u32 {
        let index = self.current.feedback_slots.len() as u32;
        self.current.feedback_slots.push(slot);
        index
    }

    pub(crate) fn proto_flags(&self) -> u8 {
        self.current.proto_flags
    }

    pub(crate) fn max_stack_size(&self) -> u8 {
        self.current.max_stack_size
    }

    fn emit_at_line(&mut self, instruction: Instruction, line: i32) -> usize {
        self.current
            .code_lines
            .extend(std::iter::repeat_n(line, instruction.word_len() as usize));
        let index = self.current.code.len();
        self.current.code.push(instruction);
        index
    }

    fn implicit_return_line(&self) -> i32 {
        self.current
            .implicit_return_line_base
            .or_else(|| self.current.code_lines.last().copied())
            .unwrap_or(1)
            .saturating_add(i32::from(self.current.implicit_return_line_delta))
    }

    fn needs_implicit_prepvarargs(&self) -> bool {
        if !self.current.implicit_prepvarargs {
            return false;
        }
        self.current
            .code
            .first()
            .is_none_or(|instruction| instruction.opcode != Opcode::PrepVarargs)
    }

    pub(crate) fn finish(mut self, debug_level: u8, fold_jumps: bool) -> BytecodeChunk {
        if self.needs_implicit_prepvarargs() {
            self.current
                .code
                .insert(0, Instruction::abc(Opcode::PrepVarargs, 0, 0, 0));
            self.current.code_lines.insert(0, 1);
        }
        let inserted_implicit_return = self.current.code.is_empty()
            || self
                .current
                .code
                .last()
                .is_some_and(|instruction| instruction.opcode != Opcode::Return);
        if inserted_implicit_return {
            self.emit_implicit_return();
        }
        if fold_jumps {
            self.current.fold_jumps();
        }
        let flags = self.current.proto_flags;
        let current = std::mem::replace(&mut self.current, ProtoWork::new());
        let proto = current.into_proto(&ProtoMetadata {
            num_params: 0,
            num_upvalues: 0,
            is_vararg: true,
            flags,
            line_defined: 1,
            debug_name: 0,
            debug_level,
        });
        let main_proto = self.protos.len() as u32;
        self.protos.push(proto);
        BytecodeChunk::Valid {
            bytecode_version: self.bytecode_version,
            type_version: DEFAULT_TYPE_VERSION,
            strings: std::mem::take(&mut self.strings),
            userdata_type_mappings: Vec::new(),
            userdata_mapping_terminator: 0,
            protos: self.protos,
            main_proto,
        }
    }

    fn add_constant(&mut self, constant: Constant) -> u32 {
        if let Some(index) = self
            .current
            .constants
            .iter()
            .position(|existing| *existing == constant)
        {
            return index as u32;
        }
        let index = self.current.constants.len();
        self.current.constants.push(constant);
        index as u32
    }
}

fn instruction_index_for_word(offsets: &[u32], word: u32) -> Option<usize> {
    offsets.iter().position(|offset| *offset == word)
}

fn is_foldable_jump_d(opcode: Opcode) -> bool {
    matches!(
        opcode,
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
            | Opcode::CmpProto
    )
}

fn patch_jump_d_instruction(instruction: &mut Instruction, offset: i16) {
    if instruction.aux.is_none() {
        *instruction = Instruction::ad(instruction.opcode, instruction.a, offset);
    } else {
        *instruction = Instruction::abc_with_aux(
            instruction.opcode,
            instruction.a,
            (offset as u16 & 0xff) as u8,
            ((offset as u16 >> 8) & 0xff) as u8,
            instruction.aux,
        );
    }
}

fn type_info_payload(
    function_type_info: &[u8],
    typed_upvalues: &[u8],
    typed_locals: &[TypedLocal],
) -> Vec<u8> {
    if function_type_info.is_empty() && typed_upvalues.is_empty() && typed_locals.is_empty() {
        return Vec::new();
    }

    let mut bytes = Vec::new();
    write_varint(&mut bytes, function_type_info.len() as u64);
    write_varint(&mut bytes, typed_upvalues.len() as u64);
    write_varint(&mut bytes, typed_locals.len() as u64);
    bytes.extend_from_slice(function_type_info);
    bytes.extend_from_slice(typed_upvalues);
    for local in typed_locals {
        bytes.push(local.type_tag);
        bytes.push(local.reg);
        write_varint(&mut bytes, u64::from(local.startpc));
        write_varint(
            &mut bytes,
            u64::from(local.endpc.saturating_sub(local.startpc)),
        );
    }
    bytes
}

/// Decodes the AST's byte-preserving string encoding back to the raw Luau string bytes a
/// constant must hold. Luau strings are byte strings, but the AST stores them in a Rust `String`
/// (ruau-ast `ast_string_from_bytes`): an ASCII byte is its own char, and a non-ASCII byte is
/// the marker `U+FFFF` followed by `"ff"` and the byte's two hex digits. Storing `value.as_bytes()`
/// directly would leak that marker form into the constant pool — e.g. `"\255"` became the seven
/// bytes of `U+FFFF` plus `"ffff"` instead of the single byte `0xFF`. A string without any marker
/// (every ordinary literal and identifier) decodes to its own bytes, so this is a no-op there.
fn decode_ast_string_bytes(value: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\u{ffff}' {
            // The marker is followed by "ff" then the original byte's two hex digits.
            let tail: String = chars.by_ref().take(4).collect();
            if let Some(byte) = tail
                .get(2..4)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            {
                bytes.push(byte);
            }
        } else {
            // Every other char the encoding produces is ASCII, so it is its own byte.
            bytes.push(ch as u8);
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::{ChunkBuilder, ProtoWork};
    use crate::{BytecodeChunk, Constant, Instruction, TableEntry, opcodes::Opcode};

    #[test]
    fn preserves_constant_and_child_proto_insertion_order() {
        let mut builder = ChunkBuilder::new();

        let string = builder.add_string_constant("key");
        assert_eq!(builder.add_string_constant("key"), string);

        let import = builder.add_import(0x0102_0304);
        assert_eq!(builder.add_import(0x0102_0304), import);

        let table = builder.add_table_shape(vec![string]);
        assert_eq!(builder.add_table_shape(vec![string]), table);

        let table_with_constants = builder.add_table_with_constants(vec![TableEntry {
            key: string,
            value: import as i32,
        }]);
        assert_eq!(
            builder.add_table_with_constants(vec![TableEntry {
                key: string,
                value: import as i32,
            }]),
            table_with_constants
        );

        let closure = builder.add_closure(7);
        assert_eq!(builder.add_closure(7), closure);

        assert_eq!(builder.add_child_proto(7), 0);
        assert_eq!(builder.add_child_proto(7), 0);
        assert_eq!(builder.add_child_proto(9), 1);
        builder.emit(Instruction::abc(Opcode::Return, 0, 1, 0));

        let BytecodeChunk::Valid {
            strings, protos, ..
        } = builder.finish(0, false)
        else {
            panic!("expected valid chunk");
        };

        assert_eq!(strings, vec![b"key".to_vec()]);
        assert_eq!(
            protos[0].constants,
            vec![
                Constant::String { string: 1 },
                Constant::Import {
                    import_id: 0x0102_0304,
                },
                Constant::Table { keys: vec![string] },
                Constant::TableWithConstants {
                    entries: vec![TableEntry {
                        key: string,
                        value: import as i32,
                    }],
                },
                Constant::Closure { proto: 7 },
            ]
        );
        assert_eq!(protos[0].child_protos, vec![7, 9]);
    }

    #[test]
    fn patches_fastcall_skip_c_by_instruction_word_offset() {
        let mut builder = ChunkBuilder::new();

        let fastcall = builder.emit(Instruction::abc_with_aux(
            Opcode::FastCall2,
            29,
            1,
            0,
            Some(2),
        ));
        builder.emit(Instruction::abc(Opcode::Move, 3, 1, 0));
        builder.emit(Instruction::abc_with_aux(
            Opcode::GetImport,
            2,
            4,
            0,
            Some(0x4000_0000),
        ));

        assert!(builder.patch_skip_c_to_current(fastcall));
        assert_eq!(builder.current_code()[fastcall].c, 4);
    }

    #[test]
    fn folds_forward_jump_chains_and_jumps_to_return() {
        let mut proto = ProtoWork::new();
        proto.code = vec![
            Instruction::ad(Opcode::JumpIfNot, 1, 2),
            Instruction::abc(Opcode::LoadN, 0, 1, 0),
            Instruction::ad(Opcode::Jump, 0, 0),
            Instruction::ad(Opcode::Jump, 0, 1),
            Instruction::abc(Opcode::LoadN, 0, 0, 0),
            Instruction::abc(Opcode::Return, 0, 2, 0),
        ];

        proto.fold_jumps();

        assert_eq!(proto.code[0].d, 4);
        assert_eq!(proto.code[2], Instruction::abc(Opcode::Return, 0, 2, 0));
        assert_eq!(proto.code[3], Instruction::abc(Opcode::Return, 0, 2, 0));
    }
}
