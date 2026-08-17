//! Apply two application policies to the same structural table layout.

use ruau::vm::{MarshaledPair, TableLayout, ValueSnapshot, classify_marshaled_table};

fn main() {
    let input = vec![
        MarshaledPair {
            key: ValueSnapshot::Integer(1),
            value: ValueSnapshot::String(b"north".to_vec()),
        },
        MarshaledPair {
            key: ValueSnapshot::Integer(3),
            value: ValueSnapshot::String(b"south".to_vec()),
        },
    ];
    let layout = classify_marshaled_table(&input);

    // A configuration loader can reject sparse input with precise detail.
    match &layout {
        TableLayout::Sparse { first_missing, .. } => {
            println!("config error: missing list item {first_missing}");
        }
        other => println!("config layout: {other:?}"),
    }

    // A patch protocol can accept the same structure as explicit keyed edits.
    match layout {
        TableLayout::Sparse { entries, .. } => println!("patch contains {entries} keyed edits"),
        other => println!("patch layout: {other:?}"),
    }
}
