use std::{
    cell::RefCell, cmp::Ordering, collections::HashSet, fmt, os::raw::c_void, ptr, rc::Rc,
    result::Result as StdResult, str,
};

use num_traits::FromPrimitive;
use rustc_hash::FxHashSet;
use serde::ser::{self, Serialize, Serializer};

use crate::{
    error::{Error, Result},
    function::Function,
    string::LuauString,
    table::{SerializableTable, Table},
    thread::Thread,
    types::{Integer, LightUserData, Number, ValueRef},
    userdata::AnyUserData,
    util::{StackGuard, check_stack},
};

/// A dynamically typed Luau value.
///
/// The non-primitive variants (eg. string/table/function/thread/userdata) contain handle types
/// into the internal Luau state. It is a logic error to mix handle types between separate
/// `Luau` instances, and doing so will result in a panic.
#[derive(Clone, Default)]
pub enum Value {
    /// The Luau value `nil`.
    #[default]
    Nil,
    /// The Luau value `true` or `false`.
    Boolean(bool),
    /// A "light userdata" object, equivalent to a raw pointer.
    LightUserData(LightUserData),
    /// An integer number.
    ///
    /// Any Luau number convertible to a `Integer` will be represented as this variant.
    Integer(Integer),
    /// A floating point number.
    Number(Number),
    /// A Luau vector.
    Vector(crate::Vector),
    /// An interned string, managed by Luau.
    ///
    /// Unlike Rust strings, Luau strings may not be valid UTF-8.
    String(LuauString),
    /// Reference to a Luau table.
    Table(Table),
    /// Reference to a Luau function (or closure).
    Function(Function),
    /// Reference to a Luau thread (or coroutine).
    Thread(Thread),
    /// Reference to a userdata object that holds a custom type which implements `UserData`.
    ///
    /// Special builtin userdata types will be represented as other `Value` variants.
    UserData(AnyUserData),
    /// A Luau buffer.
    Buffer(crate::Buffer),
    /// `Error` is a special builtin userdata type. When received from Luau it is implicitly cloned.
    Error(Box<Error>),
    /// Any other value not known to ruau.
    Other(#[doc(hidden)] ValueRef),
}

pub use self::Value::Nil;

impl Value {
    /// A special value (lightuserdata) to represent null value.
    ///
    /// It can be used in Luau tables without downsides of `nil`.
    pub const NULL: Self = Self::LightUserData(LightUserData(ptr::null_mut()));

    /// Returns type name of this value.
    pub fn type_name(&self) -> &'static str {
        match *self {
            Self::Nil => "nil",
            Self::Boolean(_) => "boolean",
            Self::LightUserData(_) => "lightuserdata",
            Self::Integer(_) => "integer",
            Self::Number(_) => "number",
            Self::Vector(_) => "vector",
            Self::String(_) => "string",
            Self::Table(_) => "table",
            Self::Function(_) => "function",
            Self::Thread(_) => "thread",
            Self::UserData(_) => "userdata",
            Self::Buffer(_) => "buffer",
            Self::Error(_) => "error",
            Self::Other(_) => "other",
        }
    }

    /// Coerces this value into an interned Luau string in a manner consistent with Luau's
    /// internal behavior.
    ///
    /// Succeeds when this value is a string (no-op), an integer, or a number.
    pub fn coerce_string(&self, lua: &crate::state::Luau) -> Result<Option<LuauString>> {
        if let Self::String(s) = self {
            return Ok(Some(s.clone()));
        }
        let raw = lua.raw();
        let state = raw.state();
        unsafe {
            let _sg = StackGuard::new(state);
            check_stack(state, 4)?;
            raw.push_value(self)?;
            let res = if raw.unlikely_memory_error() {
                ffi::lua_tolstring(state, -1, ptr::null_mut())
            } else {
                protect_lua!(state, 1, 1, |state| {
                    ffi::lua_tolstring(state, -1, ptr::null_mut())
                })?
            };
            Ok(if res.is_null() {
                None
            } else {
                Some(LuauString(raw.pop_ref()))
            })
        }
    }

