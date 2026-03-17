use crate::{
    models::{
        CompositionModel, EnumModel, Field, Model, ModelType, RequestModel, ResponseModel,
        TypeAliasModel, UnionModel, UnionType, UnionVariant,
    },
    Result,
};
use indexmap::IndexMap;
use openapiv3::{
    AdditionalProperties, OpenAPI, Parameter, ParameterSchemaOrContent, ReferenceOr, Schema,
    SchemaKind, StringFormat, Type, VariantOrUnknownOrEmpty,
};
use std::collections::HashSet;

const X_RUST_TYPE: &str = "x-rust-type";
const X_RUST_ATTRS: &str = "x-rust-attrs";

/// Information about a field extracted from OpenAPI schema
#[derive(Debug)]
struct FieldInfo {
    field_type: String,
    format: String,
    is_nullable: bool,
    is_array_ref: bool,
    description: Option<String>,
    custom_attrs: Option<Vec<String>>,
}

/// Converts camelCase to PascalCase
/// Example: "createRole" -> "CreateRole", "listRoles" -> "ListRoles", "listRoles-Input" -> "ListRolesInput"
pub(crate) fn to_pascal_case(input: &str) -> String {
    input
        .split(&['-', '_'][..])
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut chars = s.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<String>()
}

/// Extracts custom Rust attributes from x-rust-attrs extension
fn extract_custom_attrs(schema: &Schema) -> Option<Vec<String>> {
    schema
        .schema_data
        .extensions
        .get(X_RUST_ATTRS)
        .and_then(|value| {
            if let Some(arr) = value.as_array() {
                let attrs: Vec<String> = arr
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();
                if attrs.is_empty() {
                    None
                } else {
                    Some(attrs)
                }
            } else {
                tracing::warn!(
                    "x-rust-attrs should be an array of strings, got: {:?}",
                    value
                );
                None
            }
        })
}

pub fn parse_openapi(
    openapi: &OpenAPI,
) -> Result<(Vec<ModelType>, Vec<RequestModel>, Vec<ResponseModel>)> {
    let mut models = Vec::new();
    let mut requests = Vec::new();
    let mut responses = Vec::new();

    let mut added_models = HashSet::new();

    let empty_schemas = IndexMap::new();
    let empty_request_bodies = IndexMap::new();

    let empty_parameters = IndexMap::new();
    let (schemas, request_bodies, parameters) = if let Some(components) = &openapi.components {
        (
            &components.schemas,
            &components.request_bodies,
            &components.parameters,
        )
    } else {
        (&empty_schemas, &empty_request_bodies, &empty_parameters)
    };

    // Parse components/schemas
    if let Some(components) = &openapi.components {
        for (name, schema) in &components.schemas {
            let model_types = parse_schema_to_model_type(name, schema, &components.schemas)?;
            for model_type in model_types {
                if added_models.insert(model_type.name().to_string()) {
                    models.push(model_type);
                }
            }
        }

        // Parse components/requestBodies - extract schemas and create models
        for (name, request_body_ref) in &components.request_bodies {
            if let ReferenceOr::Item(request_body) = request_body_ref {
                for media_type in request_body.content.values() {
                    if let Some(schema) = &media_type.schema {
                        let model_types =
                            parse_schema_to_model_type(name, schema, &components.schemas)?;
                        for model_type in model_types {
                            if added_models.insert(model_type.name().to_string()) {
                                models.push(model_type);
                            }
                        }
                    }
                }
            }
        }

        // Parse components/parameters - create structs for reusable query/path/header/cookie params
        for (name, param_ref) in &components.parameters {
            if let ReferenceOr::Item(parameter) = param_ref {
                let param_data = parameter.parameter_data_ref();
                if let ParameterSchemaOrContent::Schema(schema_ref) = &param_data.format {
                    // Resolve schema ref (for $ref, look up in components.schemas)
                    let resolved_schema_ref: &ReferenceOr<Schema> = match schema_ref {
                        ReferenceOr::Item(_) => schema_ref,
                        ReferenceOr::Reference { reference } => reference
                            .strip_prefix("#/components/schemas/")
                            .and_then(|schema_name| components.schemas.get(schema_name))
                            .unwrap_or(schema_ref),
                    };

                    // Object schema with properties -> use parse_schema_to_model_type (PaginateParam etc.)
                    let is_object_with_properties = matches!(
                        resolved_schema_ref,
                        ReferenceOr::Item(schema)
                            if matches!(
                                &schema.schema_kind,
                                SchemaKind::Type(Type::Object(obj)) if !obj.properties.is_empty()
                            )
                    );

                    if is_object_with_properties {
                        let model_types = parse_schema_to_model_type(
                            name,
                            resolved_schema_ref,
                            &components.schemas,
                        )?;
                        for model_type in model_types {
                            if added_models.insert(model_type.name().to_string()) {
                                models.push(model_type);
                            }
                        }
                    } else {
                        // Primitive or ref (e.g. SexParam) -> single-field struct
                        let (field_type, format) =
                            extract_type_and_format(schema_ref, &components.schemas)?;
                        let is_nullable = match schema_ref {
                            ReferenceOr::Item(schema) => schema.schema_data.nullable,
                            ReferenceOr::Reference { reference } => reference
                                .strip_prefix("#/components/schemas/")
                                .and_then(|schema_name| components.schemas.get(schema_name))
                                .and_then(|s| s.as_item())
                                .map(|s| s.schema_data.nullable)
                                .unwrap_or(false),
                        };
                        let model = ModelType::Struct(Model {
                            name: to_pascal_case(name),
                            fields: vec![Field {
                                name: param_data.name.clone(),
                                field_type,
                                format,
                                is_required: param_data.required,
                                is_nullable,
                                is_array_ref: false,
                                flatten: false,
                                description: param_data.description.clone(),
                                custom_attrs: match schema_ref {
                                    ReferenceOr::Item(s) => extract_custom_attrs(s),
                                    ReferenceOr::Reference { reference } => reference
                                        .strip_prefix("#/components/schemas/")
                                        .and_then(|schema_name| components.schemas.get(schema_name))
                                        .and_then(|r| r.as_item())
                                        .and_then(extract_custom_attrs),
                                },
                            }],
                            custom_attrs: None,
                            description: param_data.description.clone(),
                        });
                        if added_models.insert(model.name().to_string()) {
                            models.push(model);
                        }
                    }
                }
            }
        }
    }

    // Parse paths
    for (_path, path_item) in openapi.paths.iter() {
        let path_item = match path_item {
            ReferenceOr::Item(item) => item,
            ReferenceOr::Reference { .. } => continue,
        };

        let operations = [
            &path_item.get,
            &path_item.post,
            &path_item.put,
            &path_item.delete,
            &path_item.patch,
        ];

        for op in operations.iter().filter_map(|o| o.as_ref()) {
            let inline_models = process_operation(
                op,
                path_item,
                &mut requests,
                &mut responses,
                schemas,
                request_bodies,
                parameters,
            )?;
            for model_type in inline_models {
                if added_models.insert(model_type.name().to_string()) {
                    models.push(model_type);
                }
            }
        }
    }

    Ok((models, requests, responses))
}

/// Resolves ReferenceOr<Parameter> to the actual Parameter (follows $ref to components/parameters).
fn resolve_parameter<'a>(
    param_ref: &'a ReferenceOr<Parameter>,
    parameters: &'a IndexMap<String, ReferenceOr<Parameter>>,
) -> Option<&'a Parameter> {
    match param_ref {
        ReferenceOr::Item(param) => Some(param),
        ReferenceOr::Reference { reference } => reference
            .strip_prefix("#/components/parameters/")
            .and_then(|name| parameters.get(name))
            .and_then(|r| r.as_item()),
    }
}

/// Checks if parameter is a query parameter.
fn is_query_parameter(param: &Parameter) -> bool {
    matches!(param, Parameter::Query { .. })
}

