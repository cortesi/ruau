//! Luau bytecode decoder and encoder.

use std::fmt;

use crate::{
    opcodes::{ConstantTag, FeedbackType},
    types::{
        BytecodeChunk, ClassShape, Constant, DebugInfo, DebugLocal, FeedbackSlot, Instruction,
        LineInfo, Proto, TableEntry, TypeInfo, UserdataTypeMapping, code_word_count,
    },
};

/// Bytecode decode failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodeError {
    message: String,
}

impl DecodeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DecodeError {}

/// Bytecode encode failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodeError {
    message: String,
}

impl EncodeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for EncodeError {}

/// Decodes one Luau bytecode blob into a chunk.
pub fn decode_chunk(bytes: &[u8]) -> Result<BytecodeChunk, DecodeError> {
    let mut reader = Reader::new(bytes);
    let version = reader.u8("bytecode version")?;
    if version == 0 {
        return Ok(BytecodeChunk::Error {
            message: bytes[1..].to_vec(),
        });
    }
    if !(3..=11).contains(&version) {
        return Err(DecodeError::new(format!(
            "unsupported bytecode version {version}"
        )));
    }
    let type_version = reader.u8("type encoding version")?;
    if !(1..=3).contains(&type_version) {
        return Err(DecodeError::new(format!(
            "unsupported bytecode type encoding version {type_version}"
        )));
    }

    let strings = decode_string_table(&mut reader)?;
    let mut userdata_type_mappings = Vec::new();
    loop {
        let type_index = reader.u8("userdata type mapping index")?;
        if type_index == 0 {
            break;
        }
        let name = reader.var_u32("userdata type name")?;
        userdata_type_mappings.push(UserdataTypeMapping { type_index, name });
    }

    let proto_count = reader.var_u32("proto count")?;
    let mut protos = Vec::with_capacity(reader.cap(proto_count));
    for _ in 0..proto_count {
        protos.push(decode_proto(&mut reader, version)?);
    }
    let main_proto = reader.var_u32("main proto id")?;
    if !reader.is_eof() {
        return Err(DecodeError::new(format!(
            "trailing {} byte(s) after main proto",
            reader.remaining()
        )));
    }

    Ok(BytecodeChunk::Valid {
        bytecode_version: version,
        type_version,
        strings,
        userdata_type_mappings,
        userdata_mapping_terminator: 0,
        protos,
        main_proto,
    })
}

fn decode_string_table(reader: &mut Reader<'_>) -> Result<Vec<Vec<u8>>, DecodeError> {
    let count = reader.var_u32("string count")?;
    let mut strings = Vec::with_capacity(reader.cap(count));
    for index in 0..count {
        let len = reader.var_u32("string length")? as usize;
        let bytes = reader.bytes(len, &format!("string table entry {}", index + 1))?;
        strings.push(bytes.to_vec());
    }
    Ok(strings)
}

