use crate::{catalog::Model, serenity, store::State};

pub fn no_mentions() -> serenity::CreateAllowedMentions {
    serenity::CreateAllowedMentions::new()
        .all_users(false)
        .all_roles(false)
        .everyone(false)
        .replied_user(false)
}

pub fn text(value: &str, limit: usize) -> String {
    if limit == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut length = 0;
    for ch in value.chars().filter(|ch| !ch.is_control() || *ch == '\n') {
        let escape = matches!(
            ch,
            '\\' | '`' | '*' | '_' | '~' | '[' | ']' | '<' | '>' | '|' | '#' | '-'
        );
        let width = ch.len_utf16() + usize::from(escape);
        if length + width > limit.saturating_sub(1) {
            result.push('…');
            break;
        }
        if escape {
            result.push('\\');
        }
        result.push(ch);
        length += width;
    }
    if result.is_empty() {
        "—".into()
    } else {
        result
    }
}

pub fn embed(title: &str) -> serenity::CreateEmbed {
    serenity::CreateEmbed::new()
        .title(text(title, 256))
        .color(0x8367ef)
        .footer(serenity::CreateEmbedFooter::new(
            "Model Radar • Catalog prices, not unlimited access • Limits may apply",
        ))
}

pub struct Browse {
    pub free_only: bool,
    pub search: String,
    pub min_context: u32,
    pub page: u32,
    pub page_size: u32,
    pub compact: bool,
}

impl Default for Browse {
    fn default() -> Self {
        Self {
            free_only: true,
            search: String::new(),
            min_context: 0,
            page: 1,
            page_size: 10,
            compact: false,
        }
    }
}

pub fn model_line(model: &Model, compact: bool) -> String {
    let pricing = if model.is_free() {
        "Free"
    } else {
        "Paid / unknown"
    };
    if compact {
        format!(
            "{} · {} tokens · {pricing}",
            text(&model.id, 220),
            model.context_length
        )
    } else {
        format!(
            "**{}**\n{} · {} tokens · {pricing}",
            text(&model.name, 85),
            text(&model.id, 220),
            model.context_length
        )
    }
}

pub fn snapshot(state: &State, options: &Browse, poll_seconds: u64) -> serenity::CreateEmbed {
    let query = options.search.to_lowercase();
    let matches: Vec<_> = state
        .catalog
        .values()
        .filter(|model| {
            (!options.free_only || model.is_free())
                && model.context_length >= u64::from(options.min_context)
                && (model.id.to_lowercase().contains(&query)
                    || model.name.to_lowercase().contains(&query))
        })
        .collect();
    let size = options.page_size.clamp(1, 10) as usize;
    let pages = matches.len().div_ceil(size).max(1);
    let page = (options.page as usize).clamp(1, pages);
    let lines = matches
        .iter()
        .skip((page - 1) * size)
        .take(size)
        .map(|model| model_line(model, options.compact))
        .collect::<Vec<_>>()
        .join("\n\n");
    let freshness = state
        .last_success
        .map(|time| format!("Last successful catalog fetch: <t:{time}:f> (<t:{time}:R>)."))
        .unwrap_or_else(|| "No successful catalog fetch recorded.".into());
    embed("Catalog snapshot")
        .description(format!("Snapshot only; not live updating. Catalog freshness, not bot uptime or health.\n{freshness}\n\n{}", if lines.is_empty() { "No matching models." } else { &lines }))
        .field("Catalog", format!("{} total · {} free · {} matching", state.catalog.len(), state.catalog.values().filter(|model| model.is_free()).count(), matches.len()), false)
        .field("Filters", format!("Scope: {}\nSearch: {}\nMinimum context: {} tokens\nCompact: {}", if options.free_only { "Free only" } else { "All models" }, text(&options.search, 240), options.min_context, options.compact), false)
        .field("Page", format!("{page}/{pages} · {size} per page"), true)
        .field("Poll interval", format!("{poll_seconds} seconds · run /status again to refresh this snapshot"), false)
}

