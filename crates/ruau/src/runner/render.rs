use ruau_vm::MarshaledValue;

use super::{
    TenantId,
    types::{
        RequestError, RequestMetrics, RequestReport, RequestReportMetadata, RequestReportOutcome,
        ResultValue,
    },
};

/// Renders a rendered error value for a one-line message. The full value is
/// available structurally on the variant; this is the human summary.
pub(super) fn render_error_value(value: &ResultValue) -> String {
    match value {
        ResultValue::Nil => "nil".to_string(),
        ResultValue::Boolean(b) => b.to_string(),
        ResultValue::Number(n) => n.to_string(),
        ResultValue::Integer(i) => i.to_string(),
        ResultValue::Vector([x, y, z]) => format!("vector({x}, {y}, {z})"),
        // The error string is tenant-controlled and unbounded; the `Display`
        // message is a one-line log line, so collapse control bytes (newlines,
        // tabs) to spaces and truncate. The full value stays on the variant.
        ResultValue::String(bytes) => summarize_error_text(&String::from_utf8_lossy(bytes)),
        ResultValue::Buffer(bytes) => format!("<buffer: {} bytes>", bytes.len()),
        ResultValue::Table(_) => "<table>".to_owned(),
        ResultValue::Opaque(ty) => format!("<{ty}>"),
    }
}

/// Collapses control characters to spaces and truncates to a bounded length, so a
/// tenant cannot blow up or inject newlines into a one-line `Display` message.
pub(super) fn summarize_error_text(text: &str) -> String {
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
pub(super) fn request_report_success(
    values: Vec<ResultValue>,
    metrics: RequestMetrics,
    metadata: RequestReportMetadata,
    tenant: TenantId,
) -> RequestReport {
    RequestReport {
        tenant,
        outcome: RequestReportOutcome::Success { values },
        metrics,
        metadata,
        failure_category: None,
        stop_reason: None,
    }
}

pub(super) fn request_report_error(
    error: RequestError,
    metrics: RequestMetrics,
    metadata: RequestReportMetadata,
    tenant: TenantId,
) -> RequestReport {
    let failure_category = Some(error.category());
    let stop_reason = error.stop_reason();
    RequestReport {
        tenant,
        outcome: RequestReportOutcome::Failure { error },
        metrics,
        metadata,
        failure_category,
        stop_reason,
    }
}
/// Renders a raw heap value into owned data. Strings are copied out; values that
/// cannot leave the VM are reduced to their type name.
impl From<MarshaledValue> for ResultValue {
    fn from(value: MarshaledValue) -> Self {
        match value {
            MarshaledValue::Nil => Self::Nil,
            MarshaledValue::Boolean(value) => Self::Boolean(value),
            MarshaledValue::Number(value) => Self::Number(value),
            MarshaledValue::Integer(value) => Self::Integer(value),
            MarshaledValue::Vector(value) => Self::Vector(value),
            MarshaledValue::LightUserdata { .. } => Self::Opaque("userdata"),
            MarshaledValue::String(bytes) => Self::String(bytes),
            MarshaledValue::Buffer(bytes) => Self::Buffer(bytes),
            MarshaledValue::Table(pairs) => Self::Table(
                pairs
                    .into_iter()
                    .map(|pair| (Self::from(pair.key), Self::from(pair.value)))
                    .collect(),
            ),
            MarshaledValue::Opaque(kind) => Self::Opaque(kind),
        }
    }
}
