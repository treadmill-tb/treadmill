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

/// Based on https://serde.rs/custom-date-format.html.
pub mod chrono {
    pub mod optional_duration {
        use chrono::TimeDelta;
        use serde::{Deserialize, Deserializer, Serializer};

        pub fn serialize<S>(duration: &Option<TimeDelta>, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            match duration.as_ref() {
                Some(duration) => serializer.serialize_some(
                    fundu::Duration::try_from(duration.clone())
                        .map_err(serde::ser::Error::custom)?
                        .to_string()
                        .as_str(),
                ),
                None => serializer.serialize_none(),
            }
        }
        pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<TimeDelta>, D::Error>
        where
            D: Deserializer<'de>,
        {
            Option::<&str>::deserialize(deserializer)?
                .map(|s| {
                    fundu::DurationParser::new()
                        .parse(s)
                        .map_err(serde::de::Error::custom)?
                        .try_into()
                        .map_err(serde::de::Error::custom)
                })
                .transpose()
        }
    }

    pub mod duration {
        use chrono::TimeDelta;
        use serde::{Deserialize, Deserializer, Serializer};

        pub fn serialize<S>(duration: &TimeDelta, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            serializer.serialize_str(
                fundu::Duration::try_from(duration.clone())
                    .map_err(serde::ser::Error::custom)?
                    .to_string()
                    .as_str(),
            )
        }

        pub fn deserialize<'de, D>(deserializer: D) -> Result<TimeDelta, D::Error>
        where
            D: Deserializer<'de>,
        {
            String::deserialize(deserializer)
                .map(|s| {
                    fundu::DurationParser::new()
                        .parse(&s)
                        .map_err(serde::de::Error::custom)?
                        .try_into()
                        .map_err(serde::de::Error::custom)
                })
                // same as Result::flatten, since that hasn't been stabilized
                .and_then(|x| x)
        }
    }
}
