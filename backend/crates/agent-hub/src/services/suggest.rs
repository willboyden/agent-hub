//! Port of services/suggest.py: collection suggestions (common prefix, source, tag, keyword), deterministic ids,
//! titles and palette colours. No I/O, no randomness: the same content always yields the same ids, titles and order.
//! Owner: WS-B1.

use std::collections::{BTreeSet, HashSet};

use hub_core::HubResult;
use hub_core::ids::slugify;
use hub_core::py::text::{is_space, take_chars};
use indexmap::IndexMap;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::content::{Catalog, Item};

pub const PALETTE: [&str; 8] = [
    "#4f8cff", "#2fbf71", "#f5a524", "#e5484d", "#8e4ec6", "#12a594", "#d6409f", "#6e7681",
];
pub const MIN_MEMBERS: usize = 3;
pub const MAX_MEMBERS: usize = 40;

const SPECIAL: [(&str, &str); 12] = [
    ("api", "API"),
    ("mcp", "MCP"),
    ("llm", "LLM"),
    ("gpu", "GPU"),
    ("cuda", "CUDA"),
    ("vllm", "vLLM"),
    ("sglang", "SGLang"),
    ("adr", "ADR"),
    ("ui", "UI"),
    ("ci", "CI"),
    ("pr", "PR"),
    ("hf", "HF"),
];

/// `KEYWORDS`, already in `sorted(KEYWORDS.items())` order: key -> (title, words, icon).
const KEYWORDS: [(&str, &str, &[&str], &str); 5] = [
    (
        "benchmark",
        "Benchmarking",
        &[
            "bench",
            "benchmark",
            "perf",
            "performance",
            "eval",
            "latency",
            "throughput",
        ],
        "gauge",
    ),
    (
        "deploy",
        "Deploy and release",
        &["deploy", "release", "publish", "rollout", "ship", "install"],
        "rocket",
    ),
    (
        "docs",
        "Documentation",
        &["doc", "docs", "documentation", "readme", "adr", "guide", "changelog"],
        "book",
    ),
    (
        "security",
        "Security",
        &[
            "security",
            "secret",
            "secrets",
            "audit",
            "sandbox",
            "egress",
            "harden",
            "hardening",
            "threat",
            "vuln",
        ],
        "shield",
    ),
    (
        "troubleshoot",
        "Troubleshooting",
        &[
            "debug",
            "troubleshoot",
            "diagnose",
            "incident",
            "triage",
            "fix",
            "monitor",
        ],
        "wrench",
    ),
];

fn order(reason: &str) -> u8 {
    match reason {
        "common_prefix" => 0,
        "source" => 1,
        "tag" => 2,
        _ => 3,
    }
}

/// `str.capitalize()` (Python 3.8+: the first character title-cased, the rest lower-cased).
fn capitalize(t: &str) -> String {
    let mut chars = t.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let head: String = match first {
        'Ǆ' | 'ǅ' | 'ǆ' => "ǅ".into(),
        'Ǉ' | 'ǈ' | 'ǉ' => "ǈ".into(),
        'Ǌ' | 'ǋ' | 'ǌ' => "ǋ".into(),
        'Ǳ' | 'ǲ' | 'ǳ' => "ǲ".into(),
        'ß' => "Ss".into(),
        'ﬀ' => "Ff".into(),
        'ﬁ' => "Fi".into(),
        'ﬂ' => "Fl".into(),
        'ﬃ' => "Ffi".into(),
        'ﬄ' => "Ffl".into(),
        'ﬅ' | 'ﬆ' => "St".into(),
        c => c.to_uppercase().collect(),
    };
    format!("{head}{}", chars.as_str().to_lowercase())
}

