use serde_json::Value;

/// Checks the structural schema subset actually interpreted by agent-core.
/// Unsupported keywords are rejected rather than silently advertising unvalidated rules.
pub fn validate_tool_schema(schema: &Value) -> Result<(), String> {
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err("Tool parameters must declare type: object".into());
    }
    validate(schema, "parameters", 0)
}

fn validate(schema: &Value, path: &str, depth: usize) -> Result<(), String> {
    if depth > 64 {
        return Err(format!("{path}: schema nesting exceeds 64"));
    }
    if schema.is_boolean() {
        return Ok(());
    }
    let object = schema
        .as_object()
        .ok_or_else(|| format!("{path}: schema must be an object or boolean"))?;
    for (keyword, value) in object {
        let location = format!("{path}.{keyword}");
        match keyword.as_str() {
            "type" => {
                let types = if let Some(kind) = value.as_str() {
                    vec![kind]
                } else if let Some(kinds) = value.as_array() {
                    if kinds.is_empty() {
                        return Err(format!("{location}: type list is empty"));
                    }
                    kinds
                        .iter()
                        .map(|kind| {
                            kind.as_str()
                                .ok_or_else(|| format!("{location}: type entries must be strings"))
                        })
                        .collect::<Result<Vec<_>, _>>()?
                } else {
                    return Err(format!("{location}: type must be a string or array"));
                };
                for (index, kind) in types.iter().enumerate() {
                    if ![
                        "null", "object", "array", "string", "boolean", "number", "integer",
                    ]
                    .contains(kind)
                        || types[..index].contains(kind)
                    {
                        return Err(format!("{location}: unknown or duplicate type {kind}"));
                    }
                }
            }
            "properties" => {
                let properties = value
                    .as_object()
                    .ok_or_else(|| format!("{location}: properties must be an object"))?;
                for (name, child) in properties {
                    validate(child, &format!("{location}.{name}"), depth + 1)?;
                }
            }
            "required" => {
                let names = value
                    .as_array()
                    .ok_or_else(|| format!("{location}: required must be an array"))?;
                for (index, name) in names.iter().enumerate() {
                    if !name.is_string() || names[..index].contains(name) {
                        return Err(format!(
                            "{location}: required entries must be unique strings"
                        ));
                    }
                }
            }
            "allOf" | "anyOf" | "oneOf" => {
                let branches = value
                    .as_array()
                    .filter(|branches| !branches.is_empty())
                    .ok_or_else(|| {
                        format!("{location}: schema branches must be a nonempty array")
                    })?;
                for (index, child) in branches.iter().enumerate() {
                    validate(child, &format!("{location}.{index}"), depth + 1)?;
                }
            }
            "items" => {
                if let Some(items) = value.as_array() {
                    for (index, child) in items.iter().enumerate() {
                        validate(child, &format!("{location}.{index}"), depth + 1)?;
                    }
                } else {
                    validate(value, &location, depth + 1)?;
                }
            }
            "additionalProperties" | "not" => validate(value, &location, depth + 1)?,
            "enum" => {
                let values = value
                    .as_array()
                    .filter(|values| !values.is_empty())
                    .ok_or_else(|| format!("{location}: enum must be a nonempty array"))?;
                if values
                    .iter()
                    .enumerate()
                    .any(|(index, item)| values[..index].contains(item))
                {
                    return Err(format!("{location}: duplicate enum value"));
                }
            }
            "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum" | "multipleOf" => {
                let number = value
                    .as_f64()
                    .filter(|number| number.is_finite())
                    .ok_or_else(|| format!("{location}: bound must be a finite number"))?;
                if keyword == "multipleOf" && number <= 0.0 {
                    return Err(format!("{location}: multipleOf must be positive"));
                }
            }
            "minProperties" | "maxProperties" | "minItems" | "maxItems" | "minLength"
            | "maxLength" => {
                if value.as_u64().is_none() {
                    return Err(format!("{location}: length must be a nonnegative integer"));
                }
            }
            "uniqueItems" | "deprecated" | "readOnly" | "writeOnly" => {
                if !value.is_boolean() {
                    return Err(format!("{location}: value must be boolean"));
                }
            }
            "title" | "description" | "$schema" | "$id" => {
                if !value.is_string() {
                    return Err(format!("{location}: annotation must be a string"));
                }
            }
            "examples" => {
                if !value.is_array() {
                    return Err(format!("{location}: examples must be an array"));
                }
            }
            "const" | "default" => {}
            _ => return Err(format!("{location}: unsupported schema keyword {keyword}")),
        }
    }
    Ok(())
}
