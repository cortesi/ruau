//! Luau C API adapter helpers.
//!
//! These fill convenience gaps around Luau's raw headers so the safe wrapper can use a small,
//! consistent C API vocabulary internally.

use std::{
    ffi::CStr,
    mem,
    os::raw::{c_char, c_int, c_void},
    ptr,
};

use super::{lauxlib::*, lua::*, luacode::*};

pub const LUA_RESUMEERROR: c_int = -1;

unsafe fn reverse_stack_segment(L: *mut lua_State, mut a: c_int, mut b: c_int) {
    while a < b {
        lua_pushvalue(L, a);
        lua_pushvalue(L, b);
        lua_replace(L, a);
        lua_replace(L, b);
        a += 1;
        b -= 1;
    }
}

//
// lua ported functions
//

pub unsafe fn lua_rotate(L: *mut lua_State, mut idx: c_int, mut n: c_int) {
    idx = lua_absindex(L, idx);
    if n > 0 {
        // Faster version
        for _ in 0..n {
            lua_insert(L, idx);
        }
        return;
    }
    let n_elems = lua_gettop(L) - idx + 1;
    if n < 0 {
        n += n_elems;
    }
    if n > 0 && n < n_elems {
        luaL_checkstack(L, 2, cstr!("not enough stack slots available"));
        n = n_elems - n;
        reverse_stack_segment(L, idx, idx + n - 1);
        reverse_stack_segment(L, idx + n, idx + n_elems - 1);
        reverse_stack_segment(L, idx, idx + n_elems - 1);
    }
}

#[inline(always)]
pub unsafe fn lua_pushinteger(L: *mut lua_State, i: lua_Integer) {
    lua_pushnumber(L, i as lua_Number);
}

#[inline(always)]
pub unsafe fn lua_tointeger(L: *mut lua_State, i: c_int) -> lua_Integer {
    lua_tointegerx(L, i, ptr::null_mut())
}

pub unsafe fn lua_tointegerx(L: *mut lua_State, i: c_int, isnum: *mut c_int) -> lua_Integer {
    let mut ok = 0;
    let n = lua_tonumberx(L, i, &mut ok);
    let n_int = n as lua_Integer;
    if ok != 0 && (n - n_int as lua_Number).abs() < lua_Number::EPSILON {
        if !isnum.is_null() {
            *isnum = 1;
        }
        return n_int;
    }
    if !isnum.is_null() {
        *isnum = 0;
    }
    0
}

#[inline(always)]
pub unsafe fn lua_pushlstring(L: *mut lua_State, s: *const c_char, l: usize) -> *const c_char {
    if l == 0 {
        lua_pushlstring_(L, cstr!(""), 0);
    } else {
        lua_pushlstring_(L, s, l);
    }
    lua_tostring(L, -1)
}

#[inline(always)]
pub unsafe fn lua_pushstring(L: *mut lua_State, s: *const c_char) -> *const c_char {
    lua_pushstring_(L, s);
    lua_tostring(L, -1)
}

#[inline(always)]
pub unsafe fn lua_geti(L: *mut lua_State, mut idx: c_int, n: lua_Integer) -> c_int {
    idx = lua_absindex(L, idx);
    lua_pushinteger(L, n);
    lua_gettable(L, idx)
}

#[inline(always)]
pub unsafe fn lua_rawgeti(L: *mut lua_State, idx: c_int, n: lua_Integer) -> c_int {
    let n = n.try_into().expect("cannot convert index from lua_Integer");
    lua_rawgeti_(L, idx, n)
}

#[inline(always)]
pub unsafe fn lua_getuservalue(L: *mut lua_State, mut idx: c_int) -> c_int {
    luaL_checkstack(L, 2, cstr!("not enough stack slots available"));
    idx = lua_absindex(L, idx);
    lua_pushliteral(L, c"__ruau_uservalues");
    if lua_rawget(L, LUA_REGISTRYINDEX) != LUA_TTABLE {
        return LUA_TNIL;
    }
    lua_pushvalue(L, idx);
    lua_rawget(L, -2);
    lua_remove(L, -2);
    lua_type(L, -1)
}

#[inline(always)]
pub unsafe fn lua_seti(L: *mut lua_State, mut idx: c_int, n: lua_Integer) {
    luaL_checkstack(L, 1, cstr!("not enough stack slots available"));
    idx = lua_absindex(L, idx);
    lua_pushinteger(L, n);
    lua_insert(L, -2);
    lua_settable(L, idx);
}

