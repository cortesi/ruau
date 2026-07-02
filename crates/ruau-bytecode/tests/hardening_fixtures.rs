//! Data-driven bytecode hardening regression tests.
#![allow(clippy::tests_outside_test_module)]

use std::{fs, path::Path};

use ruau_bytecode::{
    BytecodeChunk, CompileOptions, ValidationError, compile_source, decode_chunk,
    disasm::disassemble_chunk, encode_chunk, validate_chunk,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct HardeningManifest {
    #[serde(default)]
    expected_outcome: ExpectedOutcome,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum ExpectedOutcome {
    #[default]
    ValidRoundtrip,
    ErrorBytecode,
}

#[test]
fn bytecode_hardening_fixtures_are_stable() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate lives under crates/ruau-bytecode");
    let root = repo_root.join("crates/ruau-bytecode/fixtures/hardening");
    let mut cases = fs::read_dir(&root)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", root.display()))
        .map(|entry| {
            entry
                .unwrap_or_else(|error| panic!("failed to read hardening entry: {error}"))
                .path()
        })
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    cases.sort();

    assert!(
        !cases.is_empty(),
        "bytecode hardening corpus should contain at least one fixture"
    );
    for case in cases {
        check_bytecode_hardening_case(&case);
    }
}

fn check_bytecode_hardening_case(case: &Path) {
    let manifest_path = case.join("manifest.toml");
    let manifest = fs::read_to_string(&manifest_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", manifest_path.display()));
    let manifest: HardeningManifest = toml::from_str(&manifest)
        .unwrap_or_else(|error| panic!("failed to parse {}: {error}", manifest_path.display()));

    let source_path = case.join("source.luau");
    let source = fs::read_to_string(&source_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", source_path.display()));

    for optimization_level in 0..=2 {
        let options = CompileOptions {
            optimization_level,
            ..CompileOptions::default()
        };
        let first = compile_source(&source, &options, None).unwrap_or_else(|error| {
            panic!(
                "{} opt {optimization_level} failed to compile: {error}",
                case.display()
            )
        });
        let second = compile_source(&source, &options, None).unwrap_or_else(|error| {
            panic!(
                "{} opt {optimization_level} failed second compile: {error}",
                case.display()
            )
        });
        assert_eq!(
            first,
            second,
            "{} opt {optimization_level} compiled nondeterministically",
            case.display()
        );
        match (&first, manifest.expected_outcome) {
            (BytecodeChunk::Valid { .. }, ExpectedOutcome::ValidRoundtrip) => {}
            (BytecodeChunk::Error { .. }, ExpectedOutcome::ErrorBytecode) => continue,
            (BytecodeChunk::Valid { .. }, ExpectedOutcome::ErrorBytecode) => {
                panic!(
                    "{} opt {optimization_level} unexpectedly produced valid bytecode",
                    case.display()
                );
            }
            (BytecodeChunk::Error { .. }, ExpectedOutcome::ValidRoundtrip) => {
                panic!(
                    "{} opt {optimization_level} produced error bytecode",
                    case.display()
                );
            }
        }
        assert_validation_clean(case, optimization_level, "compiled chunk", &first);
        let encoded = encode_chunk(&first).unwrap_or_else(|error| {
            panic!(
                "{} opt {optimization_level} failed to encode: {error}",
                case.display()
            )
        });
        let decoded = decode_chunk(&encoded).unwrap_or_else(|error| {
            panic!(
                "{} opt {optimization_level} failed to decode: {error}",
                case.display()
            )
        });
        assert_validation_clean(case, optimization_level, "decoded chunk", &decoded);
        assert_eq!(
            decoded,
            first,
            "{} opt {optimization_level} failed encode/decode roundtrip",
            case.display()
        );
    }
}

fn assert_validation_clean(
    case: &Path,
    optimization_level: u8,
    context: &str,
    chunk: &BytecodeChunk,
) {
    let errors = validate_chunk(chunk);
    assert!(
        errors.is_empty(),
        "{} opt {optimization_level} {context} failed bytecode validation:\n{}\n\n{}",
        case.display(),
        format_validation_errors(&errors),
        disassemble_chunk(chunk)
    );
}

fn format_validation_errors(errors: &[ValidationError]) -> String {
    errors
        .iter()
        .map(|error| {
            let location = match (error.proto_index, error.instruction_index) {
                (Some(proto), Some(instruction)) => {
                    format!("proto {proto}, instruction {instruction}")
                }
                (Some(proto), None) => format!("proto {proto}"),
                (None, Some(instruction)) => format!("instruction {instruction}"),
                (None, None) => "chunk".to_owned(),
            };
            format!("{location}: {:?}: {}", error.kind, error.message)
        })
        .collect::<Vec<_>>()
        .join("\n")
}
