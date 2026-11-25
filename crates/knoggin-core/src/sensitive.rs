// SPDX-License-Identifier: AGPL-3.0-only
//! `Sensitive<T>`: a value that must never reach a log, a metric or a span.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Holds a sensitive value. `Debug` and `Display` print `[redacted]`; serde,
/// the OpenAPI schema and the TypeScript type see the value inside.
///
/// utoipa refers to every generic field type by name unless told to inline it, so a
/// derived schema marks a `Sensitive<T>` field `#[schema(inline)]`; it then shows `T`.
#[derive(Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sensitive<T>(T);

impl<T> Sensitive<T> {
    /// Wraps a value.
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// The value, for the one place that needs it.
    pub const fn expose(&self) -> &T {
        &self.0
    }

    /// Unwraps the value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> From<T> for Sensitive<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

impl<T> fmt::Debug for Sensitive<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl<T> fmt::Display for Sensitive<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl<T: Serialize> Serialize for Sensitive<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Sensitive<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(Self)
    }
}

/// `utoipa` derives the schema of a generic field through `ComposeSchema`, handing in
/// the schemas of the type arguments; the wrapper passes the inner one through.
/// `PartialSchema` follows from utoipa's blanket impl over `ComposeSchema`.
impl<T: utoipa::PartialSchema> utoipa::__dev::ComposeSchema for Sensitive<T> {
    fn compose(
        generics: Vec<utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>>,
    ) -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        generics.into_iter().next().unwrap_or_else(T::schema)
    }
}

impl<T: utoipa::ToSchema> utoipa::ToSchema for Sensitive<T> {
    fn name() -> std::borrow::Cow<'static, str> {
        T::name()
    }

    fn schemas(
        schemas: &mut Vec<(
            String,
            utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
        )>,
    ) {
        T::schemas(schemas);
    }
}

/// In TypeScript the wrapper disappears, as `Box<T>` does.
impl<T: ts_rs::TS> ts_rs::TS for Sensitive<T> {
    type WithoutGenerics = Self;
    type OptionInnerType = Self;

    fn name(cfg: &ts_rs::Config) -> String {
        T::name(cfg)
    }

    fn inline(cfg: &ts_rs::Config) -> String {
        T::inline(cfg)
    }

    fn inline_flattened(cfg: &ts_rs::Config) -> String {
        T::inline_flattened(cfg)
    }

    fn visit_dependencies(v: &mut impl ts_rs::TypeVisitor)
    where
        Self: 'static,
    {
        T::visit_dependencies(v);
    }

    fn visit_generics(v: &mut impl ts_rs::TypeVisitor)
    where
        Self: 'static,
    {
        T::visit_generics(v);
        v.visit::<T>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, Deserialize, utoipa::ToSchema, ts_rs::TS)]
    #[serde(rename_all = "camelCase")]
    struct Holder {
        #[schema(inline)]
        api_key_name: Sensitive<String>,
        #[schema(inline)]
        note: Option<Sensitive<String>>,
    }

    #[test]
    fn debug_and_display_redact() {
        let s = Sensitive::new("marker-7f3a".to_owned());
        assert_eq!(format!("{s:?}"), "[redacted]");
        assert_eq!(format!("{s}"), "[redacted]");
        assert_eq!(s.expose(), "marker-7f3a");
    }

    #[test]
    fn serde_is_transparent() {
        let h = Holder {
            api_key_name: Sensitive::new("laptop".to_owned()),
            note: None,
        };
        let json = serde_json::to_string(&h).unwrap();
        assert_eq!(json, r#"{"apiKeyName":"laptop","note":null}"#);
        let back: Holder = serde_json::from_str(r#"{"apiKeyName":"x","note":"y"}"#).unwrap();
        assert_eq!(back.api_key_name.expose(), "x");
        assert_eq!(back.note.unwrap().into_inner(), "y");
    }

    #[test]
    fn schema_and_typescript_show_the_plain_value() {
        use utoipa::{PartialSchema, ToSchema};
        assert_eq!(
            serde_json::to_value(Sensitive::<String>::schema()).unwrap(),
            serde_json::to_value(String::schema()).unwrap()
        );
        assert_eq!(Sensitive::<String>::name(), String::name());
        let schema = serde_json::to_value(Holder::schema()).unwrap();
        assert_eq!(
            schema["properties"]["apiKeyName"]["type"],
            serde_json::json!("string")
        );
        let ts = <Holder as ts_rs::TS>::decl(&ts_rs::Config::default());
        assert!(ts.contains("apiKeyName: string"), "{ts}");
        assert!(ts.contains("note: string | null"), "{ts}");
    }
}