pub fn model_details(model: &Model, compact: bool, show_pricing: bool) -> serenity::CreateEmbed {
    let mut result = embed(&model.name)
        .field("Model ID", text(&model.id, 1024), false)
        .field("Context", format!("{} tokens", model.context_length), true)
        .field(
            "Free catalog pricing",
            if model.is_free() {
                "Yes"
            } else {
                "No / unknown"
            },
            true,
        );
    if !compact {
        result = result.description(text(&model.description, 2000));
    }
    if show_pricing {
        let prices = model
            .pricing
            .iter()
            .take(10)
            .map(|(key, value)| format!("{}: {}", text(key, 40), text(value, 45)))
            .collect::<Vec<_>>()
            .join("\n");
        result = result.field(
            "Raw USD prices (prompt/completion per token)",
            if prices.is_empty() {
                "Unavailable"
            } else {
                &prices
            },
            false,
        );
    }
    result
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::collections::BTreeMap;

    pub fn assert_embed_bounds(value: &serde_json::Value) {
        let units =
            |value: &serde_json::Value| value.as_str().unwrap_or_default().encode_utf16().count();
        assert!(units(&value["title"]) <= 256);
        assert!(units(&value["description"]) <= 4096);
        assert!(units(&value["footer"]["text"]) <= 2048);
        let mut total =
            units(&value["title"]) + units(&value["description"]) + units(&value["footer"]["text"]);
        if let Some(fields) = value["fields"].as_array() {
            assert!(fields.len() <= 25);
            for field in fields {
                assert!((1..=256).contains(&units(&field["name"])));
                assert!((1..=1024).contains(&units(&field["value"])));
                total += units(&field["name"]) + units(&field["value"]);
            }
        }
        assert!(total <= 6000, "embed has {total} UTF-16 units");
    }

    fn model(id: &str, free: bool, context: u64) -> Model {
        Model {
            id: id.into(),
            name: "Example".into(),
            description: "Description".into(),
            context_length: context,
            pricing: BTreeMap::from([
                ("prompt".into(), if free { "0" } else { "1" }.into()),
                ("completion".into(), "0".into()),
            ]),
        }
    }

    #[test]
    fn markdown_escaping_preserves_id_characters() {
        assert_eq!(
            text("vendor/model_under`tick", 1024),
            "vendor/model\\_under\\`tick"
        );
        assert_eq!(text("a\\b[*]", 1024), "a\\\\b\\[\\*\\]");
        let payload = serde_json::to_value(model_details(
            &model("vendor/model_under`tick", true, 8192),
            false,
            true,
        ))
        .unwrap();
        assert_eq!(payload["fields"][0]["value"], "vendor/model\\_under\\`tick");
        assert_embed_bounds(&payload);
    }

    #[test]
    fn unicode_lengths_and_all_payloads_are_bounded() {
        let large = "\u{1d11e}_`\\[]".repeat(2000);
        for limit in [0, 1, 2, 3, 40, 256, 1024, 2000] {
            assert!(text(&large, limit).encode_utf16().count() <= limit);
        }
        let mut example = model(&large, false, u64::MAX);
        example.name = large.clone();
        example.description = large.clone();
        example.pricing = (0..30)
            .map(|index| (format!("{index}{large}"), large.clone()))
            .collect();
        for compact in [false, true] {
            for show_pricing in [false, true] {
                let value =
                    serde_json::to_value(model_details(&example, compact, show_pricing)).unwrap();
                assert_embed_bounds(&value);
                assert_eq!(
                    value["fields"].as_array().unwrap().len(),
                    if show_pricing { 4 } else { 3 }
                );
                assert_eq!(value.get("description").is_none(), compact);
            }
            let state = State {
                catalog: (0..30)
                    .map(|index| (index.to_string(), example.clone()))
                    .collect(),
                last_success: Some(u64::MAX),
                ..Default::default()
            };
            for page_size in [0, 1, 10, u32::MAX] {
                let options = Browse {
                    free_only: false,
                    compact,
                    page_size,
                    ..Default::default()
                };
                assert_embed_bounds(
                    &serde_json::to_value(snapshot(&state, &options, 86400)).unwrap(),
                );
            }
        }
    }

    #[test]
    fn snapshot_filters_pagination_and_freshness() {
        let state = State {
            catalog: [
                model("a/free", true, 100),
                model("b/paid", false, 200),
                model("c/free", true, 50),
            ]
            .into_iter()
            .map(|model| (model.id.clone(), model))
            .collect(),
            last_success: Some(123),
            ..Default::default()
        };
        let value = serde_json::to_value(snapshot(&state, &Browse::default(), 60)).unwrap();
        let description = value["description"].as_str().unwrap();
        assert!(description.contains("not live updating"));
        assert!(description.contains("not bot uptime or health"));
        assert!(description.contains("<t:123:f>"));
        assert!(!description.contains("b/paid"));
        assert!(description.contains("a/free") && description.contains("c/free"));
        let options = Browse {
            free_only: false,
            search: "PAID".into(),
            min_context: 200,
            page: u32::MAX,
            page_size: 1,
            compact: true,
        };
        let value = serde_json::to_value(snapshot(&state, &options, 75)).unwrap();
        assert!(value["description"].as_str().unwrap().contains("b/paid"));
        assert_eq!(value["fields"][2]["value"], "1/1 · 1 per page");
        assert!(
            value["fields"][3]["value"]
                .as_str()
                .unwrap()
                .contains("75 seconds")
        );
        let options = Browse {
            min_context: 201,
            ..options
        };
        let value = serde_json::to_value(snapshot(&state, &options, 600)).unwrap();
        assert!(
            value["description"]
                .as_str()
                .unwrap()
                .contains("No matching models.")
        );
        let value =
            serde_json::to_value(snapshot(&State::default(), &Browse::default(), 600)).unwrap();
        assert!(
            value["description"]
                .as_str()
                .unwrap()
                .contains("No successful catalog fetch")
        );
        let options = Browse {
            free_only: false,
            page_size: 1,
            page: 2,
            ..Default::default()
        };
        let value = serde_json::to_value(snapshot(&state, &options, 600)).unwrap();
        assert!(value["description"].as_str().unwrap().contains("b/paid"));
        assert!(!value["description"].as_str().unwrap().contains("a/free"));
        assert_eq!(value["fields"][2]["value"], "2/3 · 1 per page");
    }
}
