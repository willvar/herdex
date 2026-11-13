//! Account-specific speed capabilities advertised by the Codex model catalog.

use serde_json::Value;
use std::collections::BTreeMap;

pub const FAST: &str = "priority";
pub const ULTRAFAST: &str = "ultrafast";

pub fn canonical_tier(tier: &str) -> &str {
    match tier {
        "fast" => FAST,
        _ => tier,
    }
}

/// Default routing and Flex retain their ordinary pass-through behavior.
/// Every other explicit tier requires positive account/model capability data.
pub fn required_tier(tier: Option<&str>) -> Option<&str> {
    match tier {
        None | Some("" | "default" | "auto" | "flex") => None,
        Some(tier) => Some(canonical_tier(tier)),
    }
}

pub fn model_allows_plan(model: &Value, plan: &str) -> bool {
    match model.get("available_in_plans") {
        None | Some(Value::Null) => true,
        Some(Value::Array(plans)) => plans.iter().any(|value| value.as_str() == Some(plan)),
        Some(_) => false,
    }
}

/// Only native service-tier objects grant capabilities. The deprecated speed
/// list is never read, and catalog IDs are not rewritten into different tiers.
pub fn catalog_tiers(model: &Value) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    let Some(tiers) = model.get("service_tiers").and_then(Value::as_array) else {
        return out;
    };
    for tier in tiers {
        let Some(id) = tier
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        // Codex's ModelServiceTier requires strings for all three fields.
        if !tier.get("name").is_some_and(Value::is_string)
            || !tier.get("description").is_some_and(Value::is_string)
        {
            continue;
        }
        out.entry(id.to_owned()).or_insert_with(|| tier.clone());
    }
    out
}

pub fn supports_tier(model: &Value, tier: &str) -> bool {
    catalog_tiers(model).contains_key(canonical_tier(tier))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_capabilities_preserve_metadata_without_inferring_from_plan_names() {
        assert!(supports_tier(
            &json!({"service_tiers": [{"id": "priority", "name": "Fast", "description": "Priority processing."}]}),
            FAST
        ));
        let tier = json!({"id": "ultrafast", "name": "Ultrafast", "description": "Synthetic capability fixture.", "extra_metadata": "preserved"});
        let model = json!({"service_tiers": [tier]});
        assert!(supports_tier(&model, ULTRAFAST));
        assert_eq!(catalog_tiers(&model)[ULTRAFAST], tier);
        assert!(!supports_tier(
            &json!({"available_in_plans": ["pro", "promax"]}),
            FAST
        ));
        assert!(!supports_tier(&json!({}), ULTRAFAST));
        let product_catalog = json!({
            "experimental": {"modes": {
                "fast": {"body": {"service_tier": "priority"}},
                "ultrafast": {"body": {"service_tier": "ultrafast"}}
            }}
        });
        assert!(!supports_tier(&product_catalog, FAST));
        assert!(!supports_tier(&product_catalog, ULTRAFAST));
    }

    #[test]
    fn deprecated_speed_lists_and_invalid_native_metadata_never_grant_capabilities() {
        let deprecated = json!({"additional_speed_tiers": ["fast", "ultrafast"]});
        assert!(!supports_tier(&deprecated, FAST));
        assert!(!supports_tier(&deprecated, ULTRAFAST));
        for modern in [
            json!([]),
            Value::Null,
            json!(false),
            json!(["priority"]),
            json!([{"name": "Fast"}]),
            json!([{"id": "priority"}]),
            json!([{"id": "priority", "name": "Fast", "description": null}]),
            json!([{"id": "priority", "name": 1, "description": "Malformed name."}]),
            json!([{"id": "", "name": "Fast", "description": "Missing ID."}]),
        ] {
            let mut model = deprecated.clone();
            model["service_tiers"] = modern;
            assert!(!supports_tier(&model, FAST));
            assert!(!supports_tier(&model, ULTRAFAST));
        }
    }

    #[test]
    fn request_tier_aliases_do_not_rewrite_catalog_capability_ids() {
        assert!(supports_tier(
            &json!({"service_tiers": [{"id": "priority", "name": "Fast", "description": "Priority processing."}]}),
            "fast"
        ));
        assert!(!supports_tier(
            &json!({"service_tiers": [{"id": "fast", "name": "Fast", "description": "Not the priority capability ID."}]}),
            FAST
        ));
        assert_eq!(required_tier(Some("fast")), Some(FAST));
        assert_eq!(required_tier(Some("future-tier")), Some("future-tier"));
        for value in [None, Some(""), Some("default"), Some("auto"), Some("flex")] {
            assert_eq!(required_tier(value), None);
        }
    }

    #[test]
    fn explicit_model_plan_restrictions_are_respected() {
        let model = json!({"available_in_plans": ["plus", "prolite", "pro", "promax"]});
        for plan in ["plus", "prolite", "pro", "promax"] {
            assert!(model_allows_plan(&model, plan));
        }
        for plan in ["", "free", "unknown"] {
            assert!(!model_allows_plan(&model, plan));
        }
        assert!(!model_allows_plan(
            &json!({"available_in_plans": []}),
            "pro"
        ));
        assert!(model_allows_plan(&json!({}), "unknown"));
    }
}
