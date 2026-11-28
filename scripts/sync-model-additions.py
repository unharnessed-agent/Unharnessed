#!/usr/bin/env python3
"""Write catalog/models.additions.v1.json: the models a provider released
that the model catalog does not list yet.

The client reads its models from the catalog repo
(PrimeIntellect-ai/prime-agent-catalog, models/catalog.v1.json), which
admits a model when a maintainer adds it by hand. This script finds the
models that exist and are not yet in that catalog, and writes them in the
same `catalog.v1` shape; the client appends them to its catalog
(crates/uh-models/src/additions.rs).
.github/workflows/model-additions.yml runs it on a schedule and commits
the file when it changes.

Three kinds of sources say which models exist:
  - models.dev, for the API providers of PROVIDERS (Anthropic, Google,
    OpenAI) and for GitHub Copilot;
  - the model list of OpenAI's Codex CLI (openai/codex,
    codex-rs/models-manager/models.json), for the ChatGPT subscription
    (provider openai-codex): the subscription does not offer the API's
    models, nor the same context windows;
  - OpenCode's public model lists (`/zen/v1/models` and
    `/zen/go/v1/models`), which describe only the models currently served
    and the wire APIs each model offers.

An API or Copilot model is admitted when all of these hold:
  - its source describes it as a chat model a session can use (tool calls,
    text-only output, not deprecated, with prices and limits);
  - the catalog does not list it;
  - it was released after every model the catalog lists for that provider
    (older absent models were left out by the catalog's maintainers).
Copilot transport selection is deliberately narrow: audited model-family
rules choose among the compiled Copilot APIs, and an unmapped family fails
closed. An OpenCode model is admitted when the provider's current endpoint
serves it as a tool-calling chat model and the catalog does not list it;
there is no date watermark because removal from that endpoint means retired.
When the endpoint recommends a newer protocol the client does not implement
(`interactions` today), a fallback is allowed only to another wire API the
same model explicitly offers and the client compiles. Every emitted entry
must match a compiled `(provider, api, baseUrl)` tuple.
A subscription model is admitted when the Codex list shows it to
subscribers (`visibility: list`) and the catalog does not list it: that
list only holds current models, so there is no date to compare.
A model already in the additions file stays, whatever its date, until the
catalog lists it or its source stops describing it as usable.

The entry mapping is the catalog exporter's
(prime-agent-catalog/exporter/generate-models.ts: `loadModelsDevData`,
`getModelsDevThinkingLevelMap`, `applyThinkingLevelMetadata`). Each run
prints where this mapping and the catalog disagree on a model both know.

usage: scripts/sync-model-additions.py [--catalog FILE-OR-URL]
           [--models-dev FILE-OR-URL] [--codex-models FILE-OR-URL]
           [--opencode-models FILE-OR-URL]
           [--opencode-go-models FILE-OR-URL]
           [--compiled-models FILE] [--out FILE]

Any failure to read or understand a source exits 1 and leaves the output
file untouched.
"""

from __future__ import annotations

import argparse
import json
import math
import re
import sys
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DEFAULT_OUT = REPO / "catalog/models.additions.v1.json"
CATALOG_URL = (
    "https://raw.githubusercontent.com/PrimeIntellect-ai/prime-agent-catalog"
    "/main/models/catalog.v1.json"
)
MODELS_DEV_URL = "https://models.dev/api.json"
CODEX_MODELS_URL = (
    "https://raw.githubusercontent.com/openai/codex/main/codex-rs/models-manager/models.json"
)
OPENCODE_MODELS_URL = "https://opencode.ai/zen/v1/models"
OPENCODE_GO_MODELS_URL = "https://opencode.ai/zen/go/v1/models"
COMPILED_MODELS_PATH = REPO / "crates/uh-ai/src/models_generated.rs"
FETCH_TIMEOUT_SECONDS = 30
MAX_SOURCE_BYTES = 64 * 1024 * 1024

# The API providers handled, keyed by catalog provider id (also their
# models.dev slug), with the transport the exporter gives their models.
# `exclude` matches ids the provider's API does not serve as plain chat
# models.
PROVIDERS = {
    "anthropic": {
        "api": "anthropic-messages",
        "baseUrl": "https://api.anthropic.com",
    },
    "google": {
        "api": "google-generative-ai",
        "baseUrl": "https://generativelanguage.googleapis.com/v1beta",
        "exclude": re.compile(r"(^|[-_.])(live|deep-research|computer-use)($|[-_.])", re.I),
    },
    "openai": {
        "api": "openai-responses",
        "baseUrl": "https://api.openai.com/v1",
    },
}

