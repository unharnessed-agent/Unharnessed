//! Live provider model lists: the models an API key can use, read from the
//! provider's own `/models` endpoint.
//!
//! The catalog repo admits a model when a maintainer adds it, and the model
//! additions (`crate::additions`) follow models.dev for a few providers
//! only, so a model a provider serves can be missing from the picker. For
//! every provider the user holds an API key for, this layer reads the
//! provider's list with that key and adds the models the catalog does not
//! know yet.
//!
//! What may be listed: a provider whose models all use one compiled
//! `(provider, api, baseUrl)` transport with a concrete base URL, on an
//! API family whose listing endpoint is known (`LISTING_APIS`). Providers
//! with several transports (the id does not tell which one a new model
//! uses), templated base URLs, ambient credentials, subscriptions signed in
//! with OAuth and the providers that already have their own live list
//! (Prime Inference, `OpenRouter`) are left out.
//!
//! The request goes to the host the provider's chat requests already go
//! to, with the same key header and the transport's compiled headers; a
//! user without a key for a provider never contacts it.
//!
//! A listed model becomes a [`Model`] only from a catalog model of the same
//! provider (its "sibling", the one whose id shares the most leading
//! tokens): the new entry is the sibling with the listed id, and with the
//! name and the limits the listing states. The transport (api, base URL,
//! headers, compat) is the sibling's, so the pinning invariant holds: a
//! listing can never introduce a transport or change where credentials are
//! sent. A listed id with no sibling is dropped: nothing says how to call
//! it.
//!
//! Merge: a model the catalog lists is always the catalog's; only new ids
//! are appended, and among them only chat models (no embeddings, speech,
//! images...), no dated snapshot of a listed id, and, when the listing
//! dates its models, none older than the newest model the catalog lists
//! (the catalog maintainers left older absent models out on purpose, the
//! rule `scripts/sync-model-additions.py` applies).
//!
//! Caching: one last-good snapshot per provider, keyed by a keyed hash of
//! the API key, so one account's list never leaks to another; 401/403
//! clears it.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

use crate::cache::{CatalogCache, RefreshOptions};
use crate::fetch::CatalogFetcher;
use crate::Model;
use uh_types::ai::ModelInput;

/// The API families whose model listing endpoint is known.
const LISTING_APIS: [&str; 5] = [
    "openai-completions",
    "openai-responses",
    "anthropic-messages",
    "google-generative-ai",
    "mistral-conversations",
];

/// Providers never listed: subscriptions signed in with OAuth or a platform
/// token (their key does not reach a public `/models`), and the providers
/// whose live list has its own module.
const NOT_LISTED: [&str; 5] = [
    "github-copilot",
    "openai-codex",
    "google-antigravity",
    "prime-inference",
    "openrouter",
];

/// The directory of the per-provider list caches, beside models.json.
const CACHE_DIR: &str = "provider-models";

/// An API key for a provider, as auth resolved it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderKey {
    pub provider: String,
    pub api_key: String,
}

/// One entry of a provider's model list: the id and what the listing says
/// about it. `None` fields are left to the sibling.
#[derive(Debug, Clone, PartialEq)]
pub struct ListedModel {
    pub id: String,
    pub name: Option<String>,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
    pub image_input: Option<bool>,
    pub reasoning: Option<bool>,
    /// Days since the Unix epoch the provider dates the model with.
    pub created_day: Option<i64>,
    /// The listing says the model is not a chat model with tools.
    pub not_chat: bool,
}

/// The listing request of one provider: where, and with which headers.
#[derive(Debug, Clone)]
struct ListingTransport {
    api: String,
    url: String,
    headers: Vec<(String, String)>,
}

impl ListingTransport {
    /// The listing endpoint of `api` under `base_url`; `None` when the
    /// family has none.
    fn new(api: &str, base_url: &str, headers: Option<&BTreeMap<String, String>>) -> Option<Self> {
        let base = base_url.trim_end_matches('/');
        let url = match api {
            "openai-completions" | "openai-responses" => format!("{base}/models"),
            "mistral-conversations" => format!("{base}/v1/models"),
            "anthropic-messages" => format!("{base}/v1/models?limit=1000"),
            "google-generative-ai" => format!("{base}/models?pageSize=1000"),
            _ => return None,
        };
        Some(Self {
            api: api.to_string(),
            url,
            headers: headers
                .into_iter()
                .flatten()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
        })
    }