fn decode_proto(reader: &mut Reader<'_>, version: u8) -> Result<Proto, DecodeError> {
    let max_stack_size = reader.u8("proto max stack size")?;
    let num_params = reader.u8("proto parameter count")?;
    let num_upvalues = reader.u8("proto upvalue count")?;
    let is_vararg = reader.u8("proto vararg flag")?;
    let flags = reader.u8("proto flags")?;
    let type_info = decode_type_info(reader)?;

    let code_words = reader.var_u32("instruction word count")?;
    let mut word_index = 0;
    let mut code = Vec::new();
    while word_index < code_words {
        let header = reader.u32("instruction header")?;
        word_index += 1;
        let opcode = crate::Opcode::from_byte((header & 0xff) as u8).ok_or_else(|| {
            DecodeError::new(format!(
                "unknown opcode {} at word {}",
                header & 0xff,
                word_index - 1
            ))
        })?;
        let aux_count = opcode.instruction_len() - 1;
        if word_index + aux_count as u32 > code_words {
            return Err(DecodeError::new(format!(
                "{opcode:?} at word {} requires {aux_count} AUX word(s), past code size {code_words}",
                word_index - 1
            )));
        }
        let aux = if aux_count == 1 {
            word_index += 1;
            Some(reader.u32("instruction aux")?)
        } else {
            None
        };
        code.push(
            Instruction::from_words(header, aux)
                .ok_or_else(|| DecodeError::new("opcode disappeared during decode"))?,
        );
    }

    let constant_count = reader.var_u32("constant count")?;
    let mut constants = Vec::with_capacity(reader.cap(constant_count));
    for _ in 0..constant_count {
        constants.push(decode_constant(reader)?);
    }

    let child_count = reader.var_u32("child proto count")?;
    let mut child_protos = Vec::with_capacity(reader.cap(child_count));
    for _ in 0..child_count {
        child_protos.push(reader.var_u32("child proto id")?);
    }

    let line_defined = reader.var_u32("line defined")?;
    let debug_name = reader.var_u32("debug name")?;
    let line_info = if reader.u8("line info flag")? != 0 {
        Some(decode_line_info(reader, code_words)?)
    } else {
        None
    };
    let debug_info = if reader.u8("debug info flag")? != 0 {
        Some(decode_debug_info(reader)?)
    } else {
        None
    };
    let feedback_slots = if version >= 11 {
        let count = reader.var_u32("feedback slot count")?;
        let mut slots = Vec::with_capacity(reader.cap(count));
        for _ in 0..count {
            let kind = FeedbackType::from_byte(reader.u8("feedback slot type")?)
                .ok_or_else(|| DecodeError::new("unknown feedback slot type"))?;
            let pc = reader.var_u32("feedback slot pc")?;
            slots.push(FeedbackSlot { kind, pc });
        }
        slots
    } else {
        Vec::new()
    };

    Ok(Proto {
        max_stack_size,
        num_params,
        num_upvalues,
        is_vararg,
        flags,
        type_info,
        code,
        constants,
        child_protos,
        line_defined,
        debug_name,
        line_info,
        debug_info,
        feedback_slots,
    })
}

fn decode_type_info(reader: &mut Reader<'_>) -> Result<TypeInfo, DecodeError> {
    let size = reader.var_u32("type info size")? as usize;
    let raw = reader.bytes(size, "type info payload")?.to_vec();
    Ok(TypeInfo { raw })
}

fn decode_constant(reader: &mut Reader<'_>) -> Result<Constant, DecodeError> {
    let tag = ConstantTag::from_byte(reader.u8("constant tag")?)
        .ok_or_else(|| DecodeError::new("unknown constant tag"))?;
    Ok(match tag {
        ConstantTag::Nil => Constant::Nil,
        ConstantTag::Boolean => Constant::Boolean {
            value: reader.u8("boolean constant")? != 0,
        },
        ConstantTag::Number => Constant::Number {
            bits: reader.u64("number constant")?,
        },
        ConstantTag::String => Constant::String {
            string: reader.var_u32("string constant id")?,
        },
        ConstantTag::Import => Constant::Import {
            import_id: reader.u32("import id")?,
        },
        ConstantTag::Table => {
            let count = reader.var_u32("table key count")?;
            let mut keys = Vec::with_capacity(reader.cap(count));
            for _ in 0..count {
                keys.push(reader.var_u32("table key")?);
            }
            Constant::Table { keys }
        }
        ConstantTag::Closure => Constant::Closure {
            proto: reader.var_u32("closure proto id")?,
        },
        ConstantTag::Vector => {
            let mut bits = [0; 4];
            for bit in &mut bits {
                *bit = reader.u32("vector component")?;
            }
            Constant::Vector { bits }
        }
        ConstantTag::TableWithConstants => {
            let count = reader.var_u32("table entry count")?;
            let mut entries = Vec::with_capacity(reader.cap(count));
            for _ in 0..count {
                entries.push(TableEntry {
                    key: reader.var_u32("table entry key")?,
                    value: reader.i32("table entry value")?,
                });
            }
            Constant::TableWithConstants { entries }
        }
        ConstantTag::Integer => {
            let negative = reader.u8("integer sign")? != 0;
            let magnitude = reader.var_u64("integer magnitude")?;
            let value = if negative {
                (!magnitude).wrapping_add(1) as i64
            } else {
                magnitude as i64
            };
            Constant::Integer { value }
        }
        ConstantTag::ClassShape => Constant::ClassShape {
            shape: decode_class_shape(reader)?,
        },
    })
}

