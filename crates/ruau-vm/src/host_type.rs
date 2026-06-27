//! Embedder-typed host userdata: registration, dispatch, and the borrow model.
//!
//! A host application registers Rust types on [`VmBuilder`](crate::VmBuilder)
//! at build time and hands *instances* of those types to scripts as `userdata`
//! values. Scripts call methods and read fields on the instances; they can
//! never construct, forge, or introspect one. This module is the design note
//! and implementation for that surface.
//!
//! # Registration and builder API
//!
//! A type is described by a [`HostTypeBuilder<T>`] — methods over `&T`, mut
//! methods over `&mut T`, and field getters — and finalized into a type-erased
//! [`HostType`] descriptor that [`VmBuilder::host_type`](crate::VmBuilder)
//! registers, parallel to how a `NativeModule` registers. Registration is
//! strictly build-time: the per-type dispatch tables are constructed once in
//! `VmBuilder::build`, before any script runs and before sandboxing, and a
//! mid-install failure (a duplicate registration or a failed allocation)
//! poisons the VM, so the first entry point errors cleanly rather than running
//! a script against a partial dispatch surface. Instances are created later,
//! inside any [`Scope`] step, with
//! `Scope::create_userdata::<T>(value)`; creating a value of an unregistered
//! type is a clean [`RuntimeError`].
//!
//! # Metatable identity and immutability
//!
//! Each registered type gets **one shared metatable** built at VM build (the
//! `Vm::set_vector_metatable` precedent): `__index` resolves methods and
//! getters, `__type` names the type for `typeof`, and `__metatable` carries the
//! type name as protection, so script-side `getmetatable` returns the name
//! string, never the table. The metatable and method table are marked
//! `readonly` at build — they are immutable under every profile, sandboxed or
//! not, so no script (and no later host step) can swap a method out from under
//! another instance. Both tables are registry-pinned for the VM's lifetime;
//! the userdata objects themselves stay GC leaves.
//!
//! Dispatch uses the ordinary `__index` metamethod path (`NAMECALL` resolves
//! through the same lookup): when a type registers no getters, `__index` is the
//! plain method table — one raw table hop per method resolution; with getters
//! it is a dispatch function that checks the method table first, then the
//! getter list. The VM has no separate `__namecall` fast path today, so there
//! is nothing faster to wire.
//!
//! # Borrow model and reentrancy
//!
//! The embedded `T` lives in a [`HostCell<T>`] — the value plus a `Cell<isize>`
//! borrow flag (`0` free, `n > 0` shared readers, `-1` exclusive) — boxed
//! behind the heap-owned userdata object, so its address is stable even when
//! the userdata arena reallocates. Borrows are runtime-checked, RefCell-style,
//! but every violation is a **catchable [`RuntimeError`]**, never a panic, and
//! never poisons the VM:
//!
//! - A `&T` method (or `Userdata::borrow::<T>`) takes a shared borrow; any
//!   number may nest.
//! - A `&mut T` method (or `borrow_mut`) takes the exclusive borrow; it fails
//!   while any other borrow is live.
//! - Reentrancy is therefore safe by construction: a method that calls back
//!   into Lua (via `Scope::call`) which re-enters a mut method on the *same*
//!   instance sees the live borrow flag and gets the catchable error — the
//!   exact scenario the conflict flag exists for.
//! - Borrowing a foreign type (`borrow::<U>` on a `T` instance, or calling a
//!   stolen method function with the wrong receiver) fails the typed downcast
//!   with a clean catchable error naming both types.
//!
//! The borrow guards are scope-branded (`UserdataRef<'s, T>`), so they cannot
//! outlive the step that minted them. Their soundness rests on two facts:
//! the boxed cell never moves (only the box pointer lives in the arena slot),
//! and **no collection can run while a guard is live** — collections run only
//! at dispatch safepoints between interpreter instructions, and while any
//! `Scope` is live (a `Vm::step`, or a host call's scope) nested execution runs
//! in the non-collecting `Nested` mode; `collectgarbage("collect")` from nested
//! script code only *requests* a collection for the next root safepoint.
//!
//! # Memory accounting
//!
//! `create_userdata::<T>` charges the heap meter for the boxed payload
//! (`size_of::<HostCell<T>>()` — the embedded `T` plus its borrow flag,
//! mirroring how buffers charge their backing bytes); the arena accounts the
//! userdata header itself like any slot. The charge is released when the
//! object drops — on the GC sweep that reclaims an unreachable instance, or
//! with the heap when the `Vm` drops.
//!
//! # GC and `Drop` ordering
//!
//! Instances are heap-owned: when the collector sweeps an unreachable
//! userdata, the boxed `HostCell<T>` drops and `T`'s `Drop` runs at the sweep
//! (between instructions, never during a scope step). Instances still
//! reachable when the `Vm` drops are dropped with the heap's arenas — embedded
//! values must not assume they outlive the VM, and a `Drop` impl must not call
//! back into the VM (it has no access to one). A borrow-while-collect cannot
//! happen: see the borrow-model section.
//!
//! # Declarations and checker integration
//!
//! A host type may carry a `.d.luau` snippet (a `declare class` block) via
//! [`HostTypeBuilder::declaration`]. The checker learns userdata methods
//! through the same builtin-definition-module path native modules use: include
//! the class declaration in (or alongside) a `NativeModule::declaration` on the
//! `SurfaceSpec`, and declare the host functions that hand out instances as
//! returning the class type. `declare class` blocks create type bindings only
//! — no global-binding obligations — so they pass the surface's
//! declaration-vs-binding audit unchanged.
//!
//! # Non-goals
//!
//! - **No script-side construction.** Scripts only ever receive instances from
//!   host functions or host-invoked callbacks; there is no `T.new` surface and
//!   no way to mint userdata from Luau.
//! - **No `newproxy` / tenant-generic userdata.** Untrusted code cannot create
//!   anonymous userdata or attach metatables to one; the only userdata that
//!   exist are embedder-registered types.
//! - **No per-instance metatables or field setters.** One shared, immutable
//!   metatable per type; script-side writes to userdata (`__newindex`) remain
//!   errors. Mutation goes through mut methods.

