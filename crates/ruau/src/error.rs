//! Luau error handling.
//!
//! This module provides the [`Error`] type returned by all fallible `ruau` operations, together
//! with extension traits for adapting Rust errors for use within Luau.

use std::{
    error::Error as StdError, fmt, io::Error as IoError, net::AddrParseError, rc::Rc,
    result::Result as StdResult, str::Utf8Error,
};

type DynStdError = dyn StdError;

/// Error type returned by `ruau` methods.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Syntax error while parsing Luau source code.
    #[error("syntax error: {message}")]
    SyntaxError {
        /// The error message as returned by Luau.
        message: String,
        /// `true` if the error can likely be fixed by appending more input to the source code.
        ///
        /// This is useful for implementing REPLs as they can query the user for more input if this
        /// is set.
        incomplete_input: bool,
    },
    /// Luau runtime error, aka `LUA_ERRRUN`.
    ///
    /// The Luau VM returns this error when a builtin operation is performed on incompatible types.
    /// Among other things, this includes invoking operators on wrong types (such as calling or
    /// indexing a `nil` value).
    #[error("runtime error: {0}")]
    RuntimeError(String),
    /// Luau memory error, aka `LUA_ERRMEM`
    ///
    /// The Luau VM returns this error when the allocator does not return the requested memory, aka
    /// it is an out-of-memory error.
    #[error("memory error: {0}")]
    MemoryError(String),
    /// Potentially unsafe action in safe mode.
    #[error("safety error: {0}")]
    SafetyError(String),
    /// Memory control is not available.
    ///
    /// This error can only happen when Luau state was not created by us and does not have the
    /// custom allocator attached.
    #[error("memory control is not available")]
    MemoryControlNotAvailable,
    /// A mutable callback has triggered Luau code that has called the same mutable callback again.
    ///
    /// This is an error because a mutable callback can only be borrowed mutably once.
    #[error("mutable callback called recursively")]
    RecursiveMutCallback,
    /// Either a callback or a userdata method has been called, but the callback or userdata has
    /// been destructed.
    ///
    /// This can happen either due to to being destructed in a previous __gc, or due to being
    /// destructed from exiting a `Luau::scope` call.
    #[error("a destructed callback or destructed userdata method was called")]
    CallbackDestructed,
    /// Not enough stack space to place arguments to Luau functions or return values from callbacks.
    ///
    /// Due to the way `ruau` works, it should not be directly possible to run out of stack space
    /// during normal use. The only way that this error can be triggered is if a `Function` is
    /// called with a huge number of arguments, or a Rust callback returns a huge number of return
    /// values.
    #[error(
        "out of Luau stack, too many arguments to a Luau function or too many return values from a callback"
    )]
    StackError,
    /// Too many arguments to [`Function::bind`].
    ///
    /// [`Function::bind`]: crate::Function::bind
    #[error("too many arguments to Function::bind")]
    BindError,
    /// Bad argument received from Luau (usually when calling a function).
    ///
    /// This error can help to identify the argument that caused the error
    /// (which is stored in the corresponding field).
    #[error("{}", BadArgumentDisplay { to, pos: *pos, name, cause })]
    BadArgument {
        /// Function that was called.
        to: Option<String>,
        /// Argument position (usually starts from 1).
        pos: usize,
        /// Argument name.
        name: Option<String>,
        /// Underlying error returned when converting argument to a Luau value.
        cause: Rc<Self>,
    },
    /// A Luau value could not be converted to the expected Rust type.
    #[error("{}", FromLuauConversionDisplay { from, to, message })]
    FromLuauConversionError {
        /// Name of the Luau type that could not be converted.
        from: &'static str,
        /// Name of the Rust type that could not be created.
        to: String,
        /// A string containing more detailed error information.
        message: Option<String>,
    },
    /// [`Thread::resume`] was called on an unresumable coroutine.
    ///
    /// A coroutine is unresumable if its main function has returned or if an error has occurred
    /// inside the coroutine. Already running coroutines are also marked as unresumable.
    ///
    /// [`Thread::status`] can be used to check if the coroutine can be resumed without causing this
    /// error.
    ///
    /// [`Thread::resume`]: crate::Thread::resume
    /// [`Thread::status`]: crate::Thread::status
    #[error("coroutine is non-resumable")]
    CoroutineUnresumable,
    /// An [`AnyUserData`] is not the expected type in a borrow.
    ///
    /// This error can only happen when manually using [`AnyUserData`], or when implementing
    /// metamethods for binary operators. Refer to the documentation of [`UserDataMethods`] for
    /// details.
    ///
    /// [`AnyUserData`]: crate::AnyUserData
    /// [`UserDataMethods`]: crate::UserDataMethods
    #[error("userdata is not expected type")]
    UserDataTypeMismatch,
    /// An [`AnyUserData`] borrow failed because it has been destructed.
    ///
    /// This error can happen either due to to being destructed in a previous __gc, or due to being
    /// destructed from exiting a `Luau::scope` call.
    ///
    /// [`AnyUserData`]: crate::AnyUserData
    #[error("userdata has been destructed")]
    UserDataDestructed,
    /// An [`AnyUserData`] immutable borrow failed.
    ///
    /// This error can occur when a method on a [`UserData`] type calls back into Luau, which then
    /// tries to call a method on the same [`UserData`] type. Consider restructuring your API to
    /// prevent these errors.
    ///
    /// [`AnyUserData`]: crate::AnyUserData
    /// [`UserData`]: crate::UserData
    #[error("error borrowing userdata")]
    UserDataBorrowError,
    /// An [`AnyUserData`] mutable borrow failed.
    ///
    /// This error can occur when a method on a [`UserData`] type calls back into Luau, which then
    /// tries to call a method on the same [`UserData`] type. Consider restructuring your API to
    /// prevent these errors.
    ///
    /// [`AnyUserData`]: crate::AnyUserData
    /// [`UserData`]: crate::UserData
    #[error("error mutably borrowing userdata")]
    UserDataBorrowMutError,
    /// A [`MetaMethod`] operation is restricted (typically for `__gc` or `__metatable`).
    ///
    /// [`MetaMethod`]: crate::MetaMethod
    #[error("metamethod {0} is restricted")]
    MetaMethodRestricted(String),
    /// A [`MetaMethod`] (eg. `__index` or `__newindex`) has invalid type.
    ///
    /// [`MetaMethod`]: crate::MetaMethod
    #[error("{}", MetaMethodTypeDisplay { method, type_name, message })]
    MetaMethodTypeError {
        /// Name of the metamethod.
        method: String,
        /// Passed value type.
        type_name: &'static str,
        /// A string containing more detailed error information.
        message: Option<String>,
    },
    /// A [`RegistryKey`] produced from a different Luau state was used.
    ///
    /// [`RegistryKey`]: crate::RegistryKey
    #[error("RegistryKey used from different Luau state")]
    MismatchedRegistryKey,
    /// A Rust callback returned `Err`, raising the contained `Error` as a Luau error.
    #[error("{}", CallbackErrorDisplay { cause, traceback })]
    CallbackError {
        /// Luau call stack backtrace.
        traceback: String,
        /// Original error returned by the Rust code.
        cause: Rc<Self>,
    },
    /// A Rust panic that was previously resumed, returned again.
    ///
    /// This error can occur only when a Rust panic resumed previously was recovered
    /// and returned again.
    #[error("previously resumed panic returned again")]
    PreviouslyResumedPanic,
    /// A pending async callback was cancelled before it completed.
    ///
    /// This is raised internally when in-flight Luau async work is dropped.
    #[error("async callback was cancelled")]
    AsyncCallbackCancelled,
    /// Serialization error.
    #[error("serialize error: {0}")]
    SerializeError(String),
    /// Deserialization error.
    #[error("deserialize error: {0}")]
    DeserializeError(String),
    /// A custom error.
    ///
    /// This can be used for returning user-defined errors from callbacks.
    ///
    /// Returning `Err(ExternalError(...))` from a Rust callback will raise the error as a Luau
    /// error. The Rust code that originally invoked the Luau code then receives a `CallbackError`,
    /// from which the original error (and a stack traceback) can be recovered.
    #[error("{0}")]
    ExternalError(Rc<DynStdError>),
    /// An error with additional context.
    #[error("{context}\n{cause}")]
    WithContext {
        /// A string containing additional context.
        context: String,
        /// Underlying error.
        cause: Rc<Self>,
    },
}