fn decode_class_shape(reader: &mut Reader<'_>) -> Result<ClassShape, DecodeError> {
    let class_name = reader.var_u32("class name")?;
    let property_count = reader.var_u32("class property count")?;
    let method_count = reader.var_u32("class method count")?;
    let mut property_names = Vec::with_capacity(reader.cap(property_count));
    for _ in 0..property_count {
        property_names.push(reader.var_u32("class property name")?);
    }
    let mut method_names = Vec::with_capacity(reader.cap(method_count));
    for _ in 0..method_count {
        method_names.push(reader.var_u32("class method name")?);
    }
    Ok(ClassShape {
        class_name,
        property_names,
        method_names,
    })
}

fn decode_line_info(
    reader: &mut Reader<'_>,
    instruction_words: u32,
) -> Result<LineInfo, DecodeError> {
    let log2_span = reader.u8("line info log span")?;
    if log2_span >= 32 {
        return Err(DecodeError::new(format!(
            "line info log span {log2_span} out of range (must be < 32)"
        )));
    }
    let mut delta_bytes = Vec::with_capacity(reader.cap(instruction_words));
    for _ in 0..instruction_words {
        delta_bytes.push(reader.u8("line info delta")?);
    }
    let intervals = ((instruction_words.saturating_sub(1)) >> log2_span) + 1;
    let mut baseline_deltas = Vec::with_capacity(reader.cap(intervals));
    for _ in 0..intervals {
        baseline_deltas.push(reader.i32("line info baseline")?);
    }
    Ok(LineInfo {
        log2_span,
        delta_bytes,
        baseline_deltas,
    })
}

fn decode_debug_info(reader: &mut Reader<'_>) -> Result<DebugInfo, DecodeError> {
    let local_count = reader.var_u32("debug local count")?;
    let mut locals = Vec::with_capacity(reader.cap(local_count));
    for _ in 0..local_count {
        locals.push(DebugLocal {
            name: reader.var_u32("debug local name")?,
            start_pc: reader.var_u32("debug local start pc")?,
            end_pc: reader.var_u32("debug local end pc")?,
            register: reader.u8("debug local register")?,
        });
    }
    let upvalue_count = reader.var_u32("debug upvalue count")?;
    let mut upvalues = Vec::with_capacity(reader.cap(upvalue_count));
    for _ in 0..upvalue_count {
        upvalues.push(reader.var_u32("debug upvalue name")?);
    }
    Ok(DebugInfo { locals, upvalues })
}

/// Encodes a chunk back into upstream wire bytes.
pub fn encode_chunk(chunk: &BytecodeChunk) -> Result<Vec<u8>, EncodeError> {
    let mut writer = Writer::default();
    match chunk {
        BytecodeChunk::Error { message } => {
            writer.u8(0);
            writer.bytes(message);
        }
        BytecodeChunk::Valid {
            bytecode_version,
            type_version,
            strings,
            userdata_type_mappings,
            userdata_mapping_terminator,
            protos,
            main_proto,
        } => {
            if *userdata_mapping_terminator != 0 {
                return Err(EncodeError::new("userdata mapping terminator must be zero"));
            }
            writer.u8(*bytecode_version);
            writer.u8(*type_version);
            writer.var_u64(strings.len() as u64);
            for string in strings {
                writer.var_u64(string.len() as u64);
                writer.bytes(string);
            }
            for mapping in userdata_type_mappings {
                writer.u8(mapping.type_index);
                writer.var_u64(mapping.name as u64);
            }
            writer.u8(0);
            writer.var_u64(protos.len() as u64);
            for proto in protos {
                encode_proto(&mut writer, *bytecode_version, proto);
            }
            writer.var_u64(*main_proto as u64);
        }
    }
    Ok(writer.bytes)
}

