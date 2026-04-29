//! Re-exports most types with an extra `Luau*` prefix to prevent name clashes.

#[doc(no_inline)]
pub use crate::{
    AnyUserData as LuauAnyUserData, BorrowedBytes as LuauBorrowedBytes,
    BorrowedStr as LuauBorrowedStr, Either as LuauEither, Error as LuauError, FromLuau,
    FromLuauMulti, Function as LuauFunction, Integer as LuauInteger, IntoLuau, IntoLuauMulti,
    LightUserData as LuauLightUserData, Luau, LuauOptions, LuauString,
    MetaMethod as LuauMetaMethod, MultiValue as LuauMultiValue, Nil as LuauNil,
    Number as LuauNumber, ObjectLike as LuauObjectLike, RegistryKey as LuauRegistryKey,
    Result as LuauResult, StdLib as LuauStdLib, Table as LuauTable, Thread as LuauThread,
    UserData as LuauUserData, UserDataFields as LuauUserDataFields,
    UserDataMetatable as LuauUserDataMetatable, UserDataMethods as LuauUserDataMethods,
    UserDataOwned as LuauUserDataOwned, UserDataRef as LuauUserDataRef,
    UserDataRefMut as LuauUserDataRefMut, UserDataRegistry as LuauUserDataRegistry,
    Value as LuauValue, Variadic as LuauVariadic, VmState as LuauVmState, WeakLuau,
    chunk::AsChunk as AsLuauChunk, chunk::Chunk as LuauChunk, chunk::ChunkMode as LuauChunkMode,
    error::ErrorContext as LuauErrorContext, error::ExternalError as LuauExternalError,
    error::ExternalResult as LuauExternalResult, function::FunctionInfo as LuauFunctionInfo,
    function::LuauNativeFn, function::LuauNativeFnMut, state::GcIncParams as LuauGcIncParams,
    state::GcMode as LuauGcMode, table::TablePairs as LuauTablePairs,
    table::TableSequence as LuauTableSequence, thread::ThreadStatus as LuauThreadStatus,
};
#[cfg(feature = "serde")]
#[doc(no_inline)]
pub use crate::{
    DeserializeOptions as LuauDeserializeOptions, LuauSerdeExt,
    SerializableValue as LuauSerializableValue, SerializeOptions as LuauSerializeOptions,
};
#[doc(no_inline)]
pub use crate::{
    Vector as LuauVector,
    chunk::{CompileConstant as LuauCompileConstant, Compiler as LuauCompiler},
    luau::{
        FsRequirer as LuauFsRequirer, HeapDump as LuauHeapDump, NavigateError as LuauNavigateError,
        Require as LuauRequire,
    },
};
#[doc(no_inline)]
pub use crate::{function::LuauNativeAsyncFn, thread::AsyncThread as LuauAsyncThread};