    /// The request headers: the transport's compiled ones, then the key in
    /// the header the family's chat requests carry it in.
    fn request_headers(&self, api_key: &str) -> Vec<(String, String)> {
        let mut headers = self.headers.clone();
        match self.api.as_str() {
            "anthropic-messages" => {
                headers.push(("x-api-key".into(), api_key.into()));
                headers.push(("anthropic-version".into(), "2023-06-01".into()));
            }
            "google-generative-ai" => headers.push(("x-goog-api-key".into(), api_key.into())),
            _ => headers.push(("Authorization".into(), format!("Bearer {api_key}"))),
        }
        headers
    }
}

/// The providers that can be listed, with their listing request: those
/// whose `models` all share one transport this module can list.
fn listing_transports(models: &[Model]) -> BTreeMap<String, ListingTransport> {
    let mut tuples: BTreeMap<&str, HashSet<(&str, &str)>> = BTreeMap::new();
    for model in models {
        tuples
            .entry(model.provider.as_str())
            .or_default()
            .insert((model.api.as_str(), model.base_url.as_str()));
    }
    tuples
        .into_iter()
        .filter(|(provider, _)| !NOT_LISTED.contains(provider))
        .filter_map(|(provider, transports)| {
            let transports: Vec<(&str, &str)> = transports.into_iter().collect();
            let [(api, base_url)] = transports[..] else {
                return None;
            };
            if !LISTING_APIS.contains(&api) || base_url.is_empty() || base_url.contains('{') {
                return None;
            }
            let headers = models
                .iter()
                .find(|model| model.provider == provider)
                .and_then(|model| model.headers.as_ref());
            Some((
                provider.to_string(),
                ListingTransport::new(api, base_url, headers)?,
            ))
        })
        .collect()
}

