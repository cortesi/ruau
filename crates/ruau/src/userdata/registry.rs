#![allow(clippy::await_holding_refcell_ref, clippy::await_holding_lock)]

use std::{any::TypeId, cell::RefCell, future, marker::PhantomData, os::raw::c_void};

use crate::{
    error::{Error, Result},
    state::{Luau, LuauLiveGuard},
    traits::{FromLuau, FromLuauMulti, IntoLuau, IntoLuauMulti},
    types::{AsyncCallback, Callback, XRc},
    userdata::{
        AnyUserData, MetaMethod, TypeIdHints, UserData, UserDataFields, UserDataMethods,
        UserDataRef, UserDataRefMut, UserDataStorage, borrow_userdata_scoped,
        borrow_userdata_scoped_mut, collect_userdata,
    },
    util::short_type_name,
    value::Value,
};

#[derive(Clone, Copy)]
enum UserDataType {
    Shared(TypeIdHints),
    Unique(*mut c_void),
}

/// Handle to registry for userdata methods and metamethods.
pub struct UserDataRegistry<T> {
    lua: LuauLiveGuard,
    raw: RawUserDataRegistry,
    userdata_type: UserDataType,
    _phantom: PhantomData<T>,
}

pub struct RawUserDataRegistry {
    // Fields
    pub(crate) fields: Vec<(String, Result<Value>)>,
    pub(crate) field_getters: Vec<(String, Callback)>,
    pub(crate) field_setters: Vec<(String, Callback)>,
    pub(crate) meta_fields: Vec<(String, Result<Value>)>,

    // Methods
    pub(crate) methods: Vec<(String, Callback)>,
    pub(crate) async_methods: Vec<(String, AsyncCallback)>,
    pub(crate) meta_methods: Vec<(String, Callback)>,
    pub(crate) async_meta_methods: Vec<(String, AsyncCallback)>,

    pub(crate) collector: ffi::lua_Destructor,
    pub(crate) destructor: ffi::lua_CFunction,
    pub(crate) type_id: Option<TypeId>,
    pub(crate) type_name: String,
    pub(crate) enable_namecall: bool,
}

impl UserDataType {
    #[inline]
    pub(crate) fn type_id(&self) -> Option<TypeId> {
        match self {
            Self::Shared(hints) => Some(hints.type_id()),
            Self::Unique(_) => None,
        }
    }
}

unsafe impl Send for UserDataType {}

impl<T: 'static> UserDataRegistry<T> {
    #[inline(always)]
    pub(crate) fn new(lua: &Luau) -> Self {
        Self::with_type(lua, UserDataType::Shared(TypeIdHints::new::<T>()))
    }
}

impl<T> UserDataRegistry<T> {
    #[inline(always)]
    pub(crate) fn new_unique(lua: &Luau, ud_ptr: *mut c_void) -> Self {
        Self::with_type(lua, UserDataType::Unique(ud_ptr))
    }

    #[inline(always)]
    fn with_type(lua: &Luau, userdata_type: UserDataType) -> Self {
        let raw = RawUserDataRegistry {
            fields: Vec::new(),
            field_getters: Vec::new(),
            field_setters: Vec::new(),
            meta_fields: Vec::new(),
            methods: Vec::new(),
            async_methods: Vec::new(),
            meta_methods: Vec::new(),
            async_meta_methods: Vec::new(),
            collector: collect_userdata::<UserDataStorage<T>>,
            destructor: super::util::destroy_userdata_storage::<T>,
            type_id: userdata_type.type_id(),
            type_name: short_type_name::<T>(),
            enable_namecall: false,
        };

        Self {
            lua: lua.guard(),
            raw,
            userdata_type,
            _phantom: PhantomData,
        }
    }

    /// Enables support for the namecall optimization in Luau.
    ///
    /// This enables methods resolution optimization in Luau for complex userdata types with methods
    /// and field getters. When enabled, Luau will use a faster lookup path for method calls when a
    /// specific syntax is used (e.g. `obj:method()`.
    ///
    /// This optimization does not play well with async methods, custom `__index` metamethod and
    /// field getters as functions. So, it is disabled by default.
    ///
    /// Use with caution.
    #[doc(hidden)]
    pub fn enable_namecall(&mut self) {
        self.raw.enable_namecall = true;
    }

