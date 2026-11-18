//! A value that must never be printed (design/STORES.md §3.2): tokens, codes, session blobs.
//!
//! [`Secret`] redacts itself in `Debug` and `Display`, so a struct holding one can derive
//! `Debug` and still be logged. Serde passes the value through unchanged, because the only
//! place a secret is serialized is its keyring entry.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    pub fn new(value: T) -> Self {
        Secret(value)
    }

    /// The value itself. Call sites are where a secret leaves Quiver (an HTTP header, a
    /// keyring write), so they are easy to find.
    pub fn expose(&self) -> &T {
        &self.0
    }

    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> From<T> for Secret<T> {
    fn from(v: T) -> Self {
        Secret(v)
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl<T: Serialize> Serialize for Secret<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Secret<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        T::deserialize(d).map(Secret)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacted_when_printed_kept_when_serialized() {
        #[derive(Debug, Serialize, Deserialize)]
        struct Tokens {
            account: String,
            refresh: Secret<String>,
        }
        let t = Tokens { account: "a1".into(), refresh: Secret::new("eg1~abc".into()) };
        let dbg = format!("{t:?} {}", t.refresh);
        assert!(!dbg.contains("eg1~abc") && dbg.contains("<redacted>"), "{dbg}");
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(json, r#"{"account":"a1","refresh":"eg1~abc"}"#);
        let back: Tokens = serde_json::from_str(&json).unwrap();
        assert_eq!(back.refresh.expose(), "eg1~abc");
    }
}