# The multi-transport providers this script handles. The base URLs are not
# taken from provider metadata: they are the compiled transports a catalog
# entry is allowed to select.
COPILOT = "github-copilot"
OPENCODE_PROVIDERS = ("opencode", "opencode-go")
MULTI_TRANSPORTS = {
    COPILOT: {
        "anthropic-messages": "https://api.individual.githubcopilot.com",
        "openai-completions": "https://api.individual.githubcopilot.com",
        "openai-responses": "https://api.individual.githubcopilot.com",
    },
    "opencode": {
        "anthropic-messages": "https://opencode.ai/zen",
        "google-generative-ai": "https://opencode.ai/zen/v1",
        "openai-completions": "https://opencode.ai/zen/v1",
        "openai-responses": "https://opencode.ai/zen/v1",
    },
    "opencode-go": {
        "anthropic-messages": "https://opencode.ai/zen/go",
        "openai-completions": "https://opencode.ai/zen/go/v1",
        "openai-responses": "https://opencode.ai/zen/go/v1",
    },
}
OPENCODE_REMOTE_APIS = {
    "messages": "anthropic-messages",
    "generate-content": "google-generative-ai",
    "chat-completions": "openai-completions",
    "responses": "openai-responses",
}
# Prefer the endpoint's recommendation. When it names a newer protocol the
# client does not implement (`interactions` today), fall back to a native
# protocol the same endpoint explicitly offers; never invent one.
OPENCODE_API_FALLBACK_ORDER = ("messages", "generate-content", "responses", "chat-completions")
ALL_ADDITION_PROVIDERS = (*PROVIDERS, COPILOT, "openai-codex", *OPENCODE_PROVIDERS)

# The ChatGPT subscription: its provider, its transport, the models.dev
# provider whose model of the same id gives the name and the prices, and
# the output limit the exporter gives every subscription model.
CODEX = "openai-codex"
CODEX_TRANSPORT = {"api": "openai-codex-responses", "baseUrl": "https://chatgpt.com/backend-api"}
CODEX_API_TWIN = "openai"
CODEX_MAX_TOKENS = 128_000

LEVELS = ("minimal", "low", "medium", "high", "xhigh", "max")
RELEASE_DATE = re.compile(r"^\d{4}-\d{2}-\d{2}$")
OPENAI_RESPONSES_NONE_REASONING_MODELS = {
    "gpt-5.1", "gpt-5.2", "gpt-5.3-codex", "gpt-5.4", "gpt-5.4-mini",
    "gpt-5.4-nano", "gpt-5.5", "gpt-5.6", "gpt-5.6-sol", "gpt-5.6-terra",
    "gpt-5.6-luna", "gpt-6-sol", "gpt-6-luna",
}  # fmt: skip
# The entry fields the mapping decides (prices and limits are models.dev's).
MAPPED_FIELDS = ("api", "baseUrl", "reasoning", "thinkingLevelMap", "input")


class SourceError(Exception):
    """A source could not be read or does not have the expected shape."""


def read_json(source: str, label: str):
    try:
        if re.match(r"^https?://", source):
            request = urllib.request.Request(
                source,
                headers={"accept": "application/json", "user-agent": "unharnessed-model-additions"},
            )
            with urllib.request.urlopen(request, timeout=FETCH_TIMEOUT_SECONDS) as response:
                raw = response.read(MAX_SOURCE_BYTES + 1)
        else:
            raw = Path(source).read_bytes()
        if len(raw) > MAX_SOURCE_BYTES:
            raise SourceError(f"{label} ({source}) is larger than {MAX_SOURCE_BYTES} bytes")
        return json.loads(raw)
    except (OSError, ValueError) as error:
        raise SourceError(f"cannot read {label} ({source}): {error}") from error


def derived_thinking_levels(model: dict) -> dict | None:
    """The exporter's `getModelsDevThinkingLevelMap`."""
    options = model.get("reasoning_options")
    effort = next(
        (
            option
            for option in (options if isinstance(options, list) else [])
            if isinstance(option, dict)
            and option.get("type") == "effort"
            and isinstance(option.get("values"), list)
        ),
        None,
    )
    if effort is None:
        return None
    values = {value for value in effort["values"] if isinstance(value, str)}
    levels = {"off": "none" if "none" in values else "off" if "off" in values else None}
    for level in LEVELS:
        levels[level] = level if level in values else None
    return levels


