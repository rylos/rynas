//! A pass-through around serde_json's deserializer that reads structs as JSON objects only.
//!
//! serde_json reads a struct from an object or from an array, so every derived `Deserialize`
//! keeps two copies of its field code (`visit_map` and `visit_seq`). The API only sends
//! objects: with structs asked as maps the array copy is never reached and the linker drops
//! it from each of the ~400 API types. Everything else (sequences, tuples, enums, options,
//! `deserialize_any` for untagged and flattened types) goes to serde_json unchanged; the
//! wrappers only carry themselves into nested values.

use serde::de::{self, DeserializeSeed, Deserializer, EnumAccess, MapAccess, SeqAccess, VariantAccess, Visitor};
use std::fmt;

/// The deserializer: `T::deserialize(Objects(&mut json))`.
pub struct Objects<D>(pub D);

struct Wrap<V>(V);
struct Seed<S>(S);
struct Map<A>(A);
struct Seq<A>(A);
struct Enum<A>(A);
struct Variant<A>(A);

macro_rules! forward {
    ($($m:ident)*) => {$(
        fn $m<V: Visitor<'de>>(self, v: V) -> Result<V::Value, D::Error> {
            self.0.$m(Wrap(v))
        }
    )*};
}

impl<'de, D: Deserializer<'de>> Deserializer<'de> for Objects<D> {
    type Error = D::Error;

    forward! {
        deserialize_any deserialize_bool deserialize_i8 deserialize_i16 deserialize_i32 deserialize_i64
        deserialize_i128 deserialize_u8 deserialize_u16 deserialize_u32 deserialize_u64 deserialize_u128
        deserialize_f32 deserialize_f64 deserialize_char deserialize_str deserialize_string
        deserialize_bytes deserialize_byte_buf deserialize_option deserialize_unit deserialize_seq
        deserialize_map deserialize_identifier deserialize_ignored_any
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(self, name: &'static str, v: V) -> Result<V::Value, D::Error> {
        self.0.deserialize_unit_struct(name, Wrap(v))
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(self, name: &'static str, v: V) -> Result<V::Value, D::Error> {
        self.0.deserialize_newtype_struct(name, Wrap(v))
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, len: usize, v: V) -> Result<V::Value, D::Error> {
        self.0.deserialize_tuple(len, Wrap(v))
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(self, name: &'static str, len: usize, v: V) -> Result<V::Value, D::Error> {
        self.0.deserialize_tuple_struct(name, len, Wrap(v))
    }

    /// The point of the module: a struct is read as a map, never as an array.
    fn deserialize_struct<V: Visitor<'de>>(self, _name: &'static str, _fields: &'static [&'static str], v: V) -> Result<V::Value, D::Error> {
        self.0.deserialize_map(Wrap(v))
    }

    fn deserialize_enum<V: Visitor<'de>>(self, name: &'static str, variants: &'static [&'static str], v: V) -> Result<V::Value, D::Error> {
        self.0.deserialize_enum(name, variants, Wrap(v))
    }

    fn is_human_readable(&self) -> bool {
        self.0.is_human_readable()
    }
}

macro_rules! visit {
    ($($m:ident: $t:ty)*) => {$(
        fn $m<E: de::Error>(self, x: $t) -> Result<V::Value, E> {
            self.0.$m(x)
        }
    )*};
}

impl<'de, V: Visitor<'de>> Visitor<'de> for Wrap<V> {
    type Value = V::Value;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.expecting(f)
    }

    visit! {
        visit_bool: bool visit_i8: i8 visit_i16: i16 visit_i32: i32 visit_i64: i64 visit_i128: i128
        visit_u8: u8 visit_u16: u16 visit_u32: u32 visit_u64: u64 visit_u128: u128
        visit_f32: f32 visit_f64: f64 visit_char: char visit_str: &str visit_borrowed_str: &'de str
        visit_string: String visit_bytes: &[u8] visit_borrowed_bytes: &'de [u8] visit_byte_buf: Vec<u8>
    }

    fn visit_none<E: de::Error>(self) -> Result<V::Value, E> {
        self.0.visit_none()
    }

    fn visit_unit<E: de::Error>(self) -> Result<V::Value, E> {
        self.0.visit_unit()
    }

    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<V::Value, D::Error> {
        self.0.visit_some(Objects(d))
    }

    fn visit_newtype_struct<D: Deserializer<'de>>(self, d: D) -> Result<V::Value, D::Error> {
        self.0.visit_newtype_struct(Objects(d))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, a: A) -> Result<V::Value, A::Error> {
        self.0.visit_seq(Seq(a))
    }

    fn visit_map<A: MapAccess<'de>>(self, a: A) -> Result<V::Value, A::Error> {
        self.0.visit_map(Map(a))
    }

    fn visit_enum<A: EnumAccess<'de>>(self, a: A) -> Result<V::Value, A::Error> {
        self.0.visit_enum(Enum(a))
    }
}

