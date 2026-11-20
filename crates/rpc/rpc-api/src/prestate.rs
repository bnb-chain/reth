//! Go-compatible prestate configuration at the raw JSON-RPC boundary.
//!
//! Repeated keys must be folded before `serde_json::Value` discards their occurrence order.
//! Typed callers use the same validator, although already-decoded values cannot recover that order.

use jsonrpsee::{
    core::server::{MethodCallback, MethodResponse},
    types::{
        error::{CALL_EXECUTION_FAILED_CODE, INTERNAL_ERROR_CODE},
        ErrorObjectOwned, Params,
    },
    RpcModule,
};
use serde::{
    de::{MapAccess, Visitor},
    Deserialize, Deserializer,
};
use serde_json::{
    value::{to_raw_value, RawValue},
    Value,
};
use std::{collections::BTreeMap, fmt, sync::Arc};

const MAX_TRACER_CONFIG_DEPTH: usize = 128;

/// Normalize Go's case-insensitive boolean fields before Alloy constructs the actual tracer.
pub fn normalize_prestate_config(config: &mut Value) -> Result<(), ErrorObjectOwned> {
    let object = match config {
        serde_json::Value::Object(object) => object,
        serde_json::Value::Null => return Ok(()),
        value => {
            let kind = match value {
                serde_json::Value::Bool(_) => "bool",
                serde_json::Value::Number(_) => "number",
                serde_json::Value::String(_) => "string",
                _ => "array",
            };
            return Err(ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                format!("json: cannot unmarshal {kind} into Go value of type native.PrestateTracerConfig"),
                None::<()>,
            ))
        }
    };
    *object = normalize_entries(std::mem::take(object))?;
    Ok(())
}

fn normalize_entries(
    entries: impl IntoIterator<Item = (String, Value)>,
) -> Result<serde_json::Map<String, Value>, ErrorObjectOwned> {
    let mut normalized = serde_json::Map::new();
    for (key, value) in entries {
        let Some(field) = ["includeEmpty", "diffMode", "disableCode", "disableStorage"]
            .into_iter()
            .find(|field| key.eq_ignore_ascii_case(field))
        else {
            normalized.insert(key, value);
            continue
        };
        match value {
            serde_json::Value::Bool(_) => {
                normalized.insert(field.to_owned(), value);
            }
            serde_json::Value::Null => {}
            value => {
                let kind = match value {
                    serde_json::Value::String(_) => "string",
                    serde_json::Value::Number(_) => "number",
                    serde_json::Value::Array(_) => "array",
                    _ => "object",
                };
                return Err(ErrorObjectOwned::owned(
                    CALL_EXECUTION_FAILED_CODE,
                    format!("json: cannot unmarshal {kind} into Go struct field PrestateTracerConfig.{field} of type bool"),
                    None::<()>,
                ))
            }
        }
    }
    Ok(normalized)
}

/// Installs raw prestate-option decoding on the debug methods present in a module.
///
/// Wrap the final module before serving it, including any chain-specific debug replacements.
/// Other tracers and parameters retain their existing decoders and errors. Callbacks keep their
/// request IDs, connection metadata, response limits and extensions.
pub fn with_prestate_config<C: Send + Sync + 'static>(mut module: RpcModule<C>) -> RpcModule<C> {
    for (name, index) in [
        ("debug_traceBlock", 1),
        ("debug_traceBlockByHash", 1),
        ("debug_traceBlockByNumber", 1),
        ("debug_traceTransaction", 1),
        ("debug_traceCall", 2),
        ("debug_traceCallMany", 2),
        ("debug_traceBadBlock", 1),
        ("debug_subscribe", 3),
    ] {
        let Some(callback) = module.remove_method(name) else { continue };
        let callback = match callback {
            MethodCallback::Sync(inner) => {
                MethodCallback::Sync(Arc::new(move |id, params, max, extensions| {
                    match normalize_params(params, index) {
                        Ok(params) => inner(id, params, max, extensions),
                        Err(error) => MethodResponse::error(id, error).with_extensions(extensions),
                    }
                }))
            }
            MethodCallback::Async(inner) => {
                MethodCallback::Async(Arc::new(move |id, params, connection, max, extensions| {
                    match normalize_params(params, index) {
                        Ok(params) => inner(id, params, connection, max, extensions),
                        Err(error) => Box::pin(std::future::ready(
                            MethodResponse::error(id, error).with_extensions(extensions),
                        )),
                    }
                }))
            }
            MethodCallback::Subscription(inner) => MethodCallback::Subscription(Arc::new(
                move |id, params, sink, state, extensions| match normalize_params(params, index) {
                    Ok(params) => inner(id, params, sink, state, extensions),
                    Err(error) => Box::pin(std::future::ready(
                        MethodResponse::error(id, error).with_extensions(extensions),
                    )),
                },
            )),
            callback => callback,
        };
        assert!(module.verify_and_insert(name, callback).is_ok(), "removed method is vacant");
    }
    module
}

