--!strict
local term = require("term")

return {
    current_cols = function(): number
        return term.current().cols
    end,
}
