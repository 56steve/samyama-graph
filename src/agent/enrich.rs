//! Governed query-time enrichment (GAK) — single-graph OSS port of the enterprise
//! ADR-031 pipeline (epic #696, sub-issues #698–#701).
//!
//! The loop: a **policy** declares which `(Label, property)` pairs are enrichable →
//! a query **surfaces** nodes → [`detect_gaps`] finds declared properties that are
//! null → the [`EnrichmentWorker`] asks the LLM to fill each, **quarantines** the
//! answer under `_enrichment` with provenance (never the real property), and
//! **honest-declines** (`UNKNOWN` ⇒ nothing written) → a later [`verify`] pass
//! promotes a pending value to the real property iff its confidence clears the
//! spec's `trust_floor`. No policy ⇒ nothing is ever enriched (the safety default).

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::graph::{GraphStore, Label, NodeId, PropertyValue};
use crate::nlq::client::NLQClient;
use crate::query::{RecordBatch, Value};

/// Reserved node property holding quarantined enrichments (never queried as data).
pub const ENRICHMENT_PROPERTY: &str = "_enrichment";
/// Confidence for an LLM (parametric, unsourced) value — deliberately low.
pub const LLM_DEFAULT_CONFIDENCE: f64 = 0.4;
/// OSS serves a single graph; the store API still takes a tenant id.
const TENANT: &str = "default";

// ─────────────────────────────────────────────────────────── config (#698)

/// Where an enriched value may come from. OSS wires the `Llm` source only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnrichSource {
    /// The LLM's parametric knowledge — unsourced, lowest trust.
    Llm,
}

/// Per-`(label, property)` policy. An empty-source spec is declared-but-inert.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct EnrichSpec {
    #[serde(default)]
    pub sources: Vec<EnrichSource>,
    /// Minimum confidence to promote out of quarantine. `0.0` = promote on any pass.
    #[serde(default)]
    pub trust_floor: f64,
}

/// Single-graph enrichment policy: `Label -> property -> spec`. No policy ⇒ inert.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct EnrichConfig {
    #[serde(default)]
    pub policies: HashMap<String, HashMap<String, EnrichSpec>>,
}

impl EnrichConfig {
    fn trust_floor_for(&self, label: &str, property: &str) -> Option<f64> {
        self.policies
            .get(label)
            .and_then(|p| p.get(property))
            .map(|s| s.trust_floor)
    }
}

// ─────────────────────────────────────────────────────────── gap detection (#699)

/// A declared-enrichable property that is missing on a surfaced node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GapEvent {
    pub node_id: u64,
    pub label: String,
    pub property: String,
}

/// Node ids a query result surfaced (its `Node`/`NodeRef` bindings). Gaps are only
/// detected on nodes the query actually returned — never inferred from an empty result.
pub fn collect_result_nodes(batch: &RecordBatch) -> Vec<NodeId> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for record in &batch.records {
        for value in record.values() {
            let id = match value {
                Value::Node(id, _) => Some(*id),
                Value::NodeRef(id) => Some(*id),
                _ => None,
            };
            if let Some(id) = id {
                if seen.insert(id) {
                    out.push(id);
                }
            }
        }
    }
    out
}

/// Detect declared-enrichable, missing properties on the given nodes.
pub fn detect_gaps(config: &EnrichConfig, store: &GraphStore, node_ids: &[NodeId]) -> Vec<GapEvent> {
    let mut seen: HashSet<(u64, String, String)> = HashSet::new();
    let mut gaps = Vec::new();
    for &id in node_ids {
        let Some(node) = store.get_node(id) else {
            continue;
        };
        let props = store.node_properties_merged(id);
        for (label, specs) in &config.policies {
            if !node.has_label(&Label::new(label.clone())) {
                continue;
            }
            for (property, spec) in specs {
                if spec.sources.is_empty() {
                    continue; // declared but inert
                }
                let missing = props.get(property).map(|v| v.is_null()).unwrap_or(true);
                if missing {
                    let key = (id.as_u64(), label.clone(), property.clone());
                    if seen.insert(key) {
                        gaps.push(GapEvent {
                            node_id: id.as_u64(),
                            label: label.clone(),
                            property: property.clone(),
                        });
                    }
                }
            }
        }
    }
    gaps
}