use std::{
    any::TypeId,
    cell::{Cell, UnsafeCell},
    ops::Deref,
    sync::Arc,
};

use ruau_vm_api::{RawGc, RawValue, RegistryRef, marker};

use crate::{
    heap::Heap,
    host::ScopedHostFunction,
    scope::{
        FromLuaMulti, IntoLua, IntoLuaMulti, MultiValue, RuntimeError, Scope, ScopedValue, Table,
        Userdata,
    },
    table::LuaTable,
};

/// The boxed payload behind a host userdata: the embedded `T` plus its runtime
/// borrow flag. Boxed once at `create_userdata`, so its address is stable for
/// the userdata's lifetime regardless of arena growth.
pub struct HostCell<T> {
    /// Borrow state: `0` free, `n > 0` shared readers, `-1` exclusive.
    borrow: Cell<isize>,
    value: UnsafeCell<T>,
}

impl<T> HostCell<T> {
    pub(crate) fn new(value: T) -> Self {
        Self {
            borrow: Cell::new(0),
            value: UnsafeCell::new(value),
        }
    }

    /// Takes a shared borrow, failing (without panicking) while the exclusive
    /// borrow is live.
    pub(crate) fn try_borrow_shared(&self) -> bool {
        let state = self.borrow.get();
        if state < 0 {
            return false;
        }
        self.borrow.set(state + 1);
        true
    }

    pub(crate) fn release_shared(&self) {
        let state = self.borrow.get();
        debug_assert!(state > 0, "shared release without a live shared borrow");
        self.borrow.set(state - 1);
    }

    /// Takes the exclusive borrow, failing (without panicking) while any
    /// borrow is live.
    pub(crate) fn try_borrow_exclusive(&self) -> bool {
        if self.borrow.get() != 0 {
            return false;
        }
        self.borrow.set(-1);
        true
    }

    pub(crate) fn release_exclusive(&self) {
        debug_assert_eq!(
            self.borrow.get(),
            -1,
            "exclusive release without the exclusive borrow"
        );
        self.borrow.set(0);
    }

    /// The embedded value, as a raw pointer for the borrow guards. The guards
    /// uphold aliasing through the borrow flag.
    pub(crate) fn value_ptr(&self) -> *mut T {
        self.value.get()
    }
}

/// One registered method's type-erased dispatch: receiver extraction, borrow,
/// argument conversion, and the user function, folded into one closure.
type MethodDispatchFn = Box<
    dyn for<'s> Fn(&Scope<'s>, MultiValue<'s>) -> Result<MultiValue<'s>, RuntimeError>
        + Send
        + Sync,
>;

/// One registered field getter's type-erased dispatch, called with the
/// receiver only.
type GetterDispatchFn = Box<
    dyn for<'s> Fn(&Scope<'s>, Userdata<'s>) -> Result<ScopedValue<'s>, RuntimeError> + Send + Sync,
>;

/// Type-erased value-equality dispatch for a registered host userdata type.
type EqualityDispatchFn = Box<
    dyn for<'s> Fn(&Scope<'s>, Userdata<'s>, Userdata<'s>) -> Result<bool, RuntimeError>
        + Send
        + Sync,
>;

type MarshalDispatchFn = Arc<
    dyn Fn(&Heap, RawGc<marker::Userdata>) -> Result<crate::MarshaledValue, String> + Send + Sync,
>;

type TostringDispatchFn =
    Arc<dyn for<'s> Fn(&Scope<'s>, Userdata<'s>) -> Result<String, RuntimeError> + Send + Sync>;

struct HostTypeMethod {
    name: String,
    dispatch: MethodDispatchFn,
}

struct HostTypeGetter {
    name: String,
    dispatch: GetterDispatchFn,
}

/// A registered host userdata type: the type-erased descriptor built by
/// [`HostTypeBuilder`] and registered on [`VmBuilder::host_type`](crate::VmBuilder).
///
/// See this crate's host-userdata documentation for the full design: shared
/// immutable metatable, catchable borrow model, memory accounting, and
/// non-goals.
pub struct HostType {
    name: String,
    type_id: TypeId,
    /// Byte footprint charged per instance: the boxed `HostCell<T>`.
    payload_size: usize,
    methods: Vec<HostTypeMethod>,
    getters: Vec<HostTypeGetter>,
    equals: Option<EqualityDispatchFn>,
    marshal: Option<MarshalDispatchFn>,
    tostring: Option<TostringDispatchFn>,
    declaration: Option<String>,
}

impl HostType {
    /// The script-visible type name (`typeof` result and error rendering).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The `.d.luau` `declare class` snippet supplied at registration, for the
    /// embedder to splice into a `NativeModule` declaration so the checker
    /// admits the type's methods.
    #[must_use]
    pub fn declaration(&self) -> Option<&str> {
        self.declaration.as_deref()
    }

    fn getter(&self, name: &[u8]) -> Option<&HostTypeGetter> {
        self.getters
            .iter()
            .find(|getter| getter.name.as_bytes() == name)
    }

    fn tostring<'s>(
        &self,
        scope: &Scope<'s>,
        userdata: Userdata<'s>,
    ) -> Result<Option<String>, RuntimeError> {
        self.tostring
            .as_ref()
            .map(|tostring| tostring(scope, userdata))
            .transpose()
    }
}

