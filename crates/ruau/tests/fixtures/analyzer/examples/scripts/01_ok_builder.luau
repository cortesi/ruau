-- expect: pass
--!strict

local todo = Todo.create()
    :content("Review plan")
    :due("2026-03-06")
    :save()

todo:complete()
local generated_id = make_id("todo")
local _: string = generated_id
