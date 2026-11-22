//! The JSON Schemas heki prints for its JSON objects, checked against the objects the
//! end-to-end tests get.

use serde_json::Value;

/// The errors of `value` against `schema`, a JSON Schema using only the keywords this function
/// knows (any other fails the test, so that the schema cannot say more than is checked).
pub fn errors(schema: &Value, value: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    check(schema, schema, value, "", &mut errors);
    errors.sort();
    errors
}

fn check(root: &Value, schema: &Value, value: &Value, at: &str, errors: &mut Vec<String>) {
    let here = if at.is_empty() { "/" } else { at };
    let rules = schema.as_object().unwrap();
    for (keyword, rule) in rules {
        match keyword.as_str() {
            "$schema" | "$id" | "$defs" | "title" | "description" | "format" => {}
            "$ref" => {
                let name = rule.as_str().unwrap().strip_prefix("#/$defs/").unwrap();
                check(root, &root["$defs"][name], value, at, errors);
            }
            "type" => {
                let types: Vec<&str> = match rule {
                    Value::String(t) => vec![t.as_str()],
                    _ => rule
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|t| t.as_str().unwrap())
                        .collect(),
                };
                let matches = |t: &str| match t {
                    "object" => value.is_object(),
                    "array" => value.is_array(),
                    "string" => value.is_string(),
                    "integer" => value.is_u64() || value.is_i64(),
                    "boolean" => value.is_boolean(),
                    "null" => value.is_null(),
                    other => panic!("unknown type `{other}`"),
                };
                if !types.iter().any(|t| matches(t)) {
                    errors.push(format!(
                        "{here}: {value} is not of type {}",
                        types.join(" or ")
                    ));
                }
            }
            "const" => {
                if value != rule {
                    errors.push(format!("{here}: {value} is not {rule}"));
                }
            }
            "enum" => {
                if !rule.as_array().unwrap().contains(value) {
                    errors.push(format!("{here}: {value} is not one of {rule}"));
                }
            }
            "minimum" => {
                if value.as_f64().is_some_and(|v| v < rule.as_f64().unwrap()) {
                    errors.push(format!("{here}: {value} is below {rule}"));
                }
            }
            "maximum" => {
                if value.as_f64().is_some_and(|v| v > rule.as_f64().unwrap()) {
                    errors.push(format!("{here}: {value} is above {rule}"));
                }
            }
            "minItems" | "maxItems" => {
                let bound = rule.as_u64().unwrap();
                let len = value.as_array().map(|a| a.len() as u64);
                let outside = len.is_some_and(|n| match keyword.as_str() {
                    "minItems" => n < bound,
                    _ => n > bound,
                });
                if outside {
                    errors.push(format!("{here}: {value} breaks {keyword} {bound}"));
                }
            }
            "anyOf" | "oneOf" => {
                let matching = rule
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|option| {
                        let mut inner = Vec::new();
                        check(root, option, value, at, &mut inner);
                        inner.is_empty()
                    })
                    .count();
                if matching == 0 || (keyword == "oneOf" && matching > 1) {
                    errors.push(format!(
                        "{here}: {value} matches {matching} of its {keyword}"
                    ));
                }
            }
            "required" => {
                for key in rule.as_array().unwrap() {
                    let key = key.as_str().unwrap();
                    if value.as_object().is_some_and(|o| !o.contains_key(key)) {
                        errors.push(format!("{here}: no `{key}`"));
                    }
                }
            }
            "properties" => {
                for (key, inner) in value.as_object().into_iter().flatten() {
                    if let Some(property) = rule.get(key) {
                        check(root, property, inner, &format!("{at}/{key}"), errors);
                    }
                }
            }
            "additionalProperties" => {
                let declared = rules.get("properties");
                let extra = value
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter(|(key, _)| declared.is_none_or(|d| d.get(key.as_str()).is_none()));
                for (key, inner) in extra {
                    match rule {
                        Value::Bool(false) => {
                            errors.push(format!("{here}: `{key}` is not allowed"))
                        }
                        Value::Bool(true) => {}
                        _ => check(root, rule, inner, &format!("{at}/{key}"), errors),
                    }
                }
            }
            "items" => {
                for (i, item) in value.as_array().into_iter().flatten().enumerate() {
                    check(root, rule, item, &format!("{at}/{i}"), errors);
                }
            }
            other => panic!("the schema uses `{other}`, which the test does not check"),
        }
    }
}
