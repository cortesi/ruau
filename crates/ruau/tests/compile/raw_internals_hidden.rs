fn main() {
    let _ = std::mem::size_of::<ruau::RawLuau>();
    let _ = std::mem::size_of::<ruau::ExtraData>();
    let _ = std::mem::size_of::<ruau::ValueRef>();
    let _ = std::mem::size_of::<ruau::Callback>();
    let _ = std::mem::size_of::<ruau::StackCtx<'static>>();
}
