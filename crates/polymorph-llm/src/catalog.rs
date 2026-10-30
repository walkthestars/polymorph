//! Model catalog.
//!
//! Discovery source is models.dev (an open capability registry; reuse it
//! rather than maintaining our own). The catalog is best-effort and
//! offline-first:
//!
//! - [`Catalog::fallback`]: pinned snapshot compiled in via `include_str!`
//!   (ids this project actually routes: GLM + Muse Spark). Builds and
//!   `models list` never depend on network.
//! - [`fetch_models_dev`]: live fetch of the models.dev document, reduced
//!   through [`normalize_models_dev`] (unknown shapes → empty map + warning,
//!   never an error — a registry redesign must not break the build).
//! - [`update_catalog`]: fetch → normalize → write cache file. `models list`
//!   merges cache over fallback (cache wins per id).
//!
//! Network appears ONLY in `fetch_models_dev`/`update_catalog` (called from
//! the `update-catalog` CLI path). Everything else is pure + offline.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// models.dev document URL. Override with `update-catalog --url` (tests use a
/// local mock server). Confirm on first live run; the pinned fallback covers
/// offline use regardless.
pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";

/// Pinned fallback: ids the role chains actually reference.
const FALLBACK_JSON: &str = include_str!("../data/models_dev_fallback.json");

/// One catalogued model. All fields but `id` are hints (routing never keys
/// on them — chains name ids explicitly in config).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
}

/// Catalog: id → info, merged from fallback (+ cache over it).
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    models: BTreeMap<String, ModelInfo>,
}

impl Catalog {
    /// Pinned compiled-in snapshot. Infallible by construction (a corrupt
    /// fallback is a programming error — panic names the file).
    pub fn fallback() -> Self {
        let map = normalize_models_dev(
            &serde_json::from_str(FALLBACK_JSON).expect("fallback catalog must parse"),
        );
        assert!(
            !map.is_empty(),
            "fallback catalog must not normalize to empty"
        );
        Self { models: map }
    }

    /// Fallback merged with a cache file (cache wins per id). A missing
    /// cache file is fine (fresh install); a corrupt one is an error naming
    /// the path (refuse, don't guess — delete + `update-catalog` to recover).
    pub fn load(cache: Option<&Path>) -> anyhow::Result<Self> {
        let mut cat = Self::fallback();
        if let Some(path) = cache {
            if path.exists() {
                let raw = std::fs::read_to_string(path).map_err(|e| {
                    anyhow::anyhow!("cannot read catalog cache {}: {e}", path.display())
                })?;
                let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
                    anyhow::anyhow!(
                        "catalog cache {} is corrupt: {e}; delete it and re-run `update-catalog`",
                        path.display()
                    )
                })?;
                for (id, info) in normalize_models_dev(&value) {
                    cat.models.insert(id, info);
                }
            }
        }
        Ok(cat)
    }

    pub fn resolve(&self, id: &str) -> Option<&ModelInfo> {
        self.models.get(id)
    }

    pub fn len(&self) -> usize {
        self.models.len()
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &ModelInfo)> {
        self.models.iter()
    }
}

/// Best-effort reduction of a models.dev-shaped document to id → info.
/// Handled shapes (pinned by tests): `{models: {id: obj}}`, `{models: [ids
/// | {id..}]}`, `{providers: {prov: {models:...}}}`, and a bare `{id: obj}`
/// map (the cache/fallback shape). Anything else → empty map.
pub fn normalize_models_dev(doc: &serde_json::Value) -> BTreeMap<String, ModelInfo> {
    let mut out = BTreeMap::new();
    let Some(obj) = doc.as_object() else {
        return out;
    };
    if let Some(models) = obj.get("models") {
        collect_models(models, None, &mut out);
    } else if let Some(providers) = obj.get("providers").and_then(|p| p.as_object()) {
        for (prov_name, prov) in providers {
            let hint = prov
                .get("provider")
                .and_then(|p| p.as_str())
                .unwrap_or(prov_name);
            if let Some(models) = prov.get("models") {
                collect_models(models, Some(hint), &mut out);
            }
        }
    } else if obj.values().all(|v| v.is_object()) {
        // Bare id → object map (fallback/cache shape).
        collect_models(doc, None, &mut out);
    }
    out
}

fn collect_models(
    node: &serde_json::Value,
    provider_hint: Option<&str>,
    out: &mut BTreeMap<String, ModelInfo>,
) {
    if let Some(map) = node.as_object() {
        for (id, info) in map {
            let (provider, context) = info_fields(info, provider_hint);
            out.insert(
                id.clone(),
                ModelInfo {
                    id: id.clone(),
                    provider,
                    context,
                },
            );
        }
    } else if let Some(list) = node.as_array() {
        for item in list {
            if let Some(id) = item.as_str() {
                out.insert(
                    id.to_string(),
                    ModelInfo {
                        id: id.to_string(),
                        provider: provider_hint.map(str::to_string),
                        context: None,
                    },
                );
            } else if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                let (provider, context) = info_fields(item, provider_hint);
                out.insert(
                    id.to_string(),
                    ModelInfo {
                        id: id.to_string(),
                        provider,
                        context,
                    },
                );
            }
        }
    }
}

fn info_fields(info: &serde_json::Value, hint: Option<&str>) -> (Option<String>, Option<u64>) {
    let provider = info
        .get("provider")
        .and_then(|p| p.as_str())
        .map(str::to_string)
        .or_else(|| hint.map(str::to_string));
    let context = ["context", "context_window", "max_input_tokens"]
        .iter()
        .find_map(|k| info.get(*k).and_then(|v| v.as_u64()));
    (provider, context)
}