fn process_operation(
    operation: &openapiv3::Operation,
    path_item: &openapiv3::PathItem,
    requests: &mut Vec<RequestModel>,
    responses: &mut Vec<ResponseModel>,
    all_schemas: &IndexMap<String, ReferenceOr<Schema>>,
    request_bodies: &IndexMap<String, ReferenceOr<openapiv3::RequestBody>>,
    parameters: &IndexMap<String, ReferenceOr<Parameter>>,
) -> Result<Vec<ModelType>> {
    let mut inline_models = Vec::new();

    // Build unified query params struct for this endpoint (operation overrides path params)
    // Collect (param_name, param, param_component_name) - component_name when param is $ref
    let mut query_params: IndexMap<String, (&Parameter, Option<String>)> = IndexMap::new();
    for param_ref in path_item
        .parameters
        .iter()
        .chain(operation.parameters.iter())
    {
        if let Some(param) = resolve_parameter(param_ref, parameters) {
            if is_query_parameter(param) {
                let name = param.parameter_data_ref().name.clone();
                let component_name = match param_ref {
                    ReferenceOr::Reference { reference } => reference
                        .strip_prefix("#/components/parameters/")
                        .map(|s| s.to_string()),
                    _ => None,
                };
                query_params.insert(name, (param, component_name));
            }
        }
    }
    if !query_params.is_empty() {
        let operation_name = to_pascal_case(operation.operation_id.as_deref().unwrap_or("Unknown"));
        let params_model_name = format!("{operation_name}Params");
        let mut param_fields = Vec::new();
        for (_param_name, (param, component_name)) in &query_params {
            let param_data = param.parameter_data_ref();
            if let ParameterSchemaOrContent::Schema(schema_ref) = &param_data.format {
                let resolved_schema: &ReferenceOr<Schema> = match schema_ref {
                    ReferenceOr::Item(_) => schema_ref,
                    ReferenceOr::Reference { reference } => reference
                        .strip_prefix("#/components/schemas/")
                        .and_then(|n| all_schemas.get(n))
                        .unwrap_or(schema_ref),
                };
                let is_object_with_props = matches!(
                    resolved_schema,
                    ReferenceOr::Item(s)
                        if matches!(
                            &s.schema_kind,
                            SchemaKind::Type(Type::Object(obj)) if !obj.properties.is_empty()
                        )
                );
                let schema_ref_to_schema = matches!(schema_ref, ReferenceOr::Reference { .. });
                let (field_type, flatten) = if is_object_with_props {
                    let model_name = component_name
                        .as_deref()
                        .map(|s| to_pascal_case(s))
                        .unwrap_or_else(|| format!("{}Param", to_pascal_case(&param_data.name)));
                    (model_name, true)
                } else if schema_ref_to_schema && component_name.is_some() {
                    let model_name = to_pascal_case(component_name.as_deref().unwrap_or(""));
                    (model_name, true)
                } else if schema_ref_to_schema {
                    let (ft, _) = extract_type_and_format(schema_ref, all_schemas)?;
                    (ft, false)
                } else {
                    let (ft, _) = extract_type_and_format(schema_ref, all_schemas)?;
                    (ft, false)
                };
                let is_nullable = match schema_ref {
                    ReferenceOr::Item(s) => s.schema_data.nullable,
                    ReferenceOr::Reference { reference } => reference
                        .strip_prefix("#/components/schemas/")
                        .and_then(|n| all_schemas.get(n))
                        .and_then(|s| s.as_item())
                        .map(|s| s.schema_data.nullable)
                        .unwrap_or(false),
                };
                // Schema/object params: no Option wrapper - optionality is in inner struct fields
                let (is_required, is_nullable) = if flatten {
                    (true, false)
                } else {
                    (param_data.required, is_nullable)
                };
                param_fields.push(Field {
                    name: param_data.name.clone(),
                    field_type,
                    format: String::new(),
                    is_required,
                    is_nullable: is_nullable,
                    is_array_ref: false,
                    flatten,
                    description: param_data.description.clone(),
                    custom_attrs: resolved_schema.as_item().and_then(extract_custom_attrs),
                });
            }
        }
        if !param_fields.is_empty() {
            inline_models.push(ModelType::Struct(Model {
                name: params_model_name,
                fields: param_fields,
                custom_attrs: None,
                description: Some(format!(
                    "Query parameters for {}",
                    operation.operation_id.as_deref().unwrap_or("operation")
                )),
            }));
        }
    }

    // Parse request body
    if let Some(request_body_ref) = &operation.request_body {
        let (request_body_data, is_inline) = match request_body_ref {
            ReferenceOr::Item(request_body) => (Some((request_body, request_body.required)), true),
            ReferenceOr::Reference { reference } => {
                if let Some(rb_name) = reference.strip_prefix("#/components/requestBodies/") {
                    (
                        request_bodies.get(rb_name).and_then(|rb_ref| match rb_ref {
                            ReferenceOr::Item(rb) => Some((rb, false)),
                            ReferenceOr::Reference { .. } => None,
                        }),
                        false,
                    )
                } else {
                    (None, false)
                }
            }
        };

        if let Some((request_body, is_required)) = request_body_data {
            for (content_type, media_type) in &request_body.content {
                if let Some(schema) = &media_type.schema {
                    let operation_name =
                        to_pascal_case(operation.operation_id.as_deref().unwrap_or("Unknown"));

                    let schema_type = if is_inline {
                        if let ReferenceOr::Item(schema_item) = schema {
                            if matches!(schema_item.schema_kind, SchemaKind::Type(Type::Object(_)))
                            {
                                let model_name = format!("{operation_name}RequestBody");
                                let model_types =
                                    parse_schema_to_model_type(&model_name, schema, all_schemas)?;
                                inline_models.extend(model_types);
                                model_name
                            } else {
                                extract_type_and_format(schema, all_schemas)?.0
                            }
                        } else {
                            extract_type_and_format(schema, all_schemas)?.0
                        }
                    } else {
                        extract_type_and_format(schema, all_schemas)?.0
                    };

                    let request = RequestModel {
                        name: format!("{operation_name}Request"),
                        content_type: content_type.clone(),
                        schema: schema_type,
                        is_required,
                    };
                    requests.push(request);
                }
            }
        }
    }

    // Parse responses
    for (status, response_ref) in operation.responses.responses.iter() {
        if let ReferenceOr::Item(response) = response_ref {
            for (content_type, media_type) in &response.content {
                if let Some(schema) = &media_type.schema {
                    let response = ResponseModel {
                        name: format!(
                            "{}Response",
                            to_pascal_case(operation.operation_id.as_deref().unwrap_or("Unknown"))
                        ),
                        status_code: status.to_string(),
                        content_type: content_type.clone(),
                        schema: extract_type_and_format(schema, all_schemas)?.0,
                        description: Some(response.description.clone()),
                    };
                    responses.push(response);
                }
            }
        }
    }
    Ok(inline_models)
}

