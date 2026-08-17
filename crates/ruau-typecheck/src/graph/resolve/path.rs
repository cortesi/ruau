//! Private path-string helpers retained for analysis tests.

/// Joins two path strings.
#[must_use]
pub(super) fn join_paths(lhs: &str, rhs: &str) -> String {
    let mut result = lhs.to_owned();
    if !result.is_empty() && !result.ends_with('/') && !result.ends_with('\\') {
        result.push('/');
    }
    result.push_str(rhs);
    result
}

/// Returns the parent path for a path string.
#[must_use]
pub(super) fn parent_path(path: &str) -> Option<String> {
    if matches!(path, "" | "." | "/") {
        return None;
    }

    #[cfg(windows)]
    if path.len() == 2 && path.ends_with(':') {
        return None;
    }

    let slash = path.rfind(['\\', '/']);
    match slash {
        Some(0) => Some("/".to_owned()),
        Some(index) => Some(path[..index].to_owned()),
        None => Some(String::new()),
    }
}

/// Splits a path on both slash spellings.
#[cfg(any())]
#[must_use]
pub(super) fn split_path(path: &str) -> Vec<&str> {
    path.split(['\\', '/']).collect()
}
