use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use ruau_ast::parse::parse_file;

use super::{
    CompileContext, CompileErrorKind, CompilerOptions, FastFlag, FunctionCompiler,
    compile_source_bytes_strict_with_compiler_options_and_cancel,
    compile_source_bytes_with_compiler_options, compile_source_with_compiler_options,
    constant_ad_operand,
};
use crate::{
    BytecodeChunk, Constant, encode_chunk,
    opcodes::{CaptureType, Opcode},
    validate_chunk,
};

#[test]
fn default_options_match_upstream_defaults() {
    let options = CompilerOptions::default();
    assert_eq!(options.optimization_level, 1);
    assert_eq!(options.debug_level, 1);
    assert_eq!(options.type_info_level, 0);
    assert_eq!(options.coverage_level, 0);
    assert!(!options.clear_dead_stack_slots);
    assert!(!options.preserve_fenv_semantics);
    assert!(CompilerOptions::for_vm_execution().clear_dead_stack_slots);
    assert!(CompilerOptions::for_vm_execution().preserve_fenv_semantics);
}

#[test]
fn dead_stack_slot_clearing_is_vm_execution_only() {
    let source = "local t = {}\nt.a = {}\nreturn t\n";

    let upstream =
        compile_source_with_compiler_options(source, &CompilerOptions::default()).expect("compile");
    let vm = compile_source_with_compiler_options(source, &CompilerOptions::for_vm_execution())
        .expect("compile");
    let (
        BytecodeChunk::Valid {
            protos: upstream, ..
        },
        BytecodeChunk::Valid { protos: vm, .. },
    ) = (upstream, vm)
    else {
        panic!("expected valid bytecode");
    };

    assert!(
        !upstream[0]
            .code
            .iter()
            .any(|instruction| instruction.opcode == Opcode::LoadNil && instruction.a == 1),
        "upstream-compatible defaults should not emit extra cleanup"
    );
    assert!(
        vm[0]
            .code
            .iter()
            .any(|instruction| instruction.opcode == Opcode::LoadNil && instruction.a == 1),
        "VM execution profile should clear the dead scratch value"
    );
}

#[test]
fn public_compile_policy_clears_dead_stack_slots() {
    let source = "local t = {}\nt.a = {}\nreturn t\n";
    let chunk = crate::compile_source(source, &crate::CompileOptions::default()).expect("compile");
    let BytecodeChunk::Valid { protos, .. } = chunk else {
        panic!("expected valid bytecode");
    };

    assert!(
        protos[0]
            .code
            .iter()
            .any(|instruction| instruction.opcode == Opcode::LoadNil && instruction.a == 1),
        "public compile policy should keep VM hardening enabled"
    );
}

#[test]
fn compiles_empty_return_shape() {
    let chunk = compile_source_with_compiler_options("return", &CompilerOptions::default())
        .expect("compile");
    let BytecodeChunk::Valid { protos, .. } = &chunk else {
        panic!("expected valid chunk");
    };
    assert_eq!(protos[0].code[0].opcode, Opcode::PrepVarargs);
    assert_eq!(protos[0].code[1].opcode, Opcode::Return);
    let bytes = encode_chunk(&chunk).expect("encode");
    assert_eq!(bytes[0], 6);
}

#[test]
fn compiles_coverage_global_call_statements() {
    let options = CompilerOptions {
        coverage_level: 1,
        ..Default::default()
    };
    let chunk =
        compile_source_with_compiler_options("\nprint(1)\nprint(2)\n", &options).expect("compile");
    let BytecodeChunk::Valid {
        bytecode_version,
        strings,
        protos,
        ..
    } = &chunk
    else {
        panic!("expected valid chunk");
    };
    assert_eq!(*bytecode_version, 7);
    assert_eq!(strings, &[b"print".to_vec()]);
    assert_eq!(
        protos[0]
            .code
            .iter()
            .map(|instruction| instruction.opcode)
            .collect::<Vec<_>>(),
        vec![
            Opcode::PrepVarargs,
            Opcode::Coverage,
            Opcode::GetImport,
            Opcode::LoadN,
            Opcode::Call,
            Opcode::Coverage,
            Opcode::GetImport,
            Opcode::LoadN,
            Opcode::Call,
            Opcode::Return,
        ]
    );
    assert_eq!(
        protos[0].constants,
        vec![
            Constant::String { string: 1 },
            Constant::Import { import_id: 1 << 30 },
        ]
    );
}

