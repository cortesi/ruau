-- expect: pass
--!strict

type Box<T> = { value: T }

type DeepBox = Box<Box<Box<Box<Box<Box<number>>>>>>

local function wrap(n: number): DeepBox
    return {
        value = {
            value = {
                value = {
                    value = {
                        value = {
                            value = n,
                        },
                    },
                },
            },
        },
    }
end

local function unwrap(box: DeepBox): number
    return box.value.value.value.value.value.value
end

local value: number = unwrap(wrap(42))
print(value)
