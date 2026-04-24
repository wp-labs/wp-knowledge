use wp_model_core::model::DataField;

pub(crate) fn chars_field(name: impl Into<String>, value: impl Into<String>) -> DataField {
    DataField::from_chars(name.into(), value.into())
}

pub(crate) fn display_chars_field(name: impl Into<String>, value: impl ToString) -> DataField {
    chars_field(name, value.to_string())
}

pub(crate) fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(nibble_to_hex(byte >> 4));
        out.push(nibble_to_hex(byte & 0x0f));
    }
    out
}

pub(crate) fn bytes_to_prefixed_hex(bytes: &[u8]) -> String {
    format!("0x{}", bytes_to_hex(bytes))
}

fn nibble_to_hex(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        10..=15 => (b'a' + (nibble - 10)) as char,
        _ => unreachable!("nibble out of range"),
    }
}

#[cfg(test)]
mod tests {
    use super::{bytes_to_hex, bytes_to_prefixed_hex, chars_field, display_chars_field};

    #[test]
    fn bytes_to_hex_formats_lowercase_hex() {
        assert_eq!(bytes_to_hex(&[0x00, 0xff, 0x10, 0xaa]), "00ff10aa");
    }

    #[test]
    fn bytes_to_prefixed_hex_adds_0x_prefix() {
        assert_eq!(bytes_to_prefixed_hex(&[0x00, 0xff]), "0x00ff");
    }

    #[test]
    fn chars_field_wraps_string_value() {
        assert_eq!(chars_field("name", "value").to_string(), "chars(value)");
    }

    #[test]
    fn display_chars_field_formats_with_to_string() {
        assert_eq!(display_chars_field("name", 42).to_string(), "chars(42)");
    }
}
