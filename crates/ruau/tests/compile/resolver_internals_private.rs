fn main() {
    let _ = ruau::resolver::filesystem::FilesystemResolver::new(".");
    let _ = ruau::resolver::path_util::normalize_path(std::path::Path::new("."));
}
