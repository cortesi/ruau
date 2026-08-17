//! Convert borrowed source and graph diagnostics into a serializable host envelope.

use std::sync::Arc;

use ruau::{
    source::{InMemorySource, ModuleId, Source},
    surface::{CheckOptions, Surface},
    typecheck::{DiagnosticRecord, ModuleDiagnosticRecord},
};
use serde::Serialize;

#[derive(Serialize)]
struct AppDiagnostic {
    module: Option<String>,
    display_name: Option<String>,
    severity: String,
    category: String,
    code: u32,
    message: String,
    payload: serde_json::Value,
}

impl From<DiagnosticRecord> for AppDiagnostic {
    fn from(record: DiagnosticRecord) -> Self {
        Self {
            module: None,
            display_name: None,
            severity: format!("{:?}", record.severity).to_lowercase(),
            category: record.category_label,
            code: record.code,
            message: record.message,
            payload: record.wire_payload,
        }
    }
}

impl From<ModuleDiagnosticRecord> for AppDiagnostic {
    fn from(record: ModuleDiagnosticRecord) -> Self {
        let mut diagnostic = Self::from(record.diagnostic);
        diagnostic.module = Some(record.module.to_string());
        diagnostic.display_name = Some(record.display_name);
        diagnostic
    }
}

fn main() -> Result<(), String> {
    let standalone = Surface::new().check(
        &Source::text(
            ModuleId::canonicalized("standalone"),
            "--!strict\nlocal n: number = 'text'",
        ),
        CheckOptions::default(),
    );
    let source = standalone
        .diagnostics()
        .records()
        .map(AppDiagnostic::from)
        .collect::<Vec<_>>();

    let modules = Arc::new(InMemorySource::new().with_module(
        ModuleId::new("app/main"),
        "--!strict\nreturn require('./missing')",
    ));
    let surface = Surface::builder()
        .module_source(modules)
        .build()
        .map_err(|error| error.to_string())?;
    let graph = surface
        .check_graph_ready(
            ruau::surface::GraphRoot::existing("app/main"),
            Default::default(),
        )
        .map_err(|error| error.to_string())?
        .diagnostics()
        .records()
        .map(AppDiagnostic::from)
        .collect::<Vec<_>>();

    let envelope = serde_json::json!({ "source": source, "graph": graph });
    println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
    Ok(())
}
