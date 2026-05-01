// The crate's unsafe helper layer must stay invisible to external consumers.
// If `mod util`, `mod state`, or `mod userdata_impl` ever become public, or
// if the helpers below get re-exported through the crate facade, this test
// catches it.

fn main() {
    let _ = ruau::util::push_string;
    let _ = ruau::util::check_stack;
    let _ = ruau::util::pop_error;
    let _ = ruau::util::protect_lua_call;
    let _ = ruau::state::callback_error_ext;
    let _ = ruau::userdata_impl::borrow_userdata_scoped::<u8, ()>;
}