/// The cache scope of a key: a keyed hash, so the key itself is never
/// written to disk.
///
/// # Panics
///
/// Never: HMAC accepts a key of any length.
#[must_use]
pub fn scope_key(provider: &str, api_key: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(api_key.as_bytes()).expect("HMAC accepts any key length");
    mac.update(b"unharnessed:provider-model-list:v1\0");
    mac.update(provider.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Parse a provider's model list: the `data` array of the `OpenAI`,
/// `Anthropic` and `Mistral` shapes, or the `models` array of Google's.
///
/// # Errors
///
/// Fails when the payload has neither array, or no entry has an id.
pub fn parse_model_list(payload: &Value) -> Result<Vec<ListedModel>, String> {
    let items = payload
        .get("data")
        .or_else(|| payload.get("models"))
        .and_then(Value::as_array)
        .ok_or("Invalid provider model list")?;
    let mut seen = HashSet::new();
    let listed: Vec<ListedModel> = items
        .iter()
        .filter_map(listed_model)
        .filter(|model| seen.insert(model.id.clone()))
        .collect();
    if listed.is_empty() {
        return Err("Provider model list has no models".into());
    }
    Ok(listed)
}

fn listed_model(item: &Value) -> Option<ListedModel> {
    let item = item.as_object()?;
    let raw_id = item
        .get("id")
        .or_else(|| item.get("name"))
        .and_then(Value::as_str)?;
    // Google names its models `models/<id>`; the id is what requests use.
    let id = raw_id.strip_prefix("models/").unwrap_or(raw_id).trim();
    if id.is_empty() {
        return None;
    }
    let text = |key: &str| {
        item.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(String::from)
    };
    let tokens = |keys: &[&str]| keys.iter().find_map(|key| positive(item.get(*key)));
    let name = text("display_name")
        .or_else(|| text("displayName"))
        .or_else(|| text("name").filter(|name| name != raw_id));
    // Hugging Face states the limit per inference provider: the largest
    // one serves the model.
    let routed_context = item
        .get("providers")
        .and_then(Value::as_array)
        .and_then(|providers| {
            providers
                .iter()
                .filter_map(|provider| positive(provider.get("context_length")))
                .max()
        });
    let context_window = tokens(&[
        "context_length",
        "context_window",
        "max_context_length",
        "inputTokenLimit",
        "max_input_tokens",
    ])
    .or(routed_context);
    let max_tokens = tokens(&[
        "outputTokenLimit",
        "max_output_tokens",
        "max_completion_tokens",
        "max_tokens",
    ]);
    let capabilities = item.get("capabilities");
    let capability = |key: &str| {
        capabilities
            .and_then(|c| c.get(key))
            .and_then(Value::as_bool)
    };
    let tags: Option<Vec<&str>> = item
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| tags.iter().filter_map(Value::as_str).collect());
    let input_modalities = item
        .get("architecture")
        .and_then(|a| a.get("input_modalities"))
        .or_else(|| item.get("modalities").and_then(|m| m.get("input")))
        .or_else(|| item.get("input_modalities"))
        .and_then(Value::as_array)
        .map(|modalities| modalities.iter().any(|m| m.as_str() == Some("image")));
    let image_input = item
        .get("supports_image_in")
        .and_then(Value::as_bool)
        .or_else(|| capability("vision"))
        .or(input_modalities);
    let reasoning = item
        .get("supports_reasoning")
        .and_then(Value::as_bool)
        .or_else(|| item.get("thinking").and_then(Value::as_bool))
        .or_else(|| tags.as_ref().map(|tags| tags.contains(&"reasoning")));
    // The release date when the listing has one (Vercel), else the date the
    // provider added the model.
    let created_day = item
        .get("released")
        .and_then(Value::as_i64)
        .or_else(|| item.get("created").and_then(Value::as_i64))
        .map(|seconds| seconds.div_euclid(86_400))
        .or_else(|| {
            item.get("created_at")
                .and_then(Value::as_str)
                .and_then(day_of_date)
        });
    let not_chat = listing_says_not_chat(item, tags.as_deref());
    Some(ListedModel {
        id: id.to_string(),
        name,
        context_window,
        max_tokens,
        image_input,
        reasoning,
        created_day,
        not_chat,
    })
}

/// Whether the listing itself says the model is not a chat model with
/// tools: its `type`, Mistral's capabilities, Vercel's tags, Hugging Face's
/// per-provider tool support, Google's generation methods.
fn listing_says_not_chat(item: &serde_json::Map<String, Value>, tags: Option<&[&str]>) -> bool {
    let capability = |key: &str| {
        item.get("capabilities")
            .and_then(|c| c.get(key))
            .and_then(Value::as_bool)
    };
    item.get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| NOT_CHAT_TYPES.contains(&kind))
        || capability("completion_chat") == Some(false)
        || capability("function_calling") == Some(false)
        || tags.is_some_and(|tags| !tags.contains(&"tool-use"))
        || item
            .get("providers")
            .and_then(Value::as_array)
            .is_some_and(|providers| {
                !providers.is_empty()
                    && providers
                        .iter()
                        .all(|p| p.get("supports_tools").and_then(Value::as_bool) == Some(false))
            })
        || item
            .get("supportedGenerationMethods")
            .and_then(Value::as_array)
            .is_some_and(|methods| {
                !methods
                    .iter()
                    .any(|m| m.as_str() == Some("generateContent"))
            })
}

/// The listing `type`s of models a session cannot chat with.
const NOT_CHAT_TYPES: [&str; 11] = [
    "embedding",
    "embeddings",
    "image",
    "video",
    "audio",
    "speech",
    "tts",
    "transcription",
    "rerank",
    "moderation",
    "realtime",
];

/// Id tokens of models that are not chat models with tools.
const NOT_CHAT_TOKENS: [&str; 20] = [
    // Batch routes do not stream (`OpenRouter`'s `:batch` rule).
    "batch",
    "tts",
    "audio",
    "realtime",
    "transcribe",
    "image",
    "imagen",
    "veo",
    "sora",
    "lyria",
    "live",
    "search",
    "guard",
    "ocr",
    "rerank",
    "aqa",
    "dall",
    "babbage",
    "davinci",
    "banana",
];

/// Id fragments of models that are not chat models with tools.
const NOT_CHAT_FRAGMENTS: [&str; 5] = [
    "embed",
    "whisper",
    "computer-use",
    "deep-research",
    "moderation",
];

fn positive(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64).filter(|tokens| *tokens > 0)
}