impl std::fmt::Debug for HostType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostType")
            .field("name", &self.name)
            .field("methods", &self.methods.len())
            .field("getters", &self.getters.len())
            .field("marshal", &self.marshal.is_some())
            .field("tostring", &self.tostring.is_some())
            .finish()
    }
}

/// Builder for one embedder-typed userdata type `T`.
///
/// Methods receive the active [`Scope`], the borrowed receiver, and converted
/// arguments; they may call back into Lua through the scope (see this crate's
/// host-userdata borrow model).
pub struct HostTypeBuilder<T: Send + 'static> {
    name: String,
    methods: Vec<HostTypeMethod>,
    getters: Vec<HostTypeGetter>,
    equals: Option<EqualityDispatchFn>,
    marshal: Option<MarshalDispatchFn>,
    tostring: Option<TostringDispatchFn>,
    declaration: Option<String>,
    _marker: std::marker::PhantomData<fn(T)>,
}

impl<T: Send + 'static> HostTypeBuilder<T> {
    /// Starts a descriptor for `T` under the script-visible type `name`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            methods: Vec::new(),
            getters: Vec::new(),
            equals: None,
            marshal: None,
            tostring: None,
            declaration: None,
            _marker: std::marker::PhantomData,
        }
    }

    /// Registers a method over a shared borrow of the receiver
    /// (`ud:name(...)` with `&T`). Shared methods may nest and may re-enter
    /// Lua; only a live exclusive borrow conflicts.
    #[must_use]
    pub fn method<A, R, F>(mut self, name: impl Into<String>, f: F) -> Self
    where
        F: for<'s> Fn(&Scope<'s>, &T, A) -> Result<R, RuntimeError> + Send + Sync + 'static,
        A: for<'s> FromLuaMulti<'s> + 'static,
        R: for<'s> IntoLuaMulti<'s> + 'static,
    {
        let name = name.into();
        let label = name.clone();
        self.methods.push(HostTypeMethod {
            name,
            dispatch: Box::new(move |scope, args| {
                let (receiver, rest) = split_receiver(&label, args)?;
                let guard = receiver.borrow::<T>(scope)?;
                let args = A::from_lua_multi(rest, scope)?;
                let result = f(scope, &guard, args)?;
                drop(guard);
                result.into_lua_multi(scope)
            }),
        });
        self
    }

    /// Registers a method over the exclusive borrow of the receiver
    /// (`ud:name(...)` with `&mut T`). Fails with a catchable error while any
    /// other borrow of the same instance is live — including a re-entrant call
    /// from Lua code this method invoked.
    #[must_use]
    pub fn method_mut<A, R, F>(mut self, name: impl Into<String>, f: F) -> Self
    where
        F: for<'s> Fn(&Scope<'s>, &mut T, A) -> Result<R, RuntimeError> + Send + Sync + 'static,
        A: for<'s> FromLuaMulti<'s> + 'static,
        R: for<'s> IntoLuaMulti<'s> + 'static,
    {
        let name = name.into();
        let label = name.clone();
        self.methods.push(HostTypeMethod {
            name,
            dispatch: Box::new(move |scope, args| {
                let (receiver, rest) = split_receiver(&label, args)?;
                let mut guard = receiver.borrow_mut::<T>(scope)?;
                let args = A::from_lua_multi(rest, scope)?;
                let result = f(scope, &mut guard, args)?;
                drop(guard);
                result.into_lua_multi(scope)
            }),
        });
        self
    }

    /// Registers a method over the raw receiver handle and unconverted
    /// arguments — the escape hatch for methods that traffic in scope-borrowed
    /// values, e.g. a builder method that returns its own receiver
    /// (`mode:bind(...)` chaining). The function borrows the payload itself
    /// (`receiver.borrow::<T>` / `borrow_mut::<T>`) under the same catchable
    /// borrow rules; `args` excludes the receiver.
    #[must_use]
    pub fn method_raw<F>(mut self, name: impl Into<String>, f: F) -> Self
    where
        F: for<'s> Fn(
                &Scope<'s>,
                Userdata<'s>,
                MultiValue<'s>,
            ) -> Result<MultiValue<'s>, RuntimeError>
            + Send
            + Sync
            + 'static,
    {
        let name = name.into();
        let label = name.clone();
        self.methods.push(HostTypeMethod {
            name,
            dispatch: Box::new(move |scope, args| {
                let (receiver, rest) = split_receiver(&label, args)?;
                f(scope, receiver, rest)
            }),
        });
        self
    }

    /// Registers a field getter (`ud.name` with `&T`). Getters take a shared
    /// borrow, like [`method`](Self::method).
    #[must_use]
    pub fn getter<R, F>(mut self, name: impl Into<String>, f: F) -> Self
    where
        F: for<'s> Fn(&Scope<'s>, &T) -> Result<R, RuntimeError> + Send + Sync + 'static,
        R: for<'s> IntoLua<'s> + 'static,
    {
        self.getters.push(HostTypeGetter {
            name: name.into(),
            dispatch: Box::new(move |scope, receiver| {
                let guard = receiver.borrow::<T>(scope)?;
                let result = f(scope, &guard)?;
                drop(guard);
                result.into_lua(scope)
            }),
        });
        self
    }

    /// Registers value equality for this userdata type (`left == right`).
    ///
    /// Different host userdata types compare `false`; two values of this type
    /// are borrowed shared and passed to `f`.
    #[must_use]
    pub fn eq_by<F>(mut self, f: F) -> Self
    where
        F: Fn(&T, &T) -> bool + Send + Sync + 'static,
    {
        self.equals = Some(Box::new(move |scope, left, right| {
            if !left.is::<T>(scope) || !right.is::<T>(scope) {
                return Ok(false);
            }
            let left = left.borrow::<T>(scope)?;
            let right = right.borrow::<T>(scope)?;
            Ok(f(&left, &right))
        }));
        self
    }

    /// Registers how values of this host type are copied through owned
    /// marshaling boundaries (`Vm::exec`, `Vm::exec_async`, and `Scope::marshal`).
    ///
    /// Types without a marshal hook remain opaque userdata.
    #[must_use]
    pub fn marshal<F>(mut self, f: F) -> Self
    where
        F: Fn(&T) -> crate::MarshaledValue + Send + Sync + 'static,
    {
        let type_name = self.name.clone();
        self.marshal = Some(Arc::new(move |heap, handle| {
            let userdata = heap
                .userdata(handle)
                .ok_or_else(|| "userdata handle no longer resolves".to_owned())?;
            let cell = userdata
                .cell_any()
                .downcast_ref::<HostCell<T>>()
                .ok_or_else(|| format!("userdata is not a '{type_name}'"))?;
            let guard = HostCellShared::try_new(cell, &type_name)?;
            Ok(f(&guard))
        }));
        self
    }

    /// Registers how `tostring(value)` and `print(value)` render values of
    /// this host type.
    ///
    /// Types without a tostring hook keep the default `userdata: 0x...` form.
    #[must_use]
    pub fn tostring<F>(mut self, f: F) -> Self
    where
        F: Fn(&T) -> String + Send + Sync + 'static,
    {
        self.tostring = Some(Arc::new(move |scope, userdata| {
            let guard = userdata.borrow::<T>(scope)?;
            Ok(f(&guard))
        }));
        self
    }

    /// Attaches the `.d.luau` `declare class` snippet for this type (see
    /// [`HostType::declaration`]).
    #[must_use]
    pub fn declaration(mut self, declaration: impl Into<String>) -> Self {
        self.declaration = Some(declaration.into());
        self
    }

    /// Attaches a typed `declare class` model for this host type.
    #[must_use]
    pub fn class(self, class: &ruau_decl::Class) -> Self {
        self.declaration(class.render())
    }

    /// Finalizes the type-erased descriptor.
    #[must_use]
    pub fn build(self) -> HostType {
        HostType {
            name: self.name,
            type_id: TypeId::of::<T>(),
            payload_size: std::mem::size_of::<HostCell<T>>(),
            methods: self.methods,
            getters: self.getters,
            equals: self.equals,
            marshal: self.marshal,
            tostring: self.tostring,
            declaration: self.declaration,
        }
    }
}

