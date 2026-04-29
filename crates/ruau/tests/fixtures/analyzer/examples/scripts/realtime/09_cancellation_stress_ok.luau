-- expect: pass
--!strict

type Token = "a" | "b" | "c" | "d" | "e" | "f" | "g" | "h"

type TokenMap = { [Token]: number }

local map: TokenMap = {
    a = 1,
    b = 2,
    c = 3,
    d = 4,
    e = 5,
    f = 6,
    g = 7,
    h = 8,
}

local total = 0
for _, key in ipairs({ "a", "b", "c", "d", "e", "f", "g", "h" }) do
    total += map[key :: Token]
end

local _: number = total
