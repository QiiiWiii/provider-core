use serde_json::Value;

pub(super) fn error_markers(value: &Value) -> Vec<String> {
    let mut markers = Vec::new();
    collect_error_markers(value, &mut markers);
    markers
}

fn collect_error_markers(value: &Value, markers: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if matches!(key.as_str(), "code" | "status" | "reason" | "message") {
                    match value {
                        Value::String(value) => markers.push(value.to_ascii_lowercase()),
                        Value::Number(value) => markers.push(value.to_string()),
                        _ => {}
                    }
                }
                collect_error_markers(value, markers);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_error_markers(value, markers);
            }
        }
        _ => {}
    }
}