#[test]
fn fenv_use_disables_import_paths_and_generic_for_fast_paths() {
    let chunk = compile_source_with_compiler_options(
        r#"
        getfenv()
        for k, v in pairs({}) do end
        return math.abs(-1)
        "#,
        &CompilerOptions::for_vm_execution(),
    )
    .expect("compile");
    let BytecodeChunk::Valid { protos, .. } = &chunk else {
        panic!("expected valid chunk");
    };
    let opcodes = protos[0]
        .code
        .iter()
        .map(|instruction| instruction.opcode)
        .collect::<Vec<_>>();
    assert!(
        opcodes.contains(&Opcode::ForGPrep),
        "fenv-sensitive generic for should use the general prep path: {opcodes:?}"
    );
    assert!(
        !opcodes.contains(&Opcode::ForGPrepNext)
            && !opcodes.contains(&Opcode::ForGPrepInext)
            && !opcodes.contains(&Opcode::GetImport),
        "fenv-sensitive source should not use import fast paths: {opcodes:?}"
    );
}

#[test]
fn compile_source_bytes_preserves_invalid_utf8_string_literals() {
    // A double-quoted literal whose only content is a lone 0xFF byte is not
    // valid UTF-8; the byte-aware compile path must carry the original byte
    // through to the chunk string table rather than the lexing surrogate.
    let mut source = b"return \"".to_vec();
    source.push(0xFF);
    source.extend_from_slice(b"\"\n");

    let chunk = compile_source_bytes_with_compiler_options(&source, &CompilerOptions::default())
        .expect("compile");
    let BytecodeChunk::Valid { strings, .. } = &chunk else {
        panic!("expected valid chunk, got {chunk:?}");
    };
    assert!(
        strings.iter().any(|entry| entry.as_slice() == [0xFF]),
        "invalid-UTF-8 string literal must survive byte-for-byte: {strings:?}"
    );
}

#[test]
fn compile_source_bytes_with_cancel_rejects_cancelled_work() {
    let cancel = Arc::new(AtomicBool::new(true));
    let err = compile_source_bytes_strict_with_compiler_options_and_cancel(
        b"return 1",
        &CompilerOptions::default(),
        Some(cancel),
    )
    .expect_err("cancelled compilation fails closed");

    assert_eq!(err.kind(), CompileErrorKind::Cancelled);
}

#[test]
fn function_compiler_polls_cancel_flag_before_lowering_statements() {
    let parse = parse_file("local x = 1\nreturn x");
    assert!(parse.errors.is_empty(), "{:?}", parse.errors);
    let root = std::rc::Rc::new(parse.root.expect("parser returns root"));
    let cancel = Arc::new(AtomicBool::new(true));
    let mut compiler = FunctionCompiler::new(
        CompileContext::with_cancel(
            std::rc::Rc::clone(&root),
            &CompilerOptions::default(),
            Some(Arc::clone(&cancel)),
        ),
        0,
    );

    let err = compiler
        .compile_root(&root)
        .expect_err("cancelled compiler should stop before lowering");

    assert!(cancel.load(Ordering::Relaxed));
    assert_eq!(err.kind(), CompileErrorKind::Cancelled);
}

#[test]
fn return_constant_call_uses_analyzer_before_multret_call_path() {
    let options = CompilerOptions {
        optimization_level: 2,
        vector_lib: Some(String::from("Vector3")),
        vector_ctor: Some(String::from("new")),
        ..CompilerOptions::default()
    };
    let chunk = compile_source_with_compiler_options("return vector.create(1, 2)", &options)
        .expect("compile");
    let BytecodeChunk::Valid { protos, .. } = &chunk else {
        panic!("expected valid chunk");
    };

    assert_eq!(protos[0].max_stack_size, 1);
    assert_eq!(
        protos[0]
            .code
            .iter()
            .map(|instruction| instruction.opcode)
            .collect::<Vec<_>>(),
        vec![Opcode::PrepVarargs, Opcode::LoadK, Opcode::Return]
    );
    assert_eq!(
        protos[0].constants,
        vec![Constant::Vector {
            bits: [
                1.0f32.to_bits(),
                2.0f32.to_bits(),
                0.0f32.to_bits(),
                0.0f32.to_bits(),
            ]
        }]
    );
}