#[inline(always)]
pub unsafe fn lua_rawseti(L: *mut lua_State, idx: c_int, n: lua_Integer) {
    let n = n.try_into().expect("cannot convert index from lua_Integer");
    lua_rawseti_(L, idx, n)
}

#[inline(always)]
pub unsafe fn lua_setuservalue(L: *mut lua_State, mut idx: c_int) {
    luaL_checkstack(L, 4, cstr!("not enough stack slots available"));
    idx = lua_absindex(L, idx);
    lua_pushliteral(L, c"__ruau_uservalues");
    lua_pushvalue(L, -1);
    if lua_rawget(L, LUA_REGISTRYINDEX) != LUA_TTABLE {
        lua_pop(L, 1);
        lua_createtable(L, 0, 2); // main table
        lua_createtable(L, 0, 1); // metatable
        lua_pushliteral(L, c"k");
        lua_setfield(L, -2, cstr!("__mode"));
        lua_setmetatable(L, -2);
        lua_pushvalue(L, -2);
        lua_pushvalue(L, -2);
        lua_rawset(L, LUA_REGISTRYINDEX);
    }
    lua_replace(L, -2);
    lua_pushvalue(L, idx);
    lua_pushvalue(L, -3);
    lua_remove(L, -4);
    lua_rawset(L, -3);
    lua_pop(L, 1);
}

#[inline(always)]
unsafe fn lua_len(L: *mut lua_State, idx: c_int) {
    match lua_type(L, idx) {
        LUA_TSTRING => {
            lua_pushnumber(L, lua_objlen(L, idx) as lua_Number);
        }
        LUA_TTABLE => {
            if luaL_callmeta(L, idx, cstr!("__len")) == 0 {
                lua_pushnumber(L, lua_objlen(L, idx) as lua_Number);
            }
        }
        LUA_TUSERDATA if luaL_callmeta(L, idx, cstr!("__len")) != 0 => {}
        _ => {
            luaL_error(
                L,
                cstr!("attempt to get length of a %s value"),
                lua_typename(L, lua_type(L, idx)),
            );
        }
    }
}

#[inline(always)]
pub unsafe fn lua_resumex(
    L: *mut lua_State,
    from: *mut lua_State,
    narg: c_int,
    nres: *mut c_int,
) -> c_int {
    let ret = if narg == LUA_RESUMEERROR {
        lua_resumeerror(L, from)
    } else {
        lua_resume_(L, from, narg)
    };
    if (ret == LUA_OK || ret == LUA_YIELD) && !(nres.is_null()) {
        *nres = lua_gettop(L);
    }
    ret
}

//
// lauxlib ported functions
//

#[inline(always)]
pub unsafe fn luaL_checkstack(L: *mut lua_State, sz: c_int, msg: *const c_char) {
    if lua_checkstack(L, sz + LUA_MINSTACK) == 0 {
        if !msg.is_null() {
            luaL_error(L, cstr!("stack overflow (%s)"), msg);
        } else {
            lua_pushliteral(L, c"stack overflow");
            lua_error(L);
        }
    }
}

#[inline(always)]
unsafe fn luaL_checkinteger(L: *mut lua_State, narg: c_int) -> lua_Integer {
    let mut isnum = 0;
    let int = lua_tointegerx(L, narg, &mut isnum);
    if isnum == 0 {
        luaL_typeerror(L, narg, lua_typename(L, LUA_TNUMBER));
    }
    int
}

pub unsafe fn luaL_optinteger(L: *mut lua_State, narg: c_int, def: lua_Integer) -> lua_Integer {
    if lua_isnoneornil(L, narg) != 0 {
        def
    } else {
        luaL_checkinteger(L, narg)
    }
}

#[inline(always)]
pub unsafe fn luaL_getmetafield(L: *mut lua_State, obj: c_int, e: *const c_char) -> c_int {
    if luaL_getmetafield_(L, obj, e) != 0 {
        lua_type(L, -1)
    } else {
        LUA_TNIL
    }
}