/// A specialized `Result` type used by `ruau`'s API.
pub type Result<T> = StdResult<T, Error>;

struct BadArgumentDisplay<'a> {
    to: &'a Option<String>,
    pos: usize,
    name: &'a Option<String>,
    cause: &'a Rc<Error>,
}

impl fmt::Display for BadArgumentDisplay<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(name) = self.name {
            write!(formatter, "bad argument `{name}`")?;
        } else {
            write!(formatter, "bad argument #{}", self.pos)?;
        }
        if let Some(to) = self.to {
            write!(formatter, " to `{to}`")?;
        }
        write!(formatter, ": {}", self.cause)
    }
}

struct FromLuauConversionDisplay<'a> {
    from: &'a &'static str,
    to: &'a String,
    message: &'a Option<String>,
}

impl fmt::Display for FromLuauConversionDisplay<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "error converting Luau {} to {}",
            self.from, self.to
        )?;
        match self.message {
            None => Ok(()),
            Some(message) => write!(formatter, " ({message})"),
        }
    }
}

struct MetaMethodTypeDisplay<'a> {
    method: &'a String,
    type_name: &'a &'static str,
    message: &'a Option<String>,
}

impl fmt::Display for MetaMethodTypeDisplay<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "metamethod {} has unsupported type {}",
            self.method, self.type_name
        )?;
        match self.message {
            None => Ok(()),
            Some(message) => write!(formatter, " ({message})"),
        }
    }
}