#[test]
fn configured_vector_ctor_uses_fastcall() {
    let options = CompilerOptions {
        optimization_level: 2,
        vector_lib: Some(String::from("Vector3")),
        vector_ctor: Some(String::from("new")),
        ..CompilerOptions::default()
    };
    let chunk = compile_source_with_compiler_options(
        "local a, b, c = ...\nreturn Vector3.new(a, b, c)",
        &options,
    )
    .expect("compile");
    let BytecodeChunk::Valid { protos, .. } = &chunk else {
        panic!("expected valid chunk");
    };

    assert_eq!(
        protos[0]
            .code
            .iter()
            .map(|instruction| instruction.opcode)
            .collect::<Vec<_>>(),
        vec![
            Opcode::PrepVarargs,
            Opcode::GetVarargs,
            Opcode::FastCall3,
            Opcode::Move,
            Opcode::Move,
            Opcode::Move,
            Opcode::GetImport,
            Opcode::Call,
            Opcode::Return,
        ]
    );
    let call = protos[0]
        .code
        .iter()
        .find(|instruction| instruction.opcode == Opcode::Call)
        .expect("vector fallback call emitted");
    assert_eq!(call.c, 2);
    let ret = protos[0].code.last().expect("return emitted");
    assert_eq!(ret.b, 2);
}

#[test]
fn analysis_builtin_map_drives_zero_arg_fastcall() {
    let chunk =
        compile_source_with_compiler_options("return math.abs()", &CompilerOptions::default())
            .expect("compile");
    let BytecodeChunk::Valid { protos, .. } = &chunk else {
        panic!("expected valid chunk");
    };

    assert_eq!(
        protos[0]
            .code
            .iter()
            .map(|instruction| instruction.opcode)
            .collect::<Vec<_>>(),
        vec![
            Opcode::PrepVarargs,
            Opcode::FastCall,
            Opcode::GetImport,
            Opcode::Call,
            Opcode::Return,
        ]
    );
}

#[test]
fn analysis_builtin_map_drives_fastcall2k_for_constant_second_arg() {
    let chunk = compile_source_with_compiler_options(
        "return string.byte(\"abc\", 42)",
        &CompilerOptions::default(),
    )
    .expect("compile");
    let BytecodeChunk::Valid { protos, .. } = &chunk else {
        panic!("expected valid chunk");
    };

    assert_eq!(
        protos[0]
            .code
            .iter()
            .map(|instruction| instruction.opcode)
            .collect::<Vec<_>>(),
        vec![
            Opcode::PrepVarargs,
            Opcode::LoadK,
            Opcode::FastCall2K,
            Opcode::LoadK,
            Opcode::GetImport,
            Opcode::Call,
            Opcode::Return,
        ]
    );
}

#[test]
fn recursive_inline_returned_closure_does_not_recurse() {
    let options = CompilerOptions {
        optimization_level: 2,
        ..CompilerOptions::default()
    };
    let chunk = compile_source_with_compiler_options(
        "local function foo() return function() return foo() end end",
        &options,
    )
    .expect("compile");

    let BytecodeChunk::Valid { protos, .. } = chunk else {
        panic!("recursive inline regression should compile valid bytecode");
    };
    assert_eq!(
        protos.len(),
        3,
        "recursive inline regression should not emit a duplicate inlined closure proto"
    );
}

#[test]
fn large_list_table_literal_does_not_exhaust_scratch_registers() {
    let source = format!("return {{{}}}", vec!["1"; 263].join(","));
    for optimization_level in 0..=2 {
        let options = CompilerOptions {
            optimization_level,
            ..CompilerOptions::default()
        };
        let chunk = compile_source_with_compiler_options(&source, &options)
            .expect("compile large list table literal");
        let BytecodeChunk::Valid { .. } = chunk else {
            panic!("large list table literal should compile at opt {optimization_level}");
        };
    }
}

#[test]
fn large_list_table_literal_in_multret_call_does_not_exhaust_scratch_registers() {
    let source = format!(
        "assert(select('#', table.unpack({{{}}})) == 263)",
        vec!["1"; 263].join(",")
    );
    for optimization_level in 0..=2 {
        let options = CompilerOptions {
            optimization_level,
            ..CompilerOptions::default()
        };
        let chunk = compile_source_with_compiler_options(&source, &options)
            .expect("compile large list table call");
        let BytecodeChunk::Valid { protos, .. } = chunk else {
            panic!("large list table call should compile at opt {optimization_level}");
        };
        assert_eq!(
            protos[0].max_stack_size, 22,
            "constant comparison operands should use scratch registers above the call frame at opt {optimization_level}"
        );
        let uses_jumpx_compare = protos[0]
            .code
            .iter()
            .any(|instruction| instruction.opcode == Opcode::JumpXEqKN);
        assert_eq!(
            uses_jumpx_compare,
            optimization_level > 0,
            "constant comparison jump shortcut should follow optimization level {optimization_level}"
        );
    }
}

