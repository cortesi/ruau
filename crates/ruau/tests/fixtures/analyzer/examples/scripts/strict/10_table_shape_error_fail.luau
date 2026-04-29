-- expect: fail
--!strict

type User = {
    id: string,
    active: boolean,
}

local user: User = {
    id = 123,
    active = true,
}

print(user)
