-- Durable-session API for inspecting the current Verber session and recent project-local sessions.

export type Id = string
export type Time = string
export type Driver = "attach" | "mcp" | "cli" | "cron" | "embedded"
export type Status = "active" | "ok" | "error" | "cancelled" | "incomplete"
export type AccessMode = "read_only" | "read_write"
export type ModuleBackend = "builtin" | "stdlib" | "mcp_server" | "luau"
export type ConfigPathKind = "user" | "project" | "explicit"

export type ConfigPath = {
    kind: ConfigPathKind,
    path: string,
}

export type ModuleIdentity = {
    backend: ModuleBackend,
    path: string?,
    manifest_hash: string,
}

export type Config = {
    modules: { [string]: boolean },
    access: { [string]: AccessMode },
}

export type OkOutcome = {
    kind: "ok",
    prints: { string },
    value: any,
}

export type RuntimeErrorOutcome = {
    kind: "runtime_error",
    prints: { string },
    error: any,
    traceback: string,
}

export type Outcome = OkOutcome | RuntimeErrorOutcome

export type Exec = {
    id: Id,
    seq: number,
    started: Time,
    finished: Time?,
    script: string,
    outcome: Outcome?,
}

declare class Session
    id: Id
    driver: Driver
    cwd: string
    verber_version: string
    started: Time
    finished: Time?
    status: Status
    config_paths: { ConfigPath }
    config: Config
    inventory: { [string]: ModuleIdentity }
    exec_count: number

    function execs(self): { Exec }
    function note(self): string
end

declare session: {
    current: () -> Session,
    recent: (limit: number?) -> { Session },
    get: (id: Id) -> Session?,
    set_note: (note: string) -> (),
}