    fn box_method<M, A, R>(&self, name: &str, method: M) -> Callback
    where
        M: Fn(&Luau, &T, A) -> Result<R> + 'static,
        A: FromLuauMulti,
        R: IntoLuauMulti,
    {
        let name = get_function_name::<T>(name);
        macro_rules! try_self_arg {
            ($res:expr) => {
                $res.map_err(|err| Error::bad_self_argument(&name, err))?
            };
        }

        let target_type = self.userdata_type;
        Box::new(move |rawlua, nargs| unsafe {
            if nargs == 0 {
                let err = Error::from_luau_conversion("missing argument", "userdata", None);
                try_self_arg!(Err(err));
            }
            let state = rawlua.state();
            // Find absolute "self" index before processing args
            let self_index = ffi::lua_absindex(state, -nargs);
            // Self was at position 1, so we pass 2 here
            let args = A::from_stack_args(nargs - 1, 2, Some(&name), &rawlua.ctx());

            match target_type {
                #[rustfmt::skip]
                UserDataType::Shared(type_hints) => {
                    let type_id = try_self_arg!(rawlua.get_userdata_type_id::<T>(state, self_index));
                    try_self_arg!(borrow_userdata_scoped(state, self_index, type_id, type_hints, |ud| {
                        method(rawlua.lua(), ud, args?)?.push_into_stack_multi(&rawlua.ctx())
                    }))
                }
                UserDataType::Unique(target_ptr)
                    if ffi::lua_touserdata(state, self_index) == target_ptr =>
                {
                    let ud = target_ptr as *mut UserDataStorage<T>;
                    try_self_arg!((*ud).try_borrow_scoped(|ud| {
                        method(rawlua.lua(), ud, args?)?.push_into_stack_multi(&rawlua.ctx())
                    }))
                }
                UserDataType::Unique(_) => {
                    try_self_arg!(rawlua.get_userdata_type_id::<T>(state, self_index));
                    Err(Error::bad_self_argument(&name, Error::UserDataTypeMismatch))
                }
            }
        })
    }

    fn box_method_mut<M, A, R>(&self, name: &str, method: M) -> Callback
    where
        M: FnMut(&Luau, &mut T, A) -> Result<R> + 'static,
        A: FromLuauMulti,
        R: IntoLuauMulti,
    {
        let name = get_function_name::<T>(name);
        macro_rules! try_self_arg {
            ($res:expr) => {
                $res.map_err(|err| Error::bad_self_argument(&name, err))?
            };
        }

        let method = RefCell::new(method);
        let target_type = self.userdata_type;
        Box::new(move |rawlua, nargs| unsafe {
            let mut method = method
                .try_borrow_mut()
                .map_err(|_| Error::RecursiveMutCallback)?;
            if nargs == 0 {
                let err = Error::from_luau_conversion("missing argument", "userdata", None);
                try_self_arg!(Err(err));
            }
            let state = rawlua.state();
            // Find absolute "self" index before processing args
            let self_index = ffi::lua_absindex(state, -nargs);
            // Self was at position 1, so we pass 2 here
            let args = A::from_stack_args(nargs - 1, 2, Some(&name), &rawlua.ctx());

            match target_type {
                #[rustfmt::skip]
                UserDataType::Shared(type_hints) => {
                    let type_id = try_self_arg!(rawlua.get_userdata_type_id::<T>(state, self_index));
                    try_self_arg!(borrow_userdata_scoped_mut(state, self_index, type_id, type_hints, |ud| {
                        method(rawlua.lua(), ud, args?)?.push_into_stack_multi(&rawlua.ctx())
                    }))
                }
                UserDataType::Unique(target_ptr)
                    if ffi::lua_touserdata(state, self_index) == target_ptr =>
                {
                    let ud = target_ptr as *mut UserDataStorage<T>;
                    try_self_arg!((*ud).try_borrow_scoped_mut(|ud| {
                        method(rawlua.lua(), ud, args?)?.push_into_stack_multi(&rawlua.ctx())
                    }))
                }
                UserDataType::Unique(_) => {
                    try_self_arg!(rawlua.get_userdata_type_id::<T>(state, self_index));
                    Err(Error::bad_self_argument(&name, Error::UserDataTypeMismatch))
                }
            }
        })
    }
    fn box_async_method<M, A, R>(&self, name: &str, method: M) -> AsyncCallback
    where
        T: 'static,
        M: AsyncFn(&Luau, UserDataRef<T>, A) -> Result<R> + 'static,
        A: FromLuauMulti + 'static,
        R: IntoLuauMulti,
    {
        let name = get_function_name::<T>(name);
        let method = XRc::new(method);
        macro_rules! try_self_arg {
            ($res:expr) => {
                match $res {
                    Ok(res) => res,
                    Err(err) => {
                        return Box::pin(future::ready(Err(Error::bad_self_argument(&name, err))))
                    }
                }
            };
        }

        Box::new(move |rawlua, nargs| unsafe {
            if nargs == 0 {
                let err = Error::from_luau_conversion("missing argument", "userdata", None);
                try_self_arg!(Err(err));
            }
            // Stack will be empty when polling the future, keep `self` on the ref thread
            let self_ud = try_self_arg!(AnyUserData::from_stack(-nargs, &rawlua.ctx()));
            let args = A::from_stack_args(nargs - 1, 2, Some(&name), &rawlua.ctx());

            let self_ud = try_self_arg!(self_ud.borrow());
            let args = match args {
                Ok(args) => args,
                Err(e) => return Box::pin(future::ready(Err(e))),
            };
            let lua = rawlua.lua();
            let method = XRc::clone(&method);
            // Luau is locked when the future is polled
            Box::pin(async move {
                method(lua, self_ud, args)
                    .await?
                    .push_into_stack_multi(&lua.raw_luau().ctx())
            })
        })
    }
    fn box_async_method_mut<M, A, R>(&self, name: &str, method: M) -> AsyncCallback
    where
        T: 'static,
        M: AsyncFn(&Luau, UserDataRefMut<T>, A) -> Result<R> + 'static,
        A: FromLuauMulti + 'static,
        R: IntoLuauMulti,
    {
        let name = get_function_name::<T>(name);
        let method = XRc::new(method);
        macro_rules! try_self_arg {
            ($res:expr) => {
                match $res {
                    Ok(res) => res,
                    Err(err) => {
                        return Box::pin(future::ready(Err(Error::bad_self_argument(&name, err))))
                    }
                }
            };
        }

        Box::new(move |rawlua, nargs| unsafe {
            if nargs == 0 {
                let err = Error::from_luau_conversion("missing argument", "userdata", None);
                try_self_arg!(Err(err));
            }
            // Stack will be empty when polling the future, keep `self` on the ref thread
            let self_ud = try_self_arg!(AnyUserData::from_stack(-nargs, &rawlua.ctx()));
            let args = A::from_stack_args(nargs - 1, 2, Some(&name), &rawlua.ctx());

            let self_ud = try_self_arg!(self_ud.borrow_mut());
            let args = match args {
                Ok(args) => args,
                Err(e) => return Box::pin(future::ready(Err(e))),
            };
            let lua = rawlua.lua();
            let method = XRc::clone(&method);
            // Luau is locked when the future is polled
            Box::pin(async move {
                method(lua, self_ud, args)
                    .await?
                    .push_into_stack_multi(&lua.raw_luau().ctx())
            })
        })
    }

