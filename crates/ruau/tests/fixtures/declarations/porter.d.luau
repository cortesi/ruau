-- Porter Luau API excerpt from Subagent.
-- Scripts orchestrate coding agents and return structured data.

export type Error = {
    kind: "timeout" | "wait_timeout" | "cancelled" | "shutdown" | "backend",
    message: string,
}

export type TokenUsage = {
    input: number,
    output: number,
    cache_read: number,
    cache_creation: number,
    cost_usd: number?,
}

export type TurnResult = {
    text: string,
    stop_reason: string,
    usage: TokenUsage?,
    duration: number?,
    session_id: string?,
    structured: any?,
}

export type Pending = {
    wait: (self: Pending, seconds: number?) -> (TurnResult?, Error?),
    cancel: (self: Pending) -> (),
}

export type Message = {
    role: "user" | "assistant",
    text: string,
}

export type Agent = {
    provider: "codex" | "gemini" | "claude",
    model: string,
    prompt: (self: Agent, input: string) -> (TurnResult?, Error?),
    spawn: (self: Agent, input: string) -> Pending,
    cancel: (self: Agent) -> (),
    reset: (self: Agent) -> (),
    shutdown: (self: Agent) -> (),
    transcript: (self: Agent) -> { Message },
}

export type CodexConfig = {
    namespace: string?,
    cwd: string?,
    instructions: string?,
    timeout_ms: number?,
    sandbox: ("read-only" | "workspace-write" | "danger-full-access")?,
    reasoning_effort: ("low" | "medium" | "high" | "xhigh")?,
    approvals: ("allow" | "deny" | "on-request")?,
}