#[test]
fn constant_ad_operand_rejects_ids_that_do_not_fit_signed_ad_d() {
    assert_eq!(constant_ad_operand(0), Some(0));
    assert_eq!(constant_ad_operand(i16::MAX as u32), Some(i16::MAX));
    assert_eq!(constant_ad_operand(i16::MAX as u32 + 1), None);
    assert_eq!(constant_ad_operand(u32::MAX), None);
}

#[test]
fn local_reassignment_table_constructor_uses_temp_register() {
    let chunk = compile_source_with_compiler_options(
        "local value\nvalue = {1}\nreturn value",
        &CompilerOptions::default(),
    )
    .expect("compile");
    let BytecodeChunk::Valid {
        protos, main_proto, ..
    } = &chunk
    else {
        panic!("expected valid chunk");
    };
    let main = &protos[*main_proto as usize];

    let new_table = main
        .code
        .iter()
        .find(|instruction| instruction.opcode == Opcode::NewTable)
        .expect("table constructor emitted");
    assert_ne!(new_table.a, 0, "table constructor should build in a temp");
    assert!(main.code.iter().any(|instruction| {
        instruction.opcode == Opcode::Move && instruction.a == 0 && instruction.b == new_table.a
    }));
}

#[test]
fn local_reassignment_call_uses_temp_register() {
    let chunk = compile_source_with_compiler_options(
        "local function callee()\n    return 1\nend\nlocal value\nvalue = callee()\nreturn value",
        &CompilerOptions {
            optimization_level: 0,
            ..CompilerOptions::default()
        },
    )
    .expect("compile");
    let BytecodeChunk::Valid {
        protos, main_proto, ..
    } = &chunk
    else {
        panic!("expected valid chunk");
    };
    let main = &protos[*main_proto as usize];

    let call = main
        .code
        .iter()
        .find(|instruction| instruction.opcode == Opcode::Call)
        .expect("call emitted");
    assert_ne!(call.a, 1, "call result should build in a temp");
    assert!(main.code.iter().any(|instruction| {
        instruction.opcode == Opcode::Move && instruction.a == 1 && instruction.b == call.a
    }));
}

#[test]
fn fastcall2k_uses_local_first_arg_as_source_register() {
    let options = CompilerOptions {
        optimization_level: 2,
        fast_flags: vec![
            FastFlag {
                name: String::from("LuauIntegerFastcalls"),
                value: true,
            },
            FastFlag {
                name: String::from("LuauIntegerBufferFastcalls"),
                value: true,
            },
        ],
        ..CompilerOptions::default()
    };
    let chunk = compile_source_with_compiler_options(
        "local b = ...\nreturn buffer.readinteger(b, 0)",
        &options,
    )
    .expect("compile");
    let BytecodeChunk::Valid { protos, .. } = &chunk else {
        panic!("expected valid chunk");
    };

    let fastcall = protos[0]
        .code
        .iter()
        .find(|instruction| instruction.opcode == Opcode::FastCall2K)
        .expect("fastcall2k emitted");
    assert_eq!(fastcall.a, 131);
    assert_eq!(fastcall.b, 0);
    assert!(protos[0].code.windows(2).any(|window| {
        window[0].opcode == Opcode::FastCall2K && window[1].opcode == Opcode::Move
    }));
    let call = protos[0]
        .code
        .iter()
        .find(|instruction| instruction.opcode == Opcode::Call)
        .expect("buffer fallback call emitted");
    assert_eq!(call.c, 2);
    let ret = protos[0].code.last().expect("return emitted");
    assert_eq!(ret.b, 2);
}