    fn box_function<F, A, R>(&self, name: &str, function: F) -> Callback
    where
        F: Fn(&Luau, A) -> Result<R> + 'static,
        A: FromLuauMulti,
        R: IntoLuauMulti,
    {
        let name = get_function_name::<T>(name);
        Box::new(move |lua, nargs| unsafe {
            let args = A::from_stack_args(nargs, 1, Some(&name), &lua.ctx())?;
            function(lua.lua(), args)?.push_into_stack_multi(&lua.ctx())
        })
    }

    fn box_function_mut<F, A, R>(&self, name: &str, function: F) -> Callback
    where
        F: FnMut(&Luau, A) -> Result<R> + 'static,
        A: FromLuauMulti,
        R: IntoLuauMulti,
    {
        let name = get_function_name::<T>(name);
        let function = RefCell::new(function);
        Box::new(move |lua, nargs| unsafe {
            let function = &mut *function
                .try_borrow_mut()
                .map_err(|_| Error::RecursiveMutCallback)?;
            let args = A::from_stack_args(nargs, 1, Some(&name), &lua.ctx())?;
            function(lua.lua(), args)?.push_into_stack_multi(&lua.ctx())
        })
    }
    fn box_async_function<F, A, R>(&self, name: &str, function: F) -> AsyncCallback
    where
        F: AsyncFn(&Luau, A) -> Result<R> + 'static,
        A: FromLuauMulti + 'static,
        R: IntoLuauMulti,
    {
        let name = get_function_name::<T>(name);
        let function = XRc::new(function);
        Box::new(move |rawlua, nargs| unsafe {
            let args = match A::from_stack_args(nargs, 1, Some(&name), &rawlua.ctx()) {
                Ok(args) => args,
                Err(e) => return Box::pin(future::ready(Err(e))),
            };
            let lua = rawlua.lua();
            let function = XRc::clone(&function);
            Box::pin(async move {
                function(lua, args)
                    .await?
                    .push_into_stack_multi(&lua.raw_luau().ctx())
            })
        })
    }