fn normalize_params(params: Params<'_>, index: usize) -> Result<Params<'static>, ErrorObjectOwned> {
    let Some(raw) = params.as_str() else { return Ok(params.into_owned()) };
    // Parse only raw containers here; the registered method still validates parameter types.
    let encoded = if params.is_object() {
        let Ok(mut fields) = serde_json::from_str::<BTreeMap<String, Box<RawValue>>>(raw) else {
            return Ok(params.into_owned())
        };
        if let Some(options) = fields.get_mut("opts") {
            normalize_options(options)?;
        }
        serde_json::to_string(&fields)
    } else {
        let Ok(mut fields) = serde_json::from_str::<Vec<Box<RawValue>>>(raw) else {
            return Ok(params.into_owned())
        };
        if let Some(options) = fields.get_mut(index) {
            normalize_options(options)?;
        }
        serde_json::to_string(&fields)
    };
    let encoded = encoded.map_err(serialization_error)?;
    Ok(Params::new(Some(&encoded)).into_owned())
}

fn normalize_options(raw: &mut Box<RawValue>) -> Result<(), ErrorObjectOwned> {
    let Ok(mut fields) = serde_json::from_str::<BTreeMap<String, Box<RawValue>>>(raw.get()) else {
        return Ok(())
    };
    let Some(tracer) = fields.get("tracer") else { return Ok(()) };
    let Ok(tracer) = serde_json::from_str::<String>(tracer.get()) else { return Ok(()) };
    if let Some(config) = fields.get_mut("tracerConfig") {
        normalize_raw_config(&tracer, config, MAX_TRACER_CONFIG_DEPTH)?;
    }
    *raw = to_raw_value(&fields).map_err(serialization_error)?;
    Ok(())
}

fn normalize_raw_config(
    tracer: &str,
    raw: &mut Box<RawValue>,
    remaining_depth: usize,
) -> Result<(), ErrorObjectOwned> {
    // Let the ordinary serde decoder reject excessive nesting; do not recurse beyond its default
    // limit while traversing RawValue containers (which defer ordinary value decoding).
    if remaining_depth == 0 {
        return Ok(())
    }
    if tracer == "prestateTracer" {
        let normalized = match serde_json::from_str::<OrderedFields>(raw.get()) {
            Ok(fields) => Value::Object(normalize_entries(fields.0)?),
            Err(_) => {
                let Ok(mut value) = serde_json::from_str::<Value>(raw.get()) else { return Ok(()) };
                normalize_prestate_config(&mut value)?;
                value
            }
        };
        *raw = to_raw_value(&normalized).map_err(serialization_error)?;
    } else if tracer == "muxTracer" {
        let Ok(mut fields) = serde_json::from_str::<BTreeMap<String, Box<RawValue>>>(raw.get())
        else {
            return Ok(())
        };
        for (tracer, config) in &mut fields {
            normalize_raw_config(tracer, config, remaining_depth - 1)?;
        }
        *raw = to_raw_value(&fields).map_err(serialization_error)?;
    }
    Ok(())
}

fn serialization_error(error: serde_json::Error) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(INTERNAL_ERROR_CODE, error.to_string(), None::<()>)
}

struct OrderedFields(Vec<(String, Value)>);