def apply_thinking_level_rules(entry: dict) -> None:
    """The exporter's `applyThinkingLevelMetadata` rules that can match one
    of PROVIDERS: what models.dev does not say about a model family. A new
    family with its own quirks needs its rule here; without one, the map
    derived from models.dev is used as is."""
    model_id, api = entry["id"], entry["api"]

    def has(*needles: str) -> bool:
        return any(needle in model_id for needle in needles)

    def merge(**levels) -> None:
        entry["thinkingLevelMap"] = {**entry.get("thinkingLevelMap", {}), **levels}

    if api == "openai-responses" and model_id.startswith("gpt-5"):
        merge(off=None)
    if api == "openai-responses" and model_id in OPENAI_RESPONSES_NONE_REASONING_MODELS:
        merge(off="none")
    if has("gpt-5.2", "gpt-5.3", "gpt-5.4", "gpt-5.5", "gpt-5.6"):
        merge(xhigh="xhigh")
    if has("gpt-5.6"):
        merge(minimal=None, max="max")
    if has("gpt-6"):
        merge(minimal=None, xhigh="xhigh", max="max")
    if api == "openai-responses" and has("gpt-6-astra"):
        merge(off=None)
    if has("opus-4-6", "opus-4.6", "sonnet-4-6", "sonnet-4.6"):
        merge(max="max")
    if has("opus-4-7", "opus-4.7", "opus-4-8", "opus-4.8", "opus-5", "sonnet-5"):
        merge(xhigh="xhigh", max="max")
    if has("fable-5", "mythos-5"):
        merge(off=None, xhigh="xhigh", max="max")
    if has("opus-5-5", "opus-5.5"):
        merge(off=None)
    if has("mythos-preview"):
        merge(off=None, max="max")
    lower = model_id.lower()
    if api == "google-generative-ai":
        if re.search(r"gemini-3(?:\.\d+)?-pro", lower):
            merge(off=None, minimal=None, low="LOW", medium=None, high="HIGH")
        if re.search(r"gemini-3(?:\.\d+)?-flash", lower):
            merge(off=None)
        if re.search(r"gemma-?4", lower):
            merge(off=None, minimal="MINIMAL", low=None, medium=None, high="HIGH")
    # models.dev lists effort levels, not whether thinking can be disabled:
    # only the always-on Anthropic models keep the derived `off: null`.
    always_on = has("fable-5", "mythos-5", "mythos-preview", "opus-5-5", "opus-5.5")
    levels = entry.get("thinkingLevelMap")
    if api == "anthropic-messages" and levels and "off" in levels and levels["off"] is None and not always_on:
        del levels["off"]


def catalog_entry(provider: str, model_id: str, model: dict, transport: dict | None = None) -> dict:
    """One models.dev model as a `catalog.v1` entry (the exporter's
    `loadModelsDevData` mapping). The result may be invalid: see
    `invalid_reason`."""
    transport = transport or PROVIDERS[provider]
    name = model.get("name")
    entry = {
        "id": model_id,
        "name": name if isinstance(name, str) and name else model_id,
        "api": transport["api"],
        "provider": provider,
        "baseUrl": transport["baseUrl"],
        "reasoning": model.get("reasoning") is True,
    }
    levels = derived_thinking_levels(model)
    if levels:
        entry["thinkingLevelMap"] = levels
    modalities = model.get("modalities")
    inputs = modalities.get("input") if isinstance(modalities, dict) else None
    if isinstance(inputs, list) and inputs:
        entry["input"] = ["text", "image"] if "image" in inputs else ["text"]
    else:
        entry["input"] = []
    cost = model.get("cost") if isinstance(model.get("cost"), dict) else {}
    entry["cost"] = {
        "input": cost.get("input"),
        "output": cost.get("output"),
        "cacheRead": 0 if cost.get("cache_read") is None else cost.get("cache_read"),
        "cacheWrite": 0 if cost.get("cache_write") is None else cost.get("cache_write"),
    }
    limit = model.get("limit") if isinstance(model.get("limit"), dict) else {}
    entry["contextWindow"] = limit.get("context")
    entry["maxTokens"] = limit.get("output")
    apply_thinking_level_rules(entry)
    return entry



