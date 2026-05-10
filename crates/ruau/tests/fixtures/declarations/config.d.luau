-- Runtime config API for inspecting startup arguments and the resolved session config.
export type AccessMode = "read_only" | "read_write"

export type PathGrant = AccessMode | {
    access: AccessMode,
    mcp: boolean?,
}

export type Config = {
    cwd: string,
    paths: { [string]: PathGrant },
}

declare config: {
    args: () -> { [string]: string },
    current: () -> Config,
}
