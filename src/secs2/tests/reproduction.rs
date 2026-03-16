#[cfg(test)]
mod reproduction_test {
    use crate::secs2::encoder;
    use crate::secs2::types::Secs2;

    #[test]
    fn test_list_encoding_bug() {
        // Create a list with one item: I2 with value 10 (takes 2 bytes + 2 byte header = 4 bytes)
        // Encoded I2: [Format(I2)|Len(1)=2, 0x00, 0x0A] -> 4 bytes total?
        // I2 format code: 0x1A (001101 00) -> 0x69 (011010 01)
        // Header: 0x69 0x02 0x00 0x0A. Total 4 bytes.

        let inner = Secs2::I2(vec![10]);
        let list = Secs2::LIST(vec![inner]);

        // Encoder should produce:
        // List Header: Format(List)|Len(1) = 1 item.
        // List Format: 0x00 -> 0x01 (000000 01).
        // Header: 0x01 0x01.
        // Total: 0x01 0x01 0x69 0x02 0x00 0x0A.

        // Current Buggy Encoder likely produces:
        // List Length = 4 bytes (size of inner).
        // Header: 0x01 0x04.
        // Total: 0x01 0x04 0x69 0x02 0x00 0x0A.

        let encoded = encoder::encode(&list).unwrap();

        // Check list length byte (index 1)
        // Correct: 1
        // Buggy: 4
        assert_eq!(
            encoded[1], 1,
            "List length should be item count (1), but found {}",
            encoded[1]
        );
    }
}