/// Days since the Unix epoch of an RFC 3339 date (`YYYY-MM-DD...`).
fn day_of_date(text: &str) -> Option<i64> {
    let date = text.get(..10)?;
    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Howard Hinnant's `days_from_civil`.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

/// The vendor path of an id (`alibaba/` in `alibaba/qwen-3-14b`) and the
/// lowercase tokens of its last segment.
fn id_parts(id: &str) -> (String, Vec<String>) {
    let lower = id.to_ascii_lowercase();
    let (vendor, name) = lower
        .rsplit_once('/')
        .map_or((String::new(), lower.as_str()), |(vendor, name)| {
            (vendor.to_string(), name)
        });
    let tokens = name
        .split(['-', '_', '.', ':'])
        .filter(|token| !token.is_empty())
        .map(String::from)
        .collect();
    (vendor, tokens)
}

fn is_not_chat_id(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    let (_, tokens) = id_parts(id);
    NOT_CHAT_FRAGMENTS
        .iter()
        .any(|fragment| lower.contains(fragment))
        || tokens
            .iter()
            .any(|token| NOT_CHAT_TOKENS.contains(&token.as_str()))
}

/// The id without a trailing snapshot suffix (`-2026-11-24`, `-20261124`,
/// `-1124`, `-001`), when it has one.
fn snapshot_base(id: &str) -> Option<&str> {
    let is_digits = |text: &str, len: usize| {
        text.len() == len && text.bytes().all(|byte| byte.is_ascii_digit())
    };
    let (head, last) = id.rsplit_once('-')?;
    if is_digits(last, 8) || is_digits(last, 4) || is_digits(last, 3) {
        return Some(head);
    }
    // `-YYYY-MM-DD`: three numeric segments.
    let day = last;
    let (head, month) = head.rsplit_once('-')?;
    let (head, year) = head.rsplit_once('-')?;
    (is_digits(year, 4) && is_digits(month, 2) && is_digits(day, 2)).then_some(head)
}

/// The catalog model a new id is built from: same vendor path, the most
/// shared leading tokens (at least one), then the longest shared prefix,
/// then the shortest id (the family's base model).
fn sibling<'a>(id: &str, known: &[&'a Model]) -> Option<&'a Model> {
    let (vendor, tokens) = id_parts(id);
    let lower = id.to_ascii_lowercase();
    known
        .iter()
        .filter_map(|model| {
            let (model_vendor, model_tokens) = id_parts(&model.id);
            if model_vendor != vendor {
                return None;
            }
            let shared = tokens
                .iter()
                .zip(&model_tokens)
                .take_while(|(a, b)| a == b)
                .count();
            let prefix = lower
                .bytes()
                .zip(model.id.to_ascii_lowercase().bytes())
                .take_while(|(a, b)| a == b)
                .count();
            (shared > 0).then_some((shared, prefix, std::cmp::Reverse(model.id.len()), *model))
        })
        .max_by_key(|(shared, prefix, shorter, _)| (*shared, *prefix, *shorter))
        .map(|(_, _, _, model)| model)
}

/// Append to `base` the models of `provider`'s list that the catalog does
/// not know, each built from its sibling (see the module docs).
#[must_use]
pub fn merge_listed_models(
    mut base: Vec<Model>,
    provider: &str,
    listed: &[ListedModel],
) -> Vec<Model> {
    let added: Vec<Model> = {
        let known: Vec<&Model> = base.iter().filter(|m| m.provider == provider).collect();
        let known_ids: HashSet<&str> = known.iter().map(|m| m.id.as_str()).collect();
        // The newest model the catalog lists, by the listing's own dates.
        let newest_known = listed
            .iter()
            .filter(|entry| known_ids.contains(entry.id.as_str()))
            .filter_map(|entry| entry.created_day)
            .max();
        listed
            .iter()
            .filter(|entry| !known_ids.contains(entry.id.as_str()))
            .filter(|entry| !entry.not_chat && !is_not_chat_id(&entry.id))
            .filter(|entry| snapshot_base(&entry.id).is_none_or(|head| !known_ids.contains(head)))
            .filter(|entry| match (entry.created_day, newest_known) {
                (Some(created), Some(newest)) => created >= newest,
                _ => true,
            })
            .filter_map(|entry| Some(listed_entry(entry, sibling(&entry.id, &known)?)))
            .collect()
    };
    base.extend(added);
    base
}