def compiled_transport_tuples(path: Path) -> set[tuple[str, str, str]]:
    """The `(provider, api, baseUrl)` tuples compiled into this repository.

    The runtime derives its pinning table from the same generated Rust file.
    Parsing it here keeps the generator honest without duplicating the whole
    table in Python.
    """
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        raise SourceError(f"cannot read the compiled model table ({path}): {error}") from error
    rows = set()
    for block in re.split(r"\n\s*RawModel\s*\{", text)[1:]:
        fields = {}
        for field in ("provider", "api", "base_url"):
            match = re.search(rf"\b{field}:\s*\"([^\"]+)\"", block)
            if match:
                fields[field] = match.group(1)
        if all(field in fields for field in ("provider", "api", "base_url")):
            rows.add((fields["provider"], fields["api"], fields["base_url"]))
    if len(rows) < 10:
        raise SourceError(f"the compiled model table ({path}) has no usable transport tuples")
    return rows


def pinning_reason(entry: dict, compiled: set[tuple[str, str, str]]) -> str | None:
    """Why transport pinning would drop this entry; None when it would not."""
    key = (entry.get("provider"), entry.get("api"), entry.get("baseUrl"))
    return None if key in compiled else "no compiled transport"


def copilot_api(model: dict) -> str | None:
    """The verified Copilot wire API for a models.dev model family.

    Copilot does not expose a public unauthenticated per-model transport
    list. These rules are deliberately narrow: a family outside the observed
    Copilot catalog shape fails closed instead of guessing between the two
    OpenAI-compatible APIs.
    """
    family = model.get("family") if isinstance(model.get("family"), str) else ""
    family = family.lower()
    if family.startswith("claude"):
        return "anthropic-messages"
    if family.startswith(("gemini", "kimi")):
        return "openai-completions"
    if family.startswith(("gpt", "grok", "mai-code")):
        return "openai-responses"
    return None


def copilot_entry(model_id: str, model: dict) -> tuple[dict | None, str | None]:
    """A Copilot addition and why its transport cannot be verified."""
    api = copilot_api(model)
    base_url = MULTI_TRANSPORTS[COPILOT].get(api or "")
    if not api or not base_url:
        return None, "no verified Copilot transport"
    return catalog_entry(COPILOT, model_id, model, {"api": api, "baseUrl": base_url}), None


def endpoint_thinking_levels(model: dict) -> dict | None:
    """The OpenCode endpoint's reasoning controls as a thinkingLevelMap.

    The endpoint is authoritative: `toggle` adds a `none` variant, while
    `supported` without any control means no level may be invented."""
    capabilities = model.get("capabilities") if isinstance(model.get("capabilities"), dict) else {}
    reasoning = capabilities.get("reasoning") if isinstance(capabilities.get("reasoning"), dict) else {}
    if reasoning.get("supported") is not True:
        return None
    efforts = reasoning.get("effort")
    values = {value for value in efforts if isinstance(value, str)} if isinstance(efforts, list) else set()
    can_disable = reasoning.get("toggle") is True or "none" in values
    levels = {"off": "none" if can_disable else "off" if "off" in values else None}
    for level in LEVELS:
        levels[level] = level if level in values else None
    return levels


def finite_price(value) -> int | float | None:
    """`value` when it is a finite JSON price."""
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value):
        return None
    return value


def opencode_cost(model: dict) -> dict:
    """The endpoint's nested text cost in the catalog's flat shape."""
    cost = model.get("cost") if isinstance(model.get("cost"), dict) else {}
    text = cost.get("text") if isinstance(cost.get("text"), dict) else {}
    input_cost = text.get("input") if isinstance(text.get("input"), dict) else {}
    output_cost = text.get("output") if isinstance(text.get("output"), dict) else {}
    cache = input_cost.get("cache") if isinstance(input_cost.get("cache"), dict) else {}
    write = cache.get("write")
    if isinstance(write, dict):
        writes = [price for price in (finite_price(value) for value in write.values()) if price is not None]
        write = min(writes) if writes else None
    return {
        "input": finite_price(input_cost.get("price")),
        "output": finite_price(output_cost.get("price")),
        "cacheRead": finite_price(cache.get("read")) or 0,
        "cacheWrite": finite_price(write) or 0,
    }