    /// Coerces this value into an integer in a manner consistent with Luau's internal behavior.
    ///
    /// Succeeds when this value is an integer, a floating-point number that is exactly
    /// representable as an integer, or a string that parses as one. See the Luau manual for
    /// details.
    pub fn coerce_integer(&self, lua: &crate::state::Luau) -> Result<Option<Integer>> {
        if let Self::Integer(i) = self {
            return Ok(Some(*i));
        }
        let raw = lua.raw();
        let state = raw.state();
        unsafe {
            let _sg = StackGuard::new(state);
            check_stack(state, 2)?;
            raw.push_value(self)?;
            let mut isint = 0;
            let i = ffi::lua_tointegerx(state, -1, &mut isint);
            Ok(if isint == 0 { None } else { Some(i) })
        }
    }

    /// Coerces this value into a number in a manner consistent with Luau's internal behavior.
    ///
    /// Succeeds when this value is a number or a string that parses as one. See the Luau manual
    /// for details.
    pub fn coerce_number(&self, lua: &crate::state::Luau) -> Result<Option<Number>> {
        if let Self::Number(n) = self {
            return Ok(Some(*n));
        }
        let raw = lua.raw();
        let state = raw.state();
        unsafe {
            let _sg = StackGuard::new(state);
            check_stack(state, 2)?;
            raw.push_value(self)?;
            let mut isnum = 0;
            let n = ffi::lua_tonumberx(state, -1, &mut isnum);
            Ok(if isnum == 0 { None } else { Some(n) })
        }
    }

    /// Compares two values for equality.
    ///
    /// Equality comparisons do not convert strings to numbers or vice versa.
    /// Tables, functions, threads, and userdata are compared by reference:
    /// two objects are considered equal only if they are the same object.
    ///
    /// If table or userdata have `__eq` metamethod then ruau will try to invoke it.
    /// The first value is checked first. If that value does not define a metamethod
    /// for `__eq`, then ruau will check the second value.
    /// Then ruau calls the metamethod with the two values as arguments, if found.
    pub fn equals(&self, other: &Self) -> Result<bool> {
        match (self, other) {
            (Self::Table(a), Self::Table(b)) => a.equals(b),
            (Self::UserData(a), Self::UserData(b)) => a.equals(b),
            (a, b) => Ok(a == b),
        }
    }

    /// Converts the value to a generic C pointer.
    ///
    /// The value can be a userdata, a table, a thread, a string, or a function; otherwise it
    /// returns NULL. Different objects will give different pointers.
    /// There is no way to convert the pointer back to its original value.
    ///
    /// Typically this function is used only for hashing and debug information.
    #[inline]
    pub fn to_pointer(&self) -> *const c_void {
        match self {
            Self::LightUserData(ud) => ud.0,
            Self::Table(Table(vref))
            | Self::Function(Function(vref))
            | Self::Thread(Thread(vref, ..))
            | Self::UserData(AnyUserData(vref))
            | Self::Other(vref) => vref.to_pointer(),
            Self::String(s) => s.to_pointer(),
            Self::Buffer(crate::Buffer(vref)) => vref.to_pointer(),
            _ => ptr::null(),
        }
    }