fn listed_entry(entry: &ListedModel, sibling: &Model) -> Model {
    let mut model = sibling.clone();
    model.id.clone_from(&entry.id);
    // Without a listed name, the id's last segment (the catalog names
    // `moonshotai/Kimi-K2.6` "Kimi-K2.6").
    model.name = entry.name.clone().unwrap_or_else(|| {
        entry
            .id
            .rsplit_once('/')
            .map_or(entry.id.as_str(), |(_, name)| name)
            .to_string()
    });
    model.featured = None;
    if let Some(context_window) = entry.context_window {
        model.context_window = context_window;
    }
    if let Some(max_tokens) = entry.max_tokens {
        model.max_tokens = max_tokens;
    }
    model.max_tokens = model.max_tokens.min(model.context_window);
    match entry.image_input {
        Some(true) if !model.input.contains(&ModelInput::Image) => {
            model.input.push(ModelInput::Image);
        }
        Some(false) => model.input.retain(|input| *input != ModelInput::Image),
        _ => {}
    }
    match entry.reasoning {
        Some(false) => {
            model.reasoning = false;
            model.thinking_level_map = None;
        }
        Some(true) => model.reasoning = true,
        None => {}
    }
    model
}

/// One provider's list cache.
struct ProviderList {
    transport: ListingTransport,
    cache: CatalogCache<Vec<ListedModel>>,
}

/// The live model lists of every provider that can be listed.
pub struct ProviderModelLists {
    lists: BTreeMap<String, ProviderList>,
}

impl ProviderModelLists {
    /// The lists of the providers `models` (the compiled catalog) can list,
    /// each persisted under `<models_dir>/provider-models/<provider>.json`.
    #[must_use]
    pub fn new(models_dir: Option<&std::path::Path>, models: &[Model]) -> Self {
        let fetcher = Arc::new(CatalogFetcher::new());
        let lists = listing_transports(models)
            .into_iter()
            .map(|(provider, transport)| {
                let parse: crate::cache::CatalogParse<Vec<ListedModel>> =
                    Arc::new(|payload, _scope| parse_model_list(payload));
                let cache_path: Option<PathBuf> =
                    models_dir.map(|dir| dir.join(CACHE_DIR).join(format!("{provider}.json")));
                let cache =
                    CatalogCache::new(&transport.url, cache_path, Arc::clone(&fetcher), parse);
                (provider, ProviderList { transport, cache })
            })
            .collect();
        Self { lists }
    }

    /// The providers whose models can be listed.
    pub fn providers(&self) -> impl Iterator<Item = &str> {
        self.lists.keys().map(String::as_str)
    }

    /// Merge the last-good list of every provider in `keys` into `base`.
    #[must_use]
    pub fn merge(&self, mut base: Vec<Model>, keys: &[ProviderKey]) -> Vec<Model> {
        for key in keys {
            let Some(list) = self.lists.get(&key.provider) else {
                continue;
            };
            if let Some(listed) = list.cache.get(&scope_key(&key.provider, &key.api_key)) {
                base = merge_listed_models(base, &key.provider, &listed);
            }
        }
        base
    }

    /// Fetch the list of every provider in `keys` (hourly-gated unless
    /// `force`); failures keep last-good, 401/403 clears that key's list.
    pub async fn refresh(&self, keys: &[ProviderKey], force: bool) {
        let refreshes = keys.iter().filter_map(|key| {
            let list = self.lists.get(&key.provider)?;
            Some(async move {
                let listed = list
                    .cache
                    .refresh(
                        &scope_key(&key.provider, &key.api_key),
                        RefreshOptions {
                            force,
                            headers: list.transport.request_headers(&key.api_key),
                            is_current: None,
                        },
                    )
                    .await;
                tracing::debug!(
                    provider = %key.provider,
                    listed = listed.map(|models| models.len()),
                    "provider model list refresh settled"
                );
            })
        });
        futures::future::join_all(refreshes).await;
    }
}

#[cfg(test)]
mod tests {
    //! The cases come from how this layer can fail:
    //! 1. a listed id the catalog knows is duplicated or overrides the
    //!    catalog's entry;
    //! 2. a model a session cannot chat with (embeddings, speech, images,
    //!    no tools) reaches the picker;
    //! 3. a dated snapshot of a known id, or a legacy model older than the
    //!    catalog's newest, clutters the picker;
    //! 4. a new id gets a transport or metadata from an unrelated model
    //!    (no sibling, another vendor path), or a transport other than its
    //!    sibling's (the pinning invariant);
    //! 5. the id keeps Google's `models/` prefix, so requests fail;
    //! 6. the listing's limits are ignored, or the output limit exceeds the
    //!    context window; a non-reasoning model keeps a thinking map;
    //! 7. a malformed list replaces the last-good one;
    //! 8. a provider is listed that must not be (several transports,
    //!    templated URL, OAuth subscription, own live list), or with the
    //!    key in the wrong header, or without its compiled headers;
    //! 9. two keys share a cache scope, or the scope reveals the key.
    use super::*;
    use serde_json::json;

