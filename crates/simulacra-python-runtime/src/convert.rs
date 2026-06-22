use monty::MontyObject;
use serde_json::Value;

/// Convert a MontyObject to a serde_json::Value.
pub fn monty_to_json(obj: &MontyObject) -> Value {
    match obj {
        MontyObject::None => Value::Null,
        MontyObject::Bool(b) => Value::Bool(*b),
        MontyObject::Int(n) => Value::Number(serde_json::Number::from(*n)),
        MontyObject::Float(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        MontyObject::String(s) => Value::String(s.clone()),
        MontyObject::Bytes(b) => Value::String(String::from_utf8_lossy(b).into_owned()),
        MontyObject::List(items) => Value::Array(items.iter().map(monty_to_json).collect()),
        MontyObject::Tuple(items) => Value::Array(items.iter().map(monty_to_json).collect()),
        MontyObject::Dict(pairs) => {
            let map: serde_json::Map<String, Value> = pairs
                .into_iter()
                .filter_map(|(k, v)| {
                    if let MontyObject::String(key) = k {
                        Some((key.clone(), monty_to_json(v)))
                    } else {
                        None
                    }
                })
                .collect();
            Value::Object(map)
        }
        _ => Value::String(format!("{obj:?}")),
    }
}

/// Convert a serde_json::Value to a MontyObject.
pub fn json_to_monty(val: &Value) -> MontyObject {
    match val {
        Value::Null => MontyObject::None,
        Value::Bool(b) => MontyObject::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                MontyObject::Int(i)
            } else if let Some(f) = n.as_f64() {
                MontyObject::Float(f)
            } else {
                MontyObject::None
            }
        }
        Value::String(s) => MontyObject::String(s.clone()),
        Value::Array(arr) => MontyObject::List(arr.iter().map(json_to_monty).collect()),
        Value::Object(map) => {
            let pairs: Vec<(MontyObject, MontyObject)> = map
                .iter()
                .map(|(k, v)| (MontyObject::String(k.clone()), json_to_monty(v)))
                .collect();
            MontyObject::Dict(pairs.into())
        }
    }
}