struct CallbackErrorDisplay<'a> {
    cause: &'a Rc<Error>,
    traceback: &'a String,
}

impl fmt::Display for CallbackErrorDisplay<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (mut cause, mut full_traceback) = (self.cause, None);
        while let Error::CallbackError {
            cause: cause2,
            traceback: traceback2,
        } = &**cause
        {
            cause = cause2;
            full_traceback = Some(traceback2);
        }
        writeln!(formatter, "{cause}")?;
        if let Some(full_traceback) = full_traceback {
            let traceback = self.traceback.trim_start_matches("stack traceback:");
            let traceback = traceback.trim_start().trim_end();
            if let Some(pos) = full_traceback.find(traceback) {
                write!(formatter, "{}", &full_traceback[..pos])?;
                writeln!(formatter, ">{}", &full_traceback[pos..].trim_end())?;
            } else {
                writeln!(formatter, "{}", full_traceback.trim_end())?;
            }
        } else {
            writeln!(formatter, "{}", self.traceback.trim_end())?;
        }
        Ok(())
    }
}

impl Error {
    /// Creates a new `RuntimeError` with the given message.
    #[inline]
    pub fn runtime<S: fmt::Display>(message: S) -> Self {
        Self::RuntimeError(message.to_string())
    }

    /// Wraps an external error object.
    #[inline]
    pub fn external<T: Into<Box<DynStdError>>>(err: T) -> Self {
        let boxed = err.into();
        match boxed.downcast::<Self>() {
            Ok(err) => *err,
            Err(boxed) => Self::ExternalError(boxed.into()),
        }
    }

    /// Attempts to downcast the external error object to a concrete type by reference.
    pub fn downcast_ref<T>(&self) -> Option<&T>
    where
        T: StdError + 'static,
    {
        match self {
            Self::ExternalError(err) => err.downcast_ref(),
            Self::WithContext { cause, .. } => Self::downcast_ref(cause),
            _ => None,
        }
    }

    /// An iterator over the chain of nested errors wrapped by this Error.
    pub fn chain(&self) -> impl Iterator<Item = &(dyn StdError + 'static)> {
        Chain {
            root: self,
            current: None,
        }
    }

    /// Returns the parent of this error.
    #[doc(hidden)]
    pub fn parent(&self) -> Option<&Self> {
        match self {
            Self::CallbackError { cause, .. } => Some(cause.as_ref()),
            Self::WithContext { cause, .. } => Some(cause.as_ref()),
            _ => None,
        }
    }

    pub(crate) fn bad_self_argument(to: &str, cause: Self) -> Self {
        Self::BadArgument {
            to: Some(to.to_string()),
            pos: 1,
            name: Some("self".to_string()),
            cause: Rc::new(cause),
        }
    }

    #[inline]
    pub(crate) fn from_luau_conversion(
        from: &'static str,
        to: impl Into<String>,
        message: impl Into<Option<String>>,
    ) -> Self {
        Self::FromLuauConversionError {
            from,
            to: to.into(),
            message: message.into(),
        }
    }
}

