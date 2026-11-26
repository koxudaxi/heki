//! The JSON Schemas heki publishes in `docs/schemas/`, one per JSON format and version, derived
//! from the types that serialize the format.

use schemars::generate::SchemaSettings;
use schemars::transform::RecursiveTransform;
use schemars::{JsonSchema, Schema};
use serde_json::Value;

/// The URL below which every schema is published, as `<name>.json`.
pub const SCHEMA_SITE: &str = "https://heki.koxudaxi.dev/schemas/";

/// The schema of what `T` serializes: draft 2020-12, with the `$id` of the published file
/// `name` and the `title` given. A field that is written as `null` is still required, because
/// the serializer always writes it.
pub fn published_schema<T: JsonSchema>(name: &str, title: &str) -> Value {
    // A `default` says what a reader assumes for a field that is missing; the serializer
    // always writes the field, or leaves it out, and a schema of what it writes says no more.
    let generator = SchemaSettings::draft2020_12()
        .for_serialize()
        .with_transform(RecursiveTransform(|schema: &mut Schema| {
            schema.remove("default");
        }))
        .into_generator();
    let mut schema: Schema = generator.into_root_schema_for::<T>();
    schema.insert("$id".to_owned(), format!("{SCHEMA_SITE}{name}.json").into());
    schema.insert("title".to_owned(), title.into());
    schema.to_value()
}