fn encode_proto(writer: &mut Writer, version: u8, proto: &Proto) {
    writer.u8(proto.max_stack_size);
    writer.u8(proto.num_params);
    writer.u8(proto.num_upvalues);
    writer.u8(proto.is_vararg);
    writer.u8(proto.flags);
    writer.var_u64(proto.type_info.raw.len() as u64);
    writer.bytes(&proto.type_info.raw);
    writer.var_u64(u64::from(code_word_count(&proto.code)));
    for instruction in &proto.code {
        writer.u32(instruction.header);
        if let Some(aux) = instruction.aux {
            writer.u32(aux);
        }
    }
    writer.var_u64(proto.constants.len() as u64);
    for constant in &proto.constants {
        encode_constant(writer, constant);
    }
    writer.var_u64(proto.child_protos.len() as u64);
    for child in &proto.child_protos {
        writer.var_u64(*child as u64);
    }
    writer.var_u64(proto.line_defined as u64);
    writer.var_u64(proto.debug_name as u64);
    if let Some(line_info) = &proto.line_info {
        writer.u8(1);
        writer.u8(line_info.log2_span);
        writer.bytes(&line_info.delta_bytes);
        for baseline in &line_info.baseline_deltas {
            writer.i32(*baseline);
        }
    } else {
        writer.u8(0);
    }
    if let Some(debug_info) = &proto.debug_info {
        writer.u8(1);
        writer.var_u64(debug_info.locals.len() as u64);
        for local in &debug_info.locals {
            writer.var_u64(local.name as u64);
            writer.var_u64(local.start_pc as u64);
            writer.var_u64(local.end_pc as u64);
            writer.u8(local.register);
        }
        writer.var_u64(debug_info.upvalues.len() as u64);
        for upvalue in &debug_info.upvalues {
            writer.var_u64(*upvalue as u64);
        }
    } else {
        writer.u8(0);
    }
    if version >= 11 {
        writer.var_u64(proto.feedback_slots.len() as u64);
        for slot in &proto.feedback_slots {
            writer.u8(slot.kind as u8);
            writer.var_u64(slot.pc as u64);
        }
    }
}

