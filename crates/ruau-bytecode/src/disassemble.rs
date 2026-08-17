//! Human-readable rendering derived from decoded bytecode chunks.

use std::fmt::Write as _;

use crate::{BytecodeChunk, Constant, Instruction, Proto};

/// Renders a decoded chunk for diagnostics.
#[must_use]
pub fn disassemble_chunk(chunk: &BytecodeChunk) -> String {
    let mut output = String::new();
    match chunk {
        BytecodeChunk::Error { message } => {
            writeln!(
                output,
                "error bytecode: {}",
                String::from_utf8_lossy(message)
            )
            .expect("write to string");
        }
        BytecodeChunk::Valid {
            bytecode_version,
            type_version,
            strings,
            userdata_type_mappings,
            protos,
            main_proto,
            ..
        } => {
            writeln!(
                output,
                "bytecode v{bytecode_version}, type v{type_version}, main proto {main_proto}"
            )
            .expect("write to string");
            if !strings.is_empty() {
                output.push_str("strings:\n");
                for (index, string) in strings.iter().enumerate() {
                    writeln!(
                        output,
                        "  S{} = {:?}",
                        index + 1,
                        String::from_utf8_lossy(string)
                    )
                    .expect("write to string");
                }
            }
            if !userdata_type_mappings.is_empty() {
                output.push_str("userdata mappings:\n");
                for mapping in userdata_type_mappings {
                    writeln!(output, "  type {} -> S{}", mapping.type_index, mapping.name)
                        .expect("write to string");
                }
            }
            for (index, proto) in protos.iter().enumerate() {
                render_proto(&mut output, index, proto);
            }
        }
    }
    output
}

fn render_proto(output: &mut String, index: usize, proto: &Proto) {
    writeln!(
        output,
        "proto {index}: max_stack={}, params={}, upvalues={}, vararg={}, flags={}, line={}, name=S{}",
        proto.max_stack_size,
        proto.num_params,
        proto.num_upvalues,
        proto.is_vararg,
        proto.flags,
        proto.line_defined,
        proto.debug_name
    )
    .expect("write to string");
    output.push_str("  code:\n");
    let mut pc = 0usize;
    for instruction in &proto.code {
        render_instruction(output, pc, instruction);
        pc += instruction.word_len() as usize;
    }
    if !proto.constants.is_empty() {
        output.push_str("  constants:\n");
        for (index, constant) in proto.constants.iter().enumerate() {
            writeln!(output, "    K{index}: {}", render_constant(constant))
                .expect("write to string");
        }
    }
    if !proto.type_info.raw.is_empty() {
        writeln!(output, "  type_info: {:?}", proto.type_info.raw).expect("write to string");
    }
    if let Some(line_info) = &proto.line_info {
        writeln!(
            output,
            "  line_info: log2_span={}, deltas={:?}, baselines={:?}",
            line_info.log2_span, line_info.delta_bytes, line_info.baseline_deltas
        )
        .expect("write to string");
    }
    if let Some(debug_info) = &proto.debug_info {
        if !debug_info.locals.is_empty() {
            output.push_str("  debug_locals:\n");
            for (index, local) in debug_info.locals.iter().enumerate() {
                writeln!(
                    output,
                    "    L{index}: name=S{}, start={}, end={}, reg={}",
                    local.name, local.start_pc, local.end_pc, local.register
                )
                .expect("write to string");
            }
        }
        if !debug_info.upvalues.is_empty() {
            writeln!(output, "  debug_upvalues: {:?}", debug_info.upvalues)
                .expect("write to string");
        }
    }
}

fn render_instruction(output: &mut String, pc: usize, instruction: &Instruction) {
    writeln!(
        output,
        "    {pc:04}: {:?} A={} B={} C={} D={} E={} header=0x{:08x}",
        instruction.opcode,
        instruction.a,
        instruction.b,
        instruction.c,
        instruction.d,
        instruction.e,
        instruction.header
    )
    .expect("write to string");
    if let Some(aux) = instruction.aux {
        writeln!(output, "          AUX0=0x{aux:08x}").expect("write to string");
    }
}

fn render_constant(constant: &Constant) -> String {
    match constant {
        Constant::Nil => "nil".to_owned(),
        Constant::Boolean { value } => value.to_string(),
        Constant::Number { bits } => format!("number bits=0x{bits:016x}"),
        Constant::String { string } => format!("string S{string}"),
        Constant::Import { import_id } => format!("import 0x{import_id:08x}"),
        Constant::Table { keys } => format!("table keys={keys:?}"),
        Constant::Closure { proto } => format!("closure proto {proto}"),
        Constant::Vector { bits } => format!("vector bits={bits:?}"),
        Constant::VectorDouble { bits } => format!("double-vector bits={bits:?}"),
        Constant::TableWithConstants { entries } => {
            format!("table-with-constants entries={entries:?}")
        }
        Constant::Integer { value } => format!("integer {value}"),
        Constant::ClassShape { shape } => {
            format!(
                "class S{} props={:?} methods={:?}",
                shape.class_name, shape.property_names, shape.method_names
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::disassemble_chunk;
    use crate::{CompileOptions, compile_source};

    #[test]
    fn renders_instruction_rows() {
        let chunk = compile_source("return 5", &CompileOptions::default(), None).expect("compile");
        let rendered = disassemble_chunk(&chunk);
        assert!(rendered.contains("0000: PrepVarargs"));
        assert!(rendered.contains("0001: LoadN"));
        assert!(rendered.contains("0002: Return"));
    }
}