    /// Converts the value to a string.
    ///
    /// This might invoke the `__tostring` metamethod for non-primitive types (eg. tables,
    /// functions).
    pub fn to_string(&self) -> Result<String> {
        unsafe fn invoke_tostring(vref: &ValueRef) -> Result<String> {
            let lua = vref.lua.raw();
            let state = lua.state();
            let _guard = StackGuard::new(state);
            check_stack(state, 3)?;

            lua.push_ref(vref);
            protect_lua!(state, 1, 1, fn(state) {
                ffi::luaL_tolstring(state, -1, ptr::null_mut());
            })?;
            Ok(LuauString(lua.pop_ref()).to_str()?.to_string())
        }

        match self {
            Self::Nil => Ok("nil".to_string()),
            Self::Boolean(b) => Ok(b.to_string()),
            Self::LightUserData(ud) if ud.0.is_null() => Ok("null".to_string()),
            Self::LightUserData(ud) => Ok(format!("lightuserdata: {:p}", ud.0)),
            Self::Integer(i) => Ok(i.to_string()),
            Self::Number(n) => Ok(n.to_string()),
            Self::Vector(v) => Ok(v.to_string()),
            Self::String(s) => Ok(s.to_str()?.to_string()),
            Self::Table(Table(vref))
            | Self::Function(Function(vref))
            | Self::Thread(Thread(vref, ..))
            | Self::UserData(AnyUserData(vref))
            | Self::Other(vref) => unsafe { invoke_tostring(vref) },
            Self::Buffer(crate::Buffer(vref)) => unsafe { invoke_tostring(vref) },
            Self::Error(err) => Ok(err.to_string()),
        }
    }

    /// Returns `true` if the value is a [`Nil`].
    #[inline]
    pub fn is_nil(&self) -> bool {
        self == &Nil
    }

    /// Returns `true` if the value is a [`NULL`].
    ///
    /// [`NULL`]: Value::NULL
    #[inline]
    pub fn is_null(&self) -> bool {
        self == &Self::NULL
    }

    /// Returns `true` if the value is a boolean.
    #[inline]
    pub fn is_boolean(&self) -> bool {
        self.as_boolean().is_some()
    }

    /// Cast the value to boolean.
    ///
    /// If the value is a Boolean, returns it or `None` otherwise.
    #[inline]
    pub fn as_boolean(&self) -> Option<bool> {
        match *self {
            Self::Boolean(b) => Some(b),
            _ => None,
        }
    }

    /// Returns `true` if the value is a [`LightUserData`].
    #[inline]
    pub fn is_light_userdata(&self) -> bool {
        self.as_light_userdata().is_some()
    }

    /// Cast the value to [`LightUserData`].
    ///
    /// If the value is a [`LightUserData`], returns it or `None` otherwise.
    #[inline]
    pub fn as_light_userdata(&self) -> Option<LightUserData> {
        match *self {
            Self::LightUserData(l) => Some(l),
            _ => None,
        }
    }

    /// Returns `true` if the value is an [`Integer`].
    #[inline]
    pub fn is_integer(&self) -> bool {
        self.as_integer().is_some()
    }

    /// Cast the value to [`Integer`].
    ///
    /// If the value is a Luau [`Integer`], returns it or `None` otherwise.
    #[inline]
    pub fn as_integer(&self) -> Option<Integer> {
        match *self {
            Self::Integer(i) => Some(i),
            _ => None,
        }
    }

    /// Cast the value to `i32`.
    ///
    /// If the value is a Luau [`Integer`], try to convert it to `i32` or return `None` otherwise.
    #[inline]
    pub fn as_i32(&self) -> Option<i32> {
        #[allow(clippy::useless_conversion)]
        self.as_integer().and_then(|i| i32::try_from(i).ok())
    }

    /// Cast the value to `u32`.
    ///
    /// If the value is a Luau [`Integer`], try to convert it to `u32` or return `None` otherwise.
    #[inline]
    pub fn as_u32(&self) -> Option<u32> {
        self.as_integer().and_then(|i| u32::try_from(i).ok())
    }

    /// Cast the value to `i64`.
    ///
    /// If the value is a Luau [`Integer`], try to convert it to `i64` or return `None` otherwise.
    #[inline]
    pub fn as_i64(&self) -> Option<i64> {
        #[cfg(target_pointer_width = "64")]
        return self.as_integer();
        #[cfg(not(target_pointer_width = "64"))]
        return self.as_integer().map(i64::from);
    }