/// `_title(text)`: words split on `[-_.\s]+`, special-cased acronyms, else capitalised.
pub fn title(text: &str) -> String {
    text.split(|c: char| matches!(c, '-' | '_' | '.') || is_space(c))
        .filter(|t| !t.is_empty())
        .map(|t| {
            SPECIAL
                .iter()
                .find(|(k, _)| *k == t)
                .map_or_else(|| capitalize(t), |(_, v)| (*v).to_string())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `_colour(cid)`: `PALETTE[int(sha256(cid).hexdigest(), 16) % 8]` (the low bits of the digest's last byte).
pub fn colour(cid: &str) -> &'static str {
    let d = Sha256::digest(cid.as_bytes());
    PALETTE[usize::from(d[31] % 8)]
}

/// `_words(item)`: lower-cased `[a-z0-9]+` runs of the name and the first 300 characters of the description.
fn words(item: &Item) -> HashSet<String> {
    let text = format!("{} {}", item.name, take_chars(&item.description(), 300)).to_lowercase();
    text.split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

struct Raw<'a> {
    reason: &'static str,
    id: String,
    title: String,
    description: String,
    icon: &'static str,
    members: Vec<&'a Item>,
}

/// `suggest(cat)`: the suggestion rows (GET /collections/suggestions `items`).
pub fn suggest(cat: &Catalog) -> HubResult<Vec<Value>> {
    let items: Vec<&Item> = cat.items.values().filter(|i| i.valid()).collect();
    let mut raw: Vec<Raw<'_>> = Vec::new();

    // 1. common hyphen prefixes (one or two tokens)
    let mut groups: IndexMap<String, Vec<&Item>> = IndexMap::new();
    for it in &items {
        if it.kind == "rule" && it.name.starts_with("imp-") {
            continue;
        }
        let toks: Vec<&str> = it.name.split('-').collect();
        for n in [1, 2] {
            if toks.len() >= n {
                let pre = toks[..n].join("-");
                if pre.chars().count() >= 3 && !pre.chars().all(|c| c.is_ascii_digit()) {
                    groups.entry(pre).or_default().push(it);
                }
            }
        }
    }
    for (pre, members) in groups {
        let members: Vec<&Item> = members
            .into_iter()
            .filter(|m| m.name == pre || m.name.starts_with(&format!("{pre}-")))
            .collect();
        let distinct: BTreeSet<(&str, &str)> = members.iter().map(|m| (m.kind.as_str(), m.name.as_str())).collect();
        if distinct.len() >= MIN_MEMBERS {
            raw.push(Raw {
                reason: "common_prefix",
                id: slugify(&pre),
                title: title(&pre),
                description: format!("Items whose names start with '{pre}'"),
                icon: "folder",
                members,
            });
        }
    }
    // 2. provenance (skills carry hub.yaml `source`)
    let mut by_src: IndexMap<String, Vec<&Item>> = IndexMap::new();
    for it in &items {
        let src = it.source();
        if !src.is_empty() {
            by_src.entry(src).or_default().push(it);
        }
    }
    for (src, members) in by_src {
        let tail = src.split_once(':').map_or(src.as_str(), |(_, t)| t);
        let label = if tail.is_empty() { src.as_str() } else { tail };
        raw.push(Raw {
            reason: "source",
            id: slugify(&format!("from-{label}")),
            title: format!("From {}", title(label)),
            description: format!("Imported from the same source ({src})"),
            icon: "download",
            members,
        });
    }
    // 3. shared tags
    let mut by_tag: IndexMap<String, Vec<&Item>> = IndexMap::new();
    for it in &items {
        for t in it.tags() {
            if t != "imported" {
                by_tag.entry(t).or_default().push(it);
            }
        }
    }
    for (tag, members) in by_tag {
        raw.push(Raw {
            reason: "tag",
            id: slugify(&format!("tag-{tag}")),
            title: format!("Tagged {}", title(&tag)),
            description: format!("Items tagged '{tag}'"),
            icon: "tag",
            members,
        });
    }
    // 4. keyword table
    for (key, ktitle, kwords, icon) in KEYWORDS {
        let set: HashSet<&str> = kwords.iter().copied().collect();
        let members: Vec<&Item> = items
            .iter()
            .copied()
            .filter(|it| words(it).iter().any(|w| set.contains(w.as_str())))
            .collect();
        let mut sorted: Vec<&str> = kwords.to_vec();
        sorted.sort_unstable();
        raw.push(Raw {
            reason: "keyword",
            id: key.to_string(),
            title: ktitle.to_string(),
            description: format!("Items that mention {}...", sorted[..4].join(", ")),
            icon,
            members,
        });
    }

    // more specific prefix wins a tie: (reason order, -len(id), id), stable
    raw.sort_by(|a, b| {
        (order(a.reason), std::cmp::Reverse(a.id.chars().count()), &a.id).cmp(&(
            order(b.reason),
            std::cmp::Reverse(b.id.chars().count()),
            &b.id,
        ))
    });
    let mut out: Vec<(u8, String, Value)> = Vec::new();
    let mut seen_sets: HashSet<Vec<(String, String)>> = HashSet::new();
    let mut seen_ids: HashSet<String> = cat.collections.keys().cloned().collect();
    for r in raw {
        let uniq: BTreeSet<(String, String)> = r.members.iter().map(|m| (m.kind.clone(), m.name.clone())).collect();
        if r.id.is_empty()
            || seen_ids.contains(&r.id)
            || uniq.len() < MIN_MEMBERS
            || cat.collections.contains_key(&r.id)
        {
            continue;
        }
        let uniq: Vec<(String, String)> = uniq.into_iter().take(MAX_MEMBERS).collect();
        if !seen_sets.insert(uniq.clone()) {
            continue; // same members under a weaker reason: keep the first
        }
        seen_ids.insert(r.id.clone());
        let members: Vec<Value> = uniq.iter().map(|(k, n)| json!({"kind": k, "name": n})).collect();
        out.push((
            order(r.reason),
            r.id.clone(),
            json!({"id": r.id, "title": r.title, "description": r.description, "icon": r.icon,
                   "color": colour(&r.id), "reason": r.reason, "members": members}),
        ));
    }
    out.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    Ok(out.into_iter().map(|(_, _, v)| v).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_and_colours_follow_python() {
        // python3: suggest._title / suggest._colour
        assert_eq!(title("search-index"), "Search Index");
        assert_eq!(title("vllm_api.gpu"), "vLLM API GPU");
        assert_eq!(title("ßtraße"), "Sstraße");
        assert_eq!(capitalize("hELLO"), "Hello");
        assert_eq!(
            colour("payments"),
            PALETTE[usize::from(Sha256::digest(b"payments")[31] % 8)]
        );
    }
}