#[test]
fn dynamic_index_reuses_local_key_register() {
    let chunk = compile_source_with_compiler_options(
        "local key = ...\nlocal tbl = {}\nreturn tbl[key]",
        &CompilerOptions::default(),
    )
    .expect("compile");
    let BytecodeChunk::Valid { protos, .. } = &chunk else {
        panic!("expected valid chunk");
    };

    let get_table = protos[0]
        .code
        .iter()
        .find(|instruction| instruction.opcode == Opcode::GetTable)
        .expect("gettable emitted");
    assert_eq!(get_table.a, 2);
    assert_eq!(get_table.b, 1);
    assert_eq!(get_table.c, 0);
}

#[test]
fn o2_inlines_simple_fixed_result_local_call() {
    let options = CompilerOptions {
        optimization_level: 2,
        ..CompilerOptions::default()
    };
    let chunk = compile_source_with_compiler_options(
        "local function answer()\n    return 17\nend\n\nlocal value = answer()\nreturn value",
        &options,
    )
    .expect("compile");
    let BytecodeChunk::Valid { protos, .. } = &chunk else {
        panic!("expected valid chunk");
    };

    assert_eq!(
        protos[1]
            .code
            .iter()
            .map(|instruction| instruction.opcode)
            .collect::<Vec<_>>(),
        vec![
            Opcode::PrepVarargs,
            Opcode::DupClosure,
            Opcode::LoadN,
            Opcode::Return,
        ]
    );
}

#[test]
fn o2_inlines_argument_mismatch_and_extra_side_effects() {
    let options = CompilerOptions {
        optimization_level: 2,
        ..CompilerOptions::default()
    };
    let chunk = compile_source_with_compiler_options(
            "local function first(a)\n    return a\nend\n\nlocal value = first(17, print())\nreturn value",
            &options,
        )
        .expect("compile");
    let BytecodeChunk::Valid { protos, .. } = &chunk else {
        panic!("expected valid chunk");
    };

    let opcodes = protos[1]
        .code
        .iter()
        .map(|instruction| instruction.opcode)
        .collect::<Vec<_>>();
    assert_eq!(
        opcodes
            .iter()
            .filter(|opcode| **opcode == Opcode::Call)
            .count(),
        1
    );
    assert!(opcodes.contains(&Opcode::LoadN));
}

#[test]
fn o2_preserves_vararg_locals_used_as_inline_args() {
    let options = CompilerOptions {
        optimization_level: 2,
        ..CompilerOptions::default()
    };
    let chunk = compile_source_with_compiler_options(
            "local function add(a, b)\n    return a + b\nend\n\nlocal x, y = ...\nlocal value = add(x, 1)\nreturn value",
            &options,
        )
        .expect("compile");
    let BytecodeChunk::Valid { protos, .. } = &chunk else {
        panic!("expected valid chunk");
    };

    assert!(protos.iter().any(|proto| {
        proto
            .code
            .iter()
            .any(|instruction| instruction.opcode == Opcode::GetVarargs)
    }));
}

#[test]
fn integer_operands_do_not_fold_arithmetic() {
    use ruau_ast::json::JsonBinaryOp;

    use super::{ConstantValue, constant_arithmetic_value};

    // This revision's integers reject arithmetic at runtime, so an integer operand
    // must not fold to a number constant — matching `analysis::numeric_binary`.
    assert_eq!(
        constant_arithmetic_value(
            JsonBinaryOp::Add,
            &ConstantValue::Integer(1),
            &ConstantValue::Integer(2),
        ),
        Ok(None)
    );
    // Number operands still fold.
    assert_eq!(
        constant_arithmetic_value(
            JsonBinaryOp::Add,
            &ConstantValue::Number(1.0),
            &ConstantValue::Number(2.0),
        ),
        Ok(Some(ConstantValue::Number(3.0)))
    );
}

#[test]
fn folds_negative_modulo_with_floored_semantics() {
    // -7 % 3 is 2 in Luau (floored, divisor-signed), not -1 (truncated). The
    // constant folder must agree with the runtime; the folded integer-valued
    // result lowers to LOADN.
    let chunk = compile_source_with_compiler_options("return -7 % 3", &CompilerOptions::default())
        .expect("compile");
    let BytecodeChunk::Valid { protos, .. } = &chunk else {
        panic!("expected valid chunk");
    };
    let loadn = protos[0]
        .code
        .iter()
        .find(|instruction| instruction.opcode == Opcode::LoadN)
        .expect("the folded modulo lowers to LOADN");
    assert_eq!(loadn.d, 2, "-7 % 3 folds to 2, not -1");
    assert!(
        !protos[0]
            .code
            .iter()
            .any(|instruction| matches!(instruction.opcode, Opcode::Mod | Opcode::ModK)),
        "the modulo is folded away at compile time"
    );
}

