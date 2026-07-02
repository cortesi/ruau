//! Ergonomic extensions over the supported host/native-module ABI.

use std::{fmt, marker::PhantomData};

use ruau_vm_api::{
    HostCall, HostContext, HostFunction, HostUnwind, HostValue, ModuleBinding, ModuleBuilder,
    OwnedValue, RuntimeErrorKind,
};

use crate::{
    AsyncHostFunction, HostType, RuntimeError, ScopedHostFunction, Stashed,
    async_module_host_callable, scoped_module_host_callable,
};

/// Error raised while decoding the immediate arguments of a leaf host function.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostArgsError {
    message: String,
}

impl HostArgsError {
    /// Builds an argument conversion error.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn into_unwind(self) -> HostUnwind {
        HostUnwind {
            error: OwnedValue::Bytes(self.message.into_bytes()),
            kind: RuntimeErrorKind::Runtime,
            script_fields: Vec::new(),
        }
    }
}

impl fmt::Display for HostArgsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HostArgsError {}

/// Converts the immediate arguments of a leaf host function into a Rust value.
///
/// This trait intentionally covers heap-free values. Use [`scoped_host_fn`]
/// for functions that need strings, tables, buffers, retained handles, app
/// data, or other scoped VM access.
///
/// [`scoped_host_fn`]: crate::scoped_host_fn
pub trait FromHostArgs: Sized {
    /// Converts a host call's arguments.
    ///
    /// # Errors
    /// Returns [`HostArgsError`] if the arity or any immediate value type is wrong.
    fn from_host_args(ctx: &dyn HostContext) -> Result<Self, HostArgsError>;
}

/// Converts a Rust value into the owned return shape used by host functions.
pub trait IntoHostReturn {
    /// Converts `self` into zero or more owned Lua values.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] when a compositional return, such as
    /// `Result<T, RuntimeError>`, should raise at the host call site instead of
    /// producing values.
    fn into_host_return(self) -> Result<Vec<OwnedValue>, RuntimeError>;
}

/// Extension methods for registering simple host functions on native modules.
///
/// The registered functions are synchronous, bounded, heap-free leaf calls.
/// They can read immediate scalar arguments and return owned scalar or string
/// data without exposing the low-level [`HostFunction`] ABI to application
/// code.
pub trait ModuleBuilderExt {
    /// Registers a synchronous leaf host function under `name`.
    fn leaf_function<F, A, R>(&mut self, name: &str, binding: ModuleBinding, f: F)
    where
        F: Fn(A) -> R + Send + Sync + 'static,
        A: FromHostArgs + 'static,
        R: IntoHostReturn + 'static;

    /// Registers a scoped host function under `name`.
    fn scoped_function(
        &mut self,
        name: &str,
        binding: ModuleBinding,
        f: Box<dyn ScopedHostFunction>,
    );

    /// Registers an async scoped host function under `name`.
    fn async_function(&mut self, name: &str, binding: ModuleBinding, f: Box<dyn AsyncHostFunction>);

    /// Registers a host userdata type owned by this module.
    fn host_type(&mut self, host_type: HostType);
}

impl<T> ModuleBuilderExt for T
where
    T: ModuleBuilder + ?Sized,
{
    fn leaf_function<F, A, R>(&mut self, name: &str, binding: ModuleBinding, f: F)
    where
        F: Fn(A) -> R + Send + Sync + 'static,
        A: FromHostArgs + 'static,
        R: IntoHostReturn + 'static,
    {
        self.function(
            name,
            binding,
            Box::new(LeafHostFunction {
                f,
                _marker: PhantomData,
            }),
        );
    }

    fn scoped_function(
        &mut self,
        name: &str,
        binding: ModuleBinding,
        f: Box<dyn ScopedHostFunction>,
    ) {
        self.host_callable(name, binding, scoped_module_host_callable(f));
    }

    fn async_function(
        &mut self,
        name: &str,
        binding: ModuleBinding,
        f: Box<dyn AsyncHostFunction>,
    ) {
        self.host_callable(name, binding, async_module_host_callable(f));
    }

    fn host_type(&mut self, host_type: HostType) {
        ModuleBuilder::host_type(
            self,
            ruau_vm_api::EngineHostType::from_engine(Box::new(host_type)),
        );
    }
}