struct HostCellShared<'a, T> {
    cell: &'a HostCell<T>,
}

impl<'a, T> HostCellShared<'a, T> {
    fn try_new(cell: &'a HostCell<T>, type_name: &str) -> Result<Self, String> {
        if !cell.try_borrow_shared() {
            return Err(format!(
                "userdata of host type '{type_name}' is mutably borrowed"
            ));
        }
        Ok(Self { cell })
    }
}

impl<T> Deref for HostCellShared<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: the shared borrow flag is held until this guard drops.
        unsafe { &*self.cell.value_ptr() }
    }
}

impl<T> Drop for HostCellShared<'_, T> {
    fn drop(&mut self) {
        self.cell.release_shared();
    }
}

/// Splits a method call's receiver from its remaining arguments.
fn split_receiver<'s>(
    method: &str,
    args: MultiValue<'s>,
) -> Result<(Userdata<'s>, MultiValue<'s>), RuntimeError> {
    let mut values = args.into_vec().into_iter();
    match values.next() {
        Some(ScopedValue::Userdata(receiver)) => {
            Ok((receiver, MultiValue::from_values(values.collect())))
        }
        other => Err(RuntimeError::runtime(format!(
            "method '{method}' must be called on a host userdata (got {}); use ':' call syntax",
            other.map_or("no value", ScopedValue::type_name),
        ))),
    }
}

/// The per-VM runtime entry behind one registered host type: the shared
/// metatable plus the registry pins that root it (and the method table) for
/// the VM's lifetime. Userdata objects reference this entry by index and stay
/// GC leaves.
pub struct HostTypeRuntime {
    pub(crate) type_id: TypeId,
    pub(crate) name: String,
    pub(crate) payload_size: usize,
    pub(crate) marshal: Option<MarshalDispatchFn>,
    pub(crate) metatable: RawGc<marker::Table>,
    /// Lifetime roots for the metatable and method table. Held, never
    /// released: the dispatch surface lives exactly as long as the VM.
    _pins: Vec<RegistryRef>,
}

/// A registered method behind one heap host-function slot.
struct MethodSlot {
    ty: Arc<HostType>,
    index: usize,
}

impl ScopedHostFunction for MethodSlot {
    fn call<'s>(
        &self,
        scope: &Scope<'s>,
        args: MultiValue<'s>,
    ) -> Result<MultiValue<'s>, RuntimeError> {
        (self.ty.methods[self.index].dispatch)(scope, args)
    }
}

/// The `__index` dispatch function installed when a type registers getters:
/// method-table hit first, then getters, then `nil` (table-like absence).
struct IndexSlot {
    ty: Arc<HostType>,
    methods: RawGc<marker::Table>,
}