def opencode_remote_api(provider: str, model: dict) -> str | None:
    """The endpoint API to use, or None when no client-supported API is offered."""
    apis = model.get("api") if isinstance(model.get("api"), dict) else {}
    recommended = model.get("recommended_api")
    if recommended in apis and recommended in OPENCODE_REMOTE_APIS and OPENCODE_REMOTE_APIS[recommended] in MULTI_TRANSPORTS[provider]:
        return recommended
    for remote_api in OPENCODE_API_FALLBACK_ORDER:
        if remote_api in apis and OPENCODE_REMOTE_APIS[remote_api] in MULTI_TRANSPORTS[provider]:
            return remote_api
    return None


def opencode_entry(provider: str, model: dict) -> tuple[dict | None, str | None]:
    """One OpenCode endpoint model as a catalog entry on a compiled transport."""
    remote_api = opencode_remote_api(provider, model)
    api = OPENCODE_REMOTE_APIS.get(remote_api or "")
    base_url = MULTI_TRANSPORTS[provider].get(api or "")
    if not remote_api or not api or not base_url:
        return None, "unsupported OpenCode transport"
    modalities = model.get("modalities") if isinstance(model.get("modalities"), dict) else {}
    inputs = modalities.get("input") if isinstance(modalities.get("input"), list) else []
    limit = model.get("limit") if isinstance(model.get("limit"), dict) else {}
    capabilities = model.get("capabilities") if isinstance(model.get("capabilities"), dict) else {}
    reasoning = capabilities.get("reasoning") if isinstance(capabilities.get("reasoning"), dict) else {}
    name = model.get("name")
    entry = {
        "id": model.get("id"),
        "name": name if isinstance(name, str) and name else model.get("id"),
        "api": api,
        "provider": provider,
        "baseUrl": base_url,
        "reasoning": reasoning.get("supported") is True,
        "input": (["text", "image"] if any(
            isinstance(value, str) and value.startswith("image/") for value in inputs
        ) else ["text"]) if inputs else [],
        "cost": opencode_cost(model),
        "contextWindow": limit.get("context"),
        "maxTokens": limit.get("output"),
    }
    levels = endpoint_thinking_levels(model)
    if levels:
        entry["thinkingLevelMap"] = levels
    return entry, None


def opencode_unusable(entry: dict, model: dict, compiled: set[tuple[str, str, str]]) -> str | None:
    """Why a session cannot use this OpenCode endpoint model."""
    capabilities = model.get("capabilities") if isinstance(model.get("capabilities"), dict) else {}
    tools = capabilities.get("tools") if isinstance(capabilities.get("tools"), dict) else {}
    modalities = model.get("modalities") if isinstance(model.get("modalities"), dict) else {}
    if model.get("type") != "chat":
        return "not a chat model"
    if tools.get("supported") is not True:
        return "no tool calls"
    if modalities.get("output") != ["text"]:
        return "output is not text only"
    return invalid_reason(entry) or pinning_reason(entry, compiled)


def opencode_candidates(payload, provider: str, compiled: set[tuple[str, str, str]]) -> dict:
    """Current chat models of one OpenCode endpoint as reconcile candidates."""
    models = payload.get("data") if isinstance(payload, dict) else None
    if not isinstance(models, list):
        raise SourceError(f"the {provider} model list has no data list")
    candidates = {}
    for model in models:
        if not isinstance(model, dict) or not isinstance(model.get("id"), str) or not model["id"]:
            continue
        if model.get("type") != "chat":
            continue
        entry, transport_error = opencode_entry(provider, model)
        if entry is None:
            entry = {"id": model["id"], "name": model["id"], "input": [], "cost": {}}
            unusable = transport_error
        else:
            unusable = opencode_unusable(entry, model, compiled)
        candidates[model["id"]] = (entry, unusable, "currently served")
    if not any(unusable is None for _, unusable, _ in candidates.values()):
        raise SourceError(f"the {provider} model list holds no supported chat model")
    return candidates

def invalid_reason(entry: dict) -> str | None:
    """Why the client's strict schema (crates/uh-models/src/schema.rs)
    would reject `entry`; None when it would not."""
    for field in ("id", "name"):
        text = entry[field]
        if not text or len(text) > 1024 or any(ord(char) < 32 or ord(char) == 127 for char in text):
            return f"unusable {field}"
    if not entry["input"]:
        return "no input modality"
    for field, price in entry["cost"].items():
        if isinstance(price, bool) or not isinstance(price, (int, float)):
            return f"no {field} price"
        if not math.isfinite(price) or not 0 <= price <= 1_000_000:
            return f"{field} price out of range"
    for field in ("contextWindow", "maxTokens"):
        tokens = entry[field]
        if isinstance(tokens, bool) or not isinstance(tokens, int) or not 1 <= tokens <= 100_000_000:
            return f"no usable {field}"
    return None


