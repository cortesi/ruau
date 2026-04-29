-- expect: pass
--!strict

local maybe_text: string? = "demo"
if maybe_text then
    local uppercase = string.upper(maybe_text)
    print(uppercase)
end
