-- Parse and serialize JSON values and navigate dynamic JSON documents.
declare class JsonNull end
export type JsonScalar = JsonNull | boolean | number | string
export type JsonArray = { JsonValue }
export type JsonObject = { [string]: JsonValue }
export type JsonValue = JsonScalar | JsonArray | JsonObject
export type Error = {
    module: string,
    kind: string,
    message: string,
}
export type JsonPath = { string | number }
export type Module = {
    null: JsonNull,
    deserialize: (src: string) -> JsonValue,
    serialize: (value: JsonValue, pretty_print: boolean?) -> string,
    object: <T>(props: T) -> T,
    array: <T>(t: {T}?) -> {T},
    as_object: (value: JsonValue) -> JsonObject?,
    as_array: (value: JsonValue) -> JsonArray?,
    get: (value: JsonValue, path: JsonPath) -> JsonValue,
    get_string: (value: JsonValue, path: JsonPath) -> string?,
    get_number: (value: JsonValue, path: JsonPath) -> number?,
    get_boolean: (value: JsonValue, path: JsonPath) -> boolean?,
    get_array: (value: JsonValue, path: JsonPath) -> JsonArray?,
    get_object: (value: JsonValue, path: JsonPath) -> JsonObject?,
}
declare json: Module
return json