impl ScopedHostFunction for IndexSlot {
    fn call<'s>(
        &self,
        scope: &Scope<'s>,
        args: MultiValue<'s>,
    ) -> Result<MultiValue<'s>, RuntimeError> {
        let mut values = args.into_vec().into_iter();
        let receiver = values.next();
        let key = values.next().unwrap_or(ScopedValue::Nil);
        let Some(ScopedValue::Userdata(receiver)) = receiver else {
            return Err(RuntimeError::runtime(format!(
                "'{}' __index dispatch called without a userdata receiver",
                self.ty.name
            )));
        };
        // Methods win over getters, from the rooted method table.
        let method: ScopedValue<'_> = Table::from_raw(self.methods).get(scope, key)?;
        if !matches!(method, ScopedValue::Nil) {
            return Ok(MultiValue::from_values(vec![method]));
        }
        if let ScopedValue::String(name) = key {
            let name = scope.string_bytes(name)?;
            if let Some(getter) = self.ty.getter(&name) {
                let value = (getter.dispatch)(scope, receiver)?;
                return Ok(MultiValue::from_values(vec![value]));
            }
        }
        Ok(MultiValue::from_values(vec![ScopedValue::Nil]))
    }
}

/// The `__eq` dispatch function installed when a type registers value equality.
struct EqualitySlot {
    /// Registered host type descriptor.
    ty: Arc<HostType>,
}

impl ScopedHostFunction for EqualitySlot {
    fn call<'s>(
        &self,
        scope: &Scope<'s>,
        args: MultiValue<'s>,
    ) -> Result<MultiValue<'s>, RuntimeError> {
        let mut values = args.into_vec().into_iter();
        let left = values.next();
        let right = values.next();
        let equal = match (left, right, self.ty.equals.as_ref()) {
            (
                Some(ScopedValue::Userdata(left)),
                Some(ScopedValue::Userdata(right)),
                Some(equals),
            ) => equals(scope, left, right)?,
            _ => false,
        };
        equal.into_lua_multi(scope)
    }
}

struct TostringSlot {
    ty: Arc<HostType>,
}

impl ScopedHostFunction for TostringSlot {
    fn call<'s>(
        &self,
        scope: &Scope<'s>,
        args: MultiValue<'s>,
    ) -> Result<MultiValue<'s>, RuntimeError> {
        let mut values = args.into_vec().into_iter();
        let receiver = values.next();
        let Some(ScopedValue::Userdata(receiver)) = receiver else {
            return Err(RuntimeError::runtime(format!(
                "'{}' __tostring dispatch called without a userdata receiver",
                self.ty.name
            )));
        };
        self.ty
            .tostring(scope, receiver)?
            .unwrap_or_else(|| self.ty.name.clone())
            .into_lua_multi(scope)
    }
}

/// Builds each registered type's dispatch surface into `heap` at VM build:
/// method table, shared readonly metatable, lifetime pins, and the registry
/// entry `create_userdata`/`tm::metatable` resolve against.
///
/// # Errors
/// Returns a message for duplicate registrations or a failed allocation; the
/// caller poisons the VM (the module-install contract).
pub fn install_host_types(heap: &mut Heap, types: &[Arc<HostType>]) -> Result<(), String> {
    for ty in types {
        install_host_type(heap, ty)
            .map_err(|reason| format!("host type `{}` failed to install: {reason}", ty.name))?;
    }
    Ok(())
}

fn install_host_type(heap: &mut Heap, ty: &Arc<HostType>) -> Result<(), String> {
    if heap.host_type_for(ty.type_id).is_some() {
        return Err("its Rust type is already registered".to_owned());
    }
    if heap
        .host_types()
        .iter()
        .any(|existing| existing.name == ty.name)
    {
        return Err("its name is already registered".to_owned());
    }
    let oom = |what: &str| format!("allocation failed building its {what}");

    // The method table: one closure slot per registered method.
    let methods = heap
        .alloc_table(LuaTable::new())
        .ok_or_else(|| oom("method table"))?;
    for (index, method) in ty.methods.iter().enumerate() {
        let closure = heap
            .alloc_scoped_host(Box::new(MethodSlot {
                ty: ty.clone(),
                index,
            }))
            .ok_or_else(|| oom("method closure"))?;
        set_table_member(heap, methods, &method.name, RawValue::Function(closure))
            .ok_or_else(|| oom("method table entry"))?;
    }

    // The shared metatable: `__index` (table fast path without getters,
    // dispatch function with), `__type` for `typeof`, `__metatable` protection.
    let metatable = heap
        .alloc_table(LuaTable::new())
        .ok_or_else(|| oom("metatable"))?;
    let index_value = if ty.getters.is_empty() {
        RawValue::Table(methods)
    } else {
        let closure = heap
            .alloc_scoped_host(Box::new(IndexSlot {
                ty: ty.clone(),
                methods,
            }))
            .ok_or_else(|| oom("__index dispatch"))?;
        RawValue::Function(closure)
    };
    set_table_member(heap, metatable, "__index", index_value).ok_or_else(|| oom("__index"))?;
    if ty.equals.is_some() {
        let closure = heap
            .alloc_scoped_host(Box::new(EqualitySlot { ty: ty.clone() }))
            .ok_or_else(|| oom("__eq dispatch"))?;
        set_table_member(heap, metatable, "__eq", RawValue::Function(closure))
            .ok_or_else(|| oom("__eq"))?;
    }
    if ty.tostring.is_some() {
        let closure = heap
            .alloc_scoped_host(Box::new(TostringSlot { ty: ty.clone() }))
            .ok_or_else(|| oom("__tostring dispatch"))?;
        set_table_member(heap, metatable, "__tostring", RawValue::Function(closure))
            .ok_or_else(|| oom("__tostring"))?;
    }
    let name = heap
        .intern_str(ty.name.as_bytes())
        .ok_or_else(|| oom("type name"))?;
    set_table_member(heap, metatable, "__type", RawValue::String(name))
        .ok_or_else(|| oom("__type"))?;
    set_table_member(heap, metatable, "__metatable", RawValue::String(name))
        .ok_or_else(|| oom("__metatable"))?;

    // Both tables are immutable from here on, under every profile.
    for table in [methods, metatable] {
        if let Some(table) = heap.table_mut(table) {
            table.readonly = true;
        }
    }

    // Lifetime roots: the metatable reaches the method table when `__index` is
    // the table itself, but not through the function dispatcher — pin both.
    let pins = vec![
        heap.pin(RawValue::Table(metatable))
            .ok_or_else(|| oom("metatable pin"))?,
        heap.pin(RawValue::Table(methods))
            .ok_or_else(|| oom("method table pin"))?,
    ];
    heap.register_host_type(HostTypeRuntime {
        type_id: ty.type_id,
        name: ty.name.clone(),
        payload_size: ty.payload_size,
        marshal: ty.marshal.clone(),
        metatable,
        _pins: pins,
    });
    Ok(())
}