    fn model(provider: &str, id: &str) -> Model {
        let mut headers = BTreeMap::new();
        headers.insert("User-Agent".to_string(), "cli/1".to_string());
        Model {
            id: id.into(),
            name: format!("catalog {id}"),
            api: "anthropic-messages".into(),
            provider: provider.into(),
            base_url: format!("https://{provider}.example"),
            reasoning: true,
            thinking_level_map: Some(
                serde_json::from_value(json!({"low": "low", "high": "high"})).unwrap(),
            ),
            input: vec![ModelInput::Text],
            cost: uh_ai::types::zero_model_cost(),
            context_window: 262_144,
            max_tokens: 32_768,
            featured: Some(true),
            headers: Some(headers),
            compat: None,
        }
    }

    fn listed(payload: &Value) -> Vec<ListedModel> {
        parse_model_list(payload).expect("valid list")
    }

    fn added(base: Vec<Model>, provider: &str, payload: &Value) -> Vec<Model> {
        let before = base.len();
        merge_listed_models(base, provider, &listed(payload))
            .into_iter()
            .skip(before)
            .collect()
    }

    fn ids(models: &[Model]) -> Vec<&str> {
        models.iter().map(|model| model.id.as_str()).collect()
    }

    #[test]
    fn a_known_id_stays_the_catalogs() {
        let base = vec![model("kimi", "k3")];
        let merged = merge_listed_models(
            base,
            "kimi",
            &listed(&json!({"data": [{"id": "k3", "display_name": "Other", "context_length": 1}]})),
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "catalog k3");
        assert_eq!(merged[0].context_window, 262_144);
    }

    #[test]
    fn a_new_id_is_its_sibling_with_the_listed_name_and_limits() {
        let base = vec![model("kimi", "k3"), model("kimi", "kimi-for-coding")];
        let new = added(
            base,
            "kimi",
            &json!({"data": [{
                "id": "k3.1-pro", "display_name": "K3.1 Pro", "type": "model",
                "context_length": 1_048_576, "supports_image_in": true,
            }]}),
        );
        assert_eq!(ids(&new), ["k3.1-pro"]);
        let sibling = model("kimi", "k3");
        let new = &new[0];
        assert_eq!(new.name, "K3.1 Pro");
        assert_eq!(new.context_window, 1_048_576);
        assert_eq!(new.max_tokens, sibling.max_tokens);
        assert_eq!(new.input, [ModelInput::Text, ModelInput::Image]);
        assert_eq!(new.featured, None);
        // The transport is the sibling's, whatever the listing says.
        assert_eq!(
            (&new.api, &new.base_url, &new.headers, &new.compat),
            (
                &sibling.api,
                &sibling.base_url,
                &sibling.headers,
                &sibling.compat
            )
        );
        assert_eq!(new.thinking_level_map, sibling.thinking_level_map);
    }

    #[test]
    fn non_chat_models_are_dropped() {
        let base = vec![
            model("p", "gpt-5"),
            model("p", "gemini-3"),
            model("p", "mistral-large"),
            model("p", "llama-4"),
            model("p", "alibaba/qwen-3"),
            model("p", "org/kimi-k3"),
        ];
        let new = added(
            base,
            "p",
            &json!({"data": [
                {"id": "gpt-5-tts"},
                {"id": "gpt-5-audio-preview"},
                {"id": "gpt-5-realtime"},
                {"id": "gpt-5-search-preview"},
                {"id": "gpt-5-image"},
                {"id": "gpt-5-embedding"},
                {"id": "gpt-5-batch"},
                {"id": "gemini-3-flash-image"},
                {"id": "gemini-3-computer-use-preview"},
                {"id": "gemini-3-live"},
                {"id": "llama-4-guard"},
                {"id": "mistral-large-ocr"},
                {"id": "mistral-large-chatless", "capabilities": {"completion_chat": false}},
                {"id": "mistral-large-toolless", "capabilities": {"function_calling": false}},
                {"id": "alibaba/qwen-3-embed", "type": "embedding", "tags": ["tool-use"]},
                {"id": "alibaba/qwen-3-plain", "type": "language", "tags": ["reasoning"]},
                {"id": "org/kimi-k3-notools", "providers": [{"supports_tools": false}]},
                {"name": "models/gemini-3-tuned", "supportedGenerationMethods": ["embedContent"]},
                {"id": "gpt-5.1"},
            ]}),
        );
        assert_eq!(ids(&new), ["gpt-5.1"]);
    }

