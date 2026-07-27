//! OpenAPI document ingestion with targeted OpenAPI 3.1.x compatibility.
//!
//! OpenAPI 3.0.x documents pass through unchanged. OpenAPI 3.1.x documents that
//! use JSON Schema type arrays for nullability (`type: [T, "null"]`) are
//! normalized to the OpenAPI 3.0.x form (`type: T`, `nullable: true`) before
//! deserialization into `openapiv3::OpenAPI`.

use crate::{Error, Result};
use openapiv3::OpenAPI;
use serde_json::Value;
use std::path::Path;

/// Parse an OpenAPI document from a file path.
///
/// Supports `.yaml`, `.yml`, and `.json` extensions.
pub fn load_openapi_from_path(path: &Path) -> Result<OpenAPI> {
    let content = std::fs::read_to_string(path)?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());

    match ext.as_deref() {
        Some("yaml") | Some("yml") => load_openapi_from_yaml(&content),
        Some("json") => load_openapi_from_json(&content),
        Some(other) => Err(Error::OpenApi(format!(
            "unsupported OpenAPI file extension '.{other}'; expected .yaml, .yml, or .json"
        ))),
        None => Err(Error::OpenApi(
            "OpenAPI file has no extension; expected .yaml, .yml, or .json".to_string(),
        )),
    }
}

/// Parse an OpenAPI document from a YAML string.
pub fn load_openapi_from_yaml(content: &str) -> Result<OpenAPI> {
    let mut value: Value = serde_yaml::from_str(content)?;
    normalize_document(&mut value)?;
    serde_json::from_value(value).map_err(Error::from)
}

/// Parse an OpenAPI document from a JSON string.
pub fn load_openapi_from_json(content: &str) -> Result<OpenAPI> {
    let mut value: Value = serde_json::from_str(content)?;
    normalize_document(&mut value)?;
    serde_json::from_value(value).map_err(Error::from)
}

fn normalize_document(value: &mut Value) -> Result<()> {
    let version = extract_openapi_version(value)?;
    // openapiv3 requires `openapi` to be a string
    if let Some(obj) = value.as_object_mut() {
        obj.insert("openapi".to_string(), Value::String(version.clone()));
    }

    if is_openapi_30(&version) {
        return Ok(());
    }
    if is_openapi_31(&version) {
        normalize_type_arrays(value, "$")?;
        return Ok(());
    }
    Err(Error::OpenApi(format!(
        "unsupported OpenAPI version '{version}'; supported: 3.0.x and 3.1.x"
    )))
}

fn extract_openapi_version(value: &Value) -> Result<String> {
    match value.get("openapi") {
        Some(Value::String(v)) => Ok(v.clone()),
        // YAML may parse unquoted 3.1 / 2.0 as a number
        Some(Value::Number(n)) => Ok(n.to_string()),
        Some(_) => Err(Error::OpenApi("openapi field must be a string".to_string())),
        None => Err(Error::OpenApi(
            "missing required openapi version field".to_string(),
        )),
    }
}

fn is_openapi_30(version: &str) -> bool {
    version.starts_with("3.0.") || version == "3.0"
}

fn is_openapi_31(version: &str) -> bool {
    version.starts_with("3.1.") || version == "3.1"
}

