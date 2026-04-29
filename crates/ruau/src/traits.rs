//! Core conversion and extension traits.
//!
//! This module provides the fundamental traits for converting values between Rust and Luau,
//! and for defining native Luau callable functions.

use std::{future::Future, os::raw::c_int, sync::Arc};

use crate::{
    error::{Error, Result},
    multi::MultiValue,
    state::{Luau, RawLuau, WeakLuau},
    util::{check_stack_for_values, parse_lookup_path, short_type_name},
    value::Value,
};

/// Opaque context handed to the stack-level methods on the conversion traits.
///
/// External implementers of [`IntoLuau`] / [`FromLuau`] / [`IntoLuauMulti`] /
/// [`FromLuauMulti`] receive a `&StackCtx<'_>` they cannot deconstruct or use directly. The
/// default trait method bodies forward through the high-level `Value` API. Internal code in
/// this crate constructs `StackCtx` via [`StackCtx::new`] when it needs to drive specialised
/// stack overrides.
pub struct StackCtx<'a> {
    pub(crate) lua: &'a RawLuau,
}

impl<'a> StackCtx<'a> {
    /// Creates a new stack context wrapping the given raw VM.
    #[inline(always)]
    pub(crate) fn new(lua: &'a RawLuau) -> Self {
        Self { lua }
    }
}

/// Trait for types convertible to [`Value`].
pub trait IntoLuau: Sized {
    /// Performs the conversion.
    fn into_luau(self, lua: &Luau) -> Result<Value>;

    /// Pushes the value into the Luau stack.
    ///
    /// # Safety
    /// This method does not check Luau stack space.
    #[doc(hidden)]
    #[inline]
    unsafe fn push_into_stack(self, ctx: &StackCtx<'_>) -> Result<()> {
        let lua = ctx.lua;
        lua.push_value(&self.into_luau(lua.lua())?)
    }
}

/// Trait for types convertible from [`Value`].
pub trait FromLuau: Sized {
    /// Performs the conversion.
    fn from_luau(value: Value, lua: &Luau) -> Result<Self>;

    /// Performs the conversion for an argument (eg. function argument).
    ///
    /// `i` is the argument index (position),
    /// `to` is a function name that received the argument.
    #[doc(hidden)]
    #[inline]
    fn from_luau_arg(arg: Value, i: usize, to: Option<&str>, lua: &Luau) -> Result<Self> {
        Self::from_luau(arg, lua).map_err(|err| Error::BadArgument {
            to: to.map(|s| s.to_string()),
            pos: i,
            name: None,
            cause: Arc::new(err),
        })
    }

    /// Performs the conversion for a value in the Luau stack at index `idx`.
    #[doc(hidden)]
    #[inline]
    unsafe fn from_stack(idx: c_int, ctx: &StackCtx<'_>) -> Result<Self> {
        let lua = ctx.lua;
        Self::from_luau(lua.stack_value(idx, None), lua.lua())
    }

    /// Same as `from_luau_arg` but for a value in the Luau stack at index `idx`.
    #[doc(hidden)]
    #[inline]
    unsafe fn from_stack_arg(
        idx: c_int,
        i: usize,
        to: Option<&str>,
        ctx: &StackCtx<'_>,
    ) -> Result<Self> {
        Self::from_stack(idx, ctx).map_err(|err| Error::BadArgument {
            to: to.map(|s| s.to_string()),
            pos: i,
            name: None,
            cause: Arc::new(err),
        })
    }
}

/// Trait for types convertible to any number of Luau values.
///
/// This is a generalization of [`IntoLuau`], allowing any number of resulting Luau values instead of
/// just one. Any type that implements [`IntoLuau`] will automatically implement this trait.
pub trait IntoLuauMulti: Sized {
    /// Performs the conversion.
    fn into_luau_multi(self, lua: &Luau) -> Result<MultiValue>;

    /// Pushes the values into the Luau stack.
    ///
    /// Returns number of pushed values.
    #[doc(hidden)]
    #[inline]
    unsafe fn push_into_stack_multi(self, ctx: &StackCtx<'_>) -> Result<c_int> {
        let lua = ctx.lua;
        let values = self.into_luau_multi(lua.lua())?;
        let len = check_stack_for_values(lua.state(), values.len())?;
        unsafe {
            for val in &values {
                lua.push_value(val)?;
            }
        }
        Ok(len)
    }
}

/// Trait for types that can be created from an arbitrary number of Luau values.
///
/// This is a generalization of [`FromLuau`], allowing an arbitrary number of Luau values to
/// participate in the conversion. Any type that implements [`FromLuau`] will automatically
/// implement this trait.
pub trait FromLuauMulti: Sized {
    /// Performs the conversion.
    ///
    /// In case `values` contains more values than needed to perform the conversion, the excess
    /// values should be ignored. This reflects the semantics of Luau when calling a function or
    /// assigning values. Similarly, if not enough values are given, conversions should assume that
    /// any missing values are nil.
    fn from_luau_multi(values: MultiValue, lua: &Luau) -> Result<Self>;