    #[test]
    fn dated_snapshots_of_known_ids_are_dropped() {
        let base = vec![model("p", "claude-x"), model("p", "gemini-3-flash")];
        let new = added(
            base,
            "p",
            &json!({"data": [
                {"id": "claude-x-20261124"},
                {"id": "claude-x-2026-11-24"},
                {"id": "claude-x-1124"},
                {"id": "gemini-3-flash-001"},
                // A snapshot of an id the catalog does not list is new.
                {"id": "claude-y-20261124"},
            ]}),
        );
        assert_eq!(ids(&new), ["claude-y-20261124"]);
    }

    #[test]
    fn models_older_than_the_newest_known_are_dropped() {
        let day = 86_400;
        let base = vec![model("p", "gpt-6"), model("p", "gpt-5")];
        let new = added(
            base,
            "p",
            &json!({"data": [
                {"id": "gpt-5", "created": 100 * day},
                {"id": "gpt-6", "created": 200 * day},
                {"id": "gpt-4-turbo", "created": 50 * day},
                {"id": "gpt-6.1", "created": 200 * day},
                {"id": "gpt-6.2", "created": 300 * day},
            ]}),
        );
        assert_eq!(ids(&new), ["gpt-6.1", "gpt-6.2"]);
        // Anthropic dates its models with an RFC 3339 `created_at`.
        let base = vec![model("p", "claude-a")];
        let new = added(
            base,
            "p",
            &json!({"data": [
                {"id": "claude-a", "created_at": "2026-06-01T00:00:00Z"},
                {"id": "claude-old", "created_at": "2025-01-01T00:00:00Z"},
                {"id": "claude-new", "created_at": "2026-11-01T00:00:00Z"},
            ]}),
        );
        assert_eq!(ids(&new), ["claude-new"]);
        // Vercel's `released` (the model's release) wins over `created`
        // (the day the gateway added it).
        let base = vec![model("p", "v/m-1")];
        let new = added(
            base,
            "p",
            &json!({"data": [
                {"id": "v/m-1", "created": 900 * day, "released": 100 * day},
                {"id": "v/m-2", "created": 500 * day, "released": 200 * day},
            ]}),
        );
        assert_eq!(ids(&new), ["v/m-2"]);
    }

    #[test]
    fn a_new_id_without_a_sibling_is_dropped() {
        let base = vec![model("p", "openai/gpt-5"), model("q", "qwen-3")];
        let new = added(
            base,
            "p",
            &json!({"data": [
                // Another vendor path.
                {"id": "alibaba/gpt-5.1"},
                // No shared leading token.
                {"id": "openai/o9"},
                // The sibling would be another provider's.
                {"id": "qwen-3.1"},
                {"id": "openai/gpt-5.1"},
            ]}),
        );
        assert_eq!(ids(&new), ["openai/gpt-5.1"]);
        // Unnamed, it is named after its last segment.
        assert_eq!(new[0].name, "gpt-5.1");
    }

    #[test]
    fn the_sibling_shares_the_most_leading_tokens_then_is_the_shortest() {
        let mut flash = model("p", "gemini-3-flash");
        flash.max_tokens = 1;
        let mut pro = model("p", "gemini-3-pro");
        pro.max_tokens = 2;
        let mut base_model = model("p", "k3");
        base_model.max_tokens = 3;
        let mut long_model = model("p", "k3-256k");
        long_model.max_tokens = 4;
        let new = added(
            vec![flash, pro, base_model, long_model],
            "p",
            &json!({"data": [{"id": "gemini-3-pro-max"}, {"id": "k3.1-pro"}]}),
        );
        assert_eq!(
            new.iter()
                .map(|m| (m.id.as_str(), m.max_tokens))
                .collect::<Vec<_>>(),
            [("gemini-3-pro-max", 2), ("k3.1-pro", 3)]
        );
    }