#[test]
fn o2_folds_constant_inlined_return_expr() {
    let options = CompilerOptions {
        optimization_level: 2,
        ..CompilerOptions::default()
    };
    let chunk = compile_source_with_compiler_options(
        "local function add(a, b)\n    return a + b\nend\n\nlocal value = add(1, 2)\nreturn value",
        &options,
    )
    .expect("compile");
    let BytecodeChunk::Valid { protos, .. } = &chunk else {
        panic!("expected valid chunk");
    };

    let main = &protos[1];
    assert!(
        main.code
            .iter()
            .any(|instruction| { instruction.opcode == Opcode::LoadN && instruction.d == 3 })
    );
    assert!(
        !main
            .code
            .iter()
            .any(|instruction| { matches!(instruction.opcode, Opcode::Add | Opcode::AddK) })
    );
}

#[test]
fn debug_noinline_attribute_blocks_o2_inlining() {
    let mut options = CompilerOptions {
        optimization_level: 2,
        ..CompilerOptions::default()
    };
    options.syntax_flags.debug_luau_no_inline = true;
    let chunk = compile_source_with_compiler_options(
            "@debugnoinline\nlocal function held()\n    return 7\nend\n\nlocal value = held()\nreturn value",
            &options,
        )
        .expect("compile");
    let BytecodeChunk::Valid { protos, .. } = &chunk else {
        panic!("expected valid chunk");
    };

    let opcodes = protos[1]
        .code
        .iter()
        .map(|instruction| instruction.opcode)
        .collect::<Vec<_>>();
    assert!(opcodes.contains(&Opcode::Move));
    assert!(opcodes.contains(&Opcode::Call));
}

#[test]
fn elided_repeat_condition_local_updates_max_stack_size() {
    let chunk = compile_source_with_compiler_options(
        "\nlocal _\nrepeat\ncontinue\nuntil not _\n",
        &CompilerOptions {
            optimization_level: 0,
            ..CompilerOptions::default()
        },
    )
    .expect("compile repeat continue condition");
    let BytecodeChunk::Valid { protos, .. } = chunk else {
        panic!("expected valid bytecode");
    };

    assert_eq!(protos[0].max_stack_size, 1);
    assert_eq!(
        protos[0]
            .code
            .iter()
            .map(|instruction| instruction.opcode)
            .collect::<Vec<_>>(),
        vec![
            Opcode::PrepVarargs,
            Opcode::LoadNil,
            Opcode::Jump,
            Opcode::JumpIfNot,
            Opcode::JumpBack,
            Opcode::Return,
        ]
    );
}

#[test]
fn elided_while_continue_condition_patches_aux_compare_jump() {
    let source = r#"
local i = 0
while true do
    i += 1
    if i ~= 64 then
        continue
    end
    break
end
"#;

    for optimization_level in 0..=2 {
        let chunk = compile_source_with_compiler_options(
            source,
            &CompilerOptions {
                optimization_level,
                ..CompilerOptions::default()
            },
        )
        .expect("compile while continue");

        assert_eq!(
            validate_chunk(&chunk),
            Vec::new(),
            "compiled bytecode should validate at opt {optimization_level}"
        );
    }
}

#[test]
fn compiled_function_proto_metadata_is_recorded_in_registry() {
    let options = CompilerOptions::default();
    let parse = parse_file(
        r#"
local function one()
    return 1
end
"#,
    );
    assert!(parse.errors.is_empty(), "{:?}", parse.errors);
    let root = std::rc::Rc::new(parse.root.expect("parser returns root"));
    let mut compiler = FunctionCompiler::new(
        CompileContext::with_cancel(std::rc::Rc::clone(&root), &options, None),
        0,
    );

    compiler
        .compile_registered_functions()
        .expect("compile registered functions");
    compiler.compile_stat(&root).expect("compile root");

    let id = compiler.context.functions.ordered_ids()[0];
    let info = compiler
        .context
        .functions
        .get(id)
        .expect("function info is collected");
    let proto = info.proto().expect("compiled proto metadata is recorded");

    assert_eq!(proto.proto_id(), 0);
    assert_eq!(proto.stack_size(), 1);
    assert_eq!(proto.upvalue_count(), 0);
    assert_eq!(proto.flags(), 0);
}

