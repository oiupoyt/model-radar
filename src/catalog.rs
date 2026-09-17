use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Error;

const ENDPOINT: &str = "https://openrouter.ai/api/v1/models";
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub context_length: u64,
    #[serde(default, deserialize_with = "deserialize_pricing")]
    pub pricing: BTreeMap<String, String>,
}

fn deserialize_pricing<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = BTreeMap::<String, serde_json::Value>::deserialize(deserializer)?;
    raw.into_iter()
        .map(|(key, value)| {
            if key == "overrides" && value.is_array() {
                let empty = value.as_array().is_some_and(Vec::is_empty);
                return Ok((
                    key,
                    if empty {
                        "0".into()
                    } else {
                        "tiered pricing".into()
                    },
                ));
            }
            let value = value.as_str().ok_or_else(|| {
                serde::de::Error::custom("expected a pricing string or overrides array")
            })?;
            Ok((key, value.to_owned()))
        })
        .collect()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub model: Model,
    pub kind: String,
}

fn is_decimal_zero(raw: &str) -> bool {
    let body = raw.strip_prefix(['+', '-']).unwrap_or(raw);
    let mantissa = if let Some((mantissa, exponent)) = body.split_once(['e', 'E']) {
        let exponent = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        if exponent.is_empty() || !exponent.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        mantissa
    } else {
        body
    };
    let mut seen_dot = false;
    let mut digits = 0usize;
    mantissa.bytes().all(|b| match b {
        b'.' if !seen_dot => {
            seen_dot = true;
            true
        }
        b'0' => {
            digits += 1;
            true
        }
        _ => false,
    }) && digits > 0
}

impl Model {
    pub fn is_free(&self) -> bool {
        if !is_decimal_zero(
            self.pricing
                .get("prompt")
                .map(String::as_str)
                .unwrap_or_default(),
        ) || !is_decimal_zero(
            self.pricing
                .get("completion")
                .map(String::as_str)
                .unwrap_or_default(),
        ) {
            return false;
        }
        self.pricing.values().all(|value| is_decimal_zero(value))
    }
}

fn validate(catalog: &BTreeMap<String, Model>) -> Result<(), Error> {
    if catalog.is_empty() {
        return Err("catalog is empty".into());
    }
    for (key, model) in catalog {
        if model.id.trim().is_empty() || model.name.trim().is_empty() {
            return Err("model identity is empty".into());
        }
        if key != &model.id {
            return Err(
                format!("catalog key {key:?} does not match model id {:?}", model.id).into(),
            );
        }
    }
    Ok(())
}

fn parse_catalog(bytes: &[u8]) -> Result<BTreeMap<String, Model>, Error> {
    #[derive(Deserialize)]
    struct Response {
        data: Vec<Model>,
    }
    let response: Response = serde_json::from_slice(bytes)?;
    let mut catalog = BTreeMap::new();
    for model in response.data {
        if model.id.trim().is_empty() || model.name.trim().is_empty() {
            return Err("model identity is empty".into());
        }
        if catalog.insert(model.id.clone(), model).is_some() {
            return Err("duplicate model id".into());
        }
    }
    validate(&catalog)?;
    Ok(catalog)
}

pub async fn fetch(client: &reqwest::Client) -> Result<BTreeMap<String, Model>, Error> {
    let mut response = client.get(ENDPOINT).send().await?.error_for_status()?;
    let mut payload: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if chunk.len() > MAX_RESPONSE_BYTES - payload.len() {
            return Err("response exceeds 8 MiB limit".into());
        }
        payload.extend_from_slice(&chunk);
    }
    parse_catalog(&payload)
}

