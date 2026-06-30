![Discord](https://img.shields.io/discord/1381424110831145070?style=flat-square&logo=rust&link=https%3A%2F%2Fdiscord.gg%2FfHmRmuBDxF)
[![Crates.io](https://img.shields.io/crates/v/ruau)](https://crates.io/crates/ruau)
[![docs.rs](https://img.shields.io/docsrs/ruau)](https://docs.rs/ruau)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

# Ruau

Ruau is an experimental pure Rust implementation of
[Luau](https://github.com/luau-lang/luau), the gradually typed
[Lua](https://www.lua.org/)-derived language created by
[Roblox](https://www.roblox.com/). Ruau includes compatible parsing,
type-checking, and a byte-code compatible VM. It does not include the legacy
old type solver JIT, native code generation, breakpoint or debugger runtimes,
generic userdata or `newproxy` compatibility.

--

I'm evaluating how and whether to publish the fuzzing, unit test and upstream
tracking machinery.

