-- expect: fail
--!strict

local other = require("./other_module")
local _: number = other.value
