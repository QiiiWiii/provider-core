use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use provider_core::ProviderError;
use serde_json::{Map, Value, json};

use super::validation::invalid;

pub(super) fn convert_content(
    value: Option<&Value>,
    role: &str,
) -> Result<Vec<Value>, ProviderError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    match value {
        Value::String(text) => Ok((!text.is_empty())
            .then(|| json!({"text": text}))
            .into_iter()
            .collect()),
        Value::Array(values) => values
            .iter()
            .map(|value| convert_content_part(value, role))
            .collect(),
        Value::Null => Ok(Vec::new()),
        _ => Err(invalid("OpenAI Responses message content is invalid")),
    }
}

pub(super) fn convert_content_part(value: &Value, role: &str) -> Result<Value, ProviderError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid("OpenAI Responses content parts must be objects"))?;
    match object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "text" | "input_text" | "output_text" | "refusal" => Ok(json!({
            "text": object.get("text").and_then(Value::as_str).unwrap_or_default()
        })),
        "input_image" | "image_url" => convert_image_part(object),
        "input_audio" | "audio" => convert_audio_part(object),
        "input_file" | "file" | "input_video" | "video" => convert_file_part(object),
        _ if role == "system" => Err(invalid("unsupported OpenAI Responses instruction part")),
        _ => Err(invalid("unsupported OpenAI Responses content part")),
    }
}

pub(super) fn convert_image_part(object: &Map<String, Value>) -> Result<Value, ProviderError> {
    let image = object
        .get("image_url")
        .or_else(|| object.get("url"))
        .and_then(|value| value.as_str().or_else(|| value.get("url")?.as_str()))
        .or_else(|| object.get("file_id").and_then(Value::as_str))
        .ok_or_else(|| invalid("OpenAI Responses image part requires an image URL"))?;
    convert_media_value(image, "image/png", "image")
}

pub(super) fn convert_audio_part(object: &Map<String, Value>) -> Result<Value, ProviderError> {
    let value = object
        .get("data")
        .or_else(|| object.get("audio_url"))
        .or_else(|| object.get("url"))
        .and_then(|value| value.as_str().or_else(|| value.get("url")?.as_str()))
        .ok_or_else(|| invalid("OpenAI Responses audio part requires data or a URL"))?;
    let format = object
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let default_mime = audio_mime_type(format);
    if value.starts_with("data:") {
        return convert_media_value(value, default_mime, "audio");
    }
    if object.get("data").is_some() {
        return Ok(json!({
            "inlineData": {"mimeType": default_mime, "data": value}
        }));
    }
    Ok(json!({"fileData": {"fileUri": value}}))
}

pub(super) fn convert_file_part(object: &Map<String, Value>) -> Result<Value, ProviderError> {
    let value = object
        .get("file_data")
        .or_else(|| object.get("file_url"))
        .or_else(|| object.get("video_url"))
        .or_else(|| object.get("url"))
        .or_else(|| object.get("file_id"))
        .and_then(|value| value.as_str().or_else(|| value.get("url")?.as_str()))
        .ok_or_else(|| invalid("OpenAI Responses file part requires a URL or file data"))?;
    let mime_type = object
        .get("mime_type")
        .or_else(|| object.get("media_type"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("application/octet-stream");
    convert_media_value(value, mime_type, "file")
}

pub(super) fn convert_media_value(
    value: &str,
    default_mime: &str,
    kind: &str,
) -> Result<Value, ProviderError> {
    if let Some((mime_type, data)) = value.strip_prefix("data:").and_then(decode_data_url) {
        return Ok(json!({"inlineData": {"mimeType": mime_type, "data": data}}));
    }
    if value.trim().is_empty() {
        return Err(invalid(match kind {
            "audio" => "OpenAI Responses audio part is empty",
            "file" => "OpenAI Responses file part is empty",
            _ => "OpenAI Responses image part is empty",
        }));
    }
    if kind == "audio" && value.starts_with("base64,") {
        return Ok(json!({
            "inlineData": {"mimeType": default_mime, "data": value.trim_start_matches("base64,")}
        }));
    }
    Ok(json!({"fileData": {"fileUri": value}}))
}

pub(super) fn audio_mime_type(format: &str) -> &str {
    match format.trim().to_ascii_lowercase().as_str() {
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        "webm" => "audio/webm",
        "pcm16" => "audio/pcm",
        "g711_ulaw" | "g711_alaw" => "audio/basic",
        _ => "audio/wav",
    }
}

pub(super) fn decode_data_url(value: &str) -> Option<(&str, String)> {
    let (metadata, encoded) = value.split_once(',')?;
    let mime_type = metadata
        .strip_prefix("data:")
        .unwrap_or(metadata)
        .split(';')
        .next()?
        .trim();
    let encoded = encoded.trim();
    let data = if metadata.contains(";base64") {
        BASE64.decode(encoded).ok()?
    } else {
        encoded.as_bytes().to_vec()
    };
    Some((mime_type, BASE64.encode(data)))
}