impl<'de, S: DeserializeSeed<'de>> DeserializeSeed<'de> for Seed<S> {
    type Value = S::Value;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<S::Value, D::Error> {
        self.0.deserialize(Objects(d))
    }
}

impl<'de, A: MapAccess<'de>> MapAccess<'de> for Map<A> {
    type Error = A::Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, k: K) -> Result<Option<K::Value>, A::Error> {
        self.0.next_key_seed(Seed(k))
    }

    fn next_value_seed<T: DeserializeSeed<'de>>(&mut self, v: T) -> Result<T::Value, A::Error> {
        self.0.next_value_seed(Seed(v))
    }

    fn size_hint(&self) -> Option<usize> {
        self.0.size_hint()
    }
}

impl<'de, A: SeqAccess<'de>> SeqAccess<'de> for Seq<A> {
    type Error = A::Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, s: T) -> Result<Option<T::Value>, A::Error> {
        self.0.next_element_seed(Seed(s))
    }

    fn size_hint(&self) -> Option<usize> {
        self.0.size_hint()
    }
}

impl<'de, A: EnumAccess<'de>> EnumAccess<'de> for Enum<A> {
    type Error = A::Error;
    type Variant = Variant<A::Variant>;

    fn variant_seed<T: DeserializeSeed<'de>>(self, s: T) -> Result<(T::Value, Self::Variant), A::Error> {
        self.0.variant_seed(Seed(s)).map(|(v, a)| (v, Variant(a)))
    }
}

impl<'de, A: VariantAccess<'de>> VariantAccess<'de> for Variant<A> {
    type Error = A::Error;

    fn unit_variant(self) -> Result<(), A::Error> {
        self.0.unit_variant()
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, s: T) -> Result<T::Value, A::Error> {
        self.0.newtype_variant_seed(Seed(s))
    }

    fn tuple_variant<V: Visitor<'de>>(self, len: usize, v: V) -> Result<V::Value, A::Error> {
        self.0.tuple_variant(len, Wrap(v))
    }

    fn struct_variant<V: Visitor<'de>>(self, fields: &'static [&'static str], v: V) -> Result<V::Value, A::Error> {
        self.0.struct_variant(fields, Wrap(v))
    }
}

#[cfg(test)]
mod tests {
    use super::Objects;
    use serde::Deserialize;
    use std::collections::BTreeMap;

    #[derive(Debug, PartialEq, Deserialize)]
    struct Inner {
        a: u32,
        #[serde(default)]
        b: Option<String>,
    }

    #[derive(Debug, PartialEq, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum Kind {
        Plain,
        Wrapped(Inner),
        Fields { x: i64 },
    }

    #[derive(Debug, PartialEq, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum Tagged {
        One { n: u8 },
        Two,
    }

    #[derive(Debug, PartialEq, Deserialize)]
    #[serde(untagged)]
    enum Either {
        Num(u64),
        Obj(Inner),
    }

    #[derive(Debug, PartialEq, Deserialize)]
    struct Outer {
        inner: Inner,
        list: Vec<Inner>,
        pairs: Vec<(String, u32)>,
        kinds: Vec<Kind>,
        tagged: Vec<Tagged>,
        either: Vec<Either>,
        map: BTreeMap<String, Inner>,
        #[serde(flatten)]
        rest: BTreeMap<String, serde_json::Value>,
    }

    fn read<T: for<'de> Deserialize<'de>>(s: &str) -> Result<T, serde_json::Error> {
        let mut de = serde_json::Deserializer::from_str(s);
        let v = T::deserialize(Objects(&mut de))?;
        de.end()?;
        Ok(v)
    }

    #[test]
    fn reads_what_serde_json_reads() {
        let s = r#"{"inner":{"a":1},"list":[{"a":2,"b":"x"}],"pairs":[["p",3]],
            "kinds":["plain",{"wrapped":{"a":4}},{"fields":{"x":-5}}],
            "tagged":[{"type":"one","n":6},{"type":"two"}],"either":[7,{"a":8}],
            "map":{"k":{"a":9}},"extra":true}"#;
        let a: Outer = read(s).unwrap();
        let b: Outer = serde_json::from_str(s).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.rest.get("extra"), Some(&serde_json::Value::Bool(true)));
    }

    #[test]
    fn a_struct_as_an_array_is_refused() {
        assert!(serde_json::from_str::<Inner>("[1,null]").is_ok());
        assert!(read::<Inner>("[1,null]").is_err());
        assert!(read::<Inner>(r#"{"a":1} x"#).is_err());
    }
}
