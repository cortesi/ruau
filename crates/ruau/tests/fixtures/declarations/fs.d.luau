-- Filesystem API for paths covered by Verber access grants.

export type Path = string
export type Glob = string
export type Data = string
export type Kind = "file" | "dir" | "symlink" | "other"

export type MetadataDiagnostic = {
    severity: "warning" | "error",
    message: string,
}

export type VerberEntryMetadata = {
    module_path: string?,
    doc: string?,
    source: "vbr" | "index"?,
    diagnostics: { MetadataDiagnostic }?,
}

export type ErrorKind =
    "notFound"
    | "alreadyExists"
    | "notEmpty"
    | "wrongKind"
    | "invalidPath"
    | "permission"
    | "invalidGlob"
    | "invalidOption"
    | "callback"
    | "io"

export type Error = {
    module: "fs",
    kind: ErrorKind,
    path: Path?,
    other: Path?,
    pattern: Glob?,
    expected: { Kind }?,
    actual: Kind?,
    message: string,
}

export type Entry = {
    path: Path,
    name: string,
    kind: Kind,
    size: number,
    mtime: number,
    verber: VerberEntryMetadata?,
}

export type ReadOptions = {
    startLine: number?,
    endLine: number?,
}

export type WriteMode = "create" | "replace" | "upsert"
export type GlobPattern = Glob | { Glob }
export type KindFilter = Kind | { Kind }

export type GlobOptions = {
    includeHidden: boolean?,
    includeIgnored: boolean?,
    followLinks: boolean?,
    maxDepth: number?,
    kind: KindFilter?,
    exclude: GlobPattern?,
}

declare fs: {
    stat: (path: Path) -> Entry?,
    list: (path: Path) -> { Entry },
    glob: (base: Path, pattern: GlobPattern, options: GlobOptions?) -> { Entry },
    read: (path: Path, options: ReadOptions?) -> Data,
    write: (path: Path, data: Data, mode: WriteMode?) -> (),
    edit: (path: Path, transform: (Data) -> (Data?)) -> (),
    mkdir: (path: Path, parents: boolean?) -> (),
    remove: (path: Path, recursive: boolean?) -> (),
    rename: (from: Path, to: Path) -> (),
}