    /// Cast the value to `u64`.
    ///
    /// If the value is a Luau [`Integer`], try to convert it to `u64` or return `None` otherwise.
    #[inline]
    pub fn as_u64(&self) -> Option<u64> {
        self.as_integer().and_then(|i| u64::try_from(i).ok())
    }

    /// Cast the value to `isize`.
    ///
    /// If the value is a Luau [`Integer`], try to convert it to `isize` or return `None` otherwise.
    #[inline]
    pub fn as_isize(&self) -> Option<isize> {
        self.as_integer().and_then(|i| isize::try_from(i).ok())
    }

    /// Cast the value to `usize`.
    ///
    /// If the value is a Luau [`Integer`], try to convert it to `usize` or return `None` otherwise.
    #[inline]
    pub fn as_usize(&self) -> Option<usize> {
        self.as_integer().and_then(|i| usize::try_from(i).ok())
    }

    /// Returns `true` if the value is a Luau [`Number`].
    #[inline]
    pub fn is_number(&self) -> bool {
        self.as_number().is_some()
    }

    /// Cast the value to [`Number`].
    ///
    /// If the value is a Luau [`Number`], returns it or `None` otherwise.
    #[inline]
    pub fn as_number(&self) -> Option<Number> {
        match *self {
            Self::Number(n) => Some(n),
            _ => None,
        }
    }

    /// Cast the value to `f32`.
    ///
    /// If the value is a Luau [`Number`], try to convert it to `f32` or return `None` otherwise.
    #[inline]
    pub fn as_f32(&self) -> Option<f32> {
        self.as_number().and_then(f32::from_f64)
    }

    /// Cast the value to `f64`.
    ///
    /// If the value is a Luau [`Number`], try to convert it to `f64` or return `None` otherwise.
    #[inline]
    pub fn as_f64(&self) -> Option<f64> {
        self.as_number()
    }

    /// Returns `true` if the value is a [`LuauString`].
    #[inline]
    pub fn is_string(&self) -> bool {
        self.as_string().is_some()
    }

    /// Cast the value to a [`LuauString`].
    ///
    /// If the value is a [`LuauString`], returns it or `None` otherwise.
    #[inline]
    pub fn as_string(&self) -> Option<&LuauString> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    /// Returns `true` if the value is a Luau [`Table`].
    #[inline]
    pub fn is_table(&self) -> bool {
        self.as_table().is_some()
    }

    /// Cast the value to [`Table`].
    ///
    /// If the value is a Luau [`Table`], returns it or `None` otherwise.
    #[inline]
    pub fn as_table(&self) -> Option<&Table> {
        match self {
            Self::Table(t) => Some(t),
            _ => None,
        }
    }

    /// Returns `true` if the value is a Luau [`Thread`].
    #[inline]
    pub fn is_thread(&self) -> bool {
        self.as_thread().is_some()
    }

    /// Cast the value to [`Thread`].
    ///
    /// If the value is a Luau [`Thread`], returns it or `None` otherwise.
    #[inline]
    pub fn as_thread(&self) -> Option<&Thread> {
        match self {
            Self::Thread(t) => Some(t),
            _ => None,
        }
    }

    /// Returns `true` if the value is a Luau [`Function`].
    #[inline]
    pub fn is_function(&self) -> bool {
        self.as_function().is_some()
    }

    /// Cast the value to [`Function`].
    ///
    /// If the value is a Luau [`Function`], returns it or `None` otherwise.
    #[inline]
    pub fn as_function(&self) -> Option<&Function> {
        match self {
            Self::Function(f) => Some(f),
            _ => None,
        }
    }

    /// Returns `true` if the value is an [`AnyUserData`].
    #[inline]
    pub fn is_userdata(&self) -> bool {
        self.as_userdata().is_some()
    }

