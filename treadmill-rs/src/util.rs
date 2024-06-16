pub mod hex_slice {
    //! Wrapper type around a slice to print it as a lower-case hex
    //! string.
    use std::fmt;

    /// Wrapper type around a slice to print it as a lower-case hex
    /// string. Implements both [`Display`](fmt::Display) and
    /// [`Debug`](fmt::Debug).
    pub struct HexSlice<'a>(pub &'a [u8]);

    impl<'a> fmt::Debug for HexSlice<'a> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            for b in self.0.iter() {
                write!(f, "{:0>2x}", *b)?;
            }

            Ok(())
        }
    }

    impl<'a> fmt::Display for HexSlice<'a> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{:?}", self)
        }
    }
}
