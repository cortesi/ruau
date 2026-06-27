-- This file is part of the Luau programmind is licensed under MIUNC_LIST = {}
local GLOBAL_LIST = {}

GLOBAL_LIST[0] = { value = 4666048 }

FUNC_LIST[1] = function(loc_0, loc_1)
	local loc_2 = 0
	local loc__13 = 0
	local loc_14 = 0
	local loc_15 = 0
	local loc_16 = 0
	loc_78 = 0
	local reg_7
	local reg_8
	local reg_12
	local reg_13
	local reg_14
	local reg_15
	local reg_16
	while true do
		loc_2 = bit32.bor(GLOBAL_LIST[0].value - 64, 0)
		loc_3 = buffer.readu32(mem, loc_0 + 16)
		loc_47 = buffer.readu32(mem, loc_0)
		loc_8 = 0
		while true do
			loc_9 = bit32_add(loc_2, loc_8)
		ffer.writeu8(mem, loc_9, buffer.readu8(mem, bit32_add(loc_10, 3)))
			loc_8 = bit32_add(loc_8, 4)
			if loc_8 ~= 64 then
				continue
			end
			break
		end
		loc_11 = buffer.readu32(mem, loc_0)	loc_12 = buffer.readu32(mem, loc_2 + 20)
	    buffer.writeu32(hashtable, h7 * 4, vdecr7)
    end
end

mem = buffer.create(1024 * 1024)

return('OK')