struct LeafHostFunction<F, A, R> {
    f: F,
    _marker: PhantomData<fn(A) -> R>,
}

impl<F, A, R> HostFunction for LeafHostFunction<F, A, R>
where
    F: Fn(A) -> R + Send + Sync,
    A: FromHostArgs,
    R: IntoHostReturn,
{
    fn call(&self, ctx: &mut dyn HostContext) -> HostCall {
        match A::from_host_args(ctx) {
            Ok(args) => match (self.f)(args).into_host_return() {
                Ok(values) => HostCall::Ready(Ok(values)),
                Err(error) => HostCall::Ready(Err(runtime_error_into_unwind(&error))),
            },
            Err(error) => HostCall::Ready(Err(error.into_unwind())),
        }
    }
}

impl FromHostArgs for () {
    fn from_host_args(ctx: &dyn HostContext) -> Result<Self, HostArgsError> {
        expect_arity(ctx, 0)
    }
}

macro_rules! impl_single_host_arg {
    ($($t:ty),+ $(,)?) => {
        $(
            impl FromHostArgs for $t {
                fn from_host_args(ctx: &dyn HostContext) -> Result<Self, HostArgsError> {
                    expect_arity(ctx, 1)?;
                    <$t as FromImmediateHostArg>::from_host_arg(0, ctx.arg(0))
                }
            }
        )+
    };
}

impl_single_host_arg!(bool, i64, f64);

macro_rules! impl_host_args_tuple {
    ($(($name:ident, $index:tt)),+ $(,)?) => {
        impl<$($name),+> FromHostArgs for ($($name,)+)
        where
            $($name: FromImmediateHostArg,)+
        {
            fn from_host_args(ctx: &dyn HostContext) -> Result<Self, HostArgsError> {
                const EXPECTED: usize = 0 $(+ { let _ = stringify!($name); 1 })+;
                expect_arity(ctx, EXPECTED)?;
                Ok(($($name::from_host_arg($index, ctx.arg($index))?,)+))
            }
        }
    };
}

impl_host_args_tuple!((A, 0));
impl_host_args_tuple!((A, 0), (B, 1));
impl_host_args_tuple!((A, 0), (B, 1), (C, 2));
impl_host_args_tuple!((A, 0), (B, 1), (C, 2), (D, 3));

impl IntoHostReturn for () {
    fn into_host_return(self) -> Result<Vec<OwnedValue>, RuntimeError> {
        Ok(Vec::new())
    }
}

impl IntoHostReturn for OwnedValue {
    fn into_host_return(self) -> Result<Vec<OwnedValue>, RuntimeError> {
        Ok(vec![self])
    }
}

impl<T> IntoHostReturn for Stashed<T> {
    fn into_host_return(self) -> Result<Vec<OwnedValue>, RuntimeError> {
        Ok(vec![self.into_owned_value()])
    }
}

impl IntoHostReturn for bool {
    fn into_host_return(self) -> Result<Vec<OwnedValue>, RuntimeError> {
        Ok(vec![OwnedValue::Boolean(self)])
    }
}

impl IntoHostReturn for i64 {
    fn into_host_return(self) -> Result<Vec<OwnedValue>, RuntimeError> {
        Ok(vec![OwnedValue::Integer(self)])
    }
}

impl IntoHostReturn for f64 {
    fn into_host_return(self) -> Result<Vec<OwnedValue>, RuntimeError> {
        Ok(vec![OwnedValue::Number(self)])
    }
}

impl IntoHostReturn for Vec<u8> {
    fn into_host_return(self) -> Result<Vec<OwnedValue>, RuntimeError> {
        Ok(vec![OwnedValue::Bytes(self)])
    }
}

impl IntoHostReturn for String {
    fn into_host_return(self) -> Result<Vec<OwnedValue>, RuntimeError> {
        self.into_bytes().into_host_return()
    }
}

impl IntoHostReturn for &'static str {
    fn into_host_return(self) -> Result<Vec<OwnedValue>, RuntimeError> {
        self.as_bytes().to_vec().into_host_return()
    }
}

