-- expect: fail
--!strict

local value: string? = "demo"
if value then
    local _: number = value
end
