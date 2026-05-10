-- MCP lifecycle and protocol escape hatch for path-discovered server declarations.

export type JsonValue = nil | boolean | number | string | buffer | {JsonValue} | {[string]: JsonValue}
export type ServerStatus = "disabled" | "discovered" | "running" | "failed"

export type Diagnostic = {
    severity: "warning" | "error",
    code: string,
    message: string,
}

export type Server = {
    path: string,
    source_path: string?,
    auth_key: string?,
    status: ServerStatus,
    diagnostics: {Diagnostic}?,
}

export type ProtocolAnnotations = {
    audience: {string}?,
    priority: number?,
    lastModified: string?,
    _meta: JsonValue?,
}

export type ProtocolTextContent = {
    type: "text",
    text: string,
    annotations: ProtocolAnnotations?,
    _meta: JsonValue?,
}

export type ProtocolResourceLinkContent = {
    type: "resource_link",
    uri: string,
    name: string,
    title: string?,
    description: string?,
    mimeType: string?,
    annotations: ProtocolAnnotations?,
    _meta: JsonValue?,
}

export type ProtocolUnknownContentBlock = {
    type: string,
    _meta: JsonValue?,
}

export type ProtocolContentBlock =
    ProtocolTextContent
    | ProtocolResourceLinkContent
    | ProtocolUnknownContentBlock

export type ProtocolCallToolResult = {
    [string]: JsonValue,
    content: {ProtocolContentBlock},
    isError: boolean?,
    structuredContent: JsonValue?,
    _meta: JsonValue?,
}

export type MetadataDiagnostic = Diagnostic

export type ToolMetadata = {
    name: string,
    original_name: string,
    description: string?,
    result_mode: "auto" | "structured" | "json_text" | "text" | "content",
    has_output_schema: boolean,
    diagnostics: {MetadataDiagnostic}?,
}

declare mcp: {
    status: () -> {Server},
    stop: (path: string) -> (),
    call_tool: (path: string, name: string, args: {[string]: JsonValue}?) -> ProtocolCallToolResult,
    tool: (path: string, name: string) -> ToolMetadata?,
    tools: (path: string) -> {ToolMetadata},
}