/// Recursively walk the document and normalize schema `type` arrays.
fn normalize_type_arrays(value: &mut Value, path: &str) -> Result<()> {
    match value {
        Value::Object(map) => {
            if let Some(type_value) = map.get("type").cloned() {
                if type_value.is_array() {
                    let (scalar_type, nullable) = normalize_type_array(&type_value, path)?;
                    map.insert("type".to_string(), Value::String(scalar_type));
                    if nullable {
                        map.insert("nullable".to_string(), Value::Bool(true));
                    }
                }
            }

            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                if let Some(child) = map.get_mut(&key) {
                    let child_path = format!("{path}.{key}");
                    normalize_type_arrays(child, &child_path)?;
                }
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                let child_path = format!("{path}[{index}]");
                normalize_type_arrays(item, &child_path)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Convert a JSON Schema type array into a scalar type and nullable flag.
///
/// Supported forms:
/// - `[T]` → type T, nullable false
/// - `[T, "null"]` / `["null", T]` → type T, nullable true
///
/// Rejected forms:
/// - multiple non-null types
/// - only `"null"`
/// - empty / non-string entries
fn normalize_type_array(type_value: &Value, path: &str) -> Result<(String, bool)> {
    let arr = type_value.as_array().ok_or_else(|| {
        Error::OpenApi(format!("{path}.type: expected array during normalization"))
    })?;

    if arr.is_empty() {
        return Err(Error::OpenApi(format!(
            "{path}.type: type array must not be empty"
        )));
    }

    let mut non_null: Vec<&str> = Vec::new();
    let mut has_null = false;

    for (index, entry) in arr.iter().enumerate() {
        let Some(s) = entry.as_str() else {
            return Err(Error::OpenApi(format!(
                "{path}.type[{index}]: type array entries must be strings"
            )));
        };
        if s == "null" {
            has_null = true;
        } else {
            non_null.push(s);
        }
    }

    match (non_null.as_slice(), has_null) {
        ([t], _) => Ok(((*t).to_string(), has_null)),
        ([], true) => Err(Error::OpenApi(format!(
            "{path}.type: type array containing only 'null' is not supported"
        ))),
        ([], false) => Err(Error::OpenApi(format!(
            "{path}.type: type array must not be empty"
        ))),
        (types, _) => Err(Error::OpenApi(format!(
            "{path}.type: multi-type unions are not supported (got [{}]); \
             only a single non-null type optionally combined with 'null' is allowed",
            types.join(", ")
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_normalize_nullable_string_type_array() {
        let yaml = r#"
openapi: 3.1.1
info:
  title: Test
  version: 1.0.0
paths: {}
components:
  schemas:
    Item:
      type: object
      properties:
        message:
          type:
            - string
            - 'null'
"#;
        let openapi = load_openapi_from_yaml(yaml).expect("parse");
        let schema = openapi
            .components
            .as_ref()
            .unwrap()
            .schemas
            .get("Item")
            .unwrap()
            .as_item()
            .unwrap();
        if let openapiv3::SchemaKind::Type(openapiv3::Type::Object(obj)) = &schema.schema_kind {
            let message = obj.properties.get("message").unwrap().as_item().unwrap();
            assert!(message.schema_data.nullable);
            assert!(matches!(
                message.schema_kind,
                openapiv3::SchemaKind::Type(openapiv3::Type::String(_))
            ));
        } else {
            panic!("expected object schema");
        }
    }

    #[test]
    fn test_normalize_reversed_null_order() {
        let json = r#"{
            "openapi": "3.1.0",
            "info": {"title": "Test", "version": "1.0.0"},
            "paths": {},
            "components": {
                "schemas": {
                    "Item": {
                        "type": "object",
                        "properties": {
                            "count": {
                                "type": ["null", "integer"],
                                "format": "int64"
                            }
                        }
                    }
                }
            }
        }"#;
        let openapi = load_openapi_from_json(json).expect("parse");
        let schema = openapi
            .components
            .as_ref()
            .unwrap()
            .schemas
            .get("Item")
            .unwrap()
            .as_item()
            .unwrap();
        if let openapiv3::SchemaKind::Type(openapiv3::Type::Object(obj)) = &schema.schema_kind {
            let count = obj.properties.get("count").unwrap().as_item().unwrap();
            assert!(count.schema_data.nullable);
            assert!(matches!(
                count.schema_kind,
                openapiv3::SchemaKind::Type(openapiv3::Type::Integer(_))
            ));
        } else {
            panic!("expected object schema");
        }
    }

    #[test]
    fn test_normalize_singleton_type_array() {
        let mut value = json!({
            "openapi": "3.1.1",
            "info": {"title": "T", "version": "1"},
            "paths": {},
            "components": {
                "schemas": {
                    "Item": {
                        "type": "object",
                        "properties": {
                            "name": {"type": ["string"]}
                        }
                    }
                }
            }
        });
        normalize_document(&mut value).expect("normalize");
        assert_eq!(
            value["components"]["schemas"]["Item"]["properties"]["name"]["type"],
            "string"
        );
        assert!(value["components"]["schemas"]["Item"]["properties"]["name"]
            .get("nullable")
            .is_none());
    }

    #[test]
    fn test_openapi_30_unchanged() {
        let mut value = json!({
            "openapi": "3.0.3",
            "info": {"title": "T", "version": "1"},
            "paths": {},
            "components": {
                "schemas": {
                    "Item": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string", "nullable": true}
                        }
                    }
                }
            }
        });
        let before = value.clone();
        normalize_document(&mut value).expect("normalize");
        assert_eq!(value, before);
    }

    #[test]
    fn test_reject_multi_type_union() {
        let yaml = r#"
openapi: 3.1.1
info:
  title: Test
  version: 1.0.0
paths: {}
components:
  schemas:
    Item:
      type: object
      properties:
        value:
          type: [string, integer]
"#;
        let err = load_openapi_from_yaml(yaml).expect_err("must reject");
        let msg = err.to_string();
        assert!(
            msg.contains("multi-type unions are not supported"),
            "unexpected error: {msg}"
        );
        assert!(
            msg.contains("components.schemas.Item.properties.value.type"),
            "error should include path: {msg}"
        );
    }

    #[test]
    fn test_reject_null_only_type_array() {
        let json = r#"{
            "openapi": "3.1.1",
            "info": {"title": "T", "version": "1"},
            "paths": {},
            "components": {
                "schemas": {
                    "Item": {
                        "type": "object",
                        "properties": {
                            "value": {"type": ["null"]}
                        }
                    }
                }
            }
        }"#;
        let err = load_openapi_from_json(json).expect_err("must reject");
        assert!(err
            .to_string()
            .contains("type array containing only 'null'"));
    }

    #[test]
    fn test_reject_empty_type_array() {
        let mut value = json!({
            "openapi": "3.1.1",
            "info": {"title": "T", "version": "1"},
            "paths": {},
            "components": {
                "schemas": {
                    "Item": {
                        "type": "object",
                        "properties": {
                            "value": {"type": []}
                        }
                    }
                }
            }
        });
        let err = normalize_document(&mut value).expect_err("must reject");
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn test_reject_non_string_type_array_entry() {
        let mut value = json!({
            "openapi": "3.1.1",
            "info": {"title": "T", "version": "1"},
            "paths": {},
            "components": {
                "schemas": {
                    "Item": {
                        "type": "object",
                        "properties": {
                            "value": {"type": [1, "null"]}
                        }
                    }
                }
            }
        });
        let err = normalize_document(&mut value).expect_err("must reject");
        assert!(err.to_string().contains("must be strings"));
    }

    #[test]
    fn test_reject_unsupported_version() {
        let yaml = r#"
openapi: "2.0"
info:
  title: Test
  version: 1.0.0
paths: {}
"#;
        let err = load_openapi_from_yaml(yaml).expect_err("must reject");
        assert!(
            err.to_string().contains("unsupported OpenAPI version"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn test_reject_missing_version() {
        let json = r#"{"info": {"title": "T", "version": "1"}, "paths": {}}"#;
        let err = load_openapi_from_json(json).expect_err("must reject");
        assert!(err.to_string().contains("missing required openapi version"));
    }

    #[test]
    fn test_normalize_nested_array_items() {
        let yaml = r#"
openapi: 3.1.1
info:
  title: Test
  version: 1.0.0
paths: {}
components:
  schemas:
    Item:
      type: object
      properties:
        tags:
          type: array
          items:
            type:
              - string
              - 'null'
"#;
        let openapi = load_openapi_from_yaml(yaml).expect("parse");
        let schema = openapi
            .components
            .as_ref()
            .unwrap()
            .schemas
            .get("Item")
            .unwrap()
            .as_item()
            .unwrap();
        if let openapiv3::SchemaKind::Type(openapiv3::Type::Object(obj)) = &schema.schema_kind {
            let tags = obj.properties.get("tags").unwrap().as_item().unwrap();
            if let openapiv3::SchemaKind::Type(openapiv3::Type::Array(arr)) = &tags.schema_kind {
                let items = arr.items.as_ref().unwrap().as_item().unwrap();
                assert!(items.schema_data.nullable);
            } else {
                panic!("expected array");
            }
        } else {
            panic!("expected object");
        }
    }

    #[test]
    fn test_load_unsupported_extension() {
        let err = load_openapi_from_path(Path::new("spec.txt")).expect_err("must reject");
        // File may not exist — either IO or extension error is acceptable depending on order.
        // Our implementation checks extension after reading, so missing file yields IO first.
        // Create a temp path check via extension-only path that doesn't exist:
        let msg = err.to_string();
        assert!(
            msg.contains("No such file") || msg.contains("unsupported OpenAPI file extension"),
            "unexpected: {msg}"
        );
    }

    #[test]
    fn test_reject_unsupported_extension_existing_file() {
        use std::io::Write;
        let dir = std::env::temp_dir();
        let path = dir.join("omg_test_spec.txt");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "openapi: 3.1.1").unwrap();
        }
        let err = load_openapi_from_path(&path).expect_err("must reject");
        let _ = std::fs::remove_file(&path);
        assert!(
            err.to_string()
                .contains("unsupported OpenAPI file extension"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn test_openapi_31_nullable_end_to_end_models() {
        use crate::generator::{generate_models, GenerateMode};
        use crate::models::ModelType;
        use crate::parser::parse_openapi;

        let yaml = r#"
openapi: 3.1.1
info:
  title: Test
  version: 1.0.0
paths: {}
components:
  schemas:
    CreateTrailRequest:
      type: object
      properties:
        message:
          type:
            - string
            - 'null'
        parent_id:
          type:
            - integer
            - 'null'
          format: int64
        title:
          type: [string]
      required:
        - parent_id
"#;
        let openapi = load_openapi_from_yaml(yaml).expect("load");
        let (models, _, _) = parse_openapi(&openapi).expect("parse");
        let model = models
            .iter()
            .find(|m| m.name() == "CreateTrailRequest")
            .expect("model");
        if let ModelType::Struct(s) = model {
            let message = s.fields.iter().find(|f| f.name == "message").unwrap();
            assert!(message.is_nullable);
            assert_eq!(message.field_type, "String");
            assert!(!message.is_required);

            let parent_id = s.fields.iter().find(|f| f.name == "parent_id").unwrap();
            assert!(parent_id.is_nullable);
            assert!(parent_id.is_required);
            assert_eq!(parent_id.field_type, "i64");

            let title = s.fields.iter().find(|f| f.name == "title").unwrap();
            assert!(!title.is_nullable);
            assert_eq!(title.field_type, "String");
        } else {
            panic!("expected struct");
        }

        let code = generate_models(&models, &[], &[], GenerateMode::MODELS).expect("gen");
        assert!(code.contains("pub message: Option<String>"));
        assert!(code.contains("pub parent_id: Option<i64>"));
        assert!(code.contains("pub title: Option<String>"));
    }

    #[test]
    fn test_openapi_31_inline_operation_schema() {
        use crate::models::ModelType;
        use crate::parser::parse_openapi;

        let json = r#"{
            "openapi": "3.1.0",
            "info": {"title": "Test", "version": "1.0.0"},
            "paths": {
                "/items": {
                    "post": {
                        "operationId": "createItem",
                        "requestBody": {
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "object",
                                        "properties": {
                                            "description": {
                                                "type": ["string", "null"]
                                            }
                                        },
                                        "required": ["description"]
                                    }
                                }
                            }
                        },
                        "responses": {"200": {"description": "OK"}}
                    }
                }
            }
        }"#;
        let openapi = load_openapi_from_json(json).expect("load");
        let (models, _, _) = parse_openapi(&openapi).expect("parse");
        let model = models
            .iter()
            .find(|m| m.name() == "CreateItemRequestBody")
            .expect("inline request body model");
        if let ModelType::Struct(s) = model {
            let desc = s.fields.iter().find(|f| f.name == "description").unwrap();
            assert!(desc.is_nullable);
            assert!(desc.is_required);
            assert_eq!(desc.field_type, "String");
        } else {
            panic!("expected struct");
        }
    }
}
