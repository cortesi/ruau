use std::{any::Any, os::raw::c_void, task::Waker};

use crate::types::{AsyncCallback, AsyncCallbackUpvalue, AsyncPollUpvalue, Callback, CallbackUpvalue};

pub trait TypeKey: Any {
    fn type_key() -> *const c_void;
}

impl TypeKey for String {
    #[inline(always)]
    fn type_key() -> *const c_void {
        static STRING_TYPE_KEY: u8 = 0;
        &STRING_TYPE_KEY as *const u8 as *const c_void
    }
}

impl TypeKey for Callback {
    #[inline(always)]
    fn type_key() -> *const c_void {
        static CALLBACK_TYPE_KEY: u8 = 0;
        &CALLBACK_TYPE_KEY as *const u8 as *const c_void
    }
}

impl TypeKey for CallbackUpvalue {
    #[inline(always)]
    fn type_key() -> *const c_void {
        static CALLBACK_UPVALUE_TYPE_KEY: u8 = 0;
        &CALLBACK_UPVALUE_TYPE_KEY as *const u8 as *const c_void
    }
}
impl TypeKey for AsyncCallback {
    #[inline(always)]
    fn type_key() -> *const c_void {
        static ASYNC_CALLBACK_TYPE_KEY: u8 = 0;
        &ASYNC_CALLBACK_TYPE_KEY as *const u8 as *const c_void
    }
}
impl TypeKey for AsyncCallbackUpvalue {
    #[inline(always)]
    fn type_key() -> *const c_void {
        static ASYNC_CALLBACK_UPVALUE_TYPE_KEY: u8 = 0;
        &ASYNC_CALLBACK_UPVALUE_TYPE_KEY as *const u8 as *const c_void
    }
}
impl TypeKey for AsyncPollUpvalue {
    #[inline(always)]
    fn type_key() -> *const c_void {
        static ASYNC_POLL_UPVALUE_TYPE_KEY: u8 = 0;
        &ASYNC_POLL_UPVALUE_TYPE_KEY as *const u8 as *const c_void
    }
}
impl TypeKey for Option<Waker> {
    #[inline(always)]
    fn type_key() -> *const c_void {
        static WAKER_TYPE_KEY: u8 = 0;
        &WAKER_TYPE_KEY as *const u8 as *const c_void
    }
}
