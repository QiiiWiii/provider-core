use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, SeqAccess, Visitor},
};

pub(super) enum JsonShape {
    Object(Vec<(String, JsonShape)>),
    Array(Vec<JsonShape>),
    Scalar,
}

impl<'de> Deserialize<'de> for JsonShape {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(JsonShapeVisitor)
    }
}

struct JsonShapeVisitor;

impl<'de> Visitor<'de> for JsonShapeVisitor {
    type Value = JsonShape;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("JSON")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some((key, value)) = map.next_entry()? {
            values.push((key, value));
        }
        Ok(JsonShape::Object(values))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element()? {
            values.push(value);
        }
        Ok(JsonShape::Array(values))
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(JsonShape::Scalar)
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(JsonShape::Scalar)
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(JsonShape::Scalar)
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(JsonShape::Scalar)
    }

    fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
        Ok(JsonShape::Scalar)
    }

    fn visit_string<E>(self, _value: String) -> Result<Self::Value, E> {
        Ok(JsonShape::Scalar)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(JsonShape::Scalar)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(JsonShape::Scalar)
    }
}

pub(super) fn keys_match(value: &JsonShape, expected: &[&str]) -> bool {
    let JsonShape::Object(values) = value else {
        return false;
    };
    values
        .iter()
        .map(|(key, _)| key.as_str())
        .eq(expected.iter().copied())
}

pub(super) fn object_value<'a>(value: &'a JsonShape, key: &str) -> Option<&'a JsonShape> {
    let JsonShape::Object(values) = value else {
        return None;
    };
    values
        .iter()
        .find_map(|(candidate, value)| (candidate == key).then_some(value))
}

pub(super) fn unique_object_value<'a>(value: &'a JsonShape, key: &str) -> Option<&'a JsonShape> {
    let JsonShape::Object(values) = value else {
        return None;
    };
    let mut matches = values
        .iter()
        .filter_map(|(candidate, value)| (candidate == key).then_some(value));
    let value = matches.next()?;
    matches.next().is_none().then_some(value)
}

pub(super) fn first_array_item(value: &JsonShape) -> Option<&JsonShape> {
    array_items(value)?.first()
}

pub(super) fn array_items(value: &JsonShape) -> Option<&[JsonShape]> {
    let JsonShape::Array(values) = value else {
        return None;
    };
    Some(values)
}