impl<T> IntoHostReturn for Option<T>
where
    T: IntoHostReturn,
{
    fn into_host_return(self) -> Result<Vec<OwnedValue>, RuntimeError> {
        match self {
            Some(value) => value.into_host_return(),
            None => Ok(vec![OwnedValue::Nil]),
        }
    }
}

impl<T> IntoHostReturn for Result<T, RuntimeError>
where
    T: IntoHostReturn,
{
    fn into_host_return(self) -> Result<Vec<OwnedValue>, RuntimeError> {
        self?.into_host_return()
    }
}

macro_rules! impl_host_return_tuple {
    ($(($name:ident, $var:ident)),+ $(,)?) => {
        impl<$($name),+> IntoHostReturn for ($($name,)+)
        where
            $($name: IntoHostReturn,)+
        {
            fn into_host_return(self) -> Result<Vec<OwnedValue>, RuntimeError> {
                let ($($var,)+) = self;
                let mut out = Vec::new();
                $(out.extend($var.into_host_return()?);)+
                Ok(out)
            }
        }
    };
}

impl_host_return_tuple!((A, a), (B, b));
impl_host_return_tuple!((A, a), (B, b), (C, c));
impl_host_return_tuple!((A, a), (B, b), (C, c), (D, d));

trait FromImmediateHostArg: Sized {
    fn from_host_arg(index: usize, value: Option<HostValue<'_>>) -> Result<Self, HostArgsError>;
}

impl FromImmediateHostArg for bool {
    fn from_host_arg(index: usize, value: Option<HostValue<'_>>) -> Result<Self, HostArgsError> {
        match value {
            Some(HostValue::Boolean(value)) => Ok(value),
            Some(other) => Err(type_error(index, "boolean", other)),
            None => Err(missing_arg_error(index)),
        }
    }
}

impl FromImmediateHostArg for i64 {
    fn from_host_arg(index: usize, value: Option<HostValue<'_>>) -> Result<Self, HostArgsError> {
        match value {
            Some(HostValue::Integer(value)) => Ok(value),
            Some(other) => Err(type_error(index, "integer", other)),
            None => Err(missing_arg_error(index)),
        }
    }
}

impl FromImmediateHostArg for f64 {
    fn from_host_arg(index: usize, value: Option<HostValue<'_>>) -> Result<Self, HostArgsError> {
        match value {
            Some(HostValue::Number(value)) => Ok(value),
            Some(HostValue::Integer(value)) => Ok(value as Self),
            Some(other) => Err(type_error(index, "number", other)),
            None => Err(missing_arg_error(index)),
        }
    }
}

fn expect_arity(ctx: &dyn HostContext, expected: usize) -> Result<(), HostArgsError> {
    let got = ctx.arg_count();
    if got == expected {
        Ok(())
    } else {
        Err(HostArgsError::new(format!(
            "expected {expected} host arguments, got {got}"
        )))
    }
}

fn missing_arg_error(index: usize) -> HostArgsError {
    HostArgsError::new(format!("missing host argument {}", index + 1))
}

fn runtime_error_into_unwind(error: &RuntimeError) -> HostUnwind {
    HostUnwind {
        error: OwnedValue::Bytes(error.message().as_bytes().to_vec()),
        kind: error.kind(),
        script_fields: error.script_fields().to_vec(),
    }
}

fn type_error(index: usize, expected: &'static str, got: HostValue<'_>) -> HostArgsError {
    HostArgsError::new(format!(
        "host argument {} expected {expected}, got {}",
        index + 1,
        host_value_type(got)
    ))
}

fn host_value_type(value: HostValue<'_>) -> &'static str {
    match value {
        HostValue::Nil => "nil",
        HostValue::Boolean(_) => "boolean",
        HostValue::Number(_) => "number",
        HostValue::Integer(_) => "integer",
        HostValue::Vector(_) => "vector",
        HostValue::LightUserdata { .. } => "lightuserdata",
        HostValue::String(_) => "string",
        HostValue::Table(_) => "table",
        HostValue::Function(_) => "function",
        HostValue::Userdata(_) => "userdata",
        HostValue::Thread(_) => "thread",
        HostValue::Buffer(_) => "buffer",
    }
}