    /// Performs the conversion for a list of arguments.
    ///
    /// `i` is an index (position) of the first argument,
    /// `to` is a function name that received the arguments.
    #[doc(hidden)]
    #[inline]
    fn from_luau_args(args: MultiValue, i: usize, to: Option<&str>, lua: &Luau) -> Result<Self> {
        let _ = (i, to);
        Self::from_luau_multi(args, lua)
    }

    /// Performs the conversion for a number of values in the Luau stack.
    #[doc(hidden)]
    #[inline]
    unsafe fn from_stack_multi(nvals: c_int, ctx: &StackCtx<'_>) -> Result<Self> {
        let lua = ctx.lua;
        let mut values = MultiValue::with_capacity(nvals as usize);
        for idx in 0..nvals {
            values.push_back(lua.stack_value(-nvals + idx, None));
        }
        Self::from_luau_multi(values, lua.lua())
    }

    /// Same as `from_luau_args` but for a number of values in the Luau stack.
    #[doc(hidden)]
    #[inline]
    unsafe fn from_stack_args(
        nargs: c_int,
        i: usize,
        to: Option<&str>,
        ctx: &StackCtx<'_>,
    ) -> Result<Self> {
        let _ = (i, to);
        Self::from_stack_multi(nargs, ctx)
    }
}

/// A trait for types that can be used as Luau objects (usually table and userdata).
pub trait ObjectLike {
    /// Gets the value associated to `key` from the object, assuming it has `__index` metamethod.
    fn get<V: FromLuau>(&self, key: impl IntoLuau) -> Result<V>;

    /// Sets the value associated to `key` in the object, assuming it has `__newindex` metamethod.
    fn set(&self, key: impl IntoLuau, value: impl IntoLuau) -> Result<()>;

    /// Calls the object as a function assuming it has `__call` metamethod.
    ///
    /// The metamethod is called with the object as its first argument, followed by the passed
    /// arguments.
    fn call<R>(&self, args: impl IntoLuauMulti) -> impl Future<Output = Result<R>>
    where
        R: FromLuauMulti;

    /// Gets the function associated to key `name` from the object and calls it,
    /// passing the object itself along with `args` as function arguments.
    fn call_method<R>(
        &self,
        name: &str,
        args: impl IntoLuauMulti,
    ) -> impl Future<Output = Result<R>>
    where
        R: FromLuauMulti;

    /// Gets the function associated to key `name` from the object and calls it,
    /// passing `args` as function arguments.
    ///
    /// This might invoke the `__index` metamethod.
    fn call_function<R>(
        &self,
        name: &str,
        args: impl IntoLuauMulti,
    ) -> impl Future<Output = Result<R>>
    where
        R: FromLuauMulti;

    /// Look up a value by a path of keys.
    ///
    /// The syntax is similar to accessing nested tables in Luau, with additional support for
    /// `?` operator to perform safe navigation.
    ///
    /// For example, the path `a[1].c` is equivalent to `table.a[1].c` in Luau.
    /// With `?` operator, `a[1]?.c` is equivalent to `table.a[1] and table.a[1].c or nil` in Luau.
    ///
    /// Bracket notation rules:
    /// - `[123]` - integer keys
    /// - `["string key"]` or `['string key']` - string keys (must be quoted)
    /// - String keys support escape sequences: `\"`, `\'`, `\\`
    fn get_path<V: FromLuau>(&self, path: &str) -> Result<V> {
        let mut current = self.to_value();
        for (key, safe_nil) in parse_lookup_path(path)? {
            current = match current {
                Value::Table(table) => table.get::<Value>(key),
                Value::UserData(ud) => ud.get::<Value>(key),
                _ => {
                    let type_name = current.type_name();
                    let err = format!("attempt to index a {type_name} value with key '{key}'");
                    Err(Error::runtime(err))
                }
            }?;
            if safe_nil && (current == Value::Nil || current == Value::NULL) {
                break;
            }
        }

        let lua = self.weak_lua().raw();
        V::from_luau(current, lua.lua())
    }

    /// Converts the object to a string in a human-readable format.
    ///
    /// This might invoke the `__tostring` metamethod.
    fn to_string(&self) -> Result<String>;

    /// Converts the object to a Luau value.
    fn to_value(&self) -> Value;

    /// Gets a reference to the associated Luau state.
    #[doc(hidden)]
    fn weak_lua(&self) -> &WeakLuau;
}

pub trait ShortTypeName {
    #[inline(always)]
    fn type_name() -> String {
        short_type_name::<Self>()
    }
}

impl<T> ShortTypeName for T {}