fn parse_schema_to_model_type(
    name: &str,
    schema: &ReferenceOr<Schema>,
    all_schemas: &IndexMap<String, ReferenceOr<Schema>>,
) -> Result<Vec<ModelType>> {
    match schema {
        ReferenceOr::Reference { .. } => Ok(Vec::new()),
        ReferenceOr::Item(schema) => {
            if let Some(rust_type) = schema.schema_data.extensions.get(X_RUST_TYPE) {
                if let Some(type_str) = rust_type.as_str() {
                    return Ok(vec![ModelType::TypeAlias(TypeAliasModel {
                        name: to_pascal_case(name),
                        target_type: type_str.to_string(),
                        description: schema.schema_data.description.clone(),
                        custom_attrs: extract_custom_attrs(schema),
                    })]);
                }
            }

            match &schema.schema_kind {
                // regular objects
                SchemaKind::Type(Type::Object(obj)) => {
                    // Special case: object with only additionalProperties (no regular properties)
                    if obj.properties.is_empty() && obj.additional_properties.is_some() {
                        let hashmap_type = match &obj.additional_properties {
                            Some(additional_props) => match additional_props {
                                openapiv3::AdditionalProperties::Any(_) => {
                                    "std::collections::HashMap<String, serde_json::Value>"
                                        .to_string()
                                }
                                openapiv3::AdditionalProperties::Schema(schema_ref) => {
                                    let (inner_type, _) =
                                        extract_type_and_format(schema_ref, all_schemas)?;
                                    format!("std::collections::HashMap<String, {inner_type}>")
                                }
                            },
                            None => {
                                "std::collections::HashMap<String, serde_json::Value>".to_string()
                            }
                        };
                        return Ok(vec![ModelType::TypeAlias(TypeAliasModel {
                            name: to_pascal_case(name),
                            target_type: hashmap_type,
                            description: schema.schema_data.description.clone(),
                            custom_attrs: extract_custom_attrs(schema),
                        })]);
                    }

                    let mut fields = Vec::new();
                    let mut inline_models = Vec::new();

                    // Process regular properties
                    for (field_name, field_schema) in &obj.properties {
                        if let ReferenceOr::Item(boxed_schema) = field_schema {
                            if matches!(boxed_schema.schema_kind, SchemaKind::Type(Type::Object(_)))
                            {
                                let struct_name = to_pascal_case(field_name);
                                let wrapped_schema = ReferenceOr::Item((**boxed_schema).clone());
                                let nested_models = parse_schema_to_model_type(
                                    &struct_name,
                                    &wrapped_schema,
                                    all_schemas,
                                )?;
                                inline_models.extend(nested_models);
                            }
                        }

                        let (field_info, inline_model) = match field_schema {
                            ReferenceOr::Item(boxed_schema) => extract_field_info(
                                field_name,
                                &ReferenceOr::Item((**boxed_schema).clone()),
                                all_schemas,
                            )?,
                            ReferenceOr::Reference { reference } => extract_field_info(
                                field_name,
                                &ReferenceOr::Reference {
                                    reference: reference.clone(),
                                },
                                all_schemas,
                            )?,
                        };
                        if let Some(inline_model) = inline_model {
                            inline_models.push(inline_model);
                        }
                        let is_required = obj.required.contains(field_name);
                        fields.push(Field {
                            name: field_name.clone(),
                            field_type: field_info.field_type,
                            format: field_info.format,
                            is_required,
                            is_array_ref: field_info.is_array_ref,
                            is_nullable: field_info.is_nullable,
                            flatten: false,
                            description: field_info.description,
                            custom_attrs: field_info.custom_attrs,
                        });
                    }

                    let mut models = inline_models;
                    if obj.properties.is_empty() && obj.additional_properties.is_none() {
                        models.push(ModelType::Struct(Model {
                            name: to_pascal_case(name),
                            fields: vec![],
                            custom_attrs: extract_custom_attrs(schema),
                            description: schema.schema_data.description.clone(),
                        }));
                    } else if !fields.is_empty() {
                        models.push(ModelType::Struct(Model {
                            name: to_pascal_case(name),
                            fields,
                            custom_attrs: extract_custom_attrs(schema),
                            description: schema.schema_data.description.clone(),
                        }));
                    }
                    Ok(models)
                }

                // allOf
                SchemaKind::AllOf { all_of } => {
                    let (all_fields, inline_models) =
                        resolve_all_of_fields(name, all_of, all_schemas)?;
                    let mut models = inline_models;

                    if !all_fields.is_empty() {
                        models.push(ModelType::Composition(CompositionModel {
                            name: to_pascal_case(name),
                            all_fields,
                            custom_attrs: extract_custom_attrs(schema),
                        }));
                    }

                    Ok(models)
                }

                // oneOf
                SchemaKind::OneOf { one_of } => {
                    let (variants, inline_models) =
                        resolve_union_variants(name, one_of, all_schemas)?;
                    let mut models = inline_models;

                    models.push(ModelType::Union(UnionModel {
                        name: to_pascal_case(name),
                        variants,
                        union_type: UnionType::OneOf,
                        custom_attrs: extract_custom_attrs(schema),
                    }));

                    Ok(models)
                }

                // anyOf
                SchemaKind::AnyOf { any_of } => {
                    let (variants, inline_models) =
                        resolve_union_variants(name, any_of, all_schemas)?;
                    let mut models = inline_models;

                    models.push(ModelType::Union(UnionModel {
                        name: to_pascal_case(name),
                        variants,
                        union_type: UnionType::AnyOf,
                        custom_attrs: extract_custom_attrs(schema),
                    }));

                    Ok(models)
                }

                // enum strings
                SchemaKind::Type(Type::String(string_type)) => {
                    if !string_type.enumeration.is_empty() {
                        let variants: Vec<String> = string_type
                            .enumeration
                            .iter()
                            .filter_map(|value| value.clone())
                            .collect();

                        if !variants.is_empty() {
                            let models = vec![ModelType::Enum(EnumModel {
                                name: to_pascal_case(name),
                                variants,
                                description: schema.schema_data.description.clone(),
                                custom_attrs: extract_custom_attrs(schema),
                            })];

                            return Ok(models);
                        }
                    }
                    Ok(Vec::new())
                }

                SchemaKind::Type(Type::Array(array)) => {
                    let mut models = Vec::new();
                    let array_name = to_pascal_case(name);

                    let items = match &array.items {
                        Some(items) => items,
                        None => return Ok(Vec::new()),
                    };

                    match items {
                        ReferenceOr::Item(item_schema) => match &item_schema.schema_kind {
                            SchemaKind::OneOf { one_of } => {
                                let item_type_name = format!("{array_name}Item");

                                let (variants, inline_models) =
                                    resolve_union_variants(&item_type_name, one_of, all_schemas)?;

                                models.extend(inline_models);

                                models.push(ModelType::Union(UnionModel {
                                    name: item_type_name.clone(),
                                    variants,
                                    union_type: UnionType::OneOf,
                                    custom_attrs: extract_custom_attrs(item_schema),
                                }));

                                models.push(ModelType::TypeAlias(TypeAliasModel {
                                    name: array_name,
                                    target_type: format!("Vec<{item_type_name}>"),
                                    description: schema.schema_data.description.clone(),
                                    custom_attrs: extract_custom_attrs(schema),
                                }));
                            }

                            SchemaKind::Type(Type::String(s)) if !s.enumeration.is_empty() => {
                                let item_type_name = format!("{array_name}Item");

                                let variants: Vec<String> =
                                    s.enumeration.iter().filter_map(|v| v.clone()).collect();

                                models.push(ModelType::Enum(EnumModel {
                                    name: item_type_name.clone(),
                                    variants,
                                    description: item_schema.schema_data.description.clone(),
                                    custom_attrs: extract_custom_attrs(item_schema),
                                }));

                                models.push(ModelType::TypeAlias(TypeAliasModel {
                                    name: array_name,
                                    target_type: format!("Vec<{item_type_name}>"),
                                    description: schema.schema_data.description.clone(),
                                    custom_attrs: extract_custom_attrs(schema),
                                }));
                            }

                            SchemaKind::Type(Type::Integer(n)) if !n.enumeration.is_empty() => {
                                let item_type_name = format!("{array_name}Item");

                                let variants: Vec<String> = n
                                    .enumeration
                                    .iter()
                                    .filter_map(|v| v.map(|num| format!("Value{num}")))
                                    .collect();

                                models.push(ModelType::Enum(EnumModel {
                                    name: item_type_name.clone(),
                                    variants,
                                    description: item_schema.schema_data.description.clone(),
                                    custom_attrs: extract_custom_attrs(item_schema),
                                }));

                                models.push(ModelType::TypeAlias(TypeAliasModel {
                                    name: array_name,
                                    target_type: format!("Vec<{item_type_name}>"),
                                    description: schema.schema_data.description.clone(),
                                    custom_attrs: extract_custom_attrs(schema),
                                }));
                            }

                            _ => {
                                let normalized_items = match items {
                                    ReferenceOr::Item(boxed_schema) => {
                                        ReferenceOr::Item((**boxed_schema).clone())
                                    }
                                    ReferenceOr::Reference { reference } => {
                                        ReferenceOr::Reference {
                                            reference: reference.clone(),
                                        }
                                    }
                                };

                                let (inner_type, _) =
                                    extract_type_and_format(&normalized_items, all_schemas)?;

                                models.push(ModelType::TypeAlias(TypeAliasModel {
                                    name: array_name,
                                    target_type: format!("Vec<{inner_type}>"),
                                    description: schema.schema_data.description.clone(),
                                    custom_attrs: extract_custom_attrs(schema),
                                }));
                            }
                        },

                        ReferenceOr::Reference { .. } => {
                            let normalized_items = match items {
                                ReferenceOr::Item(boxed_schema) => {
                                    ReferenceOr::Item((**boxed_schema).clone())
                                }
                                ReferenceOr::Reference { reference } => ReferenceOr::Reference {
                                    reference: reference.clone(),
                                },
                            };

                            let (inner_type, _) =
                                extract_type_and_format(&normalized_items, all_schemas)?;

                            models.push(ModelType::TypeAlias(TypeAliasModel {
                                name: array_name,
                                target_type: format!("Vec<{inner_type}>"),
                                description: schema.schema_data.description.clone(),
                                custom_attrs: extract_custom_attrs(schema),
                            }));
                        }
                    }

                    Ok(models)
                }

                _ => Ok(Vec::new()),
            }
        }
    }
}

fn extract_type_and_format(
    schema: &ReferenceOr<Schema>,
    all_schemas: &IndexMap<String, ReferenceOr<Schema>>,
) -> Result<(String, String)> {
    match schema {
        ReferenceOr::Reference { reference } => {
            let type_name = reference.split('/').next_back().unwrap_or("Unknown");

            if let Some(ReferenceOr::Item(schema)) = all_schemas.get(type_name) {
                if matches!(schema.schema_kind, SchemaKind::OneOf { .. }) {
                    return Ok((to_pascal_case(type_name), "oneOf".to_string()));
                }
            }
            Ok((to_pascal_case(type_name), "reference".to_string()))
        }

        ReferenceOr::Item(schema) => match &schema.schema_kind {
            SchemaKind::Type(Type::String(string_type)) => match &string_type.format {
                VariantOrUnknownOrEmpty::Item(fmt) => match fmt {
                    StringFormat::DateTime => {
                        Ok(("DateTime<Utc>".to_string(), "date-time".to_string()))
                    }
                    StringFormat::Date => Ok(("NaiveDate".to_string(), "date".to_string())),
                    _ => Ok(("String".to_string(), format!("{fmt:?}"))),
                },
                VariantOrUnknownOrEmpty::Unknown(unknown_format) => {
                    if unknown_format.to_lowercase() == "uuid" {
                        Ok(("Uuid".to_string(), "uuid".to_string()))
                    } else {
                        Ok(("String".to_string(), unknown_format.clone()))
                    }
                }
                _ => Ok(("String".to_string(), "string".to_string())),
            },
            SchemaKind::Type(Type::Integer(_)) => Ok(("i64".to_string(), "integer".to_string())),
            SchemaKind::Type(Type::Number(_)) => Ok(("f64".to_string(), "number".to_string())),
            SchemaKind::Type(Type::Boolean(_)) => Ok(("bool".to_string(), "boolean".to_string())),
            SchemaKind::Type(Type::Array(arr)) => {
                if let Some(items) = &arr.items {
                    match items {
                        ReferenceOr::Item(boxed_schema) => extract_type_and_format(
                            &ReferenceOr::Item((**boxed_schema).clone()),
                            all_schemas,
                        ),

                        ReferenceOr::Reference { reference } => extract_type_and_format(
                            &ReferenceOr::Reference {
                                reference: reference.clone(),
                            },
                            all_schemas,
                        ),
                    }
                } else {
                    Ok(("serde_json::Value".to_string(), "array".to_string()))
                }
            }
            SchemaKind::Type(Type::Object(_obj)) => {
                Ok(("serde_json::Value".to_string(), "object".to_string()))
            }
            SchemaKind::AllOf { all_of } if all_of.len() == 1 => {
                extract_type_and_format(&all_of[0], all_schemas)
            }
            _ => Ok(("serde_json::Value".to_string(), "unknown".to_string())),
        },
    }
}

