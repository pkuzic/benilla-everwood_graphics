//! Deserializers that accept what a Lua JSON encoder actually emits.
//!
//! Lua has one table type, so an encoder cannot tell an empty array from an empty object (`{}`
//! for `[]`), and Lua scripts commonly write flags as `0`/`1`. The reference server is Lua, so
//! the wire types accept both spellings; they always SERIALIZE the canonical form (`[]`, `true`).

use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use std::fmt;
use std::marker::PhantomData;

/// `true`/`false`, a number (non-zero = true), or `null` (= false).
pub(crate) fn bool_or_int<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = bool;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a boolean or 0/1")
        }
        fn visit_bool<E>(self, v: bool) -> Result<bool, E> {
            Ok(v)
        }
        fn visit_i64<E>(self, v: i64) -> Result<bool, E> {
            Ok(v != 0)
        }
        fn visit_u64<E>(self, v: u64) -> Result<bool, E> {
            Ok(v != 0)
        }
        fn visit_f64<E>(self, v: f64) -> Result<bool, E> {
            Ok(v != 0.0)
        }
        fn visit_unit<E>(self) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_none<E>(self) -> Result<bool, E> {
            Ok(false)
        }
    }
    d.deserialize_any(V)
}

/// A JSON array, or an EMPTY object standing for an empty array. A non-empty object is an error.
pub(crate) fn vec_or_empty_map<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct V<T>(PhantomData<T>);
    impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("an array (or an empty object)")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<T>, A::Error> {
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(4096));
            while let Some(item) = seq.next_element()? {
                out.push(item);
            }
            Ok(out)
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Vec<T>, A::Error> {
            if map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {
                return Err(de::Error::custom(
                    "expected an array, got a non-empty object",
                ));
            }
            Ok(Vec::new())
        }
        fn visit_unit<E>(self) -> Result<Vec<T>, E> {
            Ok(Vec::new())
        }
    }
    d.deserialize_any(V(PhantomData))
}

/// [`vec_or_empty_map`] for an optional field (`null` or absent = `None`).
pub(crate) fn opt_vec_or_empty_map<'de, D, T>(d: D) -> Result<Option<Vec<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct V<T>(PhantomData<T>);
    impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
        type Value = Option<Vec<T>>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("an array, an empty object or null")
        }
        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
            vec_or_empty_map(d).map(Some)
        }
    }
    d.deserialize_option(V(PhantomData))
}

/// MONKEY (EME projects): an optional flag: `true`/`false`/`0`/`1` → `Some`, `null` → `None`
/// (use with `#[serde(default)]` so an absent field is `None` too).
pub(crate) fn opt_bool_or_int<'de, D: Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    let v: Option<serde_json::Value> = Option::deserialize(d)?;
    Ok(match v {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Bool(b)) => Some(b),
        Some(serde_json::Value::Number(n)) => Some(n.as_f64().is_some_and(|f| f != 0.0)),
        Some(_) => return Err(de::Error::custom("a boolean or 0/1")),
    })
}
