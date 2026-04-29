declare class Todo
    content: string?
    done: boolean
    due: string?
    function complete(self): ()
    function cancel(self): ()
end

declare class TodoBuilder
    function content(self, content: string): TodoBuilder
    function due(self, due: string): TodoBuilder
    function save(self): Todo
end

declare Todo: {
    create: () -> TodoBuilder,
}

declare function make_id(prefix: string): string