/// Extracts field information including type, format, and nullable flag from OpenAPI schema
fn extract_field_info(
    field_name: &str,
    schema: &ReferenceOr<Schema>,
    all_schemas: &IndexMap<String, ReferenceOr<Schema>>,
) -> Result<(FieldInfo, Option<ModelType>)> {
    let (mut field_type, format) = extract_type_and_format(schema, all_schemas)?;

    let (is_nullable, is_array_ref, en, description, custom_attrs) = match schema {
        ReferenceOr::Reference { reference } => {
            let mut is_array_ref = false;
            let mut is_nullable = false;
            let mut custom_attrs = None;

            if let Some(type_name) = reference.strip_prefix("#/components/schemas/") {
                if let Some(ReferenceOr::Item(schema)) = all_schemas.get(type_name) {
                    is_nullable = schema.schema_data.nullable;
                    custom_attrs = extract_custom_attrs(schema);

                    if let SchemaKind::Type(Type::Array(array)) = &schema.schema_kind {
                        let is_items_one_of = match &array.items {
                            Some(ReferenceOr::Item(item_schema)) => {
                                matches!(item_schema.schema_kind, SchemaKind::OneOf { .. })
                            }
                            _ => false,
                        };

                        is_array_ref = !is_items_one_of;
                    }
                }
            }

            (is_nullable, is_array_ref, None, None, custom_attrs)
        }

        ReferenceOr::Item(schema) => {
            if let Some(rust_type) = schema.schema_data.extensions.get(X_RUST_TYPE) {
                if let Some(type_str) = rust_type.as_str() {
                    field_type = type_str.to_string();
                }
            }

            let is_nullable = schema.schema_data.nullable;
            let is_array_ref = matches!(schema.schema_kind, SchemaKind::Type(Type::Array(_)));
            let description = schema.schema_data.description.clone();
            let custom_attrs = extract_custom_attrs(schema);

            let maybe_enum = match &schema.schema_kind {
                SchemaKind::Type(Type::String(s)) if !s.enumeration.is_empty() => {
                    let variants: Vec<String> =
                        s.enumeration.iter().filter_map(|v| v.clone()).collect();
                    field_type = to_pascal_case(field_name);
                    Some(ModelType::Enum(EnumModel {
                        name: to_pascal_case(field_name),
                        variants,
                        description: schema.schema_data.description.clone(),
                        custom_attrs: extract_custom_attrs(schema),
                    }))
                }
                SchemaKind::Type(Type::Object(obj)) => {
                    if obj.properties.is_empty() {
                        if let Some(additional_props) = &obj.additional_properties {
                            match additional_props {
                                AdditionalProperties::Schema(schema) => {
                                    let (value_type, _) =
                                        extract_type_and_format(&schema.clone(), all_schemas)?;

                                    field_type = format!(
                                        "std::collections::HashMap<String, {}>",
                                        value_type
                                    );
                                }

                                AdditionalProperties::Any(true) => {
                                    field_type =
                                        "std::collections::HashMap<String, serde_json::Value>"
                                            .to_string();
                                }

                                AdditionalProperties::Any(false) => {
                                    // technically: no additional props allowed
                                    field_type = "serde_json::Value".to_string();
                                }
                            }
                            None
                        } else {
                            field_type = "serde_json::Value".to_string();
                            None
                        }
                    } else {
                        let struct_name = to_pascal_case(field_name);
                        field_type = struct_name.clone();

                        let wrapped_schema = ReferenceOr::Item(schema.clone());
                        let models =
                            parse_schema_to_model_type(&struct_name, &wrapped_schema, all_schemas)?;

                        models
                            .into_iter()
                            .find(|m| matches!(m, ModelType::Struct(_)))
                    }
                }
                _ => None,
            };
            (
                is_nullable,
                is_array_ref,
                maybe_enum,
                description,
                custom_attrs,
            )
        }
    };

    Ok((
        FieldInfo {
            field_type,
            format,
            is_nullable,
            is_array_ref,
            description,
            custom_attrs,
        },
        en,
    ))
}

fn resolve_all_of_fields(
    _name: &str,
    all_of: &[ReferenceOr<Schema>],
    all_schemas: &IndexMap<String, ReferenceOr<Schema>>,
) -> Result<(Vec<Field>, Vec<ModelType>)> {
    let mut all_fields: IndexMap<String, Field> = IndexMap::new();
    let mut models = Vec::new();
    let mut all_required_fields = HashSet::new();

    for schema_ref in all_of {
        let schema_to_check = match schema_ref {
            ReferenceOr::Reference { reference } => reference
                .strip_prefix("#/components/schemas/")
                .and_then(|schema_name| all_schemas.get(schema_name)),
            ReferenceOr::Item(_) => Some(schema_ref),
        };

        if let Some(ReferenceOr::Item(schema)) = schema_to_check {
            if let SchemaKind::Type(Type::Object(obj)) = &schema.schema_kind {
                all_required_fields.extend(obj.required.iter().cloned());
            }
        }
    }

    // Try hard to replace all_fields entries that are serde_json::Value
    // Notes:
    //  - Most of the substitions are fairly straightforward, Value, Optional Value.
    //  - HashMap is more complex to understand, we are replacing a Value HashMap
    //    with an actual structure type
    fn less_value(fields: Vec<Field>, all_fields: &mut IndexMap<String, Field>) {
        for field in fields {
            if let Some(existing_field) = all_fields.get_mut(&field.name) {
                // Value
                if existing_field.field_type == "serde_json::Value" {
                    *existing_field = field;
                } else if existing_field.field_type == "Option<serde_json::Value>" {
                    existing_field.field_type = format!("Option<{}>", field.field_type);
                // HashMap Value
                } else if existing_field.field_type
                    == "std::collections::HashMap<String, serde_json::Value>"
                {
                    *existing_field = field;
                } else if existing_field.field_type
                    == "Option<std::collections::HashMap<String, serde_json::Value>>"
                {
                    existing_field.field_type = format!("Option<{}>", field.field_type);
                // Vec Value
                } else if existing_field.field_type == "Vec<serde_json::Value>" {
                    existing_field.field_type = format!("Vec<{}>", field.field_type);
                } else if existing_field.field_type == "Option<Vec<serde_json::Value>>" {
                    existing_field.field_type = format!("Option<Vec<{}>>", field.field_type);
                }
            } else {
                all_fields.insert(field.name.clone(), field);
            }
        }
    }

    // Now collect fields from all schemas
    for schema_ref in all_of {
        match schema_ref {
            ReferenceOr::Reference { reference } => {
                if let Some(schema_name) = reference.strip_prefix("#/components/schemas/") {
                    if let Some(referenced_schema) = all_schemas.get(schema_name) {
                        let (fields, inline_models) =
                            extract_fields_from_schema(referenced_schema, all_schemas)?;
                        // If we have an all_fields entry that is of type serde_json::Value, then we should replace it.
                        less_value(fields, &mut all_fields);
                        models.extend(inline_models);
                    }
                }
            }
            ReferenceOr::Item(_schema) => {
                let (fields, inline_models) = extract_fields_from_schema(schema_ref, all_schemas)?;
                // If we have an all_fields entry that is of type serde_json::Value, then we should replace it.
                less_value(fields, &mut all_fields);
                models.extend(inline_models);
            }
        }
    }

    // Update is_required for fields based on the merged required set
    for field in all_fields.values_mut() {
        if all_required_fields.contains(&field.name) {
            field.is_required = true;
        }
    }

    Ok((all_fields.into_values().collect(), models))
}

fn resolve_union_variants(
    name: &str,
    schemas: &[ReferenceOr<Schema>],
    all_schemas: &IndexMap<String, ReferenceOr<Schema>>,
) -> Result<(Vec<UnionVariant>, Vec<ModelType>)> {
    use std::collections::BTreeSet;

    let mut variants = Vec::new();
    let mut models = Vec::new();
    let mut enum_values: BTreeSet<String> = BTreeSet::new();
    let mut is_all_simple_enum = true;

    for schema_ref in schemas {
        let resolved = match schema_ref {
            ReferenceOr::Reference { reference } => reference
                .strip_prefix("#/components/schemas/")
                .and_then(|n| all_schemas.get(n)),
            ReferenceOr::Item(_) => Some(schema_ref),
        };

        let Some(resolved_schema) = resolved else {
            is_all_simple_enum = false;
            continue;
        };

        match resolved_schema {
            ReferenceOr::Item(schema) => match &schema.schema_kind {
                SchemaKind::Type(Type::String(s)) if !s.enumeration.is_empty() => {
                    enum_values.extend(s.enumeration.iter().filter_map(|v| v.as_ref().cloned()));
                }
                SchemaKind::Type(Type::Integer(n)) if !n.enumeration.is_empty() => {
                    enum_values.extend(
                        n.enumeration
                            .iter()
                            .filter_map(|v| v.map(|num| format!("Value{num}"))),
                    );
                }

                _ => is_all_simple_enum = false,
            },
            ReferenceOr::Reference { reference } => {
                if let Some(n) = reference.strip_prefix("#/components/schemas/") {
                    if let Some(ReferenceOr::Item(inner)) = all_schemas.get(n) {
                        if let SchemaKind::Type(Type::String(s)) = &inner.schema_kind {
                            let values: Vec<String> = s
                                .enumeration
                                .iter()
                                .filter_map(|v| v.as_ref().cloned())
                                .collect();
                            enum_values.extend(values);
                        } else {
                            is_all_simple_enum = false;
                        }
                    }
                }
            }
        }
    }
    if is_all_simple_enum && !enum_values.is_empty() {
        let enum_name = to_pascal_case(name);
        let enum_model = ModelType::Enum(EnumModel {
            name: enum_name.clone(),
            variants: enum_values.iter().map(|v| to_pascal_case(v)).collect(),
            description: None,
            custom_attrs: None, // Collective enum from multiple schemas, no single source for attrs
        });

        return Ok((vec![], vec![enum_model]));
    }

    // fallback for usual union-schemas
    for (index, schema_ref) in schemas.iter().enumerate() {
        match schema_ref {
            ReferenceOr::Reference { reference } => {
                if let Some(schema_name) = reference.strip_prefix("#/components/schemas/") {
                    if let Some(referenced_schema) = all_schemas.get(schema_name) {
                        if let ReferenceOr::Item(schema) = referenced_schema {
                            if matches!(schema.schema_kind, SchemaKind::OneOf { .. }) {
                                variants.push(UnionVariant {
                                    name: to_pascal_case(schema_name),
                                    fields: vec![],
                                    primitive_type: None,
                                });
                            } else {
                                let (fields, inline_models) =
                                    extract_fields_from_schema(referenced_schema, all_schemas)?;
                                variants.push(UnionVariant {
                                    name: to_pascal_case(schema_name),
                                    fields,
                                    primitive_type: None,
                                });
                                models.extend(inline_models);
                            }
                        }
                    }
                }
            }
            ReferenceOr::Item(schema) => match &schema.schema_kind {
                SchemaKind::Type(Type::String(_)) => {
                    variants.push(UnionVariant {
                        name: "String".to_string(),
                        fields: vec![],
                        primitive_type: Some("String".to_string()),
                    });
                }

                SchemaKind::Type(Type::Integer(_)) => {
                    variants.push(UnionVariant {
                        name: "Integer".to_string(),
                        fields: vec![],
                        primitive_type: Some("i64".to_string()),
                    });
                }

                SchemaKind::Type(Type::Number(_)) => {
                    variants.push(UnionVariant {
                        name: "Number".to_string(),
                        fields: vec![],
                        primitive_type: Some("f64".to_string()),
                    });
                }

                SchemaKind::Type(Type::Boolean(_)) => {
                    variants.push(UnionVariant {
                        name: "Boolean".to_string(),
                        fields: vec![],
                        primitive_type: Some("Boolean".to_string()),
                    });
                }

                _ => {
                    let (fields, inline_models) =
                        extract_fields_from_schema(schema_ref, all_schemas)?;
                    let variant_name = format!("Variant{index}");
                    variants.push(UnionVariant {
                        name: variant_name,
                        fields,
                        primitive_type: None,
                    });
                    models.extend(inline_models);
                }
            },
        }
    }

    Ok((variants, models))
}

