//! Conservative semantic validation for decoded bytecode chunks.

#[cfg(test)]
use std::cell::Cell;

use serde::{Deserialize, Serialize};

use crate::{
    BytecodeChunk, Constant, Instruction, Proto,
    opcodes::{
        BuiltinFunction, CaptureType, FORGLOOP_INEXT_BIT, IMPORT_PATH_COMPONENT_MASK,
        IMPORT_PATH_COUNT_SHIFT, JUMPX_K_INDEX_MASK, Opcode, import_component_shift,
    },
};

/// One structural bytecode validation failure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationError {
    /// Proto index containing the failure, if proto-local.
    pub proto_index: Option<usize>,
    /// Instruction index containing the failure, if instruction-local.
    pub instruction_index: Option<usize>,
    /// Error category.
    pub kind: ValidationErrorKind,
    /// Human-readable diagnostic.
    pub message: String,
}

/// Structural bytecode validation error categories.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ValidationErrorKind {
    /// A proto id does not refer to an existing proto.
    InvalidProtoReference,
    /// A string id does not refer to the string table or the empty string.
    InvalidStringReference,
    /// A constant id does not refer to the proto constant table.
    InvalidConstantReference,
    /// An instruction's pre-decoded operand fields do not match its header word.
    InconsistentInstruction,
    /// An instruction has the wrong static AUX arity for its opcode.
    InvalidAuxArity,
    /// An AUX payload references impossible data.
    InvalidAuxPayload,
    /// A jump target is outside the proto or lands inside an AUX word.
    InvalidJumpTarget,
    /// A register operand exceeds the proto stack size.
    InvalidRegister,
    /// Line info is malformed for the proto instruction words.
    InvalidLineInfo,
    /// Debug info is malformed for the proto.
    InvalidDebugInfo,
    /// Feedback metadata is malformed for the proto.
    InvalidFeedbackSlot,
    /// Closure capture instructions do not match the referenced child proto.
    InvalidClosureCapture,
    /// Proto metadata is malformed.
    InvalidProtoMetadata,
}

/// Returns all structural validation errors in `chunk`.
#[must_use]
pub fn validate_chunk(chunk: &BytecodeChunk) -> Vec<ValidationError> {
    validate_chunk_inner(chunk).errors
}

#[cfg(test)]
fn validate_chunk_with_boundary_probe_count(
    chunk: &BytecodeChunk,
) -> (Vec<ValidationError>, usize) {
    let result = validate_chunk_inner(chunk);
    (result.errors, result.boundary_probes)
}

fn validate_chunk_inner(chunk: &BytecodeChunk) -> ValidationResult {
    let BytecodeChunk::Valid {
        strings,
        userdata_type_mappings,
        protos,
        main_proto,
        ..
    } = chunk
    else {
        return ValidationResult {
            errors: Vec::new(),
            #[cfg(test)]
            boundary_probes: 0,
        };
    };

    let mut validator = Validator {
        strings,
        protos,
        errors: Vec::new(),
        #[cfg(test)]
        boundary_probes: 0,
    };

    if !proto_id_is_valid(*main_proto, protos) {
        validator.push_chunk(
            ValidationErrorKind::InvalidProtoReference,
            format!(
                "main proto id {main_proto} is outside {} protos",
                protos.len()
            ),
        );
    }

    for mapping in userdata_type_mappings {
        validator.check_string_id(None, None, mapping.name, "userdata type mapping name");
    }

    for (proto_index, proto) in protos.iter().enumerate() {
        validator.validate_proto(proto_index, proto);
    }

    ValidationResult {
        errors: validator.errors,
        #[cfg(test)]
        boundary_probes: validator.boundary_probes,
    }
}

struct ValidationResult {
    errors: Vec<ValidationError>,
    #[cfg(test)]
    boundary_probes: usize,
}

struct Validator<'a> {
    strings: &'a [Vec<u8>],
    protos: &'a [Proto],
    errors: Vec<ValidationError>,
    #[cfg(test)]
    boundary_probes: usize,
}

struct InstructionBoundaryIndex {
    word_offsets: Vec<u32>,
    instruction_words: Vec<bool>,
    code_words: u32,
    #[cfg(test)]
    probes: Cell<usize>,
}

impl InstructionBoundaryIndex {
    fn new(code: &[Instruction]) -> Result<Self, &'static str> {
        let mut word_offsets = Vec::with_capacity(code.len());
        let mut next_word = 0u32;
        for instruction in code {
            word_offsets.push(next_word);
            next_word = next_word
                .checked_add(instruction.word_len())
                .ok_or("proto code is too large")?;
        }

        let word_count = usize::try_from(next_word).map_err(|_| "proto code is too large")?;
        let mut instruction_words = vec![false; word_count];
        for &word in &word_offsets {
            let word = usize::try_from(word).map_err(|_| "proto code is too large")?;
            instruction_words[word] = true;
        }