    pub(crate) fn check_meta_field(lua: &Luau, name: &str, value: impl IntoLuau) -> Result<Value> {
        let value = value.into_luau(lua)?;
        if name == MetaMethod::Index || name == MetaMethod::NewIndex {
            match value {
                Value::Nil | Value::Table(_) | Value::Function(_) => {}
                _ => {
                    return Err(Error::MetaMethodTypeError {
                        method: name.to_string(),
                        type_name: value.type_name(),
                        message: Some("expected nil, table or function".to_string()),
                    });
                }
            }
        }
        value.into_luau(lua)
    }

    #[inline(always)]
    pub(crate) fn into_raw(self) -> RawUserDataRegistry {
        self.raw
    }
}

// Returns function name for the type `T`, without the module path
fn get_function_name<T>(name: &str) -> String {
    format!("{}.{name}", short_type_name::<T>())
}

impl<T> UserDataFields<T> for UserDataRegistry<T> {
    fn add_field<V>(&mut self, name: impl Into<String>, value: V)
    where
        V: IntoLuau + 'static,
    {
        let name = name.into();
        self.raw
            .fields
            .push((name, value.into_luau(self.lua.lua())));
    }

    fn add_field_method_get<M, R>(&mut self, name: impl Into<String>, method: M)
    where
        M: Fn(&Luau, &T) -> Result<R> + 'static,
        R: IntoLuau,
    {
        let name = name.into();
        let callback = self.box_method(&name, move |lua, data, ()| method(lua, data));
        self.raw.field_getters.push((name, callback));
    }

    fn add_field_method_set<M, A>(&mut self, name: impl Into<String>, method: M)
    where
        M: FnMut(&Luau, &mut T, A) -> Result<()> + 'static,
        A: FromLuau,
    {
        let name = name.into();
        let callback = self.box_method_mut(&name, method);
        self.raw.field_setters.push((name, callback));
    }

    fn add_field_function_get<F, R>(&mut self, name: impl Into<String>, function: F)
    where
        F: Fn(&Luau, AnyUserData) -> Result<R> + 'static,
        R: IntoLuau,
    {
        let name = name.into();
        let callback = self.box_function(&name, function);
        self.raw.field_getters.push((name, callback));
    }

    fn add_field_function_set<F, A>(&mut self, name: impl Into<String>, mut function: F)
    where
        F: FnMut(&Luau, AnyUserData, A) -> Result<()> + 'static,
        A: FromLuau,
    {
        let name = name.into();
        let callback =
            self.box_function_mut(&name, move |lua, (data, val)| function(lua, data, val));
        self.raw.field_setters.push((name, callback));
    }

    fn add_meta_field<V>(&mut self, name: impl Into<String>, value: V)
    where
        V: IntoLuau + 'static,
    {
        let lua = self.lua.lua();
        let name = name.into();
        let field = Self::check_meta_field(lua, &name, value).and_then(|v| v.into_luau(lua));
        self.raw.meta_fields.push((name, field));
    }

    fn add_meta_field_with<F, R>(&mut self, name: impl Into<String>, f: F)
    where
        F: FnOnce(&Luau) -> Result<R> + 'static,
        R: IntoLuau,
    {
        let lua = self.lua.lua();
        let name = name.into();
        let field = f(lua)
            .and_then(|v| Self::check_meta_field(lua, &name, v).and_then(|v| v.into_luau(lua)));
        self.raw.meta_fields.push((name, field));
    }
}

impl<T> UserDataMethods<T> for UserDataRegistry<T> {
    fn add_method<M, A, R>(&mut self, name: impl Into<String>, method: M)
    where
        M: Fn(&Luau, &T, A) -> Result<R> + 'static,
        A: FromLuauMulti + 'static,
        R: IntoLuauMulti,
    {
        let name = name.into();
        let callback = self.box_method(&name, method);
        self.raw.methods.push((name, callback));
    }

    fn add_method_mut<M, A, R>(&mut self, name: impl Into<String>, method: M)
    where
        M: FnMut(&Luau, &mut T, A) -> Result<R> + 'static,
        A: FromLuauMulti + 'static,
        R: IntoLuauMulti,
    {
        let name = name.into();
        let callback = self.box_method_mut(&name, method);
        self.raw.methods.push((name, callback));
    }
    fn add_async_method<M, A, R>(&mut self, name: impl Into<String>, method: M)
    where
        T: 'static,
        M: AsyncFn(&Luau, UserDataRef<T>, A) -> Result<R> + 'static,
        A: FromLuauMulti + 'static,
        R: IntoLuauMulti,
    {
        let name = name.into();
        let callback = self.box_async_method(&name, method);
        self.raw.async_methods.push((name, callback));
    }
    fn add_async_method_mut<M, A, R>(&mut self, name: impl Into<String>, method: M)
    where
        T: 'static,
        M: AsyncFn(&Luau, UserDataRefMut<T>, A) -> Result<R> + 'static,
        A: FromLuauMulti + 'static,
        R: IntoLuauMulti,
    {
        let name = name.into();
        let callback = self.box_async_method_mut(&name, method);
        self.raw.async_methods.push((name, callback));
    }