fn encode_constant(writer: &mut Writer, constant: &Constant) {
    match constant {
        Constant::Nil => writer.u8(ConstantTag::Nil as u8),
        Constant::Boolean { value } => {
            writer.u8(ConstantTag::Boolean as u8);
            writer.u8(u8::from(*value));
        }
        Constant::Number { bits } => {
            writer.u8(ConstantTag::Number as u8);
            writer.u64(*bits);
        }
        Constant::String { string } => {
            writer.u8(ConstantTag::String as u8);
            writer.var_u64(*string as u64);
        }
        Constant::Import { import_id } => {
            writer.u8(ConstantTag::Import as u8);
            writer.u32(*import_id);
        }
        Constant::Table { keys } => {
            writer.u8(ConstantTag::Table as u8);
            writer.var_u64(keys.len() as u64);
            for key in keys {
                writer.var_u64(*key as u64);
            }
        }
        Constant::Closure { proto } => {
            writer.u8(ConstantTag::Closure as u8);
            writer.var_u64(*proto as u64);
        }
        Constant::Vector { bits } => {
            writer.u8(ConstantTag::Vector as u8);
            for bit in bits {
                writer.u32(*bit);
            }
        }
        Constant::TableWithConstants { entries } => {
            writer.u8(ConstantTag::TableWithConstants as u8);
            writer.var_u64(entries.len() as u64);
            for entry in entries {
                writer.var_u64(entry.key as u64);
                writer.i32(entry.value);
            }
        }
        Constant::Integer { value } => {
            writer.u8(ConstantTag::Integer as u8);
            if *value < 0 {
                writer.u8(1);
                writer.var_u64((!(*value as u64)).wrapping_add(1));
            } else {
                writer.u8(0);
                writer.var_u64(*value as u64);
            }
        }
        Constant::ClassShape { shape } => {
            writer.u8(ConstantTag::ClassShape as u8);
            writer.var_u64(shape.class_name as u64);
            writer.var_u64(shape.property_names.len() as u64);
            writer.var_u64(shape.method_names.len() as u64);
            for name in &shape.property_names {
                writer.var_u64(*name as u64);
            }
            for name in &shape.method_names {
                writer.var_u64(*name as u64);
            }
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn is_eof(&self) -> bool {
        self.offset == self.bytes.len()
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }

    /// A pre-allocation capacity capped by the bytes still available. A chunk
    /// cannot hold more length-prefixed elements than it has bytes (each element
    /// consumes at least one byte), so a hostile count cannot trigger a huge
    /// speculative allocation; if the declared count really is larger than the
    /// data, the element loop fails cleanly when the reader runs out of bytes.
    fn cap(&self, count: u32) -> usize {
        (count as usize).min(self.remaining())
    }

    fn bytes(&mut self, len: usize, label: &str) -> Result<&'a [u8], DecodeError> {
        if self.remaining() < len {
            return Err(DecodeError::new(format!(
                "unexpected end while reading {label}: need {len} byte(s), have {}",
                self.remaining()
            )));
        }
        let start = self.offset;
        self.offset += len;
        Ok(&self.bytes[start..self.offset])
    }

    fn u8(&mut self, label: &str) -> Result<u8, DecodeError> {
        Ok(self.bytes(1, label)?[0])
    }

    fn u32(&mut self, label: &str) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.fixed_bytes(label)?))
    }

    fn i32(&mut self, label: &str) -> Result<i32, DecodeError> {
        Ok(i32::from_le_bytes(self.fixed_bytes(label)?))
    }

    fn u64(&mut self, label: &str) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.fixed_bytes(label)?))
    }

    fn fixed_bytes<const N: usize>(&mut self, label: &str) -> Result<[u8; N], DecodeError> {
        let mut bytes = [0; N];
        bytes.copy_from_slice(self.bytes(N, label)?);
        Ok(bytes)
    }

    fn var_u32(&mut self, label: &str) -> Result<u32, DecodeError> {
        let value = self.var_u64(label)?;
        u32::try_from(value)
            .map_err(|_| DecodeError::new(format!("{label} varint {value} overflows u32")))
    }

    fn var_u64(&mut self, label: &str) -> Result<u64, DecodeError> {
        let mut result = 0u64;
        let mut shift = 0;
        loop {
            if shift >= 64 {
                return Err(DecodeError::new(format!("{label} varint is too large")));
            }
            let byte = self.u8(label)?;
            result |= u64::from(byte & VARINT_PAYLOAD_MASK) << shift;
            if byte & VARINT_CONTINUATION_BIT == 0 {
                return Ok(result);
            }
            shift += 7;
        }
    }
}

#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    fn bytes(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn i32(&mut self, value: i32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn var_u64(&mut self, value: u64) {
        write_varint(&mut self.bytes, value);
    }
}

/// LEB128 varint: low seven bits of each byte carry payload.
const VARINT_PAYLOAD_MASK: u8 = 0x7f;
/// LEB128 varint: the high bit marks a continuation byte.
const VARINT_CONTINUATION_BIT: u8 = 0x80;

