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
            write!(f, "{self:?}")
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
                Some(duration) => {
                    serializer.serialize_some(fundu::Duration::from(*duration).to_string().as_str())
                }
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
            serializer.serialize_str(fundu::Duration::from(*duration).to_string().as_str())
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

/// A value that must not appear in logs.
///
/// The wrapper is transparent on the wire — these types exist to be sent, and
/// serialize as the bare inner value — and redacts only [`Debug`], which is
/// what tracing and `{:?}` reach for. There is deliberately no `Display`,
/// `Deref`, `AsRef` or `PartialEq`: every read is a visible
/// [`expose`](Self::expose), and no comparison silently picks up a
/// non-constant-time `==`.
#[derive(Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    pub fn new(value: T) -> Self {
        Secret(value)
    }

    pub fn expose(&self) -> &T {
        &self.0
    }

    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> std::fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

#[cfg(test)]
mod tests {
    use super::Secret;
    use crate::api::switchboard::AuthToken;

    #[test]
    fn debug_does_not_leak_the_value() {
        let secret = Secret::new("hunter2".to_string());
        assert!(!format!("{secret:?}").contains("hunter2"));

        let token = AuthToken([0xab; 32]);
        let rendered = format!("{token:?}");
        assert!(!rendered.contains("ab"));
        assert!(!rendered.contains(&token.encode_for_http()));
    }

    #[test]
    fn serde_is_transparent() {
        let secret = Secret::new("hunter2".to_string());
        assert_eq!(
            serde_json::to_string(&secret).unwrap(),
            serde_json::to_string("hunter2").unwrap()
        );
        assert_eq!(
            serde_json::from_str::<Secret<String>>("\"hunter2\"")
                .unwrap()
                .expose(),
            "hunter2"
        );
    }
}