    /// Cast the value to [`AnyUserData`].
    ///
    /// If the value is an [`AnyUserData`], returns it or `None` otherwise.
    #[inline]
    pub fn as_userdata(&self) -> Option<&AnyUserData> {
        match self {
            Self::UserData(ud) => Some(ud),
            _ => None,
        }
    }

    /// Cast the value to a [`Buffer`].
    ///
    /// If the value is [`Buffer`], returns it or `None` otherwise.
    ///
    /// [`Buffer`]: crate::Buffer
    #[inline]
    pub fn as_buffer(&self) -> Option<&crate::Buffer> {
        match self {
            Self::Buffer(b) => Some(b),
            _ => None,
        }
    }

    /// Returns `true` if the value is a [`Buffer`].
    ///
    /// [`Buffer`]: crate::Buffer
    #[inline]
    pub fn is_buffer(&self) -> bool {
        self.as_buffer().is_some()
    }

    /// Returns `true` if the value is an [`Error`].
    #[inline]
    pub fn is_error(&self) -> bool {
        self.as_error().is_some()
    }

    /// Cast the value to [`Error`].
    ///
    /// If the value is an [`Error`], returns it or `None` otherwise.
    pub fn as_error(&self) -> Option<&Error> {
        match self {
            Self::Error(e) => Some(e),
            _ => None,
        }
    }