pub unsafe fn luaL_loadbufferenv(
    L: *mut lua_State,
    data: *const c_char,
    mut size: usize,
    name: *const c_char,
    mode: *const c_char,
    mut env: c_int,
) -> c_int {
    unsafe extern "C" {
        fn free(p: *mut c_void);
    }

    unsafe extern "C" fn data_dtor(_: *mut lua_State, data: *mut c_void) {
        free(*(data as *mut *mut c_char) as *mut c_void);
    }

    let chunk_is_text = size == 0 || (*data as u8) >= b'\t';
    if !mode.is_null() {
        let modeb = CStr::from_ptr(mode).to_bytes();
        if !chunk_is_text && !modeb.contains(&b'b') {
            lua_pushfstring(
                L,
                cstr!("attempt to load a binary chunk (mode is '%s')"),
                mode,
            );
            return LUA_ERRSYNTAX;
        } else if chunk_is_text && !modeb.contains(&b't') {
            lua_pushfstring(
                L,
                cstr!("attempt to load a text chunk (mode is '%s')"),
                mode,
            );
            return LUA_ERRSYNTAX;
        }
    }

    let status = if chunk_is_text {
        if env < 0 {
            env -= 1;
        }
        let data_ud =
            lua_newuserdatadtor(L, mem::size_of::<*mut c_char>(), data_dtor) as *mut *mut c_char;
        let data = luau_compile_(data, size, ptr::null_mut(), &mut size);
        ptr::write(data_ud, data);
        // By deferring the `free(data)` to the userdata destructor, we ensure that
        // even if `luau_load` throws an error, the `data` is still released.
        let status = luau_load(L, name, data, size, env);
        lua_replace(L, -2); // replace data with the result
        status
    } else {
        luau_load(L, name, data, size, env)
    };

    if status != 0 {
        if lua_isstring(L, -1) != 0 && CStr::from_ptr(lua_tostring(L, -1)) == c"not enough memory" {
            // A case for Luau >= 0.679
            return LUA_ERRMEM;
        }
        return LUA_ERRSYNTAX;
    }

    LUA_OK
}

#[inline(always)]
pub unsafe fn luaL_len(L: *mut lua_State, idx: c_int) -> lua_Integer {
    let mut isnum = 0;
    luaL_checkstack(L, 1, cstr!("not enough stack slots available"));
    lua_len(L, idx);
    let res = lua_tointegerx(L, -1, &mut isnum);
    lua_pop(L, 1);
    if isnum == 0 {
        luaL_error(L, cstr!("object length is not an integer"));
    }
    res
}

pub unsafe fn luaL_tolstring(L: *mut lua_State, mut idx: c_int, len: *mut usize) -> *const c_char {
    idx = lua_absindex(L, idx);
    if luaL_callmeta(L, idx, cstr!("__tostring")) == 0 {
        match lua_type(L, idx) {
            LUA_TNIL => {
                lua_pushliteral(L, c"nil");
            }
            LUA_TSTRING | LUA_TNUMBER => {
                lua_pushvalue(L, idx);
            }
            LUA_TBOOLEAN => {
                if lua_toboolean(L, idx) == 0 {
                    lua_pushliteral(L, c"false");
                } else {
                    lua_pushliteral(L, c"true");
                }
            }
            t => {
                let tt = luaL_getmetafield(L, idx, cstr!("__type"));
                let name = if tt == LUA_TSTRING {
                    lua_tostring(L, -1)
                } else {
                    lua_typename(L, t)
                };
                lua_pushfstring(L, cstr!("%s: %p"), name, lua_topointer(L, idx));
                if tt != LUA_TNIL {
                    lua_replace(L, -2); // remove '__type'
                }
            }
        };
    } else if lua_isstring(L, -1) == 0 {
        luaL_error(L, cstr!("'__tostring' must return a string"));
    }
    lua_tolstring(L, -1, len)
}

unsafe fn luaL_getsubtable(L: *mut lua_State, idx: c_int, fname: *const c_char) -> c_int {
    let abs_i = lua_absindex(L, idx);
    luaL_checkstack(L, 3, cstr!("not enough stack slots available"));
    lua_pushstring_(L, fname);
    if lua_gettable(L, abs_i) == LUA_TTABLE {
        return 1;
    }
    lua_pop(L, 1);
    lua_newtable(L);
    lua_pushstring_(L, fname);
    lua_pushvalue(L, -2);
    lua_settable(L, abs_i);
    0
}

pub unsafe fn luaL_requiref(
    L: *mut lua_State,
    modname: *const c_char,
    openf: lua_CFunction,
    glb: c_int,
) {
    luaL_checkstack(L, 3, cstr!("not enough stack slots available"));
    luaL_getsubtable(L, LUA_REGISTRYINDEX, LUA_LOADED_TABLE);
    if lua_getfield(L, -1, modname) == LUA_TNIL {
        lua_pop(L, 1);
        lua_pushcfunction(L, openf);
        lua_pushstring(L, modname);
        lua_call(L, 1, 1);
        lua_pushvalue(L, -1);
        lua_setfield(L, -3, modname);
    }
    if glb != 0 {
        lua_pushvalue(L, -1);
        lua_setglobal(L, modname);
    } else {
        lua_pushnil(L);
        lua_setglobal(L, modname);
    }
    lua_replace(L, -2);
}
