-- Host introspection API for the current Verber runtime session. Use this module to inspect
-- retained host-owned processes and to fetch module API source from inside Luau.

export type ModuleBackend = "builtin" | "stdlib" | "mcp_server" | "luau"
export type ProcessState =
    "running"
    | "graceful_close_requested"
    | "eof_observed"
    | "kill_requested"
    | "dead"
    | "wait_failed"

export type ProcessInfo = {
    name: string,
    backend: ModuleBackend,
    pid: number?,
    state: ProcessState,
    reason: string?,
}

declare verber: {
    version: () -> string,
    processes: () -> {ProcessInfo},
    api: (path: string) -> string?,
}