impl<'de> Deserialize<'de> for OrderedFields {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldsVisitor;
        impl<'de> Visitor<'de> for FieldsVisitor {
            type Value = OrderedFields;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a prestate configuration object")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut fields = Vec::new();
                while let Some(field) = map.next_entry()? {
                    fields.push(field);
                }
                Ok(OrderedFields(fields))
            }
        }
        deserializer.deserialize_map(FieldsVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn raw_keys_are_folded_in_occurrence_order_for_all_boolean_fields() {
        for field in ["includeEmpty", "diffMode", "disableCode", "disableStorage"] {
            for (tail, expected) in [("true", true), ("null", false)] {
                let config = format!(
                    r#"{{"{field}":true,"{}":false,"{field}":{tail}}}"#,
                    field.to_uppercase()
                );
                for mux in [false, true] {
                    let (tracer, config) = if mux {
                        ("muxTracer", format!(r#"{{"prestateTracer":{config}}}"#))
                    } else {
                        ("prestateTracer", config.clone())
                    };
                    let mut raw = RawValue::from_string(config).unwrap();
                    normalize_raw_config(tracer, &mut raw, MAX_TRACER_CONFIG_DEPTH).unwrap();
                    let result: Value = serde_json::from_str(raw.get()).unwrap();
                    let result = if mux { &result["prestateTracer"] } else { &result };
                    assert_eq!(result, &json!({(field):expected}));
                }
            }
        }
    }

    #[test]
    fn invalid_earlier_values_are_not_hidden_by_later_duplicates() {
        let mut raw =
            RawValue::from_string(r#"{"includeEmpty":1,"includeEmpty":true}"#.into()).unwrap();
        let error =
            normalize_raw_config("prestateTracer", &mut raw, MAX_TRACER_CONFIG_DEPTH).unwrap_err();
        assert_eq!(error.code(), CALL_EXECUTION_FAILED_CODE);
        assert_eq!(error.message(), "json: cannot unmarshal number into Go struct field PrestateTracerConfig.includeEmpty of type bool");
    }

    #[test]
    fn named_positional_and_other_tracer_parameters_keep_their_decoders() {
        let options = r#"{"tracer":"prestateTracer","tracerConfig":{"includeEmpty":true,"IncludeEmpty":false,"includeEmpty":true}}"#;
        for index in [1, 2, 3] {
            let raw = format!("[{}{options}]", "null,".repeat(index));
            let params = normalize_params(Params::new(Some(&raw)), index).unwrap();
            let values: Value = serde_json::from_str(params.as_str().unwrap()).unwrap();
            assert_eq!(values[index]["tracerConfig"], json!({"includeEmpty":true}));
        }
        let raw = format!(r#"{{"opts":{options},"tx_hash":"0x1234"}}"#);
        let params = normalize_params(Params::new(Some(&raw)), 1).unwrap();
        let values: Value = serde_json::from_str(params.as_str().unwrap()).unwrap();
        assert_eq!(values["opts"]["tracerConfig"], json!({"includeEmpty":true}));
        assert_eq!(values["tx_hash"], "0x1234");
        for raw in [
            "null",
            "[]",
            "{}",
            r#"[null,{"tracer":"callTracer","tracerConfig":{"includeEmpty":"not a prestate option"}}]"#,
        ] {
            let params = normalize_params(Params::new(Some(raw)), 1).unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(params.as_str().unwrap()).unwrap(),
                serde_json::from_str::<Value>(raw).unwrap()
            );
        }
        assert!(normalize_params(Params::new(None), 1).unwrap().as_str().is_none());
    }

    #[tokio::test]
    async fn module_wrapper_preserves_ids_and_other_methods() {
        let mut module = RpcModule::new(());
        for name in ["debug_traceCall", "other"] {
            module
                .register_async_method(name, |params, _, _| async move { params.parse::<Value>() })
                .unwrap();
        }
        module
            .register_method("debug_traceTransaction", |params, _, _| params.parse::<Value>())
            .unwrap();
        let module = with_prestate_config(module);
        for (method, index, expected) in [
            ("debug_traceCall", 2, json!({"includeEmpty":true})),
            ("debug_traceTransaction", 1, json!({"includeEmpty":true})),
            ("other", 2, json!({"includeEmpty":true,"IncludeEmpty":false})),
        ] {
            let prefix = "null,".repeat(index);
            let request = format!(
                r#"{{"jsonrpc":"2.0","id":"caller-id","method":"{method}","params":[{prefix}{{"tracer":"prestateTracer","tracerConfig":{{"includeEmpty":false,"IncludeEmpty":false,"includeEmpty":true}}}}]}}"#
            );
            let (response, _) = module.raw_json_request(&request, 1).await.unwrap();
            let response: Value = serde_json::from_str(response.get()).unwrap();
            assert_eq!(response["id"], "caller-id");
            assert_eq!(response["result"][index]["tracerConfig"], expected);
        }
    }

    #[test]
    fn raw_mux_traversal_defers_excessive_nesting_to_the_rpc_decoder() {
        let mut config = r#"{"includeEmpty":true}"#.to_owned();
        for _ in 0..MAX_TRACER_CONFIG_DEPTH + 1 {
            config = format!(r#"{{"muxTracer":{config}}}"#);
        }
        let mut raw = RawValue::from_string(config).unwrap();
        normalize_raw_config("muxTracer", &mut raw, MAX_TRACER_CONFIG_DEPTH).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(raw.get()).unwrap_err().classify(),
            serde_json::error::Category::Syntax
        );
    }
}