    #[test]
    fn google_ids_drop_the_models_prefix() {
        let entries = listed(&json!({"models": [{
            "name": "models/gemini-3.9-pro",
            "displayName": "Gemini 3.9 Pro",
            "inputTokenLimit": 2_000_000,
            "outputTokenLimit": 65_536,
            "supportedGenerationMethods": ["generateContent", "countTokens"],
            "thinking": true,
        }]}));
        assert_eq!(entries[0].id, "gemini-3.9-pro");
        assert_eq!(entries[0].name.as_deref(), Some("Gemini 3.9 Pro"));
        assert_eq!(entries[0].context_window, Some(2_000_000));
        assert_eq!(entries[0].max_tokens, Some(65_536));
        assert_eq!(entries[0].reasoning, Some(true));
        assert!(!entries[0].not_chat);
    }

    #[test]
    fn listed_limits_apply_and_the_output_fits_the_context() {
        let new = added(
            vec![model("p", "m-1")],
            "p",
            &json!({"data": [
                {"id": "m-2", "context_window": 8_192},
                {"id": "m-3", "max_context_length": 400_000, "max_tokens": 64_000},
                {"id": "m-4", "providers": [{"context_length": 4_096}, {"context_length": 131_072}]},
            ]}),
        );
        assert_eq!(
            new.iter()
                .map(|m| (m.context_window, m.max_tokens))
                .collect::<Vec<_>>(),
            [(8_192, 8_192), (400_000, 64_000), (131_072, 32_768)]
        );
    }

    #[test]
    fn a_listed_non_reasoning_model_has_no_thinking_map() {
        let new = added(
            vec![model("p", "m-1")],
            "p",
            &json!({"data": [{"id": "m-2", "supports_reasoning": false, "supports_image_in": false}]}),
        );
        assert!(!new[0].reasoning);
        assert_eq!(new[0].thinking_level_map, None);
        assert_eq!(new[0].input, [ModelInput::Text]);
    }

    #[test]
    fn a_malformed_list_is_an_error() {
        assert!(parse_model_list(&json!({"object": "list"})).is_err());
        assert!(parse_model_list(&json!({"data": []})).is_err());
        assert!(parse_model_list(&json!({"data": [{"object": "model"}, {"id": "  "}]})).is_err());
        let entries = listed(&json!({"data": [{"id": "a"}, {"id": "a", "display_name": "dup"}]}));
        assert_eq!(entries.len(), 1, "the first entry of a duplicated id wins");
        assert_eq!(entries[0].name, None);
    }

    #[test]
    fn only_single_transport_providers_with_a_known_listing_are_listed() {
        let transports = listing_transports(crate::transports::compiled_models());
        for provider in [
            "kimi-coding",
            "moonshotai",
            "anthropic",
            "google",
            "mistral",
            "openai",
        ] {
            assert!(transports.contains_key(provider), "{provider} is listed");
        }
        for provider in [
            "opencode",
            "cloudflare-workers-ai",
            "google-vertex",
            "azure-openai-responses",
            "amazon-bedrock",
            "github-copilot",
            "openai-codex",
            "openrouter",
            "prime-inference",
        ] {
            assert!(
                !transports.contains_key(provider),
                "{provider} is not listed"
            );
        }
        let kimi = &transports["kimi-coding"];
        assert_eq!(kimi.url, "https://api.kimi.com/coding/v1/models?limit=1000");
        let headers = kimi.request_headers("sk-kimi-1");
        assert!(headers.contains(&("User-Agent".into(), "KimiCLI/1.5".into())));
        assert!(headers.contains(&("x-api-key".into(), "sk-kimi-1".into())));
        assert!(!headers.iter().any(|(name, _)| name == "Authorization"));
        let moonshot = &transports["moonshotai"];
        assert_eq!(moonshot.url, "https://api.moonshot.ai/v1/models");
        assert!(moonshot
            .request_headers("sk-1")
            .contains(&("Authorization".into(), "Bearer sk-1".into())));
        let google = &transports["google"];
        assert!(google
            .request_headers("g-1")
            .contains(&("x-goog-api-key".into(), "g-1".into())));
        assert_eq!(
            transports["mistral"].url,
            "https://api.mistral.ai/v1/models"
        );
    }

    #[test]
    fn each_key_has_its_own_scope_and_the_scope_hides_the_key() {
        let a = scope_key("kimi-coding", "sk-kimi-a");
        assert_ne!(a, scope_key("kimi-coding", "sk-kimi-b"));
        assert_ne!(a, scope_key("moonshotai", "sk-kimi-a"));
        assert!(!a.contains("sk-kimi-a"));
        assert_eq!(a, scope_key("kimi-coding", "sk-kimi-a"));
    }
}