    fn add_function<F, A, R>(&mut self, name: impl Into<String>, function: F)
    where
        F: Fn(&Luau, A) -> Result<R> + 'static,
        A: FromLuauMulti,
        R: IntoLuauMulti,
    {
        let name = name.into();
        let callback = self.box_function(&name, function);
        self.raw.methods.push((name, callback));
    }

    fn add_function_mut<F, A, R>(&mut self, name: impl Into<String>, function: F)
    where
        F: FnMut(&Luau, A) -> Result<R> + 'static,
        A: FromLuauMulti,
        R: IntoLuauMulti,
    {
        let name = name.into();
        let callback = self.box_function_mut(&name, function);
        self.raw.methods.push((name, callback));
    }
    fn add_async_function<F, A, R>(&mut self, name: impl Into<String>, function: F)
    where
        F: AsyncFn(&Luau, A) -> Result<R> + 'static,
        A: FromLuauMulti + 'static,
        R: IntoLuauMulti,
    {
        let name = name.into();
        let callback = self.box_async_function(&name, function);
        self.raw.async_methods.push((name, callback));
    }

    fn add_meta_method<M, A, R>(&mut self, name: impl Into<String>, method: M)
    where
        M: Fn(&Luau, &T, A) -> Result<R> + 'static,
        A: FromLuauMulti,
        R: IntoLuauMulti,
    {
        let name = name.into();
        let callback = self.box_method(&name, method);
        self.raw.meta_methods.push((name, callback));
    }

    fn add_meta_method_mut<M, A, R>(&mut self, name: impl Into<String>, method: M)
    where
        M: FnMut(&Luau, &mut T, A) -> Result<R> + 'static,
        A: FromLuauMulti,
        R: IntoLuauMulti,
    {
        let name = name.into();
        let callback = self.box_method_mut(&name, method);
        self.raw.meta_methods.push((name, callback));
    }

    fn add_meta_function<F, A, R>(&mut self, name: impl Into<String>, function: F)
    where
        F: Fn(&Luau, A) -> Result<R> + 'static,
        A: FromLuauMulti,
        R: IntoLuauMulti,
    {
        let name = name.into();
        let callback = self.box_function(&name, function);
        self.raw.meta_methods.push((name, callback));
    }

    fn add_meta_function_mut<F, A, R>(&mut self, name: impl Into<String>, function: F)
    where
        F: FnMut(&Luau, A) -> Result<R> + 'static,
        A: FromLuauMulti,
        R: IntoLuauMulti,
    {
        let name = name.into();
        let callback = self.box_function_mut(&name, function);
        self.raw.meta_methods.push((name, callback));
    }
}

macro_rules! lua_userdata_impl {
    ($type:ty) => {
        impl<T: UserData + 'static> UserData for $type {
            fn register(registry: &mut UserDataRegistry<Self>) {
                let mut orig_registry = UserDataRegistry::new(registry.lua.lua());
                T::register(&mut orig_registry);

                // Copy all fields, methods, etc. from the original registry
                (registry.raw.fields).extend(orig_registry.raw.fields);
                (registry.raw.field_getters).extend(orig_registry.raw.field_getters);
                (registry.raw.field_setters).extend(orig_registry.raw.field_setters);
                (registry.raw.meta_fields).extend(orig_registry.raw.meta_fields);
                (registry.raw.methods).extend(orig_registry.raw.methods);
                (registry.raw.async_methods).extend(orig_registry.raw.async_methods);
                (registry.raw.meta_methods).extend(orig_registry.raw.meta_methods);
                (registry.raw.async_meta_methods).extend(orig_registry.raw.async_meta_methods);
            }
        }
    };
}

// A special proxy object for UserData
pub struct UserDataProxy<T>(pub(crate) PhantomData<T>);

// `UserDataProxy` holds no real `T` value, only a type marker, so it is always safe to send/share.
unsafe impl<T> Send for UserDataProxy<T> {}
unsafe impl<T> Sync for UserDataProxy<T> {}

lua_userdata_impl!(UserDataProxy<T>);
