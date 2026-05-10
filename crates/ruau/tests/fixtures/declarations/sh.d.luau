-- Process API for resolving executables and running child processes without a shell.

export type Path = string
export type Data = string

export type ErrorKind =
    "notFound"
    | "permission"
    | "invalidArgs"
    | "invalidOption"
    | "cancelled"
    | "timeout"
    | "io"

export type Error = {
    module: "sh",
    kind: ErrorKind,
    program: string?,
    hostPath: Path?,
    message: string,
}

export type Result = {
    program: Path,
    args: { string },
    cwd: Path,
    exitCode: number?,
    signal: number?,
    stdout: Data,
    stderr: Data,
}

export type RunOptions = {
    cwd: Path?,
    stdin: Data?,
    env: { [string]: string }?,
    clearEnv: boolean?,
    timeout: number?,
}

declare sh: {
    which: (name: string) -> Path?,
    run: (program: string, args: { string }?, options: RunOptions?) -> Result,
}
