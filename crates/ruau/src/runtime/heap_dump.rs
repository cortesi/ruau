use std::{collections::HashMap, hash::Hash, os::raw::c_char};

use serde_json::Value as Json;

use crate::state::ExtraData;

/// Represents a heap dump of a Luau memory state.
pub struct HeapDump {
    data: Json,
    buf: Box<str>,
}

impl HeapDump {
    /// Dumps the current Luau heap state.
    /// # Safety
    ///
    /// `state` must be a valid Luau state owned by a live VM.
    pub(crate) unsafe fn new(state: *mut ffi::lua_State) -> Option<Self> {
        /// # Safety
        ///
        /// Called by Luau as a category-name callback; `state` is owned by Luau for the
        /// dump duration.
        unsafe extern "C" fn category_name(state: *mut ffi::lua_State, cat: u8) -> *const c_char {
            (&*ExtraData::get(state))
                .mem_categories
                .get(cat as usize)
                .map(|s| s.as_ptr())
                .unwrap_or(cstr!("unknown"))
        }

        let mut buf = Vec::new();
        // SAFETY: tmpfile/fseek/ftell/rewind/fread/fclose form a self-contained libc round
        // trip; lua_gcdump writes the heap snapshot into the temp file. We only set buf.set_len
        // after fread succeeds, so any memory inside is initialised.
        unsafe {
            let file = libc::tmpfile();
            if file.is_null() {
                return None;
            }
            ffi::lua_gcdump(state, file as *mut _, Some(category_name));
            libc::fseek(file, 0, libc::SEEK_END);
            let len = libc::ftell(file) as usize;
            libc::rewind(file);
            if len > 0 {
                buf.reserve(len);
                libc::fread(buf.as_mut_ptr() as *mut _, 1, len, file);
                buf.set_len(len);
            }
            libc::fclose(file);
        }

        let buf = String::from_utf8(buf).ok()?.into_boxed_str();
        let data: Json = serde_json::from_str(&buf).ok()?;
        Some(Self { data, buf })
    }

    /// Returns the raw JSON representation of the heap dump.
    ///
    /// The JSON structure is an internal detail and may change in future versions.
    #[doc(hidden)]
    pub fn to_json(&self) -> &str {
        &self.buf
    }

    /// Returns the total size of the Luau heap in bytes.
    pub fn size(&self) -> u64 {
        self.data
            .get("stats")
            .and_then(|s| s.get("size"))
            .and_then(Json::as_u64)
            .unwrap_or_default()
    }

    /// Returns a mapping from object type to (count, total size in bytes).
    ///
    /// If `category` is provided, only objects in that category are considered.
    pub fn size_by_type<'a>(&'a self, category: Option<&str>) -> HashMap<&'a str, (usize, u64)> {
        self.size_by_type_inner(category).unwrap_or_default()
    }

    fn size_by_type_inner<'a>(
        &'a self,
        category: Option<&str>,
    ) -> Option<HashMap<&'a str, (usize, u64)>> {
        let category_id = match category {
            Some(cat) => Some(self.find_category_id(cat)?),
            None => None,
        };

        let mut size_by_type = HashMap::new();
        let objects = self.data.get("objects")?.as_object()?;
        for obj in objects.values() {
            if let Some(cat_id) = category_id
                && obj.get("cat").and_then(Json::as_i64)? != cat_id
            {
                continue;
            }
            let ty = obj.get("type")?.as_str()?;
            let sz = obj.get("size")?.as_u64()?;
            update_size(&mut size_by_type, ty, sz);
        }
        Some(size_by_type)
    }

    /// Returns a mapping from category name to total size in bytes.
    pub fn size_by_category(&self) -> HashMap<&str, u64> {
        let mut size_by_category = HashMap::new();
        if let Some(categories) = self
            .data
            .get("stats")
            .and_then(|s| s.get("categories"))
            .and_then(Json::as_object)
        {
            for cat in categories.values() {
                if let Some(cat_name) = cat.get("name").and_then(Json::as_str) {
                    size_by_category.insert(
                        cat_name,
                        cat.get("size").and_then(Json::as_u64).unwrap_or_default(),
                    );
                }
            }
        }
        size_by_category
    }

    /// Returns a mapping from userdata type to (count, total size in bytes).
    pub fn size_by_userdata<'a>(
        &'a self,
        category: Option<&str>,
    ) -> HashMap<&'a str, (usize, u64)> {
        self.size_by_userdata_inner(category).unwrap_or_default()
    }

    fn size_by_userdata_inner<'a>(
        &'a self,
        category: Option<&str>,
    ) -> Option<HashMap<&'a str, (usize, u64)>> {
        let category_id = match category {
            Some(cat) => Some(self.find_category_id(cat)?),
            None => None,
        };

        let mut size_by_userdata = HashMap::new();
        let objects = self.data.get("objects")?.as_object()?;
        for obj in objects.values() {
            if obj.get("type").and_then(Json::as_str) != Some("userdata") {
                continue;
            }
            if let Some(cat_id) = category_id
                && obj.get("cat").and_then(Json::as_i64)? != cat_id
            {
                continue;
            }

            // Determine userdata type from metatable
            let mut ud_type = "unknown";
            if let Some(metatable_addr) = obj.get("metatable").and_then(Json::as_str)
                && let Some(metatable_obj) = objects.get(metatable_addr)
                && let Some(t) = get_key(objects, metatable_obj, "__type")
            {
                ud_type = t;
            }
            let sz = obj.get("size")?.as_u64()?;
            update_size(&mut size_by_userdata, ud_type, sz);
        }
        Some(size_by_userdata)
    }

    /// Finds the category ID for a given category name.
    fn find_category_id(&self, category: &str) -> Option<i64> {
        let categories = self.data.get("stats")?.get("categories")?.as_object()?;
        for (cat_id, cat) in categories {
            if cat.get("name").and_then(Json::as_str) == Some(category) {
                return cat_id.parse().ok();
            }
        }
        None
    }
}

/// Updates the size mapping for a given key.
fn update_size<K: Eq + Hash>(size_type: &mut HashMap<K, (usize, u64)>, key: K, size: u64) {
    let (count, total_size) = size_type.entry(key).or_insert((0, 0));
    *count += 1;
    *total_size += size;
}

/// Retrieves the string value associated with a given `key` from the heap-dump representation
/// of a Luau table `tbl`.
fn get_key<'a>(
    objects: &'a serde_json::Map<String, Json>,
    tbl: &Json,
    key: &str,
) -> Option<&'a str> {
    let pairs = tbl.get("pairs")?.as_array()?;
    for kv in pairs.chunks_exact(2) {
        let (Some(key_addr), Some(val_addr)) = (kv[0].as_str(), kv[1].as_str()) else {
            continue;
        };
        let key_obj = objects.get(key_addr)?;
        if key_obj.get("type").and_then(Json::as_str) == Some("string")
            && key_obj.get("data").and_then(Json::as_str) == Some(key)
        {
            let val_obj = objects.get(val_addr)?;
            if val_obj.get("type").and_then(Json::as_str) == Some("string") {
                return val_obj.get("data").and_then(Json::as_str);
            }
            break;
        }
    }
    None
}
