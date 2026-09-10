//! Demonstrates transport-free SML and SECS-II binary conversion.
//! This example runs with default features disabled and needs no Tokio runtime.

use secs_rust::{
    secs2::codec::{encode_to_vec, Secs2Decoder},
    sml::{parse_item, FormatStyle, SmlFormatter},
};

/// Parses an item, checks its binary round trip, and emits canonical SML.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let item = parse_item(r#"<L [2] <B [3] 0x01 0x02 0x03> <A [5] "HELLO">>"#)?;
    let encoded = encode_to_vec(&item)?;
    let decoded = Secs2Decoder::default().decode_item(&encoded)?;
    assert_eq!(decoded, item);
    println!("{} encoded bytes", encoded.len());
    println!(
        "{}",
        SmlFormatter::new(FormatStyle::Compact).format_item(&decoded)?
    );
    Ok(())
}
