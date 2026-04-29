-- expect: fail
--!strict

local _todo = Todo.create()
    :content("Ship release")
    :due(42)
    :save()