/// Trait for converting [`std::error::Error`] into Luau [`Error`].
pub trait ExternalError {
    /// Converts this error into a Luau [`Error`].
    fn into_luau_err(self) -> Error;
}

impl<E: Into<Box<DynStdError>>> ExternalError for E {
    fn into_luau_err(self) -> Error {
        Error::external(self)
    }
}

/// Trait for converting [`std::result::Result`] into Luau [`Result`].
pub trait ExternalResult<T> {
    /// Converts this result's error into a Luau [`Error`].
    fn into_luau_err(self) -> Result<T>;
}

impl<T, E> ExternalResult<T> for StdResult<T, E>
where
    E: ExternalError,
{
    fn into_luau_err(self) -> Result<T> {
        self.map_err(|e| e.into_luau_err())
    }
}

/// Provides the `context` method for [`Error`] and `Result<T, Error>`.
pub trait ErrorContext {
    /// Wraps the error value with additional context.
    fn context<C: fmt::Display>(self, context: C) -> Self;

    /// Wrap the error value with additional context that is evaluated lazily
    /// only once an error does occur.
    fn with_context<C: fmt::Display>(self, f: impl FnOnce(&Error) -> C) -> Self;
}

impl ErrorContext for Error {
    fn context<C: fmt::Display>(self, context: C) -> Self {
        let context = context.to_string();
        match self {
            Self::WithContext { cause, .. } => Self::WithContext { context, cause },
            _ => Self::WithContext {
                context,
                cause: Rc::new(self),
            },
        }
    }

    fn with_context<C: fmt::Display>(self, f: impl FnOnce(&Self) -> C) -> Self {
        let context = f(&self).to_string();
        match self {
            Self::WithContext { cause, .. } => Self::WithContext { context, cause },
            _ => Self::WithContext {
                context,
                cause: Rc::new(self),
            },
        }
    }
}

impl<T> ErrorContext for Result<T> {
    fn context<C: fmt::Display>(self, context: C) -> Self {
        self.map_err(|err| err.context(context))
    }

    fn with_context<C: fmt::Display>(self, f: impl FnOnce(&Error) -> C) -> Self {
        self.map_err(|err| err.with_context(f))
    }
}

impl From<AddrParseError> for Error {
    fn from(err: AddrParseError) -> Self {
        Self::external(err)
    }
}

impl From<IoError> for Error {
    fn from(err: IoError) -> Self {
        Self::external(err)
    }
}

impl From<Utf8Error> for Error {
    fn from(err: Utf8Error) -> Self {
        Self::external(err)
    }
}

impl From<crate::resolver::ModuleResolveError> for Error {
    fn from(err: crate::resolver::ModuleResolveError) -> Self {
        Self::external(err)
    }
}

impl serde::ser::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self::SerializeError(msg.to_string())
    }
}

impl serde::de::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self::DeserializeError(msg.to_string())
    }
}

#[cfg(feature = "anyhow")]
impl From<anyhow::Error> for Error {
    fn from(err: anyhow::Error) -> Self {
        let messages = err
            .chain()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>();
        match messages.split_last() {
            Some((root, contexts)) => contexts
                .iter()
                .rev()
                .fold(Self::RuntimeError(root.clone()), |err, context| {
                    err.context(context)
                }),
            None => Self::RuntimeError(String::new()),
        }
    }
}

struct Chain<'a> {
    root: &'a Error,
    current: Option<&'a (dyn StdError + 'static)>,
}

impl<'a> Iterator for Chain<'a> {
    type Item = &'a (dyn StdError + 'static);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let error: Option<&dyn StdError> = match self.current {
                None => {
                    self.current = Some(self.root);
                    self.current
                }
                Some(current) => match current.downcast_ref::<Error>()? {
                    Error::BadArgument { cause, .. }
                    | Error::CallbackError { cause, .. }
                    | Error::WithContext { cause, .. } => {
                        self.current = Some(&**cause);
                        self.current
                    }
                    Error::ExternalError(err) => {
                        self.current = Some(&**err);
                        self.current
                    }
                    _ => None,
                },
            };

            // Skip `ExternalError` as it only wraps the underlying error
            // without meaningful context
            if let Some(Error::ExternalError(_)) = error?.downcast_ref::<Error>() {
                continue;
            }

            return self.current;
        }
    }
}

#[cfg(test)]
mod assertions {
    static_assertions::assert_not_impl_any!(super::Error: Send, Sync);
}