def unusable_reason(
    provider: str,
    model_id: str,
    model: dict,
    compiled: set[tuple[str, str, str]],
    entry: dict | None = None,
) -> str | None:
    """Why a session cannot use this models.dev model; None when it can."""
    if model.get("tool_call") is not True:
        return "no tool calls"
    if model.get("status") == "deprecated":
        return "deprecated"
    modalities = model.get("modalities")
    outputs = modalities.get("output") if isinstance(modalities, dict) else None
    if outputs != ["text"]:
        return "output is not text only"
    exclude = PROVIDERS.get(provider, {}).get("exclude")
    if exclude and exclude.search(model_id):
        return "not served as a chat model"
    entry = entry or catalog_entry(provider, model_id, model)
    return invalid_reason(entry) or pinning_reason(entry, compiled)


def release_date(model: dict) -> str | None:
    date = model.get("release_date")
    return date if isinstance(date, str) and RELEASE_DATE.match(date) else None


def supported_levels(levels: dict | None) -> dict:
    """What a `thinkingLevelMap` means to the client
    (crates/uh-types/src/ai/thinking_levels.rs): a level left out is
    supported under its own name, except xhigh and max."""
    levels = levels or {}
    default = {level: None if level in ("xhigh", "max") else level for level in ("off", *LEVELS)}
    return {level: levels[level] if level in levels else default[level] for level in default}