/// Appends `value` to `bytes` as an unsigned LEB128 varint.
pub fn write_varint(bytes: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & u64::from(VARINT_PAYLOAD_MASK)) as u8
            | (u8::from(value > u64::from(VARINT_PAYLOAD_MASK)) << 7);
        bytes.push(byte);
        value >>= 7;
        if value == 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{decode_chunk, encode_chunk};
    use crate::{
        BytecodeChunk, ClassShape, Constant, DebugInfo, DebugLocal, FeedbackSlot, FeedbackType,
        Instruction, LineInfo, Opcode, Proto, TableEntry, TypeInfo, UserdataTypeMapping,
    };

    #[test]
    fn error_bytecode_roundtrips() {
        let bytes = b"\0compile failed";
        let chunk = decode_chunk(bytes).expect("decode error chunk");
        assert_eq!(
            chunk,
            BytecodeChunk::Error {
                message: b"compile failed".to_vec()
            }
        );
        assert_eq!(encode_chunk(&chunk).expect("encode"), bytes);
    }

    #[test]
    fn rejects_unknown_valid_version() {
        let err = decode_chunk(&[12, 3]).expect_err("version is unsupported");
        assert!(err.to_string().contains("unsupported bytecode version 12"));
    }

    #[test]
    fn rejects_unsupported_type_encoding_version() {
        let err = decode_chunk(&[6, 4]).expect_err("type encoding version is unsupported");
        assert!(
            err.to_string()
                .contains("unsupported bytecode type encoding version 4")
        );
    }

    #[test]
    fn rich_wire_shapes_roundtrip() {
        let chunk = BytecodeChunk::Valid {
            bytecode_version: 11,
            type_version: 3,
            strings: vec![
                b"main".to_vec(),
                b"Widget".to_vec(),
                b"width".to_vec(),
                b"height".to_vec(),
                b"resize".to_vec(),
                b"userdataName".to_vec(),
                b"upvalueName".to_vec(),
            ],
            userdata_type_mappings: vec![UserdataTypeMapping {
                type_index: 64,
                name: 6,
            }],
            userdata_mapping_terminator: 0,
            protos: vec![minimal_proto(), rich_proto()],
            main_proto: 1,
        };

        let bytes = encode_chunk(&chunk).expect("encode rich chunk");
        let decoded = decode_chunk(&bytes).expect("decode rich chunk");

        assert_eq!(decoded, chunk);
        assert_eq!(encode_chunk(&decoded).expect("re-encode rich chunk"), bytes);
        let BytecodeChunk::Valid {
            userdata_type_mappings,
            protos,
            ..
        } = decoded
        else {
            panic!("expected valid chunk");
        };
        assert_eq!(userdata_type_mappings[0].type_index, 64);
        assert_eq!(protos[1].code[0].opcode, Opcode::Coverage);
        assert!(matches!(protos[1].constants[7], Constant::Vector { .. }));
        assert!(matches!(
            protos[1].constants[9],
            Constant::Integer { value: -42 }
        ));
        assert!(matches!(
            protos[1].constants[10],
            Constant::ClassShape { .. }
        ));
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

    fn rich_proto() -> Proto {
        Proto {
            max_stack_size: 4,
            num_params: 1,
            num_upvalues: 1,
            is_vararg: 1,
            flags: 0,
            type_info: TypeInfo {
                raw: vec![1, 2, 3, 4],
            },
            code: vec![
                Instruction::abc(Opcode::Coverage, 0, 0, 0),
                Instruction::abc_with_aux(Opcode::CallFb, 0, 1, 2, Some(0xfeed_beef)),
                Instruction::abc(Opcode::Return, 0, 1, 0),
            ],
            constants: vec![
                Constant::Nil,
                Constant::Boolean { value: true },
                Constant::Number {
                    bits: 1.25_f64.to_bits(),
                },
                Constant::String { string: 1 },
                Constant::Import {
                    import_id: 0x0102_0304,
                },
                Constant::Table { keys: vec![1, 2] },
                Constant::Closure { proto: 0 },
                Constant::Vector {
                    bits: [
                        1.0_f32.to_bits(),
                        2.0_f32.to_bits(),
                        3.0_f32.to_bits(),
                        4.0_f32.to_bits(),
                    ],
                },
                Constant::TableWithConstants {
                    entries: vec![
                        TableEntry { key: 1, value: -1 },
                        TableEntry { key: 2, value: 6 },
                    ],
                },
                Constant::Integer { value: -42 },
                Constant::ClassShape {
                    shape: ClassShape {
                        class_name: 2,
                        property_names: vec![3, 4],
                        method_names: vec![5],
                    },
                },
            ],
            child_protos: vec![0],
            line_defined: 10,
            debug_name: 1,
            line_info: Some(LineInfo {
                log2_span: 1,
                delta_bytes: vec![0, 0, 1, 1],
                baseline_deltas: vec![10, 2],
            }),
            debug_info: Some(DebugInfo {
                locals: vec![DebugLocal {
                    name: 1,
                    start_pc: 0,
                    end_pc: 3,
                    register: 0,
                }],
                upvalues: vec![7],
            }),
            feedback_slots: vec![FeedbackSlot {
                kind: FeedbackType::CallTarget,
                pc: 1,
            }],
        }
    }

    /// Regression corpus: hostile byte sequences found by the `bytecode_decode`
    /// and `vm_load` fuzz targets. Each previously triggered a panic or an
    /// unbounded allocation; the decoder must now return cleanly (`Ok` or `Err`)
    /// on every one — never panic or OOM. Reproducers are kept inline because the
    /// fuzz corpus directory is not checked in.
    #[test]
    fn decode_survives_hostile_inputs() {
        // Unbounded length-prefix allocation: a huge declared string count once
        // drove a multi-gigabyte `Vec::with_capacity`.
        const UNBOUNDED_ALLOC: &[u8] = &[0x03, 0x03, 0xff, 0xff, 0xff, 0xff, 0x04, 0xa6];
        // Line-info `log2_span` of 0x25 (>= 32) once overflowed a `u32` shift.
        const SHIFT_OVERFLOW: &[u8] = &[
            0x06, 0x03, 0x00, 0x00, 0x25, 0x00, 0x3d, 0x7c, 0x2f, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0x00,
        ];
        // A line-delta stream whose running `i32` sum once overflowed.
        const ADD_OVERFLOW: &[u8] = &[
            0x03, 0x03, 0x00, 0x00, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x28, 0x03,
            0x5b, 0xfe, 0x00, 0x2d, 0x00, 0xd9, 0x2d, 0x00, 0x00, 0x04, 0x00, 0x00, 0xaf, 0x01,
            0x00, 0x03, 0x00, 0x00, 0x41, 0x03, 0x00, 0x00, 0x01, 0x80, 0x00, 0x00, 0x00, 0x8d,
            0x80, 0x2b, 0x03, 0x01, 0x00, 0x00,
        ];

        // The declared string count now hits the remaining-bytes bound before
        // any allocation, so decoding fails on the truncated string table.
        assert!(
            decode_chunk(UNBOUNDED_ALLOC).is_err(),
            "truncated string table must be rejected"
        );
        // The out-of-range span is now rejected up front instead of shifting.
        assert!(
            decode_chunk(SHIFT_OVERFLOW).is_err(),
            "log2_span >= 32 must be rejected"
        );
        // The running line-delta sum now wraps instead of overflowing, and the
        // rest of this chunk is well-formed, so it decodes successfully.
        assert!(
            decode_chunk(ADD_OVERFLOW).is_ok(),
            "wrapped line-delta stream decodes cleanly"
        );
    }
}