fn set_table_member(
    heap: &mut Heap,
    table: RawGc<marker::Table>,
    name: &str,
    value: RawValue,
) -> Option<()> {
    let key = RawValue::String(heap.intern_str(name.as_bytes())?);
    heap.table_mut(table)?.set(key, value);
    Some(())
}

#[cfg(any())]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use ruau_bytecode::{CompileOptions, compile_source};

    use super::*;
    use crate::{Ambient, Limits, MarshaledValue, Profile, RuntimeErrorKind, Vm};

    /// The embedded test type: a counter that can report its drops.
    struct Counter {
        count: i64,
        dropped: Option<Arc<AtomicUsize>>,
    }

    impl Drop for Counter {
        fn drop(&mut self) {
            if let Some(dropped) = &self.dropped {
                dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// A second registered type, for foreign-type rejection.
    struct Gauge {
        level: i64,
    }

    fn counter(count: i64) -> Counter {
        Counter {
            count,
            dropped: None,
        }
    }

    fn counter_get(_: &Scope<'_>, counter: &Counter, (): ()) -> Result<i64, RuntimeError> {
        Ok(counter.count)
    }

    fn counter_add(_: &Scope<'_>, counter: &mut Counter, by: i64) -> Result<i64, RuntimeError> {
        counter.count += by;
        Ok(counter.count)
    }

    /// A mut method that re-enters Lua while its exclusive borrow is live: the
    /// global `probe` closure captures the same userdata and calls `add` on it,
    /// which must fail with the catchable borrow-conflict error. The conflict
    /// message is captured and returned so the test can assert it verbatim.
    fn counter_reenter(
        scope: &Scope<'_>,
        _counter: &mut Counter,
        (): (),
    ) -> Result<String, RuntimeError> {
        let probe = scope
            .global_function(b"probe")
            .ok_or_else(|| RuntimeError::runtime("global probe is missing"))?;
        match scope.call_protected::<_, i64>(probe, ())? {
            Ok(_) => Err(RuntimeError::runtime(
                "re-entrant mut borrow unexpectedly succeeded",
            )),
            Err(error) => match error.value() {
                ScopedValue::String(text) => {
                    Ok(String::from_utf8_lossy(&scope.string_bytes(text)?).into_owned())
                }
                other => Err(RuntimeError::runtime(format!(
                    "expected a string error, got {other:?}"
                ))),
            },
        }
    }

    fn counter_type() -> crate::HostType {
        HostTypeBuilder::<Counter>::new("Counter")
            .method("get", counter_get)
            .method_mut("add", counter_add)
            .method_mut("reenter", counter_reenter)
            .method_raw("chain", |scope, receiver, args| {
                // Script number literals arrive as `Number`; accept either.
                let by = f64::from_lua_multi(args, scope)? as i64;
                receiver.borrow_mut::<Counter>(scope)?.count += by;
                Ok(MultiValue::from_values(vec![ScopedValue::Userdata(
                    receiver,
                )]))
            })
            .getter("count", |_, counter: &Counter| Ok(counter.count))
            .eq_by(|left, right| left.count == right.count)
            .marshal(|counter| MarshaledValue::Integer(counter.count))
            .tostring(|counter| format!("Counter({})", counter.count))
            .build()
    }

    fn gauge_type() -> crate::HostType {
        HostTypeBuilder::<Gauge>::new("Gauge")
            .method("read", |_, gauge: &Gauge, (): ()| Ok(gauge.level))
            .build()
    }

    fn host_type_vm() -> Vm {
        Vm::builder()
            .ambient(Ambient::deterministic(0))
            .limits(Limits::unlimited())
            .profile(Profile::full())
            .host_type(counter_type())
            .host_type(gauge_type())
            .build()
            .expect("vm builds")
    }

    /// Compiles and runs `source` so its global functions are defined.
    fn install(vm: &mut Vm, source: &str) {
        let chunk = compile_source(source, &CompileOptions::for_vm_execution()).expect("compile");
        let module = vm.load(&chunk).expect("load");
        vm.call(&module, Default::default())
            .expect("setup chunk runs");
    }

    #[test]
    fn script_method_mut_method_and_getter_dispatch() {
        let mut vm = host_type_vm();
        install(
            &mut vm,
            "function probe(ud) return ud:get() end\n\
             function bump(ud, by) return ud:add(by) end\n\
             function read(ud) return ud.count end\n\
             function chained(ud) return ud:chain(1):chain(2):get() end\n\
             function missing(ud) return ud.absent end",
        );
        vm.step(|s| {
            let ud = s.create_userdata(counter(7))?;
            let probe = s.global_function(b"probe").expect("probe");
            assert_eq!(s.call::<_, i64>(probe, (ud,))?, 7);

            // A mut method mutates the embedded value across calls.
            let bump = s.global_function(b"bump").expect("bump");
            assert_eq!(s.call::<_, i64>(bump, (ud, 5_i64))?, 12);
            assert_eq!(s.call::<_, i64>(probe, (ud,))?, 12);

            // Field getter via the `__index` dispatch function.
            let read = s.global_function(b"read").expect("read");
            assert_eq!(s.call::<_, i64>(read, (ud,))?, 12);

            // A raw method returns its receiver, so scripts can chain.
            let chained = s.global_function(b"chained").expect("chained");
            assert_eq!(s.call::<_, i64>(chained, (ud,))?, 15);

            // Unknown members read as nil, like a table.
            let missing = s.global_function(b"missing").expect("missing");
            assert!(matches!(
                s.call::<_, ScopedValue<'_>>(missing, (ud,))?,
                ScopedValue::Nil
            ));
            Ok(())
        })
        .expect("step");
    }

    #[test]
    fn host_userdata_can_register_value_equality() {
        let mut vm = host_type_vm();
        install(
            &mut vm,
            "function same(left, right) return left == right end",
        );
        vm.step(|s| {
            let same = s.global_function(b"same").expect("same");
            let first = s.create_userdata(counter(7))?;
            let also_first = s.create_userdata(counter(7))?;
            let second = s.create_userdata(counter(8))?;
            let gauge = s.create_userdata(Gauge { level: 7 })?;

            assert!(s.call::<_, bool>(same, (first, also_first))?);
            assert!(!s.call::<_, bool>(same, (first, second))?);
            assert!(!s.call::<_, bool>(same, (first, gauge))?);
            Ok(())
        })
        .expect("step");
    }

    #[test]
    fn typeof_names_the_type_and_the_metatable_is_protected() {
        let mut vm = host_type_vm();
        install(
            &mut vm,
            "function kind(ud) return typeof(ud), type(ud), getmetatable(ud) end\n\
             function write(ud) ud.x = 1 end",
        );
        vm.step(|s| {
            let ud = s.create_userdata(counter(0))?;
            let kind = s.global_function(b"kind").expect("kind");
            let (type_of, plain, meta): (String, String, String) = s.call(kind, (ud,))?;
            assert_eq!(type_of, "Counter");
            assert_eq!(plain, "userdata");
            // `__metatable` protection: scripts see the name, never the table.
            assert_eq!(meta, "Counter");

            // Writing to a userdata is a catchable error, not a mutation path.
            let write = s.global_function(b"write").expect("write");
            let error = s
                .call::<_, ()>(write, (ud,))
                .expect_err("userdata writes are rejected");
            assert_eq!(error.kind(), RuntimeErrorKind::Runtime);
            Ok(())
        })
        .expect("step");
    }

    #[test]
    fn reentrant_mut_borrow_is_a_catchable_error_and_the_vm_stays_usable() {
        let mut vm = host_type_vm();
        install(
            &mut vm,
            "function setup(ud) probe = function() return ud:add(1) end end\n\
             function trigger(ud) return ud:reenter() end\n\
             function bump(ud, by) return ud:add(by) end",
        );
        vm.step(|s| {
            let ud = s.create_userdata(counter(0))?;
            let setup = s.global_function(b"setup").expect("setup");
            s.call::<_, ()>(setup, (ud,))?;

            // `reenter` holds the exclusive borrow and re-enters Lua; the inner
            // `add` sees the live borrow and fails with the catchable error.
            let trigger = s.global_function(b"trigger").expect("trigger");
            let message: String = s.call(trigger, (ud,))?;
            assert!(
                message.contains("userdata of host type 'Counter' is already borrowed"),
                "unexpected conflict message: {message}"
            );

            // The borrow flags were released on unwind: the same instance still
            // takes a mut method, and the failed inner add never applied.
            let bump = s.global_function(b"bump").expect("bump");
            assert_eq!(s.call::<_, i64>(bump, (ud, 3_i64))?, 3);
            Ok(())
        })
        .expect("the VM is not poisoned by a borrow conflict");
    }

    #[test]
    fn foreign_type_borrow_fails_cleanly_from_scripts_and_the_host() {
        let mut vm = host_type_vm();
        install(
            &mut vm,
            "function cross(c, g)\n\
                 local stolen = g.read\n\
                 local ok, err = pcall(stolen, c)\n\
                 return ok, tostring(err)\n\
             end",
        );
        vm.step(|s| {
            let c = s.create_userdata(counter(1))?;
            let g = s.create_userdata(Gauge { level: 9 })?;

            // A stolen method called with the wrong receiver fails the typed
            // downcast as a catchable script error.
            let cross = s.global_function(b"cross").expect("cross");
            let (ok, message): (bool, String) = s.call(cross, (c, g))?;
            assert!(!ok, "cross-type method call must fail");
            assert!(
                message.contains("userdata is not a") && message.contains("'Counter'"),
                "unexpected cross-type message: {message}"
            );

            // Host-side typed downcast failure is the same clean error.
            let error = c
                .borrow::<Gauge>(s)
                .err()
                .expect("borrowing a Counter as a Gauge fails");
            assert!(error.message().contains("userdata is not a"));
            Ok(())
        })
        .expect("step");
    }

    #[test]
    fn host_side_borrow_rules_nest_shared_and_reject_conflicts() {
        let mut vm = host_type_vm();
        vm.step(|s| {
            let ud = s.create_userdata(counter(4))?;

            // Shared borrows nest; an exclusive borrow is rejected while they live.
            let first = ud.borrow::<Counter>(s)?;
            let second = ud.borrow::<Counter>(s)?;
            assert_eq!(first.count, 4);
            assert_eq!(second.count, 4);
            let conflict = ud.borrow_mut::<Counter>(s).err().expect("mut conflicts");
            assert!(conflict.message().contains("is already borrowed"));
            drop(first);
            let still = ud.borrow_mut::<Counter>(s).err().expect("still conflicts");
            assert!(still.message().contains("is already borrowed"));
            drop(second);

            // With all shared borrows gone the exclusive borrow proceeds, and
            // blocks shared borrows in turn.
            let mut exclusive = ud.borrow_mut::<Counter>(s)?;
            exclusive.count += 1;
            let conflict = ud.borrow::<Counter>(s).err().expect("shared conflicts");
            assert!(conflict.message().contains("is already mutably borrowed"));
            drop(exclusive);
            assert_eq!(ud.borrow::<Counter>(s)?.count, 5);
            Ok(())
        })
        .expect("step");
    }

    #[test]
    fn host_userdata_marshal_hook_applies_inside_a_scope_step() {
        let mut vm = host_type_vm();
        vm.step(|s| {
            let ud = s.create_userdata(counter(42))?;
            let marshaled = s.marshal(ScopedValue::Userdata(ud))?;
            assert_eq!(marshaled, MarshaledValue::Integer(42));
            Ok(())
        })
        .expect("step");
    }

    #[test]
    fn host_userdata_tostring_hook_formats_script_strings() {
        let mut vm = host_type_vm();
        install(&mut vm, "function render(ud) return tostring(ud) end");
        vm.step(|s| {
            let ud = s.create_userdata(counter(9))?;
            let render = s.global_function(b"render").expect("render");
            assert_eq!(s.call::<_, String>(render, (ud,))?, "Counter(9)");
            Ok(())
        })
        .expect("step");
    }

    #[test]
    fn creating_an_unregistered_type_fails_cleanly() {
        struct Unregistered;
        let mut vm = host_type_vm();
        vm.step(|s| {
            let error = s
                .create_userdata(Unregistered)
                .expect_err("unregistered types are rejected");
            assert!(error.message().contains("is not registered"));
            Ok(())
        })
        .expect("step");
    }

    #[test]
    fn duplicate_registration_poisons_the_build() {
        let mut vm = Vm::builder()
            .ambient(Ambient::deterministic(0))
            .limits(Limits::unlimited())
            .profile(Profile::full())
            .host_type(counter_type())
            .host_type(counter_type())
            .build()
            .expect("the build returns a (poisoned) VM");
        let error = vm
            .step(|_s| Ok(()))
            .expect_err("a duplicate host type poisons the VM");
        assert_eq!(error.kind(), RuntimeErrorKind::PanicPoison);
    }

    #[test]
    fn gc_drops_the_embedded_value_and_releases_its_memory_charge() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut vm = host_type_vm();
        let before = vm.heap().total_bytes();
        vm.step(|s| {
            s.create_userdata(Counter {
                count: 1,
                dropped: Some(dropped.clone()),
            })?;
            Ok(())
        })
        .expect("step");
        let charged = vm.heap().total_bytes();
        assert!(
            charged >= before + std::mem::size_of::<HostCell<Counter>>(),
            "creating a userdata charges at least its payload: {before} -> {charged}"
        );
        assert_eq!(dropped.load(Ordering::Relaxed), 0);

        // The unreferenced instance is swept: the embedded value drops and the
        // payload charge is released.
        assert!(vm.collect().completed());
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        assert!(
            vm.heap().total_bytes() < charged,
            "the sweep releases the payload charge"
        );
    }

    #[test]
    fn embedded_values_drop_with_the_vm() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut vm = host_type_vm();
        let stash = vm
            .step(|s| {
                let ud = s.create_userdata(Counter {
                    count: 1,
                    dropped: Some(dropped.clone()),
                })?;
                s.stash_userdata(ud)
            })
            .expect("step");
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
        drop(vm);
        drop(stash);
        assert_eq!(
            dropped.load(Ordering::Relaxed),
            1,
            "a still-rooted instance drops with the VM's heap"
        );
    }

    #[test]
    fn stashed_userdata_survives_collection_and_fetches_in_a_later_step() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut vm = host_type_vm();
        install(&mut vm, "function bump(ud, by) return ud:add(by) end");
        let stashed = vm
            .step(|s| {
                let ud = s.create_userdata(Counter {
                    count: 30,
                    dropped: Some(dropped.clone()),
                })?;
                s.stash_userdata(ud)
            })
            .expect("stash step");

        // The stash roots the instance across an explicit full collection.
        assert!(vm.collect().completed());
        assert_eq!(dropped.load(Ordering::Relaxed), 0);

        vm.step(|s| {
            let ud = s.fetch_userdata(&stashed)?;
            let bump = s.global_function(b"bump").expect("bump");
            assert_eq!(s.call::<_, i64>(bump, (ud, 12_i64))?, 42);
            assert_eq!(ud.borrow::<Counter>(s)?.count, 42);
            Ok(())
        })
        .expect("fetch step");

        // Releasing the stash makes the instance collectable again.
        drop(stashed);
        vm.step(|_s| Ok(())).expect("drain step");
        assert!(vm.collect().completed());
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn the_generic_value_stash_round_trips_userdata() {
        let mut vm = host_type_vm();
        let stashed = vm
            .step(|s| {
                let ud = s.create_userdata(counter(11))?;
                s.stash_value(ScopedValue::Userdata(ud))
            })
            .expect("stash step");
        vm.collect();
        vm.step(|s| {
            let ScopedValue::Userdata(ud) = s.fetch_value(&stashed)? else {
                panic!("stashed userdata came back as the wrong kind");
            };
            assert_eq!(ud.borrow::<Counter>(s)?.count, 11);
            Ok(())
        })
        .expect("fetch step");
    }
}
