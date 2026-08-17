//! Tag methods (metamethods): event names and metatable lookup (port `ltm.cpp`).
//!
//! A metamethod is a string-keyed entry in a value's metatable. Tables carry
//! their own metatable (`LuaTable::metatable`); strings use the shared string
//! metatable on the heap; host userdata use their registered type's shared
//! metatable. Other basic-type metatables currently resolve to `None`.

use crate::{
    api::{RawGc, RawValue, marker},
    call::{Exec, err_memory},
    heap::Heap,
};

/// A metamethod event; its byte name is the key looked up in a metatable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetaEvent {
    Index,
    NewIndex,
    Add,
    Sub,
    Mul,
    Div,
    IDiv,
    Mod,
    Pow,
    Unm,
    Eq,
    Lt,
    Le,
    Len,
    Call,
    Concat,
    ToString,
    Iter,
}

impl MetaEvent {
    /// Every metamethod event, in discriminant order (indexes the heap's
    /// pre-interned name cache).
    pub const ALL: [Self; 18] = [
        Self::Index,
        Self::NewIndex,
        Self::Add,
        Self::Sub,
        Self::Mul,
        Self::Div,
        Self::IDiv,
        Self::Mod,
        Self::Pow,
        Self::Unm,
        Self::Eq,
        Self::Lt,
        Self::Le,
        Self::Len,
        Self::Call,
        Self::Concat,
        Self::ToString,
        Self::Iter,
    ];

    /// The metatable key for this event (`__index`, `__add`, …).
    #[must_use]
    pub fn name(self) -> &'static [u8] {
        match self {
            Self::Index => b"__index",
            Self::NewIndex => b"__newindex",
            Self::Add => b"__add",
            Self::Sub => b"__sub",
            Self::Mul => b"__mul",
            Self::Div => b"__div",
            Self::IDiv => b"__idiv",
            Self::Mod => b"__mod",
            Self::Pow => b"__pow",
            Self::Unm => b"__unm",
            Self::Eq => b"__eq",
            Self::Lt => b"__lt",
            Self::Le => b"__le",
            Self::Len => b"__len",
            Self::Call => b"__call",
            Self::Concat => b"__concat",
            Self::ToString => b"__tostring",
            Self::Iter => b"__iter",
        }
    }
}

/// The metatable governing `value`, if any. Tables carry their own; strings
/// share the VM's string metatable (so `("s"):method()` resolves through the
/// `string` library); host userdata share their registered type's metatable
/// (see [`crate::host_type`]). The remaining basic-type metatables currently
/// resolve to `None`.
#[must_use]
pub fn metatable(heap: &Heap, value: RawValue) -> Option<RawGc<marker::Table>> {
    match value {
        RawValue::Table(handle) => heap.table(handle).and_then(|t| t.metatable()),
        RawValue::String(_) => heap.string_metatable(),
        RawValue::Vector(_) => heap.vector_metatable(),
        RawValue::Userdata(handle) => heap.userdata_metatable(handle),
        _ => None,
    }
}

/// Looks up the metamethod handler for `value`'s metatable and `event`, returning
/// `Some(handler)` for a non-`nil` entry and `None` when the value has no
/// metatable or the entry is absent.
///
/// # Errors
/// Returns an error only if interning the event name runs out of memory.
pub fn get_metamethod(heap: &Heap, value: RawValue, event: MetaEvent) -> Exec<Option<RawValue>> {
    let Some(mt) = metatable(heap, value) else {
        return Ok(None);
    };
    // The event names are pre-interned at heap construction (and GC-rooted),
    // so a metamethod probe never re-hashes the name.
    let key = heap
        .metamethod_name(event)
        .ok_or_else(|| err_memory("out of memory interning a metamethod name"))?;
    let handler = heap
        .table(mt)
        .map_or(RawValue::Nil, |t| t.get(RawValue::String(key)));
    Ok(match handler {
        RawValue::Nil => None,
        other => Some(other),
    })
}