fn extract_fields_from_schema(
    schema_ref: &ReferenceOr<Schema>,
    _all_schemas: &IndexMap<String, ReferenceOr<Schema>>,
) -> Result<(Vec<Field>, Vec<ModelType>)> {
    let mut fields = Vec::new();
    let mut inline_models = Vec::new();

    match schema_ref {
        ReferenceOr::Reference { .. } => Ok((fields, inline_models)),
        ReferenceOr::Item(schema) => {
            match &schema.schema_kind {
                SchemaKind::Type(Type::Object(obj)) => {
                    for (field_name, field_schema) in &obj.properties {
                        let (field_info, inline_model) = match field_schema {
                            ReferenceOr::Item(boxed_schema) => extract_field_info(
                                field_name,
                                &ReferenceOr::Item((**boxed_schema).clone()),
                                _all_schemas,
                            )?,
                            ReferenceOr::Reference { reference } => extract_field_info(
                                field_name,
                                &ReferenceOr::Reference {
                                    reference: reference.clone(),
                                },
                                _all_schemas,
                            )?,
                        };

                        let is_nullable = field_info.is_nullable
                            || field_name == "value"
                            || field_name == "default_value";

                        let field_type = field_info.field_type.clone();

                        let is_required = obj.required.contains(field_name);
                        fields.push(Field {
                            name: field_name.clone(),
                            field_type,
                            format: field_info.format,
                            is_required,
                            is_nullable,
                            is_array_ref: field_info.is_array_ref,
                            flatten: false,
                            description: field_info.description,
                            custom_attrs: field_info.custom_attrs,
                        });
                        if let Some(inline_model) = inline_model {
                            match &inline_model {
                                ModelType::Struct(m) if m.fields.is_empty() => {}
                                _ => inline_models.push(inline_model),
                            }
                        }
                    }
                }
                SchemaKind::Type(Type::String(s)) if !s.enumeration.is_empty() => {
                    let name = schema
                        .schema_data
                        .title
                        .clone()
                        .unwrap_or_else(|| "AnonymousStringEnum".to_string());

                    let enum_model = ModelType::Enum(EnumModel {
                        name,
                        variants: s
                            .enumeration
                            .iter()
                            .filter_map(|v| v.as_ref().map(|s| to_pascal_case(s)))
                            .collect(),
                        description: schema.schema_data.description.clone(),
                        custom_attrs: extract_custom_attrs(schema),
                    });

                    inline_models.push(enum_model);
                }
                SchemaKind::Type(Type::Integer(n)) if !n.enumeration.is_empty() => {
                    let name = schema
                        .schema_data
                        .title
                        .clone()
                        .unwrap_or_else(|| "AnonymousIntEnum".to_string());

                    let enum_model = ModelType::Enum(EnumModel {
                        name,
                        variants: n
                            .enumeration
                            .iter()
                            .filter_map(|v| v.map(|num| format!("Value{num}")))
                            .collect(),
                        description: schema.schema_data.description.clone(),
                        custom_attrs: extract_custom_attrs(schema),
                    });

                    inline_models.push(enum_model);
                }

                _ => {}
            }

            Ok((fields, inline_models))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_inline_request_body_generates_model() {
        let openapi_spec: OpenAPI = serde_json::from_value(json!({
            "openapi": "3.0.0",
            "info": { "title": "Test API", "version": "1.0.0" },
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
                                            "name": { "type": "string" },
                                            "value": { "type": "integer" }
                                        },
                                        "required": ["name"]
                                    }
                                }
                            }
                        },
                        "responses": { "200": { "description": "OK" } }
                    }
                }
            }
        }))
        .expect("Failed to deserialize OpenAPI spec");

        let (models, requests, _responses) =
            parse_openapi(&openapi_spec).expect("Failed to parse OpenAPI spec");

        // 1. Verify that request model was created
        assert_eq!(requests.len(), 1);
        let request_model = &requests[0];
        assert_eq!(request_model.name, "CreateItemRequest");

        // 2. Verify that request schema references a NEW model, not Value
        assert_eq!(request_model.schema, "CreateItemRequestBody");

        // 3. Verify that the request body model itself was generated
        let inline_model = models.iter().find(|m| m.name() == "CreateItemRequestBody");
        assert!(
            inline_model.is_some(),
            "Expected a model named 'CreateItemRequestBody' to be generated"
        );

        if let Some(ModelType::Struct(model)) = inline_model {
            assert_eq!(model.fields.len(), 2);
            assert_eq!(model.fields[0].name, "name");
            assert_eq!(model.fields[0].field_type, "String");
            assert!(model.fields[0].is_required);

            assert_eq!(model.fields[1].name, "value");
            assert_eq!(model.fields[1].field_type, "i64");
            assert!(!model.fields[1].is_required);
        } else {
            panic!("Expected a Struct model for CreateItemRequestBody");
        }
    }

    #[test]
    fn test_parse_ref_request_body_works() {
        let openapi_spec: OpenAPI = serde_json::from_value(json!({
            "openapi": "3.0.0",
            "info": { "title": "Test API", "version": "1.0.0" },
            "components": {
                "schemas": {
                    "ItemData": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" }
                        }
                    }
                },
                "requestBodies": {
                    "CreateItem": {
                        "content": {
                            "application/json": {
                                "schema": { "$ref": "#/components/schemas/ItemData" }
                            }
                        }
                    }
                }
            },
            "paths": {
                "/items": {
                    "post": {
                        "operationId": "createItem",
                        "requestBody": { "$ref": "#/components/requestBodies/CreateItem" },
                        "responses": { "200": { "description": "OK" } }
                    }
                }
            }
        }))
        .expect("Failed to deserialize OpenAPI spec");

        let (models, requests, _responses) =
            parse_openapi(&openapi_spec).expect("Failed to parse OpenAPI spec");

        // Verify that request model was created
        assert_eq!(requests.len(), 1);
        let request_model = &requests[0];
        assert_eq!(request_model.name, "CreateItemRequest");

        // Verify that schema references an existing model
        assert_eq!(request_model.schema, "ItemData");

        // Verify that ItemData model exists in the models list
        assert!(models.iter().any(|m| m.name() == "ItemData"));
    }

    #[test]
    fn test_parse_no_request_body() {
        let openapi_spec: OpenAPI = serde_json::from_value(json!({
            "openapi": "3.0.0",
            "info": { "title": "Test API", "version": "1.0.0" },
            "paths": {
                "/items": {
                    "get": {
                        "operationId": "listItems",
                        "responses": { "200": { "description": "OK" } }
                    }
                }
            }
        }))
        .expect("Failed to deserialize OpenAPI spec");

        let (_models, requests, _responses) =
            parse_openapi(&openapi_spec).expect("Failed to parse OpenAPI spec");

        // Verify that no request models were created
        assert!(requests.is_empty());
    }

    #[test]
    fn test_parse_components_parameters() {
        let openapi_spec: OpenAPI = serde_json::from_value(json!({
            "openapi": "3.0.0",
            "info": { "title": "Test API", "version": "1.0.0" },
            "paths": {},
            "components": {
                "schemas": {
                    "Sex": {
                        "type": "string",
                        "enum": ["MALE", "FEMALE"]
                    }
                },
                "parameters": {
                    "LimitParam": {
                        "name": "limit",
                        "in": "query",
                        "description": "Maximum number of items to return per page",
                        "required": false,
                        "schema": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 100,
                            "default": 20
                        }
                    },
                    "OffsetParam": {
                        "name": "offset",
                        "in": "query",
                        "description": "Number of items to skip",
                        "required": false,
                        "schema": {
                            "type": "integer",
                            "minimum": 0,
                            "default": 0
                        }
                    },
                    "SexParam": {
                        "name": "sex",
                        "in": "query",
                        "description": "Filter by sex",
                        "required": false,
                        "schema": { "$ref": "#/components/schemas/Sex" }
                    }
                }
            }
        }))
        .expect("Failed to deserialize OpenAPI spec");

        let (models, _, _) = parse_openapi(&openapi_spec).expect("Failed to parse OpenAPI spec");

        // Verify LimitParam struct
        let limit_param = models.iter().find(|m| m.name() == "LimitParam");
        assert!(limit_param.is_some(), "Expected LimitParam model");
        if let Some(ModelType::Struct(model)) = limit_param {
            assert_eq!(model.fields.len(), 1);
            assert_eq!(model.fields[0].name, "limit");
            assert_eq!(model.fields[0].field_type, "i64");
            assert!(!model.fields[0].is_required);
        }

        // Verify OffsetParam struct
        let offset_param = models.iter().find(|m| m.name() == "OffsetParam");
        assert!(offset_param.is_some(), "Expected OffsetParam model");
        if let Some(ModelType::Struct(model)) = offset_param {
            assert_eq!(model.fields.len(), 1);
            assert_eq!(model.fields[0].name, "offset");
            assert_eq!(model.fields[0].field_type, "i64");
            assert!(!model.fields[0].is_required);
        }

        // Verify SexParam struct (schema ref resolves to Sex enum)
        let sex_param = models.iter().find(|m| m.name() == "SexParam");
        assert!(sex_param.is_some(), "Expected SexParam model");
        if let Some(ModelType::Struct(model)) = sex_param {
            assert_eq!(model.fields.len(), 1);
            assert_eq!(model.fields[0].name, "sex");
            assert_eq!(model.fields[0].field_type, "Sex");
            assert!(!model.fields[0].is_required);
        }

        // Verify Sex enum was also generated (from schemas)
        assert!(models.iter().any(|m| m.name() == "Sex"));
    }

    #[test]
    fn test_endpoint_params_struct_with_flatten() {
        let openapi_spec: OpenAPI = serde_json::from_value(json!({
            "openapi": "3.0.0",
            "info": { "title": "Test API", "version": "1.0.0" },
            "paths": {
                "/items": {
                    "get": {
                        "operationId": "listItems",
                        "parameters": [
                            { "$ref": "#/components/parameters/PaginateParam" },
                            {
                                "name": "filter",
                                "in": "query",
                                "schema": { "type": "string" }
                            }
                        ],
                        "responses": { "200": { "description": "OK" } }
                    }
                }
            },
            "components": {
                "parameters": {
                    "PaginateParam": {
                        "name": "pagination",
                        "in": "query",
                        "schema": {
                            "type": "object",
                            "properties": {
                                "limit": { "type": "integer", "default": 20 },
                                "offset": { "type": "integer", "default": 0 }
                            }
                        }
                    }
                }
            }
        }))
        .expect("Failed to deserialize OpenAPI spec");

        let (models, _, _) = parse_openapi(&openapi_spec).expect("Failed to parse OpenAPI spec");

        // ListItemsParams should exist with pagination (flatten) and filter
        let params = models.iter().find(|m| m.name() == "ListItemsParams");
        assert!(
            params.is_some(),
            "Expected ListItemsParams for endpoint with query params"
        );
        if let Some(ModelType::Struct(model)) = params {
            let pagination_field = model.fields.iter().find(|f| f.name == "pagination");
            assert!(pagination_field.is_some(), "Expected pagination field");
            assert!(
                pagination_field.unwrap().flatten,
                "pagination should have flatten for GET query"
            );
            assert_eq!(pagination_field.unwrap().field_type, "PaginateParam");

            let filter_field = model.fields.iter().find(|f| f.name == "filter");
            assert!(filter_field.is_some(), "Expected filter field");
            assert!(!filter_field.unwrap().flatten);
        }
    }

    #[test]
    fn test_parse_parameter_with_object_schema_paginate_param() {
        let openapi_spec: OpenAPI = serde_json::from_value(json!({
            "openapi": "3.0.0",
            "info": { "title": "Test API", "version": "1.0.0" },
            "paths": {},
            "components": {
                "parameters": {
                    "PaginateParam": {
                        "name": "pagination",
                        "in": "query",
                        "description": "Pagination parameters (limit and offset)",
                        "required": false,
                        "style": "form",
                        "explode": true,
                        "schema": {
                            "type": "object",
                            "properties": {
                                "limit": {
                                    "type": "integer",
                                    "minimum": 1,
                                    "maximum": 100,
                                    "default": 20,
                                    "description": "Maximum number of items to return per page"
                                },
                                "offset": {
                                    "type": "integer",
                                    "minimum": 0,
                                    "default": 0,
                                    "description": "Number of items to skip"
                                }
                            }
                        }
                    }
                }
            }
        }))
        .expect("Failed to deserialize OpenAPI spec");

        let (models, _, _) = parse_openapi(&openapi_spec).expect("Failed to parse OpenAPI spec");

        // Verify PaginateParam struct has both limit and offset fields
        let paginate_param = models.iter().find(|m| m.name() == "PaginateParam");
        assert!(
            paginate_param.is_some(),
            "Expected PaginateParam model with object schema"
        );
        if let Some(ModelType::Struct(model)) = paginate_param {
            assert_eq!(model.fields.len(), 2, "PaginateParam should have 2 fields");
            let limit_field = model.fields.iter().find(|f| f.name == "limit");
            let offset_field = model.fields.iter().find(|f| f.name == "offset");
            assert!(limit_field.is_some(), "Expected limit field");
            assert!(offset_field.is_some(), "Expected offset field");
            assert_eq!(limit_field.unwrap().field_type, "i64");
            assert_eq!(offset_field.unwrap().field_type, "i64");
            assert!(!limit_field.unwrap().is_required);
            assert!(!offset_field.unwrap().is_required);
        }
    }

    #[test]
    fn test_nullable_reference_field() {
        // Test verifies that nullable is correctly read from the target schema when using $ref
        let openapi_spec: OpenAPI = serde_json::from_value(json!({
            "openapi": "3.0.0",
            "info": { "title": "Test API", "version": "1.0.0" },
            "paths": {},
            "components": {
                "schemas": {
                    "NullableUser": {
                        "type": "object",
                        "nullable": true,
                        "properties": {
                            "name": { "type": "string" }
                        }
                    },
                    "Post": {
                        "type": "object",
                        "properties": {
                            "author": {
                                "$ref": "#/components/schemas/NullableUser"
                            }
                        }
                    }
                }
            }
        }))
        .expect("Failed to deserialize OpenAPI spec");

        let (models, _, _) = parse_openapi(&openapi_spec).expect("Failed to parse OpenAPI spec");

        // Find Post model
        let post_model = models.iter().find(|m| m.name() == "Post");
        assert!(post_model.is_some(), "Expected Post model to be generated");

        if let Some(ModelType::Struct(post)) = post_model {
            let author_field = post.fields.iter().find(|f| f.name == "author");
            assert!(author_field.is_some(), "Expected author field");

            // Verify that nullable is correctly handled for reference type
            // (nullable is taken from the target schema NullableUser)
            let author = author_field.unwrap();
            assert!(
                author.is_nullable,
                "Expected author field to be nullable (from referenced schema)"
            );
        } else {
            panic!("Expected Post to be a Struct");
        }
    }

    #[test]
    fn test_allof_single_ref_nullable_field() {
        // allOf with one $ref, description and nullable as siblings (canonical format)
        let openapi_spec: OpenAPI = serde_json::from_value(json!({
            "openapi": "3.0.0",
            "info": { "title": "Test API", "version": "1.0.0" },
            "paths": {},
            "components": {
                "schemas": {
                    "SceneSeries": {
                        "type": "object",
                        "nullable": false,
                        "required": ["slug", "label"],
                        "properties": {
                            "slug": { "type": "string" },
                            "label": { "type": "string" }
                        }
                    },
                    "SceneItem": {
                        "type": "object",
                        "properties": {
                            "series": {
                                "allOf": [{ "$ref": "#/components/schemas/SceneSeries" }],
                                "description": "Series this scene belongs to; null if standalone",
                                "nullable": true
                            }
                        }
                    }
                }
            }
        }))
        .expect("Failed to deserialize OpenAPI spec");

        let (models, _, _) = parse_openapi(&openapi_spec).expect("Failed to parse OpenAPI spec");

        let scene_item = models.iter().find(|m| m.name() == "SceneItem");
        assert!(scene_item.is_some(), "Expected SceneItem model");

        if let Some(ModelType::Struct(model)) = scene_item {
            let series_field = model.fields.iter().find(|f| f.name == "series");
            assert!(series_field.is_some(), "Expected series field");
            let series = series_field.unwrap();
            assert_eq!(
                series.field_type, "SceneSeries",
                "Expected field type SceneSeries, got {}",
                series.field_type
            );
            assert!(
                series.is_nullable,
                "Expected series field to be nullable (sibling nullable: true)"
            );
        } else {
            panic!("Expected SceneItem to be a Struct");
        }
    }

    #[test]
    fn test_allof_required_fields_merge() {
        let openapi_spec: OpenAPI = serde_json::from_value(json!({
            "openapi": "3.0.0",
            "info": { "title": "Test API", "version": "1.0.0" },
            "paths": {},
            "components": {
                "schemas": {
                    "BaseEntity": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "created": { "type": "string" }
                        },
                        "required": ["id"]
                    },
                    "Person": {
                        "allOf": [
                            { "$ref": "#/components/schemas/BaseEntity" },
                            {
                                "type": "object",
                                "properties": {
                                    "name": { "type": "string" },
                                    "age": { "type": "integer" }
                                },
                                "required": ["name"]
                            }
                        ]
                    }
                }
            }
        }))
        .expect("Failed to deserialize OpenAPI spec");

        let (models, _, _) = parse_openapi(&openapi_spec).expect("Failed to parse OpenAPI spec");

        // Find Person model
        let person_model = models.iter().find(|m| m.name() == "Person");
        assert!(
            person_model.is_some(),
            "Expected Person model to be generated"
        );

        if let Some(ModelType::Composition(person)) = person_model {
            // Verify that id (from BaseEntity) is required
            let id_field = person.all_fields.iter().find(|f| f.name == "id");
            assert!(id_field.is_some(), "Expected id field");
            assert!(
                id_field.unwrap().is_required,
                "Expected id to be required from BaseEntity"
            );

            // Verify that name (from second object) is required
            let name_field = person.all_fields.iter().find(|f| f.name == "name");
            assert!(name_field.is_some(), "Expected name field");
            assert!(
                name_field.unwrap().is_required,
                "Expected name to be required from inline object"
            );

            // Verify that created and age are not required
            let created_field = person.all_fields.iter().find(|f| f.name == "created");
            assert!(created_field.is_some(), "Expected created field");
            assert!(
                !created_field.unwrap().is_required,
                "Expected created to be optional"
            );

            let age_field = person.all_fields.iter().find(|f| f.name == "age");
            assert!(age_field.is_some(), "Expected age field");
            assert!(
                !age_field.unwrap().is_required,
                "Expected age to be optional"
            );
        } else {
            panic!("Expected Person to be a Composition");
        }
    }

    #[test]
    fn test_x_rust_type_generates_type_alias() {
        let openapi_spec: OpenAPI = serde_json::from_value(json!({
            "openapi": "3.0.0",
            "info": { "title": "Test API", "version": "1.0.0" },
            "paths": {},
            "components": {
                "schemas": {
                    "User": {
                        "type": "object",
                        "x-rust-type": "crate::domain::User",
                        "description": "Custom domain user type",
                        "properties": {
                            "name": { "type": "string" },
                            "age": { "type": "integer" }
                        }
                    }
                }
            }
        }))
        .expect("Failed to deserialize OpenAPI spec");

        let (models, _, _) = parse_openapi(&openapi_spec).expect("Failed to parse OpenAPI spec");

        // Verify that TypeAlias is created, not Struct
        let user_model = models.iter().find(|m| m.name() == "User");
        assert!(user_model.is_some(), "Expected User model");

        match user_model.unwrap() {
            ModelType::TypeAlias(alias) => {
                assert_eq!(alias.name, "User");
                assert_eq!(alias.target_type, "crate::domain::User");
                assert_eq!(
                    alias.description,
                    Some("Custom domain user type".to_string())
                );
            }
            _ => panic!("Expected TypeAlias, got different type"),
        }
    }

    #[test]
    fn test_x_rust_type_works_with_enum() {
        let openapi_spec: OpenAPI = serde_json::from_value(json!({
            "openapi": "3.0.0",
            "info": { "title": "Test API", "version": "1.0.0" },
            "paths": {},
            "components": {
                "schemas": {
                    "Status": {
                        "type": "string",
                        "enum": ["active", "inactive"],
                        "x-rust-type": "crate::domain::Status"
                    }
                }
            }
        }))
        .expect("Failed to deserialize OpenAPI spec");

        let (models, _, _) = parse_openapi(&openapi_spec).expect("Failed to parse OpenAPI spec");

        let status_model = models.iter().find(|m| m.name() == "Status");
        assert!(status_model.is_some(), "Expected Status model");

        // Should be TypeAlias, not Enum
        assert!(
            matches!(status_model.unwrap(), ModelType::TypeAlias(_)),
            "Expected TypeAlias for enum with x-rust-type"
        );
    }

    #[test]
    fn test_x_rust_type_works_with_oneof() {
        let openapi_spec: OpenAPI = serde_json::from_value(json!({
            "openapi": "3.0.0",
            "info": { "title": "Test API", "version": "1.0.0" },
            "paths": {},
            "components": {
                "schemas": {
                    "Payment": {
                        "oneOf": [
                            { "type": "object", "properties": { "card": { "type": "string" } } },
                            { "type": "object", "properties": { "cash": { "type": "number" } } }
                        ],
                        "x-rust-type": "payments::Payment"
                    }
                }
            }
        }))
        .expect("Failed to deserialize OpenAPI spec");

        let (models, _, _) = parse_openapi(&openapi_spec).expect("Failed to parse OpenAPI spec");

        let payment_model = models.iter().find(|m| m.name() == "Payment");
        assert!(payment_model.is_some(), "Expected Payment model");

        // Should be TypeAlias, not Union
        match payment_model.unwrap() {
            ModelType::TypeAlias(alias) => {
                assert_eq!(alias.target_type, "payments::Payment");
            }
            _ => panic!("Expected TypeAlias for oneOf with x-rust-type"),
        }
    }

    #[test]
    fn test_x_rust_attrs_on_struct() {
        let openapi_spec: OpenAPI = serde_json::from_value(json!({
            "openapi": "3.0.0",
            "info": { "title": "Test API", "version": "1.0.0" },
            "paths": {},
            "components": {
                "schemas": {
                    "User": {
                        "type": "object",
                        "x-rust-attrs": [
                            "#[derive(Serialize, Deserialize)]",
                            "#[serde(rename_all = \"camelCase\")]"
                        ],
                        "properties": {
                            "name": { "type": "string" }
                        }
                    }
                }
            }
        }))
        .expect("Failed to deserialize OpenAPI spec");

        let (models, _, _) = parse_openapi(&openapi_spec).expect("Failed to parse OpenAPI spec");

        let user_model = models.iter().find(|m| m.name() == "User");
        assert!(user_model.is_some(), "Expected User model");

        match user_model.unwrap() {
            ModelType::Struct(model) => {
                assert!(model.custom_attrs.is_some(), "Expected custom_attrs");
                let attrs = model.custom_attrs.as_ref().unwrap();
                assert_eq!(attrs.len(), 2);
                assert_eq!(attrs[0], "#[derive(Serialize, Deserialize)]");
                assert_eq!(attrs[1], "#[serde(rename_all = \"camelCase\")]");
            }
            _ => panic!("Expected Struct model"),
        }
    }

    #[test]
    fn test_x_rust_attrs_on_enum() {
        let openapi_spec: OpenAPI = serde_json::from_value(json!({
            "openapi": "3.0.0",
            "info": { "title": "Test API", "version": "1.0.0" },
            "paths": {},
            "components": {
                "schemas": {
                    "Status": {
                        "type": "string",
                        "enum": ["active", "inactive"],
                        "x-rust-attrs": ["#[serde(rename_all = \"UPPERCASE\")]"]
                    }
                }
            }
        }))
        .expect("Failed to deserialize OpenAPI spec");

        let (models, _, _) = parse_openapi(&openapi_spec).expect("Failed to parse OpenAPI spec");

        let status_model = models.iter().find(|m| m.name() == "Status");
        assert!(status_model.is_some(), "Expected Status model");

        match status_model.unwrap() {
            ModelType::Enum(enum_model) => {
                assert!(enum_model.custom_attrs.is_some());
                let attrs = enum_model.custom_attrs.as_ref().unwrap();
                assert_eq!(attrs.len(), 1);
                assert_eq!(attrs[0], "#[serde(rename_all = \"UPPERCASE\")]");
            }
            _ => panic!("Expected Enum model"),
        }
    }

    #[test]
    fn test_x_rust_attrs_with_x_rust_type() {
        let openapi_spec: OpenAPI = serde_json::from_value(json!({
            "openapi": "3.0.0",
            "info": { "title": "Test API", "version": "1.0.0" },
            "paths": {},
            "components": {
                "schemas": {
                    "User": {
                        "type": "object",
                        "x-rust-type": "crate::domain::User",
                        "x-rust-attrs": ["#[cfg_attr(test, derive(Default))]"],
                        "properties": {
                            "name": { "type": "string" }
                        }
                    }
                }
            }
        }))
        .expect("Failed to deserialize OpenAPI spec");

        let (models, _, _) = parse_openapi(&openapi_spec).expect("Failed to parse OpenAPI spec");

        let user_model = models.iter().find(|m| m.name() == "User");
        assert!(user_model.is_some(), "Expected User model");

        // Should be TypeAlias with attributes
        match user_model.unwrap() {
            ModelType::TypeAlias(alias) => {
                assert_eq!(alias.target_type, "crate::domain::User");
                assert!(alias.custom_attrs.is_some());
                let attrs = alias.custom_attrs.as_ref().unwrap();
                assert_eq!(attrs.len(), 1);
                assert_eq!(attrs[0], "#[cfg_attr(test, derive(Default))]");
            }
            _ => panic!("Expected TypeAlias with custom attrs"),
        }
    }

    #[test]
    fn test_x_rust_attrs_empty_array() {
        let openapi_spec: OpenAPI = serde_json::from_value(json!({
            "openapi": "3.0.0",
            "info": { "title": "Test API", "version": "1.0.0" },
            "paths": {},
            "components": {
                "schemas": {
                    "User": {
                        "type": "object",
                        "x-rust-attrs": [],
                        "properties": {
                            "name": { "type": "string" }
                        }
                    }
                }
            }
        }))
        .expect("Failed to deserialize OpenAPI spec");

        let (models, _, _) = parse_openapi(&openapi_spec).expect("Failed to parse OpenAPI spec");

        let user_model = models.iter().find(|m| m.name() == "User");
        assert!(user_model.is_some());

        match user_model.unwrap() {
            ModelType::Struct(model) => {
                // Empty array should result in None
                assert!(model.custom_attrs.is_none());
            }
            _ => panic!("Expected Struct"),
        }
    }

    #[test]
    fn test_x_rust_type_on_string_property() {
        let openapi_spec: OpenAPI = serde_json::from_value(json!({
            "openapi": "3.0.0",
            "info": { "title": "Test API", "version": "1.0.0" },
            "paths": {},
            "components": {
                "schemas": {
                    "Document": {
                        "type": "object",
                        "description": "Document with custom version type",
                        "properties": {
                            "title": { "type": "string", "description": "Document title." },
                            "content": { "type": "string", "description": "Document content." },
                            "version": {
                                "type": "string",
                                "format": "semver",
                                "x-rust-type": "semver::Version",
                                "description": "Semantic version."
                            }
                        },
                        "required": ["title", "content", "version"]
                    }
                }
            }
        }))
        .expect("Failed to deserialize OpenAPI spec");

        let (models, _, _) = parse_openapi(&openapi_spec).expect("Failed to parse OpenAPI spec");

        let document_model = models.iter().find(|m| m.name() == "Document");
        assert!(document_model.is_some(), "Expected Document model");

        match document_model.unwrap() {
            ModelType::Struct(model) => {
                // Verify that version field has custom type
                let version_field = model.fields.iter().find(|f| f.name == "version");
                assert!(version_field.is_some(), "Expected version field");
                assert_eq!(version_field.unwrap().field_type, "semver::Version");

                // Verify other fields have regular types
                let title_field = model.fields.iter().find(|f| f.name == "title");
                assert_eq!(title_field.unwrap().field_type, "String");

                let content_field = model.fields.iter().find(|f| f.name == "content");
                assert_eq!(content_field.unwrap().field_type, "String");
            }
            _ => panic!("Expected Struct"),
        }
    }

    #[test]
    fn test_x_rust_type_on_integer_property() {
        let openapi_spec: OpenAPI = serde_json::from_value(json!({
            "openapi": "3.0.0",
            "info": { "title": "Test API", "version": "1.0.0" },
            "paths": {},
            "components": {
                "schemas": {
                    "Configuration": {
                        "type": "object",
                        "description": "Configuration with custom duration type",
                        "properties": {
                            "timeout": {
                                "type": "integer",
                                "x-rust-type": "std::time::Duration",
                                "description": "Timeout duration."
                            },
                            "retries": { "type": "integer" }
                        },
                        "required": ["timeout", "retries"]
                    }
                }
            }
        }))
        .expect("Failed to deserialize OpenAPI spec");

        let (models, _, _) = parse_openapi(&openapi_spec).expect("Failed to parse OpenAPI spec");

        let config_model = models.iter().find(|m| m.name() == "Configuration");
        assert!(config_model.is_some(), "Expected Configuration model");

        match config_model.unwrap() {
            ModelType::Struct(model) => {
                // Verify that timeout field has custom type
                let timeout_field = model.fields.iter().find(|f| f.name == "timeout");
                assert!(timeout_field.is_some(), "Expected timeout field");
                assert_eq!(timeout_field.unwrap().field_type, "std::time::Duration");

                // Verify other field has regular i64 type
                let retries_field = model.fields.iter().find(|f| f.name == "retries");
                assert_eq!(retries_field.unwrap().field_type, "i64");
            }
            _ => panic!("Expected Struct"),
        }
    }

    #[test]
    fn test_x_rust_type_on_number_property() {
        let openapi_spec: OpenAPI = serde_json::from_value(json!({
            "openapi": "3.0.0",
            "info": { "title": "Test API", "version": "1.0.0" },
            "paths": {},
            "components": {
                "schemas": {
                    "Product": {
                        "type": "object",
                        "description": "Product with custom decimal type",
                        "properties": {
                            "price": {
                                "type": "number",
                                "x-rust-type": "decimal::Decimal",
                                "description": "Product price."
                            },
                            "quantity": { "type": "number" }
                        },
                        "required": ["price", "quantity"]
                    }
                }
            }
        }))
        .expect("Failed to deserialize OpenAPI spec");

        let (models, _, _) = parse_openapi(&openapi_spec).expect("Failed to parse OpenAPI spec");

        let product_model = models.iter().find(|m| m.name() == "Product");
        assert!(product_model.is_some(), "Expected Product model");

        match product_model.unwrap() {
            ModelType::Struct(model) => {
                // Verify that price field has custom type
                let price_field = model.fields.iter().find(|f| f.name == "price");
                assert!(price_field.is_some(), "Expected price field");
                assert_eq!(price_field.unwrap().field_type, "decimal::Decimal");

                // Verify other field has regular f64 type
                let quantity_field = model.fields.iter().find(|f| f.name == "quantity");
                assert_eq!(quantity_field.unwrap().field_type, "f64");
            }
            _ => panic!("Expected Struct"),
        }
    }

    #[test]
    fn test_x_rust_type_on_nullable_property() {
        let openapi_spec: OpenAPI = serde_json::from_value(json!({
            "openapi": "3.0.0",
            "info": { "title": "Test API", "version": "1.0.0" },
            "paths": {},
            "components": {
                "schemas": {
                    "Settings": {
                        "type": "object",
                        "description": "Settings with nullable custom type",
                        "properties": {
                            "settings": {
                                "type": "string",
                                "x-rust-type": "serde_json::Value",
                                "nullable": true,
                                "description": "Optional settings."
                            }
                        }
                    }
                }
            }
        }))
        .expect("Failed to deserialize OpenAPI spec");

        let (models, _, _) = parse_openapi(&openapi_spec).expect("Failed to parse OpenAPI spec");

        let settings_model = models.iter().find(|m| m.name() == "Settings");
        assert!(settings_model.is_some(), "Expected Settings model");

        match settings_model.unwrap() {
            ModelType::Struct(model) => {
                let settings_field = model.fields.iter().find(|f| f.name == "settings");
                assert!(settings_field.is_some(), "Expected settings field");

                let field = settings_field.unwrap();
                assert_eq!(field.field_type, "serde_json::Value");
                assert!(field.is_nullable, "Expected field to be nullable");
            }
            _ => panic!("Expected Struct"),
        }
    }

    #[test]
    fn test_multiple_properties_with_x_rust_type() {
        let openapi_spec: OpenAPI = serde_json::from_value(json!({
            "openapi": "3.0.0",
            "info": { "title": "Test API", "version": "1.0.0" },
            "paths": {},
            "components": {
                "schemas": {
                    "ComplexModel": {
                        "type": "object",
                        "description": "Model with multiple custom-typed properties",
                        "properties": {
                            "id": {
                                "type": "string",
                                "format": "uuid",
                                "x-rust-type": "uuid::Uuid"
                            },
                            "price": {
                                "type": "number",
                                "x-rust-type": "decimal::Decimal"
                            },
                            "timeout": {
                                "type": "integer",
                                "x-rust-type": "std::time::Duration"
                            },
                            "regular_field": { "type": "string" }
                        },
                        "required": ["id", "price", "timeout"]
                    }
                }
            }
        }))
        .expect("Failed to deserialize OpenAPI spec");

        let (models, _, _) = parse_openapi(&openapi_spec).expect("Failed to parse OpenAPI spec");

        let model = models.iter().find(|m| m.name() == "ComplexModel");
        assert!(model.is_some(), "Expected ComplexModel model");

        match model.unwrap() {
            ModelType::Struct(struct_model) => {
                // Verify all custom types
                let id_field = struct_model.fields.iter().find(|f| f.name == "id");
                assert_eq!(id_field.unwrap().field_type, "uuid::Uuid");

                let price_field = struct_model.fields.iter().find(|f| f.name == "price");
                assert_eq!(price_field.unwrap().field_type, "decimal::Decimal");

                let timeout_field = struct_model.fields.iter().find(|f| f.name == "timeout");
                assert_eq!(timeout_field.unwrap().field_type, "std::time::Duration");

                // Verify regular field
                let regular_field = struct_model
                    .fields
                    .iter()
                    .find(|f| f.name == "regular_field");
                assert_eq!(regular_field.unwrap().field_type, "String");

                // Verify nullable flags for required/optional fields
                assert!(!id_field.unwrap().is_nullable, "id should not be nullable");
                assert!(
                    !price_field.unwrap().is_nullable,
                    "price should not be nullable"
                );
                assert!(
                    !timeout_field.unwrap().is_nullable,
                    "timeout should not be nullable"
                );
                // regular_field is not in required, but generator doesn't mark it as nullable
                // (this is expected behavior - nullable only for explicitly nullable fields)
            }
            _ => panic!("Expected Struct"),
        }
    }

    #[test]
    fn test_x_rust_attrs_on_field() {
        let openapi_spec: OpenAPI = serde_json::from_value(json!({
            "openapi": "3.0.0",
            "info": { "title": "Test API", "version": "1.0.0" },
            "paths": {},
            "components": {
                "schemas": {
                    "FrontendEvent": {
                        "type": "object",
                        "properties": {
                            "field": {
                                "type": "integer",
                                "minimum": 0,
                                "maximum": 100,
                                "nullable": true,
                                "x-rust-attrs": ["#[validate(range(min = 0, max = 100))]"]
                            },
                            "name": { "type": "string" }
                        }
                    }
                }
            }
        }))
        .expect("Failed to deserialize OpenAPI spec");

        let (models, _, _) = parse_openapi(&openapi_spec).expect("Failed to parse OpenAPI spec");

        let model = models.iter().find(|m| m.name() == "FrontendEvent");
        assert!(model.is_some(), "Expected FrontendEvent model");

        match model.unwrap() {
            ModelType::Struct(struct_model) => {
                let field = struct_model.fields.iter().find(|f| f.name == "field");
                assert!(field.is_some(), "Expected progress_percent field");
                let field = field.unwrap();
                assert_eq!(field.field_type, "i64");
                assert!(
                    field.custom_attrs.is_some(),
                    "Expected field-level x-rust-attrs"
                );
                let attrs = field.custom_attrs.as_ref().unwrap();
                assert_eq!(attrs.len(), 1);
                assert_eq!(attrs[0], "#[validate(range(min = 0, max = 100))]");

                let name_field = struct_model.fields.iter().find(|f| f.name == "name");
                assert!(name_field.unwrap().custom_attrs.is_none());
            }
            _ => panic!("Expected Struct"),
        }
    }
}