pub fn changes(
    previous: &BTreeMap<String, Model>,
    current: &BTreeMap<String, Model>,
) -> Vec<Event> {
    let mut events = Vec::new();
    for (id, model) in current {
        match previous.get(id) {
            None => events.push(Event {
                model: model.clone(),
                kind: "new".to_string(),
            }),
            Some(old) if !old.is_free() && model.is_free() => events.push(Event {
                model: model.clone(),
                kind: "paid_to_free".to_string(),
            }),
            _ => {}
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, pricing: &[(&str, &str)]) -> Model {
        Model {
            id: id.to_string(),
            name: id.to_string(),
            description: String::new(),
            context_length: 4096,
            pricing: pricing
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    fn map(models: Vec<Model>) -> BTreeMap<String, Model> {
        models.into_iter().map(|m| (m.id.clone(), m)).collect()
    }

    #[test]
    fn free_requires_exact_decimal_zero() {
        assert!(model("a", &[("prompt", "0"), ("completion", "0")]).is_free());
        assert!(model("a", &[("prompt", "0.0"), ("completion", "-0")]).is_free());
        assert!(model("a", &[("prompt", "+0.00000"), ("completion", "0")]).is_free());
        assert!(
            !model(
                "a",
                &[("prompt", "0.00000000000000000001"), ("completion", "0")]
            )
            .is_free()
        );
        assert!(!model("a", &[("prompt", "1e-9"), ("completion", "0")]).is_free());
        assert!(!model("a", &[("prompt", ""), ("completion", "0")]).is_free());
        assert!(!model("a", &[("prompt", "abc"), ("completion", "0")]).is_free());
        assert!(
            !model(
                "a",
                &[("prompt", "0"), ("completion", "0"), ("request", "0.1")]
            )
            .is_free()
        );
        assert!(!model("a", &[]).is_free());
    }

    #[test]
    fn decimal_zero_edge_cases() {
        for value in ["0", "00", "-0", "+0.00", "0e-9999", "0E+9999", ".0", "0."] {
            assert!(
                model("a", &[("prompt", value), ("completion", value)]).is_free(),
                "{value}"
            );
        }
        for value in [
            "1e-9999", "-1e-9999", "NaN", "inf", "-1", ".", "+", "-", "", "0e", "0e+", "0e1.0",
            "0e0e0", "0..0", "--0", "0x0", " 0", "0 ", "０",
        ] {
            assert!(
                !model("a", &[("prompt", value), ("completion", "0")]).is_free(),
                "{value}"
            );
            assert!(
                !model("a", &[("prompt", "0"), ("completion", value)]).is_free(),
                "{value}"
            );
            assert!(
                !model(
                    "a",
                    &[("prompt", "0"), ("completion", "0"), ("unknown", value)]
                )
                .is_free(),
                "{value}"
            );
        }
        let tiny = format!("0.{}1", "0".repeat(400));
        assert!(!model("a", &[("prompt", &tiny), ("completion", "0")]).is_free());
        assert!(!model("a", &[("prompt", "0")]).is_free());
        assert!(!model("a", &[("completion", "0")]).is_free());
        assert!(
            model(
                "a",
                &[("prompt", "0"), ("completion", "0"), ("unknown", "0.0")]
            )
            .is_free()
        );
    }

    #[test]
    fn changes_detects_new_and_paid_to_free() {
        let prev = map(vec![
            model("a", &[("prompt", "1"), ("completion", "1")]),
            model("b", &[("prompt", "0"), ("completion", "0")]),
        ]);
        let cur = map(vec![
            model("a", &[("prompt", "0"), ("completion", "0")]),
            model("b", &[("prompt", "0"), ("completion", "0")]),
            model("c", &[("prompt", "0"), ("completion", "0")]),
            model("d", &[("prompt", "2"), ("completion", "3")]),
        ]);
        let events = changes(&prev, &cur);
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, vec!["paid_to_free", "new", "new"]);
        assert_eq!(events[0].model.id, "a");
        assert_eq!(events[1].model.id, "c");
        assert_eq!(events[2].model.id, "d");
        assert!(!events[2].model.is_free());
    }

    #[test]
    fn changes_ignores_removals_and_unchanged() {
        let prev = map(vec![model("a", &[("prompt", "0"), ("completion", "0")])]);
        let cur = BTreeMap::new();
        assert!(changes(&prev, &cur).is_empty());
        assert_eq!(changes(&cur, &prev).len(), 1);
        assert!(changes(&prev, &prev).is_empty());
        let paid = map(vec![model("a", &[("prompt", "1"), ("completion", "1")])]);
        assert!(changes(&prev, &paid).is_empty());
        assert!(changes(&paid, &paid).is_empty());
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(parse_catalog(b"{\"data\":[]}").is_err());
        assert!(parse_catalog(b"{}").is_err());
    }

    #[test]
    fn parse_rejects_malformed() {
        for body in [
            "not json",
            r#"{"data":null}"#,
            r#"{"data":{}}"#,
            r#"{"data":[{"id":"","name":"A"}]}"#,
            r#"{"data":[{"id":"  ","name":"A"}]}"#,
            r#"{"data":[{"id":"a","name":"  "}]}"#,
            r#"{"data":[{"name":"A"}]}"#,
            r#"{"data":[{"id":"a"}]}"#,
            r#"{"data":[{"id":1,"name":"A"}]}"#,
            r#"{"data":[{"id":"a","name":"A","context_length":-1}]}"#,
            r#"{"data":[{"id":"a","name":"A","pricing":{"prompt":0}}]}"#,
            r#"{"data":[{"id":"a","name":"A","description":null}]}"#,
            r#"{"data":[{"id":"a","name":"A"}]} trailing"#,
        ] {
            assert!(parse_catalog(body.as_bytes()).is_err(), "{body}");
        }
    }

    #[test]
    fn tiered_pricing_is_accepted_but_not_assumed_free() {
        let body = br#"{"data":[{"id":"a","name":"A","pricing":{"prompt":"0","completion":"0","overrides":[{"min_prompt_tokens":100,"prompt":"1"}]}}]}"#;
        let catalog = parse_catalog(body).unwrap();
        assert!(!catalog["a"].is_free());
        let saved = serde_json::to_vec(&catalog["a"]).unwrap();
        let restored: Model = serde_json::from_slice(&saved).unwrap();
        assert!(!restored.is_free());
        let body = br#"{"data":[{"id":"a","name":"A","pricing":{"prompt":"0","completion":"0","overrides":[]}}]}"#;
        assert!(parse_catalog(body).unwrap()["a"].is_free());
    }

    #[test]
    fn parse_rejects_duplicates() {
        let body = r#"{"data":[{"id":"a","name":"A"},{"id":"a","name":"B"}]}"#;
        assert!(parse_catalog(body.as_bytes()).is_err());
    }

    #[test]
    fn parse_accepts_valid() {
        let body = r#"{"data":[{"id":"a","name":"A","context_length":8192,"pricing":{"prompt":"0","completion":"0"}}]}"#;
        let catalog = parse_catalog(body.as_bytes()).unwrap();
        assert_eq!(catalog.len(), 1);
        assert!(catalog["a"].is_free());
        assert_eq!(catalog["a"].context_length, 8192);
    }

    #[test]
    fn parse_defaults_optional_metadata_and_prices() {
        let catalog = parse_catalog(br#"{"data":[{"id":"a","name":"A"}]}"#).unwrap();
        assert_eq!(catalog["a"].description, "");
        assert_eq!(catalog["a"].context_length, 0);
        assert!(catalog["a"].pricing.is_empty());
        assert!(!catalog["a"].is_free());
    }

    #[test]
    fn validate_rejects_empty_catalog() {
        assert!(validate(&BTreeMap::new()).is_err());
    }
}
