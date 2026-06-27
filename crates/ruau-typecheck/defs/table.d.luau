declare table: {
    concat: <V>(t: { V }, sep: string?, i: number?, j: number?) -> string,
    insert: (<V>(t: { V }, value: V) -> ()) & (<V>(t: { V }, pos: number, value: V) -> ()),
    maxn: <V>(t: { V }) -> number,
    remove: <V>(t: { V }, number?) -> V?,
    sort: <V>(t: { V }, comp: ((V, V) -> boolean)?) -> (),
    create: <V>(count: number, value: V?) -> { V },
    find: <V>(haystack: { V }, needle: V, init: number?) -> number?,

    unpack: <V>(list: { V }, i: number?, j: number?) -> ...V,
    pack: <V>(...V) -> { n: number, [number]: V },

    getn: <V>(t: { V }) -> number,
    foreach: <K, V>(t: { [K]: V }, f: (K, V) -> ()) -> (),
    foreachi: <V>({ V }, (number, V) -> ()) -> (),

    move: <V>(src: { V }, a: number, b: number, t: number, dst: { V }?) -> { V },

    freeze: <T>(t: T) -> T,
    clone: <T>(t: T) -> T,
    clear: (table: {}) -> (),
    isfrozen: (t: {}) -> boolean,
}