    /// Wrap reference to this Value into [`SerializableValue`].
    ///
    /// This allows customizing serialization behavior using serde.
    pub fn to_serializable(&self) -> SerializableValue<'_> {
        SerializableValue::new(self, Default::default(), None)
    }

    // Compares two values.
    // Used to sort values for Debug printing.
    pub(crate) fn sort_cmp(&self, other: &Self) -> Ordering {
        fn cmp_num(a: Number, b: Number) -> Ordering {
            match (a, b) {
                _ if a < b => Ordering::Less,
                _ if a > b => Ordering::Greater,
                _ => Ordering::Equal,
            }
        }

        match (self, other) {
            // Nil
            (Self::Nil, Self::Nil) => Ordering::Equal,
            (Self::Nil, _) => Ordering::Less,
            (_, Self::Nil) => Ordering::Greater,
            // Null (a special case)
            (Self::LightUserData(ud1), Self::LightUserData(ud2)) if ud1 == ud2 => Ordering::Equal,
            (Self::LightUserData(ud1), _) if ud1.0.is_null() => Ordering::Less,
            (_, Self::LightUserData(ud2)) if ud2.0.is_null() => Ordering::Greater,
            // Boolean
            (Self::Boolean(a), Self::Boolean(b)) => a.cmp(b),
            (Self::Boolean(_), _) => Ordering::Less,
            (_, Self::Boolean(_)) => Ordering::Greater,
            // Integer && Number
            (Self::Integer(a), Self::Integer(b)) => a.cmp(b),
            (Self::Integer(a), Self::Number(b)) => cmp_num(*a as Number, *b),
            (Self::Number(a), Self::Integer(b)) => cmp_num(*a, *b as Number),
            (Self::Number(a), Self::Number(b)) => cmp_num(*a, *b),
            (Self::Integer(_) | Self::Number(_), _) => Ordering::Less,
            (_, Self::Integer(_) | Self::Number(_)) => Ordering::Greater,
            // Vector (Luau)
            (Self::Vector(a), Self::Vector(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
            // String
            (Self::String(a), Self::String(b)) => a.as_bytes().cmp(&b.as_bytes()),
            (Self::String(_), _) => Ordering::Less,
            (_, Self::String(_)) => Ordering::Greater,
            // Other variants can be ordered by their pointer
            (a, b) => a.to_pointer().cmp(&b.to_pointer()),
        }
    }

    pub(crate) fn fmt_pretty(
        &self,
        fmt: &mut fmt::Formatter,
        recursive: bool,
        ident: usize,
        visited: &mut HashSet<*const c_void>,
    ) -> fmt::Result {
        match self {
            Self::Nil => write!(fmt, "nil"),
            Self::Boolean(b) => write!(fmt, "{b}"),
            Self::LightUserData(ud) if ud.0.is_null() => write!(fmt, "null"),
            Self::LightUserData(ud) => write!(fmt, "lightuserdata: {:?}", ud.0),
            Self::Integer(i) => write!(fmt, "{i}"),
            Self::Number(n) => write!(fmt, "{n}"),
            Self::Vector(v) => write!(fmt, "{v}"),
            Self::String(s) => write!(fmt, "{s:?}"),
            Self::Table(t) if recursive && !visited.contains(&t.to_pointer()) => {
                t.fmt_pretty(fmt, ident, visited)
            }
            t @ Self::Table(_) => write!(fmt, "table: {:?}", t.to_pointer()),
            f @ Self::Function(_) => write!(fmt, "function: {:?}", f.to_pointer()),
            t @ Self::Thread(_) => write!(fmt, "thread: {:?}", t.to_pointer()),
            Self::UserData(ud) => ud.fmt_pretty(fmt),
            buf @ Self::Buffer(_) => write!(fmt, "buffer: {:?}", buf.to_pointer()),
            Self::Error(e) if recursive => write!(fmt, "{e:?}"),
            Self::Error(_) => write!(fmt, "error"),
            Self::Other(v) => write!(fmt, "other: {:?}", v.to_pointer()),
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        if fmt.alternate() {
            return self.fmt_pretty(fmt, true, 0, &mut HashSet::new());
        }

        match self {
            Self::Nil => write!(fmt, "Nil"),
            Self::Boolean(b) => write!(fmt, "Boolean({b})"),
            Self::LightUserData(ud) => write!(fmt, "{ud:?}"),
            Self::Integer(i) => write!(fmt, "Integer({i})"),
            Self::Number(n) => write!(fmt, "Number({n})"),
            Self::Vector(v) => write!(fmt, "{v:?}"),
            Self::String(s) => write!(fmt, "String({s:?})"),
            Self::Table(t) => write!(fmt, "{t:?}"),
            Self::Function(f) => write!(fmt, "{f:?}"),
            Self::Thread(t) => write!(fmt, "{t:?}"),
            Self::UserData(ud) => write!(fmt, "{ud:?}"),
            Self::Buffer(buf) => write!(fmt, "{buf:?}"),
            Self::Error(e) => write!(fmt, "Error({e:?})"),
            Self::Other(v) => write!(fmt, "Other({v:?})"),
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Nil, Self::Nil) => true,
            (Self::Boolean(a), Self::Boolean(b)) => a == b,
            (Self::LightUserData(a), Self::LightUserData(b)) => a == b,
            (Self::Integer(a), Self::Integer(b)) => *a == *b,
            (Self::Integer(a), Self::Number(b)) => *a as Number == *b,
            (Self::Number(a), Self::Integer(b)) => *a == *b as Number,
            (Self::Number(a), Self::Number(b)) => *a == *b,
            (Self::Vector(v1), Self::Vector(v2)) => v1 == v2,
            (Self::String(a), Self::String(b)) => a == b,
            (Self::Table(a), Self::Table(b)) => a == b,
            (Self::Function(a), Self::Function(b)) => a == b,
            (Self::Thread(a), Self::Thread(b)) => a == b,
            (Self::UserData(a), Self::UserData(b)) => a == b,
            (Self::Buffer(a), Self::Buffer(b)) => a == b,
            _ => false,
        }
    }
}

/// A wrapped [`Value`] with customized serialization behavior.
pub struct SerializableValue<'a> {
    value: &'a Value,
    options: crate::serde::de::DeserializeOptions,
    // In many cases we don't need `visited` map, so don't allocate memory by default
    visited: Option<Rc<RefCell<FxHashSet<*const c_void>>>>,
}