// ─────────────────────────────────────────────────────────── worker (#700)

/// A filled value ready to be quarantined (produced off the store lock).
#[derive(Debug, Clone)]
pub struct Outcome {
    pub node_id: u64,
    pub property: String,
    pub value: String,
    pub confidence: f64,
    pub method: String,
    pub prompt_hash: String,
}

fn prompt_hash(prompt: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    prompt.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Prompt to complete `property` of a `label`, with honest-decline for instance facts.
pub fn build_prompt(label: &str, property: &str, context: &[(String, String)]) -> String {
    let mut ctx = String::new();
    for (k, v) in context {
        if k == ENRICHMENT_PROPERTY {
            continue;
        }
        ctx.push_str(&format!("- {}: {}\n", k, v));
    }
    if ctx.is_empty() {
        ctx.push_str("(no other properties known)\n");
    }
    format!(
        "You are completing the `{prop}` of a {label} in a knowledge graph.\n\
         Known properties:\n{ctx}\n\
         If this {label} denotes a well-known general concept or type, give a concise, factual \
         `{prop}` for it (one or two plain-text sentences; no preamble, quotes, or restating the \
         name). Only if `{prop}` is an instance-specific value that cannot be reasonably inferred \
         from general knowledge (e.g. a particular unit's serial number, install date, or bespoke \
         wiring) return exactly UNKNOWN.",
        label = label,
        prop = property,
        ctx = ctx,
    )
}

/// The enrichment worker: an LLM client + the model label for provenance.
pub struct EnrichmentWorker {
    client: NLQClient,
    model: String,
}

impl EnrichmentWorker {
    pub fn new(client: NLQClient, model: String) -> Self {
        Self { client, model }
    }

    /// Fill one gap from the LLM. `context` = the node's known properties. Returns
    /// `None` when the model declines (`UNKNOWN`) or returns nothing — never fabricates.
    pub async fn fill(&self, gap: &GapEvent, context: &[(String, String)]) -> Option<Outcome> {
        let prompt = build_prompt(&gap.label, &gap.property, context);
        // NLQClient::generate_cypher is a generic chat call (misnamed); reuse it.
        let answer = self.client.generate_cypher(&prompt).await.ok()?;
        let value = answer.trim();
        if value.is_empty() || value.eq_ignore_ascii_case("UNKNOWN") {
            return None; // honest-decline
        }
        Some(Outcome {
            node_id: gap.node_id,
            property: gap.property.clone(),
            value: value.to_string(),
            confidence: LLM_DEFAULT_CONFIDENCE,
            method: format!("llm:{}", self.model),
            prompt_hash: prompt_hash(&prompt),
        })
    }
}

// ─────────────────────────── quarantine + verification (#700/#701)

fn read_enrichment_map(store: &GraphStore, node_id: NodeId) -> HashMap<String, PropertyValue> {
    match store.node_properties_merged(node_id).get(ENRICHMENT_PROPERTY) {
        Some(PropertyValue::Map(m)) => m.clone(),
        _ => HashMap::new(),
    }
}

/// Persist an outcome into `_enrichment.<property>` (quarantined; the real property is untouched).
pub fn quarantine(store: &mut GraphStore, out: &Outcome) -> Result<(), String> {
    let node_id = NodeId(out.node_id);
    let mut root = read_enrichment_map(store, node_id);
    let mut entry: HashMap<String, PropertyValue> = HashMap::new();
    entry.insert("value".into(), PropertyValue::String(out.value.clone()));
    entry.insert("source".into(), PropertyValue::String("llm".into()));
    entry.insert("status".into(), PropertyValue::String("pending_verification".into()));
    entry.insert("confidence".into(), PropertyValue::Float(out.confidence));
    entry.insert("method".into(), PropertyValue::String(out.method.clone()));
    entry.insert("prompt_hash".into(), PropertyValue::String(out.prompt_hash.clone()));
    root.insert(out.property.clone(), PropertyValue::Map(entry));
    store
        .set_node_property(TENANT, node_id, ENRICHMENT_PROPERTY, PropertyValue::Map(root))
        .map_err(|e| format!("{:?}", e))
}

/// Report from a verification pass.
#[derive(Debug, Default, Serialize)]
pub struct VerifyReport {
    pub nodes_processed: usize,
    pub promoted: usize,
    pub still_pending: usize,
}

/// Verify+promote pending enrichments on the given nodes: for each pending property
/// whose confidence clears its `trust_floor`, set the real property and mark it verified.
pub fn verify(config: &EnrichConfig, store: &mut GraphStore, node_ids: &[NodeId]) -> VerifyReport {
    let mut rep = VerifyReport::default();
    for &id in node_ids {
        let mut root = read_enrichment_map(store, id);
        if root.is_empty() {
            continue;
        }
        rep.nodes_processed += 1;
        // Which label does this node carry that has a policy? Use the first matching.
        let label = store.get_node(id).and_then(|n| {
            config
                .policies
                .keys()
                .find(|l| n.has_label(&Label::new((*l).clone())))
                .cloned()
        });
        let mut promotions: Vec<(String, String)> = Vec::new(); // (property, value)
        for (property, entry) in root.iter() {
            let PropertyValue::Map(e) = entry else { continue };
            let status = match e.get("status") {
                Some(PropertyValue::String(s)) => s.clone(),
                _ => continue,
            };
            if status != "pending_verification" {
                continue;
            }
            let confidence = match e.get("confidence") {
                Some(PropertyValue::Float(f)) => *f,
                _ => 0.0,
            };
            let floor = label
                .as_deref()
                .and_then(|l| config.trust_floor_for(l, property))
                .unwrap_or(0.0);
            if confidence >= floor {
                if let Some(PropertyValue::String(v)) = e.get("value") {
                    promotions.push((property.clone(), v.clone()));
                }
            } else {
                rep.still_pending += 1;
            }
        }
        for (property, value) in promotions {
            // Promote onto the real property.
            if store
                .set_node_property(TENANT, id, property.clone(), PropertyValue::String(value))
                .is_ok()
            {
                // Mark the quarantined entry verified (retain provenance).
                if let Some(PropertyValue::Map(e)) = root.get_mut(&property) {
                    e.insert("status".into(), PropertyValue::String("verified".into()));
                }
                rep.promoted += 1;
            }
        }
        let _ = store.set_node_property(TENANT, id, ENRICHMENT_PROPERTY, PropertyValue::Map(root));
    }
    rep
}

/// Build an [`EnrichmentWorker`] from environment config (same knobs as `/api/nlq`).
pub fn worker_from_env() -> Result<EnrichmentWorker, String> {
    use crate::persistence::tenant::{LLMProvider, NLQConfig};
    let provider = match std::env::var("NLQ_PROVIDER").unwrap_or_default().to_lowercase().as_str() {
        "ollama" => LLMProvider::Ollama,
        "gemini" => LLMProvider::Gemini,
        "anthropic" => LLMProvider::Anthropic,
        "azure" | "azureopenai" => LLMProvider::AzureOpenAI,
        "mock" => LLMProvider::Mock,
        _ => LLMProvider::OpenAI,
    };
    let model = std::env::var("NLQ_MODEL").unwrap_or_else(|_| "gpt-4o".to_string());
    let config = NLQConfig {
        enabled: true,
        provider,
        model: model.clone(),
        api_key: std::env::var("OPENAI_API_KEY").ok(),
        api_base_url: std::env::var("NLQ_API_BASE_URL").ok(),
        system_prompt: None,
    };
    let client = NLQClient::new(&config).map_err(|e| e.to_string())?;
    Ok(EnrichmentWorker::new(client, model))
}

/// Process-global single-graph enrichment policy (OSS serves one graph). Avoids threading
/// the config through `AppState` + every construction site.
pub fn global_config() -> &'static std::sync::RwLock<EnrichConfig> {
    static G: std::sync::OnceLock<std::sync::RwLock<EnrichConfig>> = std::sync::OnceLock::new();
    G.get_or_init(|| std::sync::RwLock::new(EnrichConfig::default()))
}