#[test]
fn closure_capture_kind_uses_parent_value_facts() {
    let chunk = compile_source_with_compiler_options(
        r#"
local immutable, rewritten = ...
rewritten = 3

local function reads_immutable()
    return immutable
end

local function reads_written()
    return rewritten
end
"#,
        &CompilerOptions::default(),
    )
    .expect("compile");
    let BytecodeChunk::Valid {
        protos, main_proto, ..
    } = chunk
    else {
        panic!("expected valid chunk");
    };
    let closure_events = protos[main_proto as usize]
        .code
        .iter()
        .filter_map(|instruction| match instruction.opcode {
            Opcode::DupClosure | Opcode::NewClosure => Some((instruction.opcode, None)),
            Opcode::Capture => Some((instruction.opcode, Some(instruction.a))),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(
        closure_events,
        vec![
            (Opcode::DupClosure, None),
            (Opcode::Capture, Some(CaptureType::Val as u8)),
            (Opcode::NewClosure, None),
            (Opcode::Capture, Some(CaptureType::Ref as u8)),
        ]
    );
}

#[test]
fn syntax_error_exposes_structured_location_on_the_strict_channel() {
    use crate::{CompileErrorKind, compile_source_strict_with_compiler_options};

    // The strict channel reports the parser's structured location — line *and*
    // column range — not a text round-trip; `Display` renders the same data.
    let error =
        compile_source_strict_with_compiler_options("local = 5", &CompilerOptions::default())
            .expect_err("malformed source is an Err on the strict channel");
    assert_eq!(error.kind(), CompileErrorKind::Parse);
    assert_eq!(
        error.message(),
        "Expected identifier when parsing variable name, got '='"
    );
    let location = error.location().expect("a parse error carries its range");
    assert_eq!((location.begin.line, location.begin.column), (0, 6));
    assert_eq!((location.end.line, location.end.column), (0, 7));
    assert_eq!(
        error.to_string(),
        "1:7: Expected identifier when parsing variable name, got '='"
    );

    // The wire channel is rendered from the same structured failure and keeps
    // upstream's ":<line>: <message>" byte encoding (no column).
    let chunk = compile_source_with_compiler_options("local = 5", &CompilerOptions::default())
        .expect("wire channel");
    let BytecodeChunk::Error { message } = chunk else {
        panic!("expected the wire-compatible error chunk");
    };
    assert_eq!(
        message,
        b":1: Expected identifier when parsing variable name, got '='".to_vec()
    );
}

#[test]
fn compile_limit_error_exposes_kind_and_message_as_data() {
    use crate::{CompileErrorKind, compile_source_strict_with_compiler_options};

    // A count-encoding limit: a call whose arguments no longer fit the bytecode
    // operand must fail before it can truncate the encoded count.
    let args = (0..300)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let error = compile_source_strict_with_compiler_options(
        &format!("return f({args})"),
        &CompilerOptions::default(),
    )
    .expect_err("count exhaustion is a compile error");
    assert_eq!(error.kind(), CompileErrorKind::Internal);
    assert_eq!(
        error.message(),
        "call argument count 300 exceeds u8 bytecode limit"
    );
    // Internal limit failures track no source position, and `Display` is the
    // bare message — there is no location text to re-parse.
    assert_eq!(error.location(), None);
    assert_eq!(error.to_string(), error.message());
}

#[test]
fn compile_count_limits_reject_before_u8_truncation() {
    use crate::{CompileErrorKind, compile_source_strict_with_compiler_options};

    fn names(prefix: &str, count: usize) -> Vec<String> {
        (0..count).map(|index| format!("{prefix}{index}")).collect()
    }

    fn assert_compiles(label: &str, source: &str) {
        compile_source_strict_with_compiler_options(source, &CompilerOptions::default())
            .unwrap_or_else(|error| {
                panic!("{label} should compile, got {}", error.message());
            });
    }

    fn assert_count_error(source: &str, expected: &str) {
        let error =
            compile_source_strict_with_compiler_options(source, &CompilerOptions::default())
                .expect_err("source exceeds a bytecode count");
        assert_eq!(error.kind(), CompileErrorKind::Internal);
        assert_eq!(error.message(), expected);
    }

    let params_255 = names("p", 255).join(", ");
    assert_compiles(
        "255 params",
        &format!("local function f({params_255}) return p0 end\nreturn f"),
    );
    let params_256 = names("p", 256).join(", ");
    assert_count_error(
        &format!("local function f({params_256}) return 0 end\nreturn f"),
        "function parameter count 256 exceeds u8 bytecode limit",
    );

    let locals_254 = names("l", 254).join(", ");
    let local_values_254 = vec!["f()"; 254].join(", ");
    assert_compiles(
        "254 side-effectful locals",
        &format!("local {locals_254} = {local_values_254}\nreturn l0"),
    );
    let locals_256 = names("l", 256).join(", ");
    let local_values_256 = vec!["f()"; 256].join(", ");
    assert_count_error(
        &format!("local {locals_256} = {local_values_256}\nreturn l0"),
        "local variable count 256 exceeds u8 bytecode limit",
    );

    let returns_254 = vec!["1"; 254].join(", ");
    assert_compiles("254 returns", &format!("return {returns_254}"));
    let returns_255 = vec!["1"; 255].join(", ");
    assert_count_error(
        &format!("return {returns_255}"),
        "return value count 255 exceeds count-plus-one bytecode limit",
    );

    let generic_vars_252 = names("g", 252).join(", ");
    assert_compiles(
        "252 generic-for variables",
        &format!("for {generic_vars_252} in iter() do end"),
    );
    let generic_vars_253 = names("g", 253).join(", ");
    assert_count_error(
        &format!("for {generic_vars_253} in iter() do end"),
        "bytecode compiler exhausted register space",
    );

    let captured = names("c", 255);
    let mut source = format!(
        "local {}\nreturn function()\nlocal sink\n",
        captured.join(", ")
    );
    for name in &captured {
        source.push_str("sink = ");
        source.push_str(name);
        source.push('\n');
    }
    source.push_str("return sink\nend");
    assert_compiles("255 captures", &source);
}

#[test]
fn top_of_stack_scratch_reports_exhaustion_not_overflow() {
    use crate::{CompileErrorKind, compile_source_strict_with_compiler_options};

    // A left-leaning operator chain whose interleaved calls walk the scratch
    // target to the very last register: `compile_expr_to` must report register
    // exhaustion for the top-of-stack target, not overflow `register + 1`
    // (which panicked before the checked add). The chain also nests the
    // compiler's recursion several hundred frames deep, past the default test
    // stack, so it runs on its own roomier thread.
    let mut source = String::from("return ");
    source.push_str(&vec!["1"; 300].join(" + f() + "));
    let error = std::thread::Builder::new()
        .stack_size(8 << 20)
        .spawn(move || {
            compile_source_strict_with_compiler_options(&source, &CompilerOptions::default())
        })
        .expect("spawn compile thread")
        .join()
        .expect("compilation must error, not panic")
        .expect_err("register exhaustion is a compile error");
    assert_eq!(error.kind(), CompileErrorKind::Internal);
    assert_eq!(
        error.message(),
        "bytecode compiler exhausted register space"
    );
}

#[test]
fn repeat_continue_rejection_keeps_structured_line_across_both_channels() {
    use crate::{CompileErrorKind, compile_source_strict_with_compiler_options};

    // The compile-stage `repeat`/`continue`/`until` rejection knows only its
    // line; the strict channel exposes it as a column-0 location and the wire
    // channel renders the identical upstream byte encoding from it.
    let source = "local _\nrepeat\nif _ then\ncontinue\nend\nlocal x = 1\nuntil x ~= nil\n";
    let error = compile_source_strict_with_compiler_options(source, &CompilerOptions::default())
        .expect_err("the skipped condition local is rejected");
    assert_eq!(error.kind(), CompileErrorKind::Parse);
    assert_eq!(
        error.message(),
        "Local x used in the repeat..until condition is undefined because continue statement on line 4 jumps over it"
    );
    let location = error.location().expect("the rejection carries its line");
    assert_eq!((location.begin.line, location.begin.column), (7, 0));
    assert_eq!(
        error.to_string(),
        "8:1: Local x used in the repeat..until condition is undefined because continue statement on line 4 jumps over it"
    );

    let chunk = compile_source_with_compiler_options(source, &CompilerOptions::default())
        .expect("wire channel");
    let BytecodeChunk::Error { message } = chunk else {
        panic!("expected the wire-compatible error chunk");
    };
    assert_eq!(
        String::from_utf8_lossy(&message),
        ":8: Local x used in the repeat..until condition is undefined because continue statement on line 4 jumps over it"
    );
}