impl Serialize for Value {
    #[inline]
    fn serialize<S: Serializer>(&self, serializer: S) -> StdResult<S::Ok, S::Error> {
        SerializableValue::new(self, Default::default(), None).serialize(serializer)
    }
}

impl<'a> SerializableValue<'a> {
    #[inline]
    pub(crate) fn new(
        value: &'a Value,
        options: crate::serde::de::DeserializeOptions,
        visited: Option<&Rc<RefCell<FxHashSet<*const c_void>>>>,
    ) -> Self {
        if let Value::Table(_) = value {
            return Self {
                value,
                options,
                // We need to always initialize the `visited` map for Tables
                visited: visited.cloned().or_else(|| Some(Default::default())),
            };
        }
        Self {
            value,
            options,
            visited: None,
        }
    }

    /// If true, an attempt to serialize types such as [`Function`], [`Thread`], [`LightUserData`]
    /// and [`Error`] will cause an error.
    /// Otherwise these types skipped when iterating or serialized as unit type.
    ///
    /// Default: **true**
    #[must_use]
    pub fn deny_unsupported_types(mut self, enabled: bool) -> Self {
        self.options.deny_unsupported_types = enabled;
        self
    }

    /// If true, an attempt to serialize a recursive table (table that refers to itself)
    /// will cause an error.
    /// Otherwise subsequent attempts to serialize the same table will be ignored.
    ///
    /// Default: **true**
    #[must_use]
    pub fn deny_recursive_tables(mut self, enabled: bool) -> Self {
        self.options.deny_recursive_tables = enabled;
        self
    }

    /// If true, keys in tables will be iterated (and serialized) in sorted order.
    ///
    /// Default: **false**
    #[must_use]
    pub fn sort_keys(mut self, enabled: bool) -> Self {
        self.options.sort_keys = enabled;
        self
    }

    /// If true, empty Luau tables will be encoded as array, instead of map.
    ///
    /// Default: **false**
    #[must_use]
    pub fn encode_empty_tables_as_array(mut self, enabled: bool) -> Self {
        self.options.encode_empty_tables_as_array = enabled;
        self
    }

    /// If true, enable detection of mixed tables.
    ///
    /// A mixed table is a table that has both array-like and map-like entries or several borders.
    ///
    /// Default: **false**
    #[must_use]
    pub fn detect_mixed_tables(mut self, enabled: bool) -> Self {
        self.options.detect_mixed_tables = enabled;
        self
    }
}

impl Serialize for SerializableValue<'_> {
    fn serialize<S>(&self, serializer: S) -> StdResult<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self.value {
            Value::Nil => serializer.serialize_unit(),
            Value::Boolean(b) => serializer.serialize_bool(*b),
            #[allow(clippy::useless_conversion)]
            Value::Integer(i) => serializer.serialize_i64((*i).into()),
            Value::Number(n) => serializer.serialize_f64(*n),
            Value::Vector(v) => v.serialize(serializer),
            Value::String(s) => s.serialize(serializer),
            Value::Table(t) => {
                let visited = self.visited.as_ref().unwrap().clone();
                SerializableTable::new(t, self.options, visited).serialize(serializer)
            }
            Value::LightUserData(ud) if ud.0.is_null() => serializer.serialize_none(),
            Value::UserData(ud) if ud.is_serializable() || self.options.deny_unsupported_types => {
                ud.serialize(serializer)
            }
            Value::Buffer(buf) => buf.serialize(serializer),
            Value::Function(_)
            | Value::Thread(_)
            | Value::UserData(_)
            | Value::LightUserData(_)
            | Value::Error(_)
            | Value::Other(_) => {
                if self.options.deny_unsupported_types {
                    let msg = format!("cannot serialize <{}>", self.value.type_name());
                    Err(ser::Error::custom(msg))
                } else {
                    serializer.serialize_unit()
                }
            }
        }
    }
}

#[cfg(test)]
mod assertions {
    use super::*;

    static_assertions::assert_not_impl_any!(Value: Send);
}
