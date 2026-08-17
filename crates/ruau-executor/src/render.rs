use ruau_vm::ValueSnapshot;

use super::{
    TenantId,
    types::{RequestError, RequestMetrics, RunMetadata, RunReport},
};

/// Renders a rendered error value for a one-line message. The full value is
/// available structurally on the variant; this is the human summary.
pub fn render_error_value(value: &ValueSnapshot) -> String {
    match value {
        ValueSnapshot::Nil => "nil".to_string(),
        ValueSnapshot::Boolean(b) => b.to_string(),
        ValueSnapshot::Number(n) => n.to_string(),
        ValueSnapshot::Integer(i) => i.to_string(),
        ValueSnapshot::Vector([x, y, z]) => format!("vector({x}, {y}, {z})"),
        ValueSnapshot::LightUserdata { .. } => "<userdata>".to_owned(),
        // The error string is tenant-controlled and unbounded; the `Display`
        // message is a one-line log line, so collapse control bytes (newlines,
        // tabs) to spaces and truncate. The full value stays on the variant.
        ValueSnapshot::String(bytes) => summarize_error_text(&String::from_utf8_lossy(bytes)),
        ValueSnapshot::Buffer(bytes) => format!("<buffer: {} bytes>", bytes.len()),
        ValueSnapshot::Table(_) => "<table>".to_owned(),
        ValueSnapshot::Opaque(ty) => format!("<{ty}>"),
    }
}

/// Collapses control characters to spaces and truncates to a bounded length, so a
/// tenant cannot blow up or inject newlines into a one-line `Display` message.
fn summarize_error_text(text: &str) -> String {
    const MAX_CHARS: usize = 200;
    let mut out = String::new();
    for (i, ch) in text.chars().enumerate() {
        if i >= MAX_CHARS {
            out.push('…');
            break;
        }
        out.push(if ch.is_control() { ' ' } else { ch });
    }
    out
}
pub fn request_report_success(
    values: Vec<ValueSnapshot>,
    metrics: RequestMetrics,
    metadata: RunMetadata,
    tenant: TenantId,
) -> RunReport {
    RunReport {
        tenant,
        #[cfg(any())]
        outcome: super::types::TestReportOutcome::Success {
            values: values.clone(),
        },
        result: Ok(values),
        metrics,
        metadata,
        failure_category: None,
        stop_reason: None,
    }
}

pub fn request_report_error(
    error: RequestError,
    metrics: RequestMetrics,
    metadata: RunMetadata,
    tenant: TenantId,
) -> RunReport {
    let failure_category = Some(error.category());
    let stop_reason = error.stop_reason();
    RunReport {
        tenant,
        #[cfg(any())]
        outcome: super::types::TestReportOutcome::Failure {
            error: error.clone(),
        },
        result: Err(error),
        metrics,
        metadata,
        failure_category,
        stop_reason,
    }
}
