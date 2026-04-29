-- expect: fail
--!strict

local todo = Todo.create():content("Call missing method"):save()
todo:archive()
