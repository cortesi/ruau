//! byte string integration tests.

use bstr::{BStr, BString};
use ruau::{Luau, Result};

#[cfg(test)]
mod tests {
    use super::*;

    const BYTE_STRING_CASES: &[(&str, &[u8])] = &[
        ("invalid_sequence_identifier", &[0xa0, 0xa1]),
        ("invalid_2_octet_sequence_2nd", &[0xc3, 0x28]),
        ("invalid_3_octet_sequence_2nd", &[0xe2, 0x28, 0xa1]),
        ("invalid_3_octet_sequence_3rd", &[0xe2, 0x82, 0x28]),
        ("invalid_4_octet_sequence_2nd", &[0xf0, 0x28, 0x8c, 0xbc]),
        ("invalid_4_octet_sequence_3rd", &[0xf0, 0x90, 0x28, 0xbc]),
        ("invalid_4_octet_sequence_4th", &[0xf0, 0x28, 0x8c, 0x28]),
        ("an_actual_string", b"Hello, world!"),
    ];

    async fn assert_globals_equal(lua: &Luau, left: &str, right: &str) -> Result<()> {
        let equal: bool = lua
            .load(
                r#"
            local left, right = ...
            return _G[left] == _G[right]
        "#,
            )
            .call((left, right))
            .await?;
        assert!(equal, "{left} != {right}");
        Ok(())
    }

    #[tokio::test]
    async fn test_byte_string_round_trip() -> Result<()> {
        let lua = Luau::new();

        lua.load(
            r#"
        invalid_sequence_identifier = "\160\161"
        invalid_2_octet_sequence_2nd = "\195\040"
        invalid_3_octet_sequence_2nd = "\226\040\161"
        invalid_3_octet_sequence_3rd = "\226\130\040"
        invalid_4_octet_sequence_2nd = "\240\040\140\188"
        invalid_4_octet_sequence_3rd = "\240\144\040\188"
        invalid_4_octet_sequence_4th = "\240\040\140\040"

        an_actual_string = "Hello, world!"
    "#,
        )
        .exec()
        .await?;

        let globals = lua.globals();

        for &(name, expected) in BYTE_STRING_CASES {
            let value = globals.get::<BString>(name)?;
            assert_eq!(&value[..], expected, "{name}");

            let bstr_name = format!("bstr_{name}");
            let value_ref: &BStr = value.as_ref();
            globals.set(bstr_name.as_str(), value_ref)?;
            assert_globals_equal(&lua, &bstr_name, name).await?;

            let bstring_name = format!("bstring_{name}");
            globals.set(bstring_name.as_str(), value)?;
            assert_globals_equal(&lua, &bstring_name, name).await?;
        }

        Ok(())
    }
}
