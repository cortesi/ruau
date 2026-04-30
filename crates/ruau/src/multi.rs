use std::{
    collections::{VecDeque, vec_deque},
    iter::FromIterator,
    mem,
    ops::{Deref, DerefMut},
    os::raw::c_int,
    result::Result as StdResult,
};

use crate::{
    error::Result,
    state::Luau,
    traits::{FromLuau, FromLuauMulti, IntoLuau, IntoLuauMulti, StackCtx},
    util::{check_stack, check_stack_for_values},
    value::{Nil, Value},
};

/// Result is convertible to [`MultiValue`] following the common Luau idiom of returning the result
/// on success, or in the case of an error, returning `nil` and an error message.
impl<T: IntoLuau, E: IntoLuau> IntoLuauMulti for StdResult<T, E> {
    #[inline]
    fn into_luau_multi(self, lua: &Luau) -> Result<MultiValue> {
        match self {
            Ok(val) => (val,).into_luau_multi(lua),
            Err(err) => (Nil, err).into_luau_multi(lua),
        }
    }

    #[inline]
    unsafe fn push_into_stack_multi(self, ctx: &StackCtx<'_>) -> Result<c_int> {
        match self {
            Ok(val) => (val,).push_into_stack_multi(ctx),
            Err(err) => (Nil, err).push_into_stack_multi(ctx),
        }
    }
}

impl<E: IntoLuau> IntoLuauMulti for StdResult<(), E> {
    #[inline]
    fn into_luau_multi(self, lua: &Luau) -> Result<MultiValue> {
        match self {
            Ok(_) => const { Ok(MultiValue::new()) },
            Err(err) => (Nil, err).into_luau_multi(lua),
        }
    }

    #[inline]
    unsafe fn push_into_stack_multi(self, ctx: &StackCtx<'_>) -> Result<c_int> {
        match self {
            Ok(_) => Ok(0),
            Err(err) => (Nil, err).push_into_stack_multi(ctx),
        }
    }
}

impl<T: IntoLuau> IntoLuauMulti for T {
    #[inline]
    fn into_luau_multi(self, lua: &Luau) -> Result<MultiValue> {
        let mut v = MultiValue::with_capacity(1);
        v.push_back(self.into_luau(lua)?);
        Ok(v)
    }

    #[inline]
    unsafe fn push_into_stack_multi(self, ctx: &StackCtx<'_>) -> Result<c_int> {
        self.push_into_stack(ctx)?;
        Ok(1)
    }
}

impl<T: FromLuau> FromLuauMulti for T {
    #[inline]
    fn from_luau_multi(mut values: MultiValue, lua: &Luau) -> Result<Self> {
        T::from_luau(values.pop_front().unwrap_or(Nil), lua)
    }

    #[inline]
    fn from_luau_args(
        mut args: MultiValue,
        i: usize,
        to: Option<&str>,
        lua: &Luau,
    ) -> Result<Self> {
        T::from_luau_arg(args.pop_front().unwrap_or(Nil), i, to, lua)
    }

    #[inline]
    unsafe fn from_stack_multi(nvals: c_int, ctx: &StackCtx<'_>) -> Result<Self> {
        if nvals == 0 {
            return T::from_luau(Nil, ctx.lua.lua());
        }
        T::from_stack(-nvals, ctx)
    }

    #[inline]
    unsafe fn from_stack_args(
        nargs: c_int,
        i: usize,
        to: Option<&str>,
        ctx: &StackCtx<'_>,
    ) -> Result<Self> {
        if nargs == 0 {
            return T::from_luau_arg(Nil, i, to, ctx.lua.lua());
        }
        T::from_stack_arg(-nargs, i, to, ctx)
    }
}

/// Multiple Luau values used for both argument passing and also for multiple return values.
#[derive(Default, Debug, Clone)]
pub struct MultiValue(VecDeque<Value>);

