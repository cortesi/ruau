//! Supported host and native-module ABI for Ruau.
//!
//! This crate is the public ABI subset that can be mounted as `ruau::abi`.
//! It intentionally excludes the raw engine representation that still lives in
//! `ruau-vm-api`: persistent raw values, raw heap handles, engine unwinds,
//! GC-policy plumbing, and execution-feature switches.
//!
//! The borrow-view handle family (`Gc` plus marker kinds) is included because
//! [`HostValue`] exposes it in its variants. Treat those handles as borrowed
//! views supplied by the VM during a host callback, not as persistent engine
//! capabilities.

pub use ruau_vm_api::{
    Gc, HeapId, HostCall, HostContext, HostError, HostFunction, HostFuture, HostPayload,
    HostReturn, HostUnwind, HostValue, ModuleBinding, ModuleBuilder, ModuleExport, ModuleTable,
    ModuleTableEntry, ModuleValue, NativeModule, OwnedValue, RegistryRef, RuntimeErrorKind,
    ScriptErrorField, marker,
};
