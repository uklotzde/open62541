use std::slice;

use crate::ua;

// Technically, `open62541_sys::ByteString` is an alias for `open62541_sys::String`. But we treat it
// as a distinct type to improve type safety. The difference is that `String` contains valid Unicode
// whereas `ByteString` may contain arbitrary byte sequences.
crate::data_type!(ByteString);

// In the implementation below, remember that `self.0.data` may be `UA_EMPTY_ARRAY_SENTINEL` for any
// strings of `length` 0. It may also be `ptr::null()` for "invalid" strings. This is similar to how
// OPC UA treats arrays (which also distinguishes between empty and invalid instances).
impl ByteString {
    /// Checks if byte string is invalid.
    ///
    /// The invalid state is defined by OPC UA. It is a third state which is distinct from empty and
    /// regular (non-empty) byte strings.
    #[must_use]
    pub fn is_invalid(&self) -> bool {
        matches!(self.array_value(), ua::ArrayValue::Invalid)
    }

    /// Checks if byte string is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        matches!(self.array_value(), ua::ArrayValue::Empty)
    }

    /// Returns byte string contents as slice.
    ///
    /// This may return [`None`] when the byte string itself is invalid (as defined by OPC UA).
    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        // Internally, `open62541` represents strings as `Byte` array and has the same special cases
        // as regular arrays, i.e. empty and invalid states.
        match self.array_value() {
            ua::ArrayValue::Invalid => None,
            ua::ArrayValue::Empty => Some(&[]),
            ua::ArrayValue::Valid(data) => {
                // `self.0.data` is valid, so we may use `self.0.length` now.
                Some(unsafe { slice::from_raw_parts(data.as_ptr(), self.0.length) })
            }
        }
    }

    fn array_value(&self) -> ua::ArrayValue<u8> {
        // Internally, `open62541` represents strings as `Byte` array and has the same special cases
        // as regular arrays, i.e. empty and invalid states.
        ua::ArrayValue::from_ptr(self.0.data)
    }
}
