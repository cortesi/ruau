-- Reduced from private-regression/slow-unit-51e5f868b591255b8c032504fcdf1d13f829c3f2.
-- The original slow-unit input no longer reproduces as slow on the current
-- compiler, but it is an invalid front-end input and should stay in the
-- hardening corpus as stable error bytecode.
omparison compatibilities
local t1, t2, c, d
t1 = {}; c = {}; setmetatable(c, t1)
d = {}
t1.__eq = function() return true end
t1.__lt = function() return true end
assert(c ~= d and not pcall(function() return c < d end))
setmetatable(d, t1)
assert(c == d and c < d and not(d <= c))

--[[
The original artifact's tail was swallowed by an unfinished block comment
after a mutated library-basics source. Keeping the unterminated long comment
preserves the front-end shape without checking in the full 30 KB generated
input.

mutated source tail
mutated source tail
mutated source tail
mutated source tail
mutated source tail
mutated source tail
mutated source tail
mutated source tail
