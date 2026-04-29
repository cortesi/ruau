-- expect: pass
--!strict

type Result<T, E> =
    { ok: true, value: T }
    | { ok: false, error: E }

local function parse_count(value: string): Result<number, string>
    local parsed = tonumber(value)
    if parsed then
        return { ok = true, value = parsed }
    end
    return { ok = false, error = "not a number" }
end

local result = parse_count("12")
if result.ok then
    local _: number = result.value
else
    local _: string = result.error
end