/// Live fetch of the models.dev document (the ONLY network call in this
/// module). HTTP/network failures are errors naming the URL.
pub async fn fetch_models_dev(url: &str) -> anyhow::Result<BTreeMap<String, ModelInfo>> {
    let value: serde_json::Value = reqwest::get(url)
        .await
        .map_err(|e| anyhow::anyhow!("catalog fetch from {url} failed: {e}"))?
        .error_for_status()
        .map_err(|e| anyhow::anyhow!("catalog fetch from {url} failed: {e}"))?
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("catalog fetch from {url} was not JSON: {e}"))?;
    Ok(normalize_models_dev(&value))
}

/// Fetch → normalize → write `dest` as `{"models": {id: info}}`.
/// An empty normalization is an error (refuse to clobber a good cache with
/// an unrecognized shape — the warning below names the URL). Returns the
/// number of models cached.
pub async fn update_catalog(url: &str, dest: &Path) -> anyhow::Result<usize> {
    let map = fetch_models_dev(url).await?;
    if map.is_empty() {
        anyhow::bail!(
            "catalog fetch from {url} normalized to zero models — cache at {} left untouched (unrecognized document shape?)",
            dest.display()
        );
    }
    let wrapper = serde_json::json!({ "models": map });
    let text = serde_json::to_string_pretty(&wrapper)?;
    std::fs::write(dest, text)
        .map_err(|e| anyhow::anyhow!("cannot write catalog cache {}: {e}", dest.display()))?;
    Ok(map.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_pins_the_routed_ids() {
        let cat = Catalog::fallback();
        assert!(cat.resolve("glm-5.3").is_some());
        assert!(cat.resolve("glm-5.3-flash").is_some());
        assert!(cat.resolve("muse-spark-1.3").is_some());
        assert!(cat.resolve("no-such-model").is_none());
    }

    #[test]
    fn normalize_map_shape_with_aliases() {
        let doc: serde_json::Value = serde_json::from_str(
            r#"{"models": {
                "a": {"provider": "p1", "context_window": 128000},
                "b": {"max_input_tokens": 64000}
            }}"#,
        )
        .unwrap();
        let map = normalize_models_dev(&doc);
        assert_eq!(map["a"].provider.as_deref(), Some("p1"));
        assert_eq!(map["a"].context, Some(128000));
        assert_eq!(map["b"].provider, None);
        assert_eq!(map["b"].context, Some(64000));
    }

    #[test]
    fn normalize_list_shapes() {
        let doc: serde_json::Value = serde_json::from_str(
            r#"{"models": ["x", {"id": "y", "provider": "p2", "context": 4096}]}"#,
        )
        .unwrap();
        let map = normalize_models_dev(&doc);
        assert_eq!(map["x"].context, None);
        assert_eq!(map["y"].provider.as_deref(), Some("p2"));
        assert_eq!(map["y"].context, Some(4096));
    }

    #[test]
    fn normalize_providers_shape_uses_section_as_hint() {
        let doc: serde_json::Value =
            serde_json::from_str(r#"{"providers": {"zv": {"models": {"m1": {}}}}}"#).unwrap();
        let map = normalize_models_dev(&doc);
        assert_eq!(map["m1"].provider.as_deref(), Some("zv"));
    }

    #[test]
    fn normalize_unknown_shapes_are_empty_not_errors() {
        for raw in [
            r#"{"version": "1.0"}"#,
            r#"[1, 2]"#,
            r#""just a string""#,
            r#"42"#,
        ] {
            let doc: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert!(normalize_models_dev(&doc).is_empty(), "{raw}");
        }
    }

    #[test]
    fn cache_merges_over_fallback_and_corrupt_cache_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("models.dev.json");
        // Missing cache = fallback alone.
        let cat = Catalog::load(Some(&cache)).unwrap();
        let n_fallback = cat.len();
        // Cache adds one id and overrides one's provider.
        std::fs::write(
            &cache,
            r#"{"models": {
                "brand-new-model": {"provider": "pn"},
                "glm-5.3": {"provider": "override"}
            }}"#,
        )
        .unwrap();
        let cat = Catalog::load(Some(&cache)).unwrap();
        assert_eq!(cat.len(), n_fallback + 1);
        assert_eq!(
            cat.resolve("glm-5.3").unwrap().provider.as_deref(),
            Some("override")
        );
        // Corrupt cache is a named error, not a silent fallback.
        std::fs::write(&cache, "not json {{{").unwrap();
        assert!(Catalog::load(Some(&cache)).is_err());
    }

    #[tokio::test]
    async fn fetch_and_update_round_trip_through_mock() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": { "mock-model": { "provider": "mock", "context": 8192 } }
            })))
            .mount(&server)
            .await;
        let url = format!("{}/api.json", server.uri());
        let map = fetch_models_dev(&url).await.unwrap();
        assert_eq!(map["mock-model"].context, Some(8192));
        // update_catalog persists the fetched map; Catalog::load merges it.
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("models.dev.json");
        let n = update_catalog(&url, &dest).await.unwrap();
        assert_eq!(n, 1);
        let cat = Catalog::load(Some(&dest)).unwrap();
        assert_eq!(
            cat.resolve("mock-model").unwrap().provider.as_deref(),
            Some("mock")
        );
        assert!(cat.resolve("glm-5.3").is_some());
    }

    #[tokio::test]
    async fn update_refuses_to_clobber_cache_on_empty_shape() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"version":"9.9"}"#))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("models.dev.json");
        std::fs::write(&dest, r#"{"models": {"kept": {}}}"#).unwrap();
        let err = update_catalog(&server.uri(), &dest).await.unwrap_err();
        assert!(err.to_string().contains("zero models"));
        // Cache file untouched.
        assert!(std::fs::read_to_string(&dest).unwrap().contains("kept"));
    }
}