impl Deref for MultiValue {
    type Target = VecDeque<Value>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for MultiValue {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl MultiValue {
    /// Creates an empty `MultiValue` containing no values.
    #[inline]
    pub const fn new() -> Self {
        Self(VecDeque::new())
    }

    /// Creates an empty `MultiValue` container with space for at least `capacity` elements.
    pub fn with_capacity(capacity: usize) -> Self {
        Self(VecDeque::with_capacity(capacity))
    }

    /// Creates a `MultiValue` container from vector of values.
    ///
    /// This method works in *O*(1) time and does not allocate any additional memory.
    #[inline]
    pub fn from_vec(vec: Vec<Value>) -> Self {
        vec.into()
    }

    /// Consumes the `MultiValue` and returns a vector of values.
    ///
    /// This method needs *O*(*n*) data movement if the circular buffer doesn't happen to be at the
    /// beginning of the allocation.
    #[inline]
    pub fn into_vec(self) -> Vec<Value> {
        self.into()
    }

    #[inline]
    pub(crate) fn from_luau_iter<T: IntoLuau>(
        lua: &Luau,
        iter: impl IntoIterator<Item = T>,
    ) -> Result<Self> {
        let iter = iter.into_iter();
        let mut multi_value = Self::with_capacity(iter.size_hint().0);
        for value in iter {
            multi_value.push_back(value.into_luau(lua)?);
        }
        Ok(multi_value)
    }
}

impl From<Vec<Value>> for MultiValue {
    #[inline]
    fn from(value: Vec<Value>) -> Self {
        Self(value.into())
    }
}

impl From<MultiValue> for Vec<Value> {
    #[inline]
    fn from(value: MultiValue) -> Self {
        value.0.into()
    }
}

impl FromIterator<Value> for MultiValue {
    #[inline]
    fn from_iter<I: IntoIterator<Item = Value>>(iter: I) -> Self {
        let mut multi_value = Self::new();
        multi_value.extend(iter);
        multi_value
    }
}

impl IntoIterator for MultiValue {
    type Item = Value;
    type IntoIter = vec_deque::IntoIter<Value>;

    #[inline]
    fn into_iter(mut self) -> Self::IntoIter {
        let deque = mem::take(&mut self.0);
        mem::forget(self);
        deque.into_iter()
    }
}

impl<'a> IntoIterator for &'a MultiValue {
    type Item = &'a Value;
    type IntoIter = vec_deque::Iter<'a, Value>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl IntoLuauMulti for MultiValue {
    #[inline]
    fn into_luau_multi(self, _: &Luau) -> Result<MultiValue> {
        Ok(self)
    }
}

impl IntoLuauMulti for &MultiValue {
    #[inline]
    fn into_luau_multi(self, _: &Luau) -> Result<MultiValue> {
        Ok(self.clone())
    }

    #[inline]
    unsafe fn push_into_stack_multi(self, ctx: &StackCtx<'_>) -> Result<c_int> {
        let lua = ctx.lua;
        let nresults = check_stack_for_values(lua.state(), self.len())?;
        for value in &self.0 {
            lua.push_value(value)?;
        }
        Ok(nresults)
    }
}

impl FromLuauMulti for MultiValue {
    #[inline]
    fn from_luau_multi(values: MultiValue, _: &Luau) -> Result<Self> {
        Ok(values)
    }
}

/// Wraps a variable number of `T`s.
///
/// Can be used to work with variadic functions more easily. Using this type as the last argument of
/// a Rust callback will accept any number of arguments from Luau and convert them to the type `T`
/// using [`FromLuau`]. `Variadic<T>` can also be returned from a callback, returning a variable
/// number of values to Luau.
///
/// The [`MultiValue`] type is equivalent to `Variadic<Value>`.
///
/// # Examples
///
/// ```
/// # use ruau::{Luau, Result, Variadic};
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> Result<()> {
/// # let lua = Luau::new();
/// let add = lua.create_function(|_, vals: Variadic<f64>| -> Result<f64> {
///     Ok(vals.iter().sum())
/// })?;
/// lua.globals().set("add", add)?;
/// assert_eq!(lua.load("add(3, 2, 5)").eval::<f32>().await?, 10.0);
/// # Ok(())
/// # }
/// ```
#[derive(Default, Debug, Clone)]
pub struct Variadic<T>(Vec<T>);

impl<T> Variadic<T> {
    /// Creates an empty `Variadic` wrapper containing no values.
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// Creates an empty `Variadic` container with space for at least `capacity` elements.
    pub fn with_capacity(capacity: usize) -> Self {
        Self(Vec::with_capacity(capacity))
    }
}

impl<T> Deref for Variadic<T> {
    type Target = Vec<T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for Variadic<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> From<Vec<T>> for Variadic<T> {
    #[inline]
    fn from(vec: Vec<T>) -> Self {
        Self(vec)
    }
}

impl<T> From<Variadic<T>> for Vec<T> {
    #[inline]
    fn from(value: Variadic<T>) -> Self {
        value.0
    }
}

impl<T> FromIterator<T> for Variadic<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self(Vec::from_iter(iter))
    }
}

impl<T> IntoIterator for Variadic<T> {
    type Item = T;
    type IntoIter = <Vec<T> as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<T: IntoLuau> IntoLuauMulti for Variadic<T> {
    #[inline]
    fn into_luau_multi(self, lua: &Luau) -> Result<MultiValue> {
        MultiValue::from_luau_iter(lua, self)
    }

    unsafe fn push_into_stack_multi(self, ctx: &StackCtx<'_>) -> Result<c_int> {
        let nresults = check_stack_for_values(ctx.lua.state(), self.len())?;
        for value in self.0 {
            value.push_into_stack(ctx)?;
        }
        Ok(nresults)
    }
}

impl<T: FromLuau> FromLuauMulti for Variadic<T> {
    #[inline]
    fn from_luau_multi(mut values: MultiValue, lua: &Luau) -> Result<Self> {
        values
            .drain(..)
            .map(|val| T::from_luau(val, lua))
            .collect::<Result<Vec<T>>>()
            .map(Variadic)
    }
}

macro_rules! impl_tuple {
    () => (
        impl IntoLuauMulti for () {
            #[inline]
            fn into_luau_multi(self, _: &Luau) -> Result<MultiValue> {
                const { Ok(MultiValue::new()) }
            }

            #[inline]
            unsafe fn push_into_stack_multi(self, _ctx: &StackCtx<'_>) -> Result<c_int> {
                Ok(0)
            }
        }

        impl FromLuauMulti for () {
            #[inline]
            fn from_luau_multi(_values: MultiValue, _lua: &Luau) -> Result<Self> {
                Ok(())
            }

            #[inline]
            unsafe fn from_stack_multi(_nvals: c_int, _ctx: &StackCtx<'_>) -> Result<Self> {
                Ok(())
            }
        }
    );

    ($last_ty:ident $last:ident) => (
        impl<$last_ty> IntoLuauMulti for ($last_ty,)
            where $last_ty: IntoLuauMulti
        {
            #[inline]
            fn into_luau_multi(self, lua: &Luau) -> Result<MultiValue> {
                let ($last,) = self;
                $last.into_luau_multi(lua)
            }

            #[inline]
            unsafe fn push_into_stack_multi(self, ctx: &StackCtx<'_>) -> Result<c_int> {
                let ($last,) = self;
                $last.push_into_stack_multi(ctx)
            }
        }

        impl<$last_ty> FromLuauMulti for ($last_ty,)
            where $last_ty: FromLuauMulti
        {
            #[inline]
            fn from_luau_multi(values: MultiValue, lua: &Luau) -> Result<Self> {
                Ok((FromLuauMulti::from_luau_multi(values, lua)?,))
            }

            #[inline]
            fn from_luau_args(args: MultiValue, i: usize, to: Option<&str>, lua: &Luau) -> Result<Self> {
                Ok((FromLuauMulti::from_luau_args(args, i, to, lua)?,))
            }

            #[inline]
            unsafe fn from_stack_multi(nvals: c_int, ctx: &StackCtx<'_>) -> Result<Self> {
                Ok((FromLuauMulti::from_stack_multi(nvals, ctx)?,))
            }

            #[inline]
            unsafe fn from_stack_args(nargs: c_int, i: usize, to: Option<&str>, ctx: &StackCtx<'_>) -> Result<Self> {
                Ok((FromLuauMulti::from_stack_args(nargs, i, to, ctx)?,))
            }
        }
    );

    ($last_ty:ident $last:ident, $($ty:ident $name:ident),+) => (
        impl<$($ty,)* $last_ty> IntoLuauMulti for ($($ty,)* $last_ty,)
            where $($ty: IntoLuau,)*
                  $last_ty: IntoLuauMulti
        {
            #[inline]
            fn into_luau_multi(self, lua: &Luau) -> Result<MultiValue> {
                let ($($name,)* $last,) = self;

                let mut results = $last.into_luau_multi(lua)?;
                push_reverse!(results, $($name.into_luau(lua)?,)*);
                Ok(results)
            }

            #[inline]
            unsafe fn push_into_stack_multi(self, ctx: &StackCtx<'_>) -> Result<c_int> {
                let ($($name,)* $last,) = self;
                let mut nresults = 0;
                $(
                    let _ = &$name;
                    nresults += 1;
                )+
                check_stack(ctx.lua.state(), nresults + 1)?;
                $(
                    $name.push_into_stack(ctx)?;
                )+
                nresults += $last.push_into_stack_multi(ctx)?;
                Ok(nresults)
            }
        }

        impl<$($ty,)* $last_ty> FromLuauMulti for ($($ty,)* $last_ty,)
            where $($ty: FromLuau,)*
                  $last_ty: FromLuauMulti
        {
            #[inline]
            fn from_luau_multi(mut values: MultiValue, lua: &Luau) -> Result<Self> {
                $(let $name = FromLuau::from_luau(values.pop_front().unwrap_or(Nil), lua)?;)+
                let $last = FromLuauMulti::from_luau_multi(values, lua)?;
                Ok(($($name,)* $last,))
            }

            #[inline]
            fn from_luau_args(mut args: MultiValue, mut i: usize, to: Option<&str>, lua: &Luau) -> Result<Self> {
                $(
                    let $name = FromLuau::from_luau_arg(args.pop_front().unwrap_or(Nil), i, to, lua)?;
                    i += 1;
                )+
                let $last = FromLuauMulti::from_luau_args(args, i, to, lua)?;
                Ok(($($name,)* $last,))
            }

            #[inline]
            unsafe fn from_stack_multi(mut nvals: c_int, ctx: &StackCtx<'_>) -> Result<Self> {
                $(
                    let $name = if nvals > 0 {
                        nvals -= 1;
                        FromLuau::from_stack(-(nvals + 1), ctx)
                    } else {
                        FromLuau::from_luau(Nil, ctx.lua.lua())
                    }?;
                )+
                let $last = FromLuauMulti::from_stack_multi(nvals, ctx)?;
                Ok(($($name,)* $last,))
            }

            #[inline]
            unsafe fn from_stack_args(mut nargs: c_int, mut i: usize, to: Option<&str>, ctx: &StackCtx<'_>) -> Result<Self> {
                $(
                    let $name = if nargs > 0 {
                        nargs -= 1;
                        FromLuau::from_stack_arg(-(nargs + 1), i, to, ctx)
                    } else {
                        FromLuau::from_luau_arg(Nil, i, to, ctx.lua.lua())
                    }?;
                    i += 1;
                )+
                let $last = FromLuauMulti::from_stack_args(nargs, i, to, ctx)?;
                Ok(($($name,)* $last,))
            }
        }
    );
}

macro_rules! push_reverse {
    ($multi_value:expr, $first:expr, $($rest:expr,)*) => (
        push_reverse!($multi_value, $($rest,)*);
        $multi_value.push_front($first);
    );

    ($multi_value:expr, $first:expr) => (
        $multi_value.push_front($first);
    );

    ($multi_value:expr,) => ();
}

impl_tuple!();
impl_tuple!(A a);
impl_tuple!(A a, B b);
impl_tuple!(A a, B b, C c);
impl_tuple!(A a, B b, C c, D d);
impl_tuple!(A a, B b, C c, D d, E e);
impl_tuple!(A a, B b, C c, D d, E e, F f);
impl_tuple!(A a, B b, C c, D d, E e, F f, G g);
impl_tuple!(A a, B b, C c, D d, E e, F f, G g, H h);
impl_tuple!(A a, B b, C c, D d, E e, F f, G g, H h, I i);
impl_tuple!(A a, B b, C c, D d, E e, F f, G g, H h, I i, J j);
impl_tuple!(A a, B b, C c, D d, E e, F f, G g, H h, I i, J j, K k);
impl_tuple!(A a, B b, C c, D d, E e, F f, G g, H h, I i, J j, K k, L l);
impl_tuple!(A a, B b, C c, D d, E e, F f, G g, H h, I i, J j, K k, L l, M m);
impl_tuple!(A a, B b, C c, D d, E e, F f, G g, H h, I i, J j, K k, L l, M m, N n);
impl_tuple!(A a, B b, C c, D d, E e, F f, G g, H h, I i, J j, K k, L l, M m, N n, O o);
impl_tuple!(A a, B b, C c, D d, E e, F f, G g, H h, I i, J j, K k, L l, M m, N n, O o, P p);

#[cfg(test)]
mod assertions {
    use super::*;

    static_assertions::assert_not_impl_any!(MultiValue: Send);
}
