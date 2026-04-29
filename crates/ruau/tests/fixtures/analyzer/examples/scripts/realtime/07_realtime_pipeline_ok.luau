-- expect: pass
--!strict

type Counter = {
    value: number,
    inc: (self: Counter, step: number?) -> number,
}

local Counter = {}
Counter.__index = Counter

function Counter.new(initial: number): Counter
    return setmetatable({ value = initial }, Counter) :: Counter
end

function Counter:inc(step: number?): number
    local delta = step or 1
    self.value += delta
    return self.value
end

local counter = Counter.new(0)
for _ = 1, 5 do
    counter:inc()
end

local final_value: number = counter:inc(10)
print(final_value)