        Ok(Self {
            word_offsets,
            instruction_words,
            code_words: next_word,
            #[cfg(test)]
            probes: Cell::new(0),
        })
    }

    fn code_words(&self) -> u32 {
        self.code_words
    }

    fn word_offset(&self, instruction_index: usize) -> Option<u32> {
        self.word_offsets.get(instruction_index).copied()
    }

    fn is_instruction_word(&self, word: u32) -> bool {
        #[cfg(test)]
        self.record_probe();

        self.contains_instruction_word(word)
    }

    fn is_instruction_word_i32(&self, word: i32) -> bool {
        #[cfg(test)]
        self.record_probe();

        let Ok(word) = u32::try_from(word) else {
            return false;
        };
        self.contains_instruction_word(word)
    }

    fn contains_instruction_word(&self, word: u32) -> bool {
        let Ok(word) = usize::try_from(word) else {
            return false;
        };
        self.instruction_words.get(word).copied().unwrap_or(false)
    }

    #[cfg(test)]
    fn record_probe(&self) {
        self.probes.set(self.probes.get() + 1);
    }

    #[cfg(test)]
    fn probe_count(&self) -> usize {
        self.probes.get()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegisterPolicy {
    None,
    FastCall0,
    FastCall1,
    FastCall2,
    FastCall3,
    Capture,
    Call,
    Return,
    NameCall,
    NumericFor,
    GenericForLoop,
    LoadNil,
    SetList,
    CompareJump,
    A,
    AB,
    #[allow(clippy::upper_case_acronyms)]
    ABC,
    AC,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReferencePolicy {
    None,
    DConstant,
    LoadKx,
    AuxStringConstant,
    NewClosureChild,
    DupClosureConstant,
    CConstant,
    BConstant,
    FastCall2K,
    JumpXEqKConstant,
    CallFb,
    BUpvalue,
}

fn register_policy(opcode: Opcode) -> RegisterPolicy {
    match opcode {
        Opcode::Nop
        | Opcode::Break
        | Opcode::Jump
        | Opcode::JumpBack
        | Opcode::Coverage
        | Opcode::NativeCall
        | Opcode::ForGPrepInext
        | Opcode::ForGPrepNext
        | Opcode::ForGPrep
        | Opcode::PrepVarargs
        | Opcode::JumpX => RegisterPolicy::None,
        Opcode::FastCall => RegisterPolicy::FastCall0,
        Opcode::FastCall1 => RegisterPolicy::FastCall1,
        Opcode::FastCall2 | Opcode::FastCall2K => RegisterPolicy::FastCall2,
        Opcode::FastCall3 => RegisterPolicy::FastCall3,
        Opcode::Capture => RegisterPolicy::Capture,
        Opcode::Call | Opcode::CallFb => RegisterPolicy::Call,
        Opcode::Return => RegisterPolicy::Return,
        Opcode::NameCall | Opcode::NameCallUdata => RegisterPolicy::NameCall,
        Opcode::ForNPrep | Opcode::ForNLoop => RegisterPolicy::NumericFor,
        Opcode::ForGLoop => RegisterPolicy::GenericForLoop,
        Opcode::LoadNil => RegisterPolicy::LoadNil,
        Opcode::SetList => RegisterPolicy::SetList,
        Opcode::LoadB
        | Opcode::LoadN
        | Opcode::LoadK
        | Opcode::GetGlobal
        | Opcode::SetGlobal
        | Opcode::GetUpval
        | Opcode::SetUpval
        | Opcode::CloseUpvals
        | Opcode::GetImport
        | Opcode::GetVarargs
        | Opcode::NewClosure
        | Opcode::DupClosure
        | Opcode::LoadKx
        | Opcode::NewTable
        | Opcode::DupTable
        | Opcode::JumpIf
        | Opcode::JumpIfNot
        | Opcode::JumpXEqKNil
        | Opcode::JumpXEqKB
        | Opcode::JumpXEqKN
        | Opcode::JumpXEqKS
        | Opcode::CmpProto
        | Opcode::NewClassMember
        | Opcode::GetUdataKs
        | Opcode::SetUdataKs => RegisterPolicy::A,
        Opcode::Move
        | Opcode::GetTableKs
        | Opcode::SetTableKs
        | Opcode::GetTableN
        | Opcode::SetTableN
        | Opcode::Not
        | Opcode::Minus
        | Opcode::Length
        | Opcode::And
        | Opcode::Or
        | Opcode::AndK
        | Opcode::OrK
        | Opcode::AddK
        | Opcode::SubK
        | Opcode::MulK
        | Opcode::DivK
        | Opcode::ModK
        | Opcode::PowK
        | Opcode::IDivK => RegisterPolicy::AB,
        Opcode::GetTable
        | Opcode::SetTable
        | Opcode::Add
        | Opcode::Sub
        | Opcode::Mul
        | Opcode::Div
        | Opcode::Mod
        | Opcode::Pow
        | Opcode::IDiv
        | Opcode::Concat => RegisterPolicy::ABC,
        Opcode::JumpIfEq
        | Opcode::JumpIfLe
        | Opcode::JumpIfLt
        | Opcode::JumpIfNotEq
        | Opcode::JumpIfNotLe
        | Opcode::JumpIfNotLt => RegisterPolicy::CompareJump,
        Opcode::SubRk | Opcode::DivRk => RegisterPolicy::AC,
    }
}

fn reference_policy(opcode: Opcode) -> ReferencePolicy {
    match opcode {
        Opcode::Nop
        | Opcode::Break
        | Opcode::LoadNil
        | Opcode::LoadB
        | Opcode::LoadN
        | Opcode::Move
        | Opcode::CloseUpvals
        | Opcode::GetImport
        | Opcode::GetTable
        | Opcode::SetTable
        | Opcode::GetTableN
        | Opcode::SetTableN
        | Opcode::Call
        | Opcode::Return
        | Opcode::Jump
        | Opcode::JumpBack
        | Opcode::JumpIf
        | Opcode::JumpIfNot
        | Opcode::JumpIfEq
        | Opcode::JumpIfLe
        | Opcode::JumpIfLt
        | Opcode::JumpIfNotEq
        | Opcode::JumpIfNotLe
        | Opcode::JumpIfNotLt
        | Opcode::Add
        | Opcode::Sub
        | Opcode::Mul
        | Opcode::Div
        | Opcode::Mod
        | Opcode::Pow
        | Opcode::And
        | Opcode::Or
        | Opcode::Concat
        | Opcode::Not
        | Opcode::Minus
        | Opcode::Length
        | Opcode::NewTable
        | Opcode::DupTable
        | Opcode::SetList
        | Opcode::ForNPrep
        | Opcode::ForNLoop
        | Opcode::ForGLoop
        | Opcode::ForGPrepInext
        | Opcode::FastCall3
        | Opcode::ForGPrepNext
        | Opcode::NativeCall
        | Opcode::GetVarargs
        | Opcode::PrepVarargs
        | Opcode::JumpX
        | Opcode::FastCall
        | Opcode::FastCall1
        | Opcode::FastCall2
        | Opcode::Coverage
        | Opcode::Capture
        | Opcode::ForGPrep
        | Opcode::JumpXEqKNil
        | Opcode::JumpXEqKB
        | Opcode::IDiv
        | Opcode::CmpProto => ReferencePolicy::None,
        Opcode::LoadK => ReferencePolicy::DConstant,
        Opcode::DupClosure => ReferencePolicy::DupClosureConstant,
        Opcode::LoadKx => ReferencePolicy::LoadKx,
        Opcode::GetGlobal
        | Opcode::SetGlobal
        | Opcode::GetTableKs
        | Opcode::SetTableKs
        | Opcode::NameCall
        | Opcode::GetUdataKs
        | Opcode::SetUdataKs
        | Opcode::NameCallUdata
        | Opcode::NewClassMember => ReferencePolicy::AuxStringConstant,
        Opcode::NewClosure => ReferencePolicy::NewClosureChild,
        Opcode::AddK
        | Opcode::SubK
        | Opcode::MulK
        | Opcode::DivK
        | Opcode::ModK
        | Opcode::PowK
        | Opcode::AndK
        | Opcode::OrK
        | Opcode::IDivK => ReferencePolicy::CConstant,
        Opcode::FastCall2K => ReferencePolicy::FastCall2K,
        Opcode::JumpXEqKN | Opcode::JumpXEqKS => ReferencePolicy::JumpXEqKConstant,
        Opcode::SubRk | Opcode::DivRk => ReferencePolicy::BConstant,
        Opcode::CallFb => ReferencePolicy::CallFb,
        Opcode::GetUpval | Opcode::SetUpval => ReferencePolicy::BUpvalue,
    }
}

impl Validator<'_> {
    fn validate_proto(&mut self, proto_index: usize, proto: &Proto) {
        if proto.num_params > proto.max_stack_size {
            self.push_proto(
                proto_index,
                ValidationErrorKind::InvalidProtoMetadata,
                format!(
                    "num_params {} exceeds max_stack_size {}",
                    proto.num_params, proto.max_stack_size
                ),
            );
        }

        self.check_string_id(Some(proto_index), None, proto.debug_name, "debug name");
        for child in &proto.child_protos {
            if !proto_id_is_valid(*child, self.protos) {
                self.push_proto(
                    proto_index,
                    ValidationErrorKind::InvalidProtoReference,
                    format!(
                        "child proto id {child} is outside {} protos",
                        self.protos.len()
                    ),
                );
            }
        }

        for (constant_index, constant) in proto.constants.iter().enumerate() {
            self.validate_constant(proto_index, proto, constant_index, constant);
        }

        let boundaries = match InstructionBoundaryIndex::new(&proto.code) {
            Ok(boundaries) => boundaries,
            Err(message) => {
                self.push_proto(
                    proto_index,
                    ValidationErrorKind::InvalidProtoMetadata,
                    message.to_owned(),
                );
                return;
            }
        };
        let code_words = boundaries.code_words();
        for (instruction_index, instruction) in proto.code.iter().enumerate() {
            self.validate_instruction(
                proto_index,
                proto,
                &boundaries,
                instruction_index,
                instruction,
            );
        }

        if let Some(line_info) = &proto.line_info {
            if line_info.delta_bytes.len() as u32 != code_words {
                self.push_proto(
                    proto_index,
                    ValidationErrorKind::InvalidLineInfo,
                    format!(
                        "line info has {} deltas for {code_words} instruction words",
                        line_info.delta_bytes.len()
                    ),
                );
            }
            match line_info.to_line_numbers() {
                Some(lines) if lines.len() as u32 == code_words => {}
                Some(lines) => self.push_proto(
                    proto_index,
                    ValidationErrorKind::InvalidLineInfo,
                    format!(
                        "line info decodes to {} lines for {code_words} instruction words",
                        lines.len()
                    ),
                ),
                None => self.push_proto(
                    proto_index,
                    ValidationErrorKind::InvalidLineInfo,
                    "line info does not decode".to_owned(),
                ),
            }
        }

        if let Some(debug_info) = &proto.debug_info {
            if debug_info.upvalues.len() != usize::from(proto.num_upvalues) {
                self.push_proto(
                    proto_index,
                    ValidationErrorKind::InvalidDebugInfo,
                    format!(
                        "debug info has {} upvalues for proto num_upvalues {}",
                        debug_info.upvalues.len(),
                        proto.num_upvalues
                    ),
                );
            }
            for name in &debug_info.upvalues {
                self.check_string_id(Some(proto_index), None, *name, "debug upvalue name");
            }
            for local in &debug_info.locals {
                self.check_string_id(Some(proto_index), None, local.name, "debug local name");
                self.check_register(
                    Some(proto_index),
                    None,
                    proto,
                    local.register,
                    "debug local",
                );
                if local.start_pc > local.end_pc || local.end_pc > code_words {
                    self.push_proto(
                        proto_index,
                        ValidationErrorKind::InvalidDebugInfo,
                        format!(
                            "debug local pc range {}..{} is outside 0..{code_words}",
                            local.start_pc, local.end_pc
                        ),
                    );
                }
            }
        }

        for (slot_index, slot) in proto.feedback_slots.iter().enumerate() {
            if !boundaries.is_instruction_word(slot.pc) {
                self.push_proto(
                    proto_index,
                    ValidationErrorKind::InvalidFeedbackSlot,
                    format!(
                        "feedback slot {slot_index} pc {} is not an instruction",
                        slot.pc
                    ),
                );
            }
        }

        self.validate_closure_captures(proto_index, proto);

        #[cfg(test)]
        {
            self.boundary_probes += boundaries.probe_count();
        }
    }

    fn validate_constant(
        &mut self,
        proto_index: usize,
        proto: &Proto,
        constant_index: usize,
        constant: &Constant,
    ) {
        match constant {
            Constant::Nil
            | Constant::Boolean { .. }
            | Constant::Number { .. }
            | Constant::Vector { .. }
            | Constant::Integer { .. } => {}
            Constant::String { string } => {
                self.check_string_id(Some(proto_index), None, *string, "string constant");
            }
            Constant::Import { import_id } => {
                let count = import_id >> IMPORT_PATH_COUNT_SHIFT;
                if !(1..=3).contains(&count) {
                    self.push_proto(
                        proto_index,
                        ValidationErrorKind::InvalidConstantReference,
                        format!("import constant {constant_index} has path count {count}"),
                    );
                    return;
                }
                for index in 0..count {
                    let constant_id =
                        (import_id >> import_component_shift(index)) & IMPORT_PATH_COMPONENT_MASK;
                    self.check_string_constant_id(
                        proto_index,
                        proto,
                        constant_id,
                        "import path component",
                    );
                }
            }
            Constant::Table { keys } => {
                for key in keys {
                    self.check_constant_id(proto_index, proto, *key, "table key");
                }
            }
            Constant::Closure { proto } => {
                if !proto_id_is_valid(*proto, self.protos) {
                    self.push_proto(
                        proto_index,
                        ValidationErrorKind::InvalidProtoReference,
                        format!(
                            "closure constant {constant_index} proto id {proto} is outside {} protos",
                            self.protos.len()
                        ),
                    );
                }
            }
            Constant::TableWithConstants { entries } => {
                for entry in entries {
                    self.check_constant_id(
                        proto_index,
                        proto,
                        entry.key,
                        "table-with-constants key",
                    );
                    if entry.value >= 0 {
                        self.check_constant_id(
                            proto_index,
                            proto,
                            entry.value as u32,
                            "table-with-constants value",
                        );
                    }
                }
            }
            Constant::ClassShape { shape } => {
                self.check_string_constant_id(
                    proto_index,
                    proto,
                    shape.class_name,
                    "class shape name",
                );
                for name in &shape.property_names {
                    self.check_string_constant_id(
                        proto_index,
                        proto,
                        *name,
                        "class shape property",
                    );
                }
                for name in &shape.method_names {
                    self.check_string_constant_id(proto_index, proto, *name, "class shape method");
                }
            }
        }
    }

    fn validate_instruction(
        &mut self,
        proto_index: usize,
        proto: &Proto,
        boundaries: &InstructionBoundaryIndex,
        instruction_index: usize,
        instruction: &Instruction,
    ) {
        if !instruction.is_header_consistent() {
            self.push_instruction(
                proto_index,
                instruction_index,
                ValidationErrorKind::InconsistentInstruction,
                format!(
                    "{:?} decoded operand fields do not match encoded header word {:#010x}",
                    instruction.opcode, instruction.header
                ),
            );
            return;
        }

        let expected_aux = instruction.opcode.instruction_len() - 1;
        if usize::from(instruction.aux.is_some()) != expected_aux {
            self.push_instruction(
                proto_index,
                instruction_index,
                ValidationErrorKind::InvalidAuxArity,
                format!(
                    "{:?} has {} AUX words, expected {expected_aux}",
                    instruction.opcode,
                    usize::from(instruction.aux.is_some())
                ),
            );
            return;
        }

        let code_words = boundaries.code_words();
        if let Some(target) = invalid_jump_target(&proto.code, boundaries, instruction_index) {
            self.push_instruction(
                proto_index,
                instruction_index,
                ValidationErrorKind::InvalidJumpTarget,
                format!(
                    "{:?} jumps to word {target}, outside instruction boundaries 0..{code_words}",
                    instruction.opcode
                ),
            );
        }

        self.validate_register_operands(proto_index, proto, instruction_index, instruction);
        self.validate_instruction_references(proto_index, proto, instruction_index, instruction);
    }

    fn validate_register_operands(
        &mut self,
        proto_index: usize,
        proto: &Proto,
        instruction_index: usize,
        instruction: &Instruction,
    ) {
        let policy = register_policy(instruction.opcode);
        match policy {
            RegisterPolicy::None | RegisterPolicy::FastCall0 | RegisterPolicy::Capture => {}
            RegisterPolicy::FastCall1 => {
                self.check_register(
                    Some(proto_index),
                    Some(instruction_index),
                    proto,
                    instruction.b,
                    "fastcall argument",
                );
            }
            RegisterPolicy::FastCall2 => {
                self.check_register(
                    Some(proto_index),
                    Some(instruction_index),
                    proto,
                    instruction.b,
                    "fastcall first argument",
                );
            }
            RegisterPolicy::FastCall3 => {
                self.check_register(
                    Some(proto_index),
                    Some(instruction_index),
                    proto,
                    instruction.b,
                    "fastcall first argument",
                );
                if let Some(aux) = instruction.aux {
                    self.check_register(
                        Some(proto_index),
                        Some(instruction_index),
                        proto,
                        (aux & 0xff) as u8,
                        "fastcall second argument",
                    );
                    self.check_register(
                        Some(proto_index),
                        Some(instruction_index),
                        proto,
                        ((aux >> 8) & 0xff) as u8,
                        "fastcall third argument",
                    );
                }
            }
            RegisterPolicy::Call => {
                self.check_register(
                    Some(proto_index),
                    Some(instruction_index),
                    proto,
                    instruction.a,
                    "call function",
                );
                self.check_register_range(
                    Some(proto_index),
                    Some(instruction_index),
                    proto,
                    instruction.a,
                    instruction.b.saturating_sub(1),
                    "call arguments",
                );
                if instruction.c > 0 {
                    self.check_register_range(
                        Some(proto_index),
                        Some(instruction_index),
                        proto,
                        instruction.a,
                        instruction.c.saturating_sub(1),
                        "call results",
                    );
                }
            }
            RegisterPolicy::Return => {
                self.check_register_range(
                    Some(proto_index),
                    Some(instruction_index),
                    proto,
                    instruction.a,
                    instruction.b.saturating_sub(1),
                    "return values",
                );
            }
            RegisterPolicy::NameCall => {
                self.check_register_range(
                    Some(proto_index),
                    Some(instruction_index),
                    proto,
                    instruction.a,
                    2,
                    "namecall output",
                );
                self.check_register(
                    Some(proto_index),
                    Some(instruction_index),
                    proto,
                    instruction.b,
                    "namecall receiver",
                );
            }
            RegisterPolicy::NumericFor => {
                self.check_register_range(
                    Some(proto_index),
                    Some(instruction_index),
                    proto,
                    instruction.a,
                    3,
                    "numeric for registers",
                );
            }
            RegisterPolicy::GenericForLoop => {
                let variable_count = instruction.aux.map_or(0, generic_for_loop_variable_count);
                self.check_register_range(
                    Some(proto_index),
                    Some(instruction_index),
                    proto,
                    instruction.a,
                    variable_count.saturating_add(3),
                    "generic for registers",
                );
            }
            RegisterPolicy::LoadNil => {
                self.check_register_range(
                    Some(proto_index),
                    Some(instruction_index),
                    proto,
                    instruction.a,
                    instruction.b.saturating_add(1),
                    "loadnil range",
                );
            }
            RegisterPolicy::SetList => {
                self.check_register(
                    Some(proto_index),
                    Some(instruction_index),
                    proto,
                    instruction.a,
                    "setlist table",
                );
                if instruction.c > 0 {
                    self.check_register_range(
                        Some(proto_index),
                        Some(instruction_index),
                        proto,
                        instruction.b,
                        instruction.c.saturating_sub(1),
                        "setlist values",
                    );
                } else {
                    self.check_register(
                        Some(proto_index),
                        Some(instruction_index),
                        proto,
                        instruction.b,
                        "setlist first value",
                    );
                }
            }
            RegisterPolicy::CompareJump => {
                self.check_register(
                    Some(proto_index),
                    Some(instruction_index),
                    proto,
                    instruction.a,
                    "compare jump left operand",
                );
                if let Some(aux) = instruction.aux {
                    self.check_register_u32(
                        Some(proto_index),
                        Some(instruction_index),
                        proto,
                        aux,
                        "compare jump right operand",
                    );
                }
            }
            RegisterPolicy::A | RegisterPolicy::AB | RegisterPolicy::ABC | RegisterPolicy::AC => {
                let operands: &[(u8, &str)] = match policy {
                    RegisterPolicy::A => &[(instruction.a, "A operand")],
                    RegisterPolicy::AB => {
                        &[(instruction.a, "A operand"), (instruction.b, "B operand")]
                    }
                    RegisterPolicy::ABC => &[
                        (instruction.a, "A operand"),
                        (instruction.b, "B operand"),
                        (instruction.c, "C operand"),
                    ],
                    RegisterPolicy::AC => {
                        &[(instruction.a, "A operand"), (instruction.c, "C operand")]
                    }
                    _ => unreachable!("outer arm covers only the register-operand policies"),
                };
                for (register, label) in operands {
                    self.check_register(
                        Some(proto_index),
                        Some(instruction_index),
                        proto,
                        *register,
                        label,
                    );
                }
            }
        }
    }

    fn validate_instruction_references(
        &mut self,
        proto_index: usize,
        proto: &Proto,
        instruction_index: usize,
        instruction: &Instruction,
    ) {
        match reference_policy(instruction.opcode) {
            ReferencePolicy::None => {}
            ReferencePolicy::DConstant => {
                self.check_constant_id(
                    proto_index,
                    proto,
                    instruction.d as u16 as u32,
                    "D constant operand",
                );
            }
            ReferencePolicy::LoadKx => {
                if let Some(constant) = instruction.aux {
                    self.check_constant_id(proto_index, proto, constant, "LOADKX constant");
                }
            }
            ReferencePolicy::AuxStringConstant => {
                if let Some(string) = instruction.aux {
                    self.check_string_constant_id(proto_index, proto, string, "string AUX");
                }
            }
            ReferencePolicy::NewClosureChild => {
                let child_index = instruction.d as u16 as usize;
                if child_index >= proto.child_protos.len() {
                    self.push_instruction(
                        proto_index,
                        instruction_index,
                        ValidationErrorKind::InvalidProtoReference,
                        format!(
                            "NEWCLOSURE child index {child_index} outside {} child protos",
                            proto.child_protos.len()
                        ),
                    );
                }
            }
            ReferencePolicy::DupClosureConstant => {
                let constant_id = instruction.d as u16 as u32;
                self.check_constant_id(proto_index, proto, constant_id, "D closure constant");
                if !matches!(
                    proto.constants.get(constant_id as usize),
                    Some(Constant::Closure { .. })
                ) {
                    self.push_instruction(
                        proto_index,
                        instruction_index,
                        ValidationErrorKind::InvalidConstantReference,
                        format!("D closure constant id {constant_id} is not a closure constant"),
                    );
                }
            }
            ReferencePolicy::CConstant => {
                self.check_constant_id(
                    proto_index,
                    proto,
                    instruction.c as u32,
                    "C constant operand",
                );
            }
            ReferencePolicy::FastCall2K => {
                if let Some(constant) = instruction.aux {
                    self.check_constant_id(proto_index, proto, constant, "fastcall constant AUX");
                }
            }
            ReferencePolicy::JumpXEqKConstant => {
                if let Some(constant) = instruction.aux {
                    self.check_constant_id(
                        proto_index,
                        proto,
                        constant & JUMPX_K_INDEX_MASK,
                        "jump constant AUX",
                    );
                }
            }
            ReferencePolicy::BConstant => {
                self.check_constant_id(
                    proto_index,
                    proto,
                    instruction.b as u32,
                    "B constant operand",
                );
            }
            ReferencePolicy::CallFb => {
                if let Some(slot) = instruction.aux
                    && slot as usize >= proto.feedback_slots.len()
                {
                    self.push_instruction(
                        proto_index,
                        instruction_index,
                        ValidationErrorKind::InvalidFeedbackSlot,
                        format!(
                            "CALLFB slot {slot} outside {} feedback slots",
                            proto.feedback_slots.len()
                        ),
                    );
                }
            }
            ReferencePolicy::BUpvalue => {
                self.check_upvalue(
                    proto_index,
                    instruction_index,
                    proto,
                    instruction.b,
                    "B upvalue operand",
                );
            }
        }

        self.validate_opcode_specific_payload(proto_index, proto, instruction_index, instruction);
    }

    fn validate_opcode_specific_payload(
        &mut self,
        proto_index: usize,
        proto: &Proto,
        instruction_index: usize,
        instruction: &Instruction,
    ) {
        if matches!(
            instruction.opcode,
            Opcode::FastCall
                | Opcode::FastCall1
                | Opcode::FastCall2
                | Opcode::FastCall2K
                | Opcode::FastCall3
        ) {
            if instruction.a == BuiltinFunction::NONE
                || instruction.a > BuiltinFunction::BUFFER_WRITEINTEGER
            {
                self.push_instruction(
                    proto_index,
                    instruction_index,
                    ValidationErrorKind::InvalidAuxPayload,
                    format!("fastcall builtin id {} is invalid", instruction.a),
                );
            }
            if instruction.c == 0 {
                self.push_instruction(
                    proto_index,
                    instruction_index,
                    ValidationErrorKind::InvalidJumpTarget,
                    "fastcall skip offset is zero".to_owned(),
                );
            }
        }

        if instruction.opcode == Opcode::Capture {
            let Some(capture_type) = CaptureType::from_byte(instruction.a) else {
                self.push_instruction(
                    proto_index,
                    instruction_index,
                    ValidationErrorKind::InvalidAuxPayload,
                    format!("capture kind {} is invalid", instruction.a),
                );
                return;
            };
            match capture_type {
                CaptureType::Val | CaptureType::Ref => self.check_register(
                    Some(proto_index),
                    Some(instruction_index),
                    proto,
                    instruction.b,
                    "capture source",
                ),
                CaptureType::Upval => self.check_upvalue(
                    proto_index,
                    instruction_index,
                    proto,
                    instruction.b,
                    "capture source upvalue",
                ),
            }
        }
    }

    fn validate_closure_captures(&mut self, proto_index: usize, proto: &Proto) {
        let mut expected_capture = vec![false; proto.code.len()];
        for (instruction_index, instruction) in proto.code.iter().enumerate() {
            let Some(child_proto_id) = self.closure_child_proto_id(proto_index, proto, instruction)
            else {
                continue;
            };
            let Some(child_proto) = self.protos.get(child_proto_id as usize) else {
                continue;
            };
            let expected = usize::from(child_proto.num_upvalues);
            for capture_offset in 0..expected {
                let capture_index = instruction_index + 1 + capture_offset;
                let Some(capture) = proto.code.get(capture_index) else {
                    self.push_instruction(
                        proto_index,
                        instruction_index,
                        ValidationErrorKind::InvalidClosureCapture,
                        format!(
                            "{:?} for child proto {child_proto_id} expects {expected} capture instruction(s), but code ended after {capture_offset}",
                            instruction.opcode
                        ),
                    );
                    break;
                };
                if capture.opcode != Opcode::Capture {
                    self.push_instruction(
                        proto_index,
                        capture_index,
                        ValidationErrorKind::InvalidClosureCapture,
                        format!(
                            "{:?} for child proto {child_proto_id} expects CAPTURE at instruction {capture_index}, found {:?}",
                            instruction.opcode, capture.opcode
                        ),
                    );
                    break;
                }
                expected_capture[capture_index] = true;
            }
        }

        for (instruction_index, instruction) in proto.code.iter().enumerate() {
            if instruction.opcode == Opcode::Capture && !expected_capture[instruction_index] {
                self.push_instruction(
                    proto_index,
                    instruction_index,
                    ValidationErrorKind::InvalidClosureCapture,
                    "CAPTURE instruction is not attached to a closure instruction".to_owned(),
                );
            }
        }
    }

    fn closure_child_proto_id(
        &self,
        _proto_index: usize,
        proto: &Proto,
        instruction: &Instruction,
    ) -> Option<u32> {
        match instruction.opcode {
            Opcode::NewClosure => proto
                .child_protos
                .get(instruction.d as u16 as usize)
                .copied(),
            Opcode::DupClosure => {
                let constant_id = instruction.d as u16 as usize;
                match proto.constants.get(constant_id) {
                    Some(Constant::Closure { proto }) => Some(*proto),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn check_constant_id(&mut self, proto_index: usize, proto: &Proto, id: u32, context: &str) {
        if id as usize >= proto.constants.len() {
            self.push_proto(
                proto_index,
                ValidationErrorKind::InvalidConstantReference,
                format!(
                    "{context} constant id {id} is outside {} constants",
                    proto.constants.len()
                ),
            );
        }
    }

    fn check_string_constant_id(
        &mut self,
        proto_index: usize,
        proto: &Proto,
        id: u32,
        context: &str,
    ) {
        let Some(constant) = proto.constants.get(id as usize) else {
            self.push_proto(
                proto_index,
                ValidationErrorKind::InvalidConstantReference,
                format!(
                    "{context} string constant id {id} is outside {} constants",
                    proto.constants.len()
                ),
            );
            return;
        };
        if !matches!(constant, Constant::String { .. }) {
            self.push_proto(
                proto_index,
                ValidationErrorKind::InvalidConstantReference,
                format!("{context} constant id {id} is not a string constant"),
            );
        }
    }

    fn check_string_id(
        &mut self,
        proto_index: Option<usize>,
        instruction_index: Option<usize>,
        id: u32,
        context: &str,
    ) {
        if id as usize > self.strings.len() {
            self.push(
                proto_index,
                instruction_index,
                ValidationErrorKind::InvalidStringReference,
                format!(
                    "{context} string id {id} is outside {} strings",
                    self.strings.len()
                ),
            );
        }
    }

    fn check_register(
        &mut self,
        proto_index: Option<usize>,
        instruction_index: Option<usize>,
        proto: &Proto,
        register: u8,
        context: &str,
    ) {
        if register >= proto.max_stack_size {
            self.push(
                proto_index,
                instruction_index,
                ValidationErrorKind::InvalidRegister,
                format!(
                    "{context} register {register} exceeds max_stack_size {}",
                    proto.max_stack_size
                ),
            );
        }
    }

    fn check_register_range(
        &mut self,
        proto_index: Option<usize>,
        instruction_index: Option<usize>,
        proto: &Proto,
        start: u8,
        count: u8,
        context: &str,
    ) {
        if count == 0 {
            return;
        }
        let end = u16::from(start) + u16::from(count) - 1;
        if end >= u16::from(proto.max_stack_size) {
            self.push(
                proto_index,
                instruction_index,
                ValidationErrorKind::InvalidRegister,
                format!(
                    "{context} register range {start}..={end} exceeds max_stack_size {}",
                    proto.max_stack_size
                ),
            );
        }
    }

    fn check_register_u32(
        &mut self,
        proto_index: Option<usize>,
        instruction_index: Option<usize>,
        proto: &Proto,
        register: u32,
        context: &str,
    ) {
        if register >= u32::from(proto.max_stack_size) {
            self.push(
                proto_index,
                instruction_index,
                ValidationErrorKind::InvalidRegister,
                format!(
                    "{context} register {register} exceeds max_stack_size {}",
                    proto.max_stack_size
                ),
            );
        }
    }

    fn check_upvalue(
        &mut self,
        proto_index: usize,
        instruction_index: usize,
        proto: &Proto,
        upvalue: u8,
        context: &str,
    ) {
        if upvalue >= proto.num_upvalues {
            self.push_instruction(
                proto_index,
                instruction_index,
                ValidationErrorKind::InvalidProtoMetadata,
                format!(
                    "{context} {upvalue} exceeds proto num_upvalues {}",
                    proto.num_upvalues
                ),
            );
        }
    }

    fn push_chunk(&mut self, kind: ValidationErrorKind, message: String) {
        self.push(None, None, kind, message);
    }

    fn push_proto(&mut self, proto_index: usize, kind: ValidationErrorKind, message: String) {
        self.push(Some(proto_index), None, kind, message);
    }

    fn push_instruction(
        &mut self,
        proto_index: usize,
        instruction_index: usize,
        kind: ValidationErrorKind,
        message: String,
    ) {
        self.push(Some(proto_index), Some(instruction_index), kind, message);
    }

    fn push(
        &mut self,
        proto_index: Option<usize>,
        instruction_index: Option<usize>,
        kind: ValidationErrorKind,
        message: String,
    ) {
        self.errors.push(ValidationError {
            proto_index,
            instruction_index,
            kind,
            message,
        });
    }
}

fn invalid_jump_target(
    code: &[Instruction],
    boundaries: &InstructionBoundaryIndex,
    instruction_index: usize,
) -> Option<i32> {
    let instruction = code.get(instruction_index)?;
    instruction.jump_offset_words()?;
    let source_word = boundaries.word_offset(instruction_index)?;
    let target = instruction.jump_target_word(source_word)?;
    let code_words = boundaries.code_words();
    if u32::try_from(target).ok() == Some(code_words) {
        return None;
    }
    (!boundaries.is_instruction_word_i32(target)).then_some(target)
}

fn proto_id_is_valid(proto: u32, protos: &[Proto]) -> bool {
    (proto as usize) < protos.len()
}

fn generic_for_loop_variable_count(aux: u32) -> u8 {
    (aux & !FORGLOOP_INEXT_BIT).min(u32::from(u8::MAX)) as u8
}

#[cfg(test)]
mod tests {
    use crate::{
        BytecodeChunk, CompileOptions, Constant, FeedbackSlot, Instruction, LineInfo, Proto,
        TypeInfo, compile_source,
        opcodes::{BuiltinFunction, CaptureType, FeedbackType, Opcode},
        validate::ValidationErrorKind,
        validate_chunk,
    };

    #[test]
    fn accepts_minimal_valid_chunk() {
        let chunk = BytecodeChunk::Valid {
            bytecode_version: 6,
            type_version: 3,
            strings: Vec::new(),
            userdata_type_mappings: Vec::new(),
            protos: vec![minimal_proto()],
            main_proto: 0,
        };

        assert_eq!(validate_chunk(&chunk), Vec::new());
    }

    #[test]
    fn accepts_empty_source_zero_stack_proto() {
        let chunk =
            compile_source("", &CompileOptions::default(), None).expect("compile empty source");

        assert_eq!(validate_chunk(&chunk), Vec::new());
    }

    #[test]
    fn accepts_numeric_for_three_register_window() {
        let chunk = compile_source("for i=2,1 do\nend", &CompileOptions::default(), None)
            .expect("compile numeric for");

        assert_eq!(validate_chunk(&chunk), Vec::new());
    }

    #[test]
    fn accepts_generic_for_aux_variable_count() {
        let chunk = compile_source(
            "for key in pairs(x) do end",
            &CompileOptions::default(),
            None,
        )
        .expect("compile generic for");

        assert_eq!(validate_chunk(&chunk), Vec::new());
    }

    #[test]
    fn rejects_invalid_main_proto_reference() {
        let chunk = BytecodeChunk::Valid {
            bytecode_version: 6,
            type_version: 3,
            strings: Vec::new(),
            userdata_type_mappings: Vec::new(),
            protos: vec![minimal_proto()],
            main_proto: 7,
        };

        let errors = validate_chunk(&chunk);
        assert!(
            errors
                .iter()
                .any(|error| error.kind == ValidationErrorKind::InvalidProtoReference)
        );
    }

    #[test]
    fn accepts_unreachable_but_valid_protos() {
        let chunk = BytecodeChunk::Valid {
            bytecode_version: 6,
            type_version: 3,
            strings: Vec::new(),
            userdata_type_mappings: Vec::new(),
            protos: vec![minimal_proto(), minimal_proto()],
            main_proto: 1,
        };

        assert_eq!(validate_chunk(&chunk), Vec::new());
    }

    #[test]
    fn rejects_an_instruction_whose_fields_diverge_from_its_header() {
        let mut proto = minimal_proto();
        // Mutate a decoded operand of the RETURN without re-encoding its header.
        proto.code[0].b = 9;

        let errors = validate_chunk(&chunk_with_proto(proto));
        assert!(
            errors
                .iter()
                .any(|error| error.kind == ValidationErrorKind::InconsistentInstruction),
            "expected an inconsistent-instruction error, got {errors:#?}"
        );

        // Mutating the header side is caught the same way.
        let mut proto = minimal_proto();
        proto.code[0].header ^= 0xff00;
        let errors = validate_chunk(&chunk_with_proto(proto));
        assert!(
            errors
                .iter()
                .any(|error| error.kind == ValidationErrorKind::InconsistentInstruction),
            "expected an inconsistent-instruction error, got {errors:#?}"
        );
    }

    #[test]
    fn rejects_jump_into_nowhere() {
        let mut proto = minimal_proto();
        proto.code = vec![Instruction::ad(Opcode::Jump, 0, 99)];
        let chunk = BytecodeChunk::Valid {
            bytecode_version: 6,
            type_version: 3,
            strings: Vec::new(),
            userdata_type_mappings: Vec::new(),
            protos: vec![proto],
            main_proto: 0,
        };

        let errors = validate_chunk(&chunk);
        assert!(
            errors
                .iter()
                .any(|error| error.kind == ValidationErrorKind::InvalidJumpTarget)
        );
    }

    #[test]
    fn accepts_jump_to_end_boundary() {
        let mut proto = minimal_proto();
        proto.code = vec![
            Instruction::ad(Opcode::Jump, 0, 1),
            Instruction::abc(Opcode::Return, 0, 1, 0),
        ];
        let chunk = BytecodeChunk::Valid {
            bytecode_version: 6,
            type_version: 3,
            strings: Vec::new(),
            userdata_type_mappings: Vec::new(),
            protos: vec![proto],
            main_proto: 0,
        };

        assert_eq!(validate_chunk(&chunk), Vec::new());
    }

    #[test]
    fn rejects_jump_into_aux_word() {
        let mut proto = minimal_proto();
        proto.constants = vec![Constant::Nil];
        proto.code = vec![
            Instruction::ad(Opcode::Jump, 0, 1),
            Instruction::abc_with_aux(Opcode::LoadKx, 0, 0, 0, Some(0)),
            Instruction::abc(Opcode::Return, 0, 1, 0),
        ];

        let errors = validate_chunk(&chunk_with_proto(proto));
        assert!(
            errors
                .iter()
                .any(|error| error.kind == ValidationErrorKind::InvalidJumpTarget)
        );
    }

    #[test]
    fn jump_target_boundary_checks_are_linear() {
        const JUMP_COUNT: usize = 4096;

        let mut proto = minimal_proto();
        proto.code = (0..JUMP_COUNT)
            .map(|_| Instruction::ad(Opcode::Jump, 0, i16::MAX))
            .collect();

        let (errors, probes) =
            super::validate_chunk_with_boundary_probe_count(&chunk_with_proto(proto));
        assert_eq!(
            errors
                .iter()
                .filter(|error| error.kind == ValidationErrorKind::InvalidJumpTarget)
                .count(),
            JUMP_COUNT
        );
        assert!(
            probes <= JUMP_COUNT,
            "jump target validation used {probes} boundary probes for {JUMP_COUNT} jumps"
        );
    }

    #[test]
    fn feedback_slot_boundary_checks_are_linear() {
        const SLOT_COUNT: usize = 4096;

        let mut proto = minimal_proto();
        proto.feedback_slots = (0..SLOT_COUNT)
            .map(|pc| FeedbackSlot {
                kind: FeedbackType::CallTarget,
                pc: pc as u32 + 1,
            })
            .collect();

        let (errors, probes) =
            super::validate_chunk_with_boundary_probe_count(&chunk_with_proto(proto));
        assert_eq!(
            errors
                .iter()
                .filter(|error| error.kind == ValidationErrorKind::InvalidFeedbackSlot)
                .count(),
            SLOT_COUNT
        );
        assert!(
            probes <= SLOT_COUNT,
            "feedback validation used {probes} boundary probes for {SLOT_COUNT} slots"
        );
    }

    #[test]
    fn rejects_invalid_constant_reference() {
        let mut proto = minimal_proto();
        proto.constants = vec![Constant::Table { keys: vec![7] }];
        let chunk = BytecodeChunk::Valid {
            bytecode_version: 6,
            type_version: 3,
            strings: Vec::new(),
            userdata_type_mappings: Vec::new(),
            protos: vec![proto],
            main_proto: 0,
        };

        let errors = validate_chunk(&chunk);
        assert!(
            errors
                .iter()
                .any(|error| error.kind == ValidationErrorKind::InvalidConstantReference)
        );
    }

    #[test]
    fn rejects_line_info_length_mismatch() {
        let mut proto = minimal_proto();
        proto.line_info = Some(LineInfo {
            log2_span: 0,
            delta_bytes: Vec::new(),
            baseline_deltas: Vec::new(),
        });
        let chunk = BytecodeChunk::Valid {
            bytecode_version: 6,
            type_version: 3,
            strings: Vec::new(),
            userdata_type_mappings: Vec::new(),
            protos: vec![proto],
            main_proto: 0,
        };

        let errors = validate_chunk(&chunk);
        assert!(
            errors
                .iter()
                .any(|error| error.kind == ValidationErrorKind::InvalidLineInfo)
        );
    }

    #[test]
    fn setlist_values_start_at_b_register() {
        let mut proto = minimal_proto();
        proto.max_stack_size = 21;
        proto.code = vec![
            Instruction::abc_with_aux(Opcode::SetList, 7, 12, 4, Some(1)),
            Instruction::abc(Opcode::Return, 0, 1, 0),
        ];
        let chunk = BytecodeChunk::Valid {
            bytecode_version: 6,
            type_version: 3,
            strings: Vec::new(),
            userdata_type_mappings: Vec::new(),
            protos: vec![proto],
            main_proto: 0,
        };

        assert_eq!(validate_chunk(&chunk), Vec::new());
    }

    #[test]
    fn rk_arithmetic_uses_b_as_constant_and_c_as_register() {
        let mut proto = minimal_proto();
        proto.max_stack_size = 11;
        proto.constants = vec![Constant::Nil, Constant::Nil, Constant::Nil, Constant::Nil];
        proto.code = vec![
            Instruction::abc(Opcode::SubRk, 10, 3, 9),
            Instruction::abc(Opcode::Return, 0, 1, 0),
        ];
        let chunk = BytecodeChunk::Valid {
            bytecode_version: 6,
            type_version: 3,
            strings: Vec::new(),
            userdata_type_mappings: Vec::new(),
            protos: vec![proto],
            main_proto: 0,
        };

        assert_eq!(validate_chunk(&chunk), Vec::new());

        let BytecodeChunk::Valid { protos, .. } = &chunk else {
            unreachable!("test chunk is valid")
        };
        let mut div_proto = protos[0].clone();
        div_proto.code[0] = Instruction::abc(Opcode::DivRk, 10, 3, 9);
        let chunk = BytecodeChunk::Valid {
            bytecode_version: 6,
            type_version: 3,
            strings: Vec::new(),
            userdata_type_mappings: Vec::new(),
            protos: vec![div_proto],
            main_proto: 0,
        };
        assert_eq!(validate_chunk(&chunk), Vec::new());
    }

    #[test]
    fn compare_jumps_use_d_offset_and_aux_register() {
        let mut proto = minimal_proto();
        proto.max_stack_size = 2;
        proto.code = vec![
            Instruction::abc_with_aux(Opcode::JumpIfNotLt, 0, 3, 0, Some(1)),
            Instruction::ad(Opcode::LoadN, 0, 0),
            Instruction::ad(Opcode::LoadN, 0, 0),
            Instruction::abc(Opcode::Return, 0, 1, 0),
        ];
        let chunk = BytecodeChunk::Valid {
            bytecode_version: 6,
            type_version: 3,
            strings: Vec::new(),
            userdata_type_mappings: Vec::new(),
            protos: vec![proto],
            main_proto: 0,
        };

        assert_eq!(validate_chunk(&chunk), Vec::new());

        let BytecodeChunk::Valid { protos, .. } = &chunk else {
            unreachable!("test chunk is valid")
        };
        let mut invalid_proto = protos[0].clone();
        invalid_proto.code[0] = Instruction::abc_with_aux(Opcode::JumpIfNotLt, 0, 3, 0, Some(2));
        let invalid = BytecodeChunk::Valid {
            bytecode_version: 6,
            type_version: 3,
            strings: Vec::new(),
            userdata_type_mappings: Vec::new(),
            protos: vec![invalid_proto],
            main_proto: 0,
        };

        let errors = validate_chunk(&invalid);
        assert!(
            errors
                .iter()
                .any(|error| error.kind == ValidationErrorKind::InvalidRegister)
        );
    }

    #[test]
    fn jumpx_eqk_aux_masks_not_flag_from_constant_id() {
        let mut proto = minimal_proto();
        proto.max_stack_size = 1;
        proto.constants = vec![Constant::Number {
            bits: 1.0f64.to_bits(),
        }];
        proto.code = vec![
            Instruction::abc_with_aux(Opcode::JumpXEqKN, 0, 1, 0, Some(0x8000_0000)),
            Instruction::abc(Opcode::Return, 0, 1, 0),
        ];
        let chunk = BytecodeChunk::Valid {
            bytecode_version: 6,
            type_version: 3,
            strings: Vec::new(),
            userdata_type_mappings: Vec::new(),
            protos: vec![proto],
            main_proto: 0,
        };

        assert_eq!(validate_chunk(&chunk), Vec::new());

        let BytecodeChunk::Valid { protos, .. } = &chunk else {
            unreachable!("test chunk is valid")
        };
        let mut invalid_proto = protos[0].clone();
        invalid_proto.code[0] =
            Instruction::abc_with_aux(Opcode::JumpXEqKN, 0, 1, 0, Some(0x0100_0001));
        let invalid = BytecodeChunk::Valid {
            bytecode_version: 6,
            type_version: 3,
            strings: Vec::new(),
            userdata_type_mappings: Vec::new(),
            protos: vec![invalid_proto],
            main_proto: 0,
        };

        let errors = validate_chunk(&invalid);
        assert!(
            errors
                .iter()
                .any(|error| error.kind == ValidationErrorKind::InvalidConstantReference)
        );
    }

    #[test]
    fn opcode_policy_covers_every_opcode() {
        for byte in 0..Opcode::COUNT {
            let opcode = Opcode::from_byte(byte).expect("opcode below COUNT");
            let _ = super::register_policy(opcode);
            let _ = super::reference_policy(opcode);
        }
        assert!(Opcode::from_byte(Opcode::COUNT).is_none());
    }

    #[test]
    fn rejects_upvalue_operands_outside_proto_upvalue_count() {
        let mut proto = minimal_proto();
        proto.max_stack_size = 2;
        proto.num_upvalues = 1;
        proto.code = vec![
            Instruction::abc(Opcode::GetUpval, 0, 1, 0),
            Instruction::abc(Opcode::Return, 0, 1, 0),
        ];
        let chunk = BytecodeChunk::Valid {
            bytecode_version: 6,
            type_version: 3,
            strings: Vec::new(),
            userdata_type_mappings: Vec::new(),
            protos: vec![proto],
            main_proto: 0,
        };

        let errors = validate_chunk(&chunk);
        assert!(
            errors
                .iter()
                .any(|error| error.kind == ValidationErrorKind::InvalidProtoMetadata)
        );
    }

    #[test]
    fn closure_instruction_requires_child_capture_count() {
        let mut root = minimal_proto();
        root.max_stack_size = 2;
        root.child_protos = vec![1];
        root.code = vec![
            Instruction::ad(Opcode::NewClosure, 0, 0),
            Instruction::abc(Opcode::Return, 0, 1, 0),
        ];
        let mut child = minimal_proto();
        child.num_upvalues = 1;
        let chunk = BytecodeChunk::Valid {
            bytecode_version: 6,
            type_version: 3,
            strings: Vec::new(),
            userdata_type_mappings: Vec::new(),
            protos: vec![root, child],
            main_proto: 0,
        };

        let errors = validate_chunk(&chunk);
        assert!(
            errors
                .iter()
                .any(|error| error.kind == ValidationErrorKind::InvalidClosureCapture)
        );
    }

    #[test]
    fn closure_capture_sources_are_checked() {
        let mut root = minimal_proto();
        root.max_stack_size = 2;
        root.child_protos = vec![1];
        root.code = vec![
            Instruction::ad(Opcode::NewClosure, 0, 0),
            Instruction::abc(Opcode::Capture, CaptureType::Val as u8, 2, 0),
            Instruction::abc(Opcode::Return, 0, 1, 0),
        ];
        let mut child = minimal_proto();
        child.num_upvalues = 1;
        let chunk = BytecodeChunk::Valid {
            bytecode_version: 6,
            type_version: 3,
            strings: Vec::new(),
            userdata_type_mappings: Vec::new(),
            protos: vec![root, child],
            main_proto: 0,
        };

        let errors = validate_chunk(&chunk);
        assert!(
            errors
                .iter()
                .any(|error| error.kind == ValidationErrorKind::InvalidRegister)
        );
    }

    #[test]
    fn rejects_invalid_fastcall_builtin_ids() {
        let mut proto = minimal_proto();
        proto.max_stack_size = 2;
        proto.code = vec![
            Instruction::abc(Opcode::FastCall1, 0, 0, 1),
            Instruction::abc(Opcode::Return, 0, 1, 0),
        ];
        let chunk = BytecodeChunk::Valid {
            bytecode_version: 6,
            type_version: 3,
            strings: Vec::new(),
            userdata_type_mappings: Vec::new(),
            protos: vec![proto],
            main_proto: 0,
        };

        let errors = validate_chunk(&chunk);
        assert!(
            errors
                .iter()
                .any(|error| error.kind == ValidationErrorKind::InvalidAuxPayload)
        );
    }

    #[test]
    fn fastcall_aux_registers_are_checked() {
        let mut proto = minimal_proto();
        proto.max_stack_size = 2;
        proto.code = vec![
            Instruction::abc_with_aux(
                Opcode::FastCall3,
                BuiltinFunction::MATH_MAX,
                0,
                1,
                Some(1 | (2 << 8)),
            ),
            Instruction::abc(Opcode::Return, 0, 1, 0),
        ];
        let chunk = BytecodeChunk::Valid {
            bytecode_version: 6,
            type_version: 3,
            strings: Vec::new(),
            userdata_type_mappings: Vec::new(),
            protos: vec![proto],
            main_proto: 0,
        };

        let errors = validate_chunk(&chunk);
        assert!(
            errors
                .iter()
                .any(|error| error.kind == ValidationErrorKind::InvalidRegister)
        );
    }

    #[test]
    fn loadkx_aux_constant_is_checked() {
        let mut proto = minimal_proto();
        proto.max_stack_size = 2;
        proto.code = vec![
            Instruction::abc_with_aux(Opcode::LoadKx, 0, 0, 0, Some(7)),
            Instruction::abc(Opcode::Return, 0, 1, 0),
        ];
        let chunk = BytecodeChunk::Valid {
            bytecode_version: 6,
            type_version: 3,
            strings: Vec::new(),
            userdata_type_mappings: Vec::new(),
            protos: vec![proto],
            main_proto: 0,
        };

        let errors = validate_chunk(&chunk);
        assert!(
            errors
                .iter()
                .any(|error| error.kind == ValidationErrorKind::InvalidConstantReference)
        );
    }

    #[test]
    fn callfb_aux_feedback_slot_is_checked() {
        let mut proto = minimal_proto();
        proto.max_stack_size = 2;
        proto.feedback_slots = vec![FeedbackSlot {
            kind: FeedbackType::CallTarget,
            pc: 0,
        }];
        proto.code = vec![
            Instruction::abc_with_aux(Opcode::CallFb, 0, 1, 1, Some(1)),
            Instruction::abc(Opcode::Return, 0, 1, 0),
        ];
        let chunk = BytecodeChunk::Valid {
            bytecode_version: 11,
            type_version: 3,
            strings: Vec::new(),
            userdata_type_mappings: Vec::new(),
            protos: vec![proto],
            main_proto: 0,
        };

        let errors = validate_chunk(&chunk);
        assert!(
            errors
                .iter()
                .any(|error| error.kind == ValidationErrorKind::InvalidFeedbackSlot)
        );
    }

    #[test]
    fn class_and_userdata_string_aux_references_are_checked() {
        let mut proto = minimal_proto();
        proto.max_stack_size = 2;
        proto.constants = vec![Constant::Nil];
        proto.code = vec![
            Instruction::abc_with_aux(Opcode::NameCallUdata, 0, 1, 0, Some(0)),
            Instruction::abc_with_aux(Opcode::NewClassMember, 0, 0, 0, Some(0)),
            Instruction::abc(Opcode::Return, 0, 1, 0),
        ];
        let chunk = BytecodeChunk::Valid {
            bytecode_version: 6,
            type_version: 3,
            strings: Vec::new(),
            userdata_type_mappings: Vec::new(),
            protos: vec![proto],
            main_proto: 0,
        };

        let errors = validate_chunk(&chunk);
        assert!(
            errors
                .iter()
                .filter(|error| error.kind == ValidationErrorKind::InvalidConstantReference)
                .count()
                >= 2
        );
    }

    fn chunk_with_proto(proto: Proto) -> BytecodeChunk {
        BytecodeChunk::Valid {
            bytecode_version: 11,
            type_version: 3,
            strings: Vec::new(),
            userdata_type_mappings: Vec::new(),
            protos: vec![proto],
            main_proto: 0,
        }
    }

    fn minimal_proto() -> Proto {
        Proto {
            max_stack_size: 1,
            num_params: 0,
            num_upvalues: 0,
            is_vararg: 0,
            flags: 0,
            type_info: TypeInfo { raw: Vec::new() },
            code: vec![Instruction::abc(Opcode::Return, 0, 1, 0)],
            constants: Vec::new(),
            child_protos: Vec::new(),
            line_defined: 0,
            debug_name: 0,
            line_info: None,
            debug_info: None,
            feedback_slots: Vec::new(),
        }
    }
}