def codex_entry(slug: str, model: dict, twin: dict | None) -> dict:
    """One model of the Codex list as a `catalog.v1` entry. The list gives
    what the subscription serves (context window, effort levels, inputs);
    `twin`, the models.dev OpenAI model of the same id, gives the name, the
    prices and whether reasoning can be turned off. The result may be
    invalid: see `invalid_reason`."""
    twin_entry = catalog_entry(CODEX_API_TWIN, slug, twin) if twin else None
    display_name = model.get("display_name")
    described = model.get("supported_reasoning_levels")
    efforts = {
        level.get("effort")
        for level in (described if isinstance(described, list) else [])
        if isinstance(level, dict)
    }
    levels = {level: level if level in efforts else None for level in LEVELS}
    twin_levels = (twin_entry or {}).get("thinkingLevelMap") or {}
    if "off" in twin_levels and twin_levels["off"] is None:
        levels["off"] = None
    # The exporter's subscription rule for the families before gpt-5.6.
    if any(family in slug for family in ("gpt-5.2", "gpt-5.3", "gpt-5.4", "gpt-5.5")):
        levels["minimal"] = "low"
    inputs = model.get("input_modalities")
    cost = twin_entry["cost"] if twin_entry else None
    priced = cost and all(
        isinstance(price, (int, float)) and not isinstance(price, bool) for price in cost.values()
    )
    limit = twin.get("limit") if twin and isinstance(twin.get("limit"), dict) else {}
    max_tokens = limit.get("output")
    usable_limit = isinstance(max_tokens, int) and not isinstance(max_tokens, bool) and max_tokens >= 1
    return {
        "id": slug,
        "name": twin_entry["name"] if twin_entry else display_name if isinstance(display_name, str) else slug,
        "api": CODEX_TRANSPORT["api"],
        "provider": CODEX,
        "baseUrl": CODEX_TRANSPORT["baseUrl"],
        "reasoning": bool(efforts),
        "thinkingLevelMap": levels,
        "input": (["text", "image"] if "image" in inputs else ["text"])
        if isinstance(inputs, list) and inputs
        else [],
        # A subscription bills no tokens: without a twin the prices are 0.
        "cost": cost if priced else {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": model.get("context_window"),
        "maxTokens": max_tokens if usable_limit else CODEX_MAX_TOKENS,
    }


def provider_models(models_dev, provider: str) -> dict:
    described = models_dev.get(provider) if isinstance(models_dev, dict) else None
    models = described.get("models") if isinstance(described, dict) else None
    if not isinstance(models, dict) or not models:
        raise SourceError(f"models.dev lists no models for {provider}")
    return {model_id: model for model_id, model in models.items() if isinstance(model, dict)}


def codex_models(codex) -> dict:
    """`{slug: model}` of the Codex list."""
    models = codex.get("models") if isinstance(codex, dict) else None
    by_slug = {
        model["slug"]: model
        for model in (models if isinstance(models, list) else [])
        if isinstance(model, dict) and isinstance(model.get("slug"), str) and model["slug"]
    }
    if not by_slug:
        raise SourceError("the Codex list holds no models")
    return by_slug


def catalog_ids(catalog) -> dict[str, dict[str, dict]]:
    """`{provider: {id: entry}}` for the catalog's entries of the providers
    handled; every one of them must have some."""
    if not isinstance(catalog, dict) or catalog.get("schemaVersion") != 1:
        raise SourceError("the catalog is not a schemaVersion 1 catalog")
    models = catalog.get("models")
    if not isinstance(models, list) or not models:
        raise SourceError("the catalog lists no models")
    listed: dict[str, dict[str, dict]] = {provider: {} for provider in ALL_ADDITION_PROVIDERS}
    for entry in models:
        if isinstance(entry, dict) and entry.get("provider") in listed and isinstance(entry.get("id"), str):
            listed[entry["provider"]][entry["id"]] = entry
    for provider, entries in listed.items():
        if not entries:
            raise SourceError(f"the catalog lists no {provider} model")
    return listed


def previous_additions(path: Path) -> set[tuple[str, str]]:
    """The `(provider, id)` pairs of the current additions file."""
    if not path.exists():
        return set()
    payload = read_json(str(path), "the current additions file")
    models = payload.get("models") if isinstance(payload, dict) else None
    if not isinstance(models, list):
        raise SourceError(f"the current additions file ({path}) has no models list")
    return {
        (entry["provider"], entry["id"])
        for entry in models
        if isinstance(entry, dict)
        and isinstance(entry.get("provider"), str)
        and isinstance(entry.get("id"), str)
    }


def reconcile(provider: str, listed: dict, candidates: dict, previous: set, report: list[str]) -> list[dict]:
    """The additions of one provider. `candidates` maps each id its source
    describes to `(entry, unusable, new)`: the entry, why a session cannot
    use it (None when it can), and why it counts as new (None when only a
    model already added may stay)."""
    additions = []
    for model_id in sorted(candidates):
        entry, unusable, new = candidates[model_id]
        was_added = (provider, model_id) in previous
        if model_id in listed:
            if was_added:
                report.append(f"removed {provider}/{model_id}: the catalog lists it now")
        elif unusable:
            if was_added:
                report.append(f"removed {provider}/{model_id}: {unusable}")
        elif was_added:
            report.append(f"kept    {provider}/{model_id}")
            additions.append(entry)
        elif new:
            report.append(f"added   {provider}/{model_id} ({new})")
            additions.append(entry)
    for added_provider, model_id in sorted(previous):
        if added_provider == provider and model_id not in candidates:
            report.append(f"removed {provider}/{model_id}: its source no longer lists it")
    return additions


def sync(
    catalog,
    models_dev,
    codex,
    opencode,
    opencode_go,
    compiled: set[tuple[str, str, str]],
    previous: set[tuple[str, str]],
) -> tuple[list[dict], list[str]]:
    """The additions and the report lines."""
    listed = catalog_ids(catalog)
    models_dev_providers = (*PROVIDERS, COPILOT)
    described = {provider: provider_models(models_dev, provider) for provider in models_dev_providers}
    subscription = codex_models(codex)
    additions: list[dict] = []
    report: list[str] = []

    def differs(provider: str, model_id: str, fields: list[str]) -> None:
        if fields:
            report.append(
                f"mapping differs from the catalog: {provider}/{model_id} ({', '.join(fields)})"
            )

    for provider in sorted(PROVIDERS):
        models = described[provider]
        dates = [
            date
            for model_id in listed[provider]
            if model_id in models and (date := release_date(models[model_id]))
        ]
        if not dates:
            raise SourceError(
                f"no release date on models.dev for any {provider} model of the catalog"
            )
        newest = max(dates)
        candidates = {}
        for model_id, model in models.items():
            entry = catalog_entry(provider, model_id, model)
            date = release_date(model)
            candidates[model_id] = (
                entry,
                unusable_reason(provider, model_id, model, compiled, entry),
                f"released {date}" if date and date > newest else None,
            )
            if model_id in listed[provider]:
                known = listed[provider][model_id]
                differs(provider, model_id, [f for f in MAPPED_FIELDS if entry.get(f) != known.get(f)])
        additions += reconcile(provider, listed[provider], candidates, previous, report)

    # Copilot has no public unauthenticated transport list. Its models.dev
    # entries keep the same newest-cataloged-release watermark as the API
    # providers; only narrow, audited family rules select a compiled API.
    provider = COPILOT
    models = described[provider]
    dates = [
        date
        for model_id in listed[provider]
        if model_id in models and (date := release_date(models[model_id]))
    ]
    if not dates:
        raise SourceError(f"no release date on models.dev for any {provider} model of the catalog")
    newest = max(dates)
    candidates = {}
    for model_id, model in models.items():
        entry, transport_error = copilot_entry(model_id, model)
        date = release_date(model)
        if entry is None:
            entry = {"id": model_id, "name": model_id, "input": [], "cost": {}}
            unusable = transport_error
        else:
            unusable = transport_error or unusable_reason(provider, model_id, model, compiled, entry)
        candidates[model_id] = (
            entry,
            unusable,
            f"released {date}" if date and date > newest else None,
        )
        if model_id in listed[provider] and entry.get("api"):
            known = listed[provider][model_id]
            differs(provider, model_id, [f for f in MAPPED_FIELDS if entry.get(f) != known.get(f)])
    additions += reconcile(provider, listed[provider], candidates, previous, report)

    # OpenCode's endpoint lists only current models and states each model's
    # recommended wire API. Unlike models.dev it needs no release watermark:
    # absence means retired, so every currently served chat model missing
    # from the catalog is an addition candidate.
    for provider, payload in (("opencode", opencode), ("opencode-go", opencode_go)):
        candidates = opencode_candidates(payload, provider, compiled)
        for model_id, (entry, _unusable, _new) in candidates.items():
            if model_id in listed[provider] and entry.get("api"):
                known = listed[provider][model_id]
                differs(provider, model_id, [f for f in MAPPED_FIELDS if entry.get(f) != known.get(f)])
        additions += reconcile(provider, listed[provider], candidates, previous, report)

    candidates = {}
    for slug, model in subscription.items():
        entry = codex_entry(slug, model, described[CODEX_API_TWIN].get(slug))
        unusable = (
            "not shown to subscribers" if model.get("visibility") != "list" else invalid_reason(entry) or pinning_reason(entry, compiled)
        )
        candidates[slug] = (entry, unusable, "shown to subscribers")
        if slug in listed[CODEX]:
            known = listed[CODEX][slug]
            fields = [f for f in ("api", "baseUrl", "reasoning", "input", "contextWindow") if entry[f] != known.get(f)]
            if supported_levels(entry["thinkingLevelMap"]) != supported_levels(known.get("thinkingLevelMap")):
                fields.append("thinkingLevelMap")
            differs(CODEX, slug, fields)
    additions += reconcile(CODEX, listed[CODEX], candidates, previous, report)
    additions.sort(key=lambda entry: (entry["provider"], entry["id"]))
    return additions, report

def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--catalog", default=CATALOG_URL, help="catalog.v1.json file or URL")
    parser.add_argument("--models-dev", default=MODELS_DEV_URL, help="models.dev api.json file or URL")
    parser.add_argument("--codex-models", default=CODEX_MODELS_URL, help="Codex models.json file or URL")
    parser.add_argument("--opencode-models", default=OPENCODE_MODELS_URL, help="OpenCode models list file or URL")
    parser.add_argument("--opencode-go-models", default=OPENCODE_GO_MODELS_URL, help="OpenCode Go models list file or URL")
    parser.add_argument("--compiled-models", type=Path, default=COMPILED_MODELS_PATH, help="generated Rust model table")
    parser.add_argument("--out", type=Path, default=DEFAULT_OUT, help="the additions file to update")
    args = parser.parse_args()
    try:
        additions, report = sync(
            read_json(args.catalog, "the catalog"),
            read_json(args.models_dev, "models.dev"),
            read_json(args.codex_models, "the Codex list"),
            read_json(args.opencode_models, "the OpenCode model list"),
            read_json(args.opencode_go_models, "the OpenCode Go model list"),
            compiled_transport_tuples(args.compiled_models),
            previous_additions(args.out),
        )
    except SourceError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    rendered = json.dumps({"schemaVersion": 1, "models": additions}, indent=2, ensure_ascii=False) + "\n"
    changed = not args.out.exists() or args.out.read_text(encoding="utf-8") != rendered
    if changed:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(rendered, encoding="utf-8")
    for line in report:
        print(line)
    print(f"{len(additions)} additions, {args.out} {'updated' if changed else 'unchanged'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
