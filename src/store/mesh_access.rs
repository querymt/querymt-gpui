//! Mesh peer sharing (querymt mesh access v1): what this machine shares with
//! each mesh peer (`outgoing`, editable), what the peer shares back
//! (`incoming`, read-only), and per-provider / per-model token quotas.
//!
//! Wire types mirror querymt's `target/generated/types.ts` (typeshare of
//! `agent-runtime/src/agent/handle/ext_mesh.rs`). Every write carries the
//! current `outgoing.revision`; the agent refuses a stale one
//! (`mesh_access_conflict`) and the peer is read again. A
//! `querymt/mesh/peerAccessChanged` notification is only a hint: the peer is
//! read again after it, never patched from it.

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use gpui_kit::{Context, Task};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{AgentRuntime, AgentsStore};
use crate::rpc::RpcError;

pub const VERSION: u64 = 1;
pub const GET: &str = "querymt/mesh/getPeerAccess";
pub const SET_ACCESS: &str = "querymt/mesh/setPeerAccess";
pub const SET_QUOTA: &str = "querymt/mesh/setPeerQuota";
pub const RESET_QUOTA: &str = "querymt/mesh/resetPeerQuota";
pub const CHANGED: &str = "querymt/mesh/peerAccessChanged";

/// Hints come in bursts (a write notifies once per change): coalesce.
const CHANGE_DEBOUNCE: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------------------
// Wire types

/// Token counts travel as exact decimal strings.
mod tokens {
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    pub fn serialize<S: Serializer>(n: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&n.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        let text = String::deserialize(d)?;
        text.parse().map_err(D::Error::custom)
    }

    pub mod opt {
        use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

        pub fn serialize<S: Serializer>(n: &Option<u64>, s: S) -> Result<S::Ok, S::Error> {
            match n {
                Some(n) => s.serialize_str(&n.to_string()),
                None => s.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
            Option::<String>::deserialize(d)?
                .map(|t| t.parse().map_err(D::Error::custom))
                .transpose()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ModelSelection {
    All,
    Selected { model_ids: Vec<String> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderGrant {
    pub provider_id: String,
    pub models: ModelSelection,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum PeerGrant {
    None,
    Full,
    Custom {
        sessions: bool,
        filesystem: bool,
        providers: Vec<ProviderGrant>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct QuotaScope {
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
}

impl QuotaScope {
    pub fn provider(provider_id: &str) -> Self {
        Self {
            provider_id: provider_id.into(),
            model_id: None,
        }
    }

    pub fn model(provider_id: &str, model_id: &str) -> Self {
        Self {
            provider_id: provider_id.into(),
            model_id: Some(model_id.into()),
        }
    }

    /// `codex` or `gpt-6.2-sol (codex)`.
    pub fn label(&self) -> String {
        match &self.model_id {
            Some(model) => format!("{model} ({})", self.provider_id),
            None => format!("all {} models", self.provider_id),
        }
    }

    /// Stable key for UI state.
    pub fn key(&self) -> String {
        match &self.model_id {
            Some(model) => format!("{}/{model}", self.provider_id),
            None => self.provider_id.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaWindow {
    #[default]
    Manual,
    Daily,
    Weekly,
    Monthly,
}

impl QuotaWindow {
    pub const ALL: [QuotaWindow; 4] = [Self::Manual, Self::Daily, Self::Weekly, Self::Monthly];

    pub fn label(self) -> &'static str {
        match self {
            Self::Manual => "Manual",
            Self::Daily => "Daily",
            Self::Weekly => "Weekly",
            Self::Monthly => "Monthly",
        }
    }

    /// What the window means, for the editor.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Manual => "Never resets on its own; reset it here.",
            Self::Daily => "Resets every day at 00:00 UTC.",
            Self::Weekly => "Resets every Monday at 00:00 UTC.",
            Self::Monthly => "Resets on the 1st of each month at 00:00 UTC.",
        }
    }

    fn adverb(self) -> &'static str {
        match self {
            Self::Manual => "",
            Self::Daily => " a day",
            Self::Weekly => " a week",
            Self::Monthly => " a month",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QuotaLimit {
    Unlimited,
    Finite {
        #[serde(with = "tokens")]
        limit_tokens: u64,
        #[serde(default)]
        window: QuotaWindow,
    },
}

impl QuotaLimit {
    /// `5 Mtok a day`, `1 token`, `Unlimited`.
    pub fn label(&self) -> String {
        match self {
            Self::Unlimited => "Unlimited".into(),
            Self::Finite {
                limit_tokens,
                window,
            } => format!("{}{}", tokens_label(*limit_tokens), window.adverb()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaOperation {
    /// Change only the amount of an existing finite quota.
    Update,
    /// Change finite/unlimited or the window: starts a new period.
    Reconfigure,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaUsage {
    #[serde(with = "tokens")]
    pub known_tokens: u64,
    #[serde(with = "tokens")]
    pub estimated_tokens: u64,
    pub has_unknown_usage: bool,
    #[serde(default, with = "tokens::opt", skip_serializing_if = "Option::is_none")]
    pub reserved_tokens: Option<u64>,
    #[serde(default, with = "tokens::opt", skip_serializing_if = "Option::is_none")]
    pub available_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_reset_at: Option<String>,
}

impl QuotaUsage {
    /// Settled usage, actual plus estimated.
    pub fn used(&self) -> u64 {
        self.known_tokens.saturating_add(self.estimated_tokens)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerQuota {
    pub scope: QuotaScope,
    pub limit: QuotaLimit,
    /// Applied to the current Custom grant (saved otherwise).
    pub active: bool,
    pub usage: QuotaUsage,
}

impl PeerQuota {
    /// A finite limit with no capacity left after usage and reservations.
    pub fn exhausted(&self) -> bool {
        matches!(self.limit, QuotaLimit::Finite { .. }) && self.usage.available_tokens == Some(0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outgoing {
    pub revision: String,
    pub grant: PeerGrant,
    pub quotas: Vec<PeerQuota>,
}

impl Outgoing {
    pub fn quota(&self, scope: &QuotaScope) -> Option<&PeerQuota> {
        self.quotas.iter().find(|q| &q.scope == scope)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessMode {
    Full,
    Custom,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveAccess {
    pub mode: AccessMode,
    pub sessions: bool,
    pub filesystem: bool,
    pub shell: bool,
    pub providers: Vec<ProviderGrant>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Incoming {
    Granted {
        effective: EffectiveAccess,
        quotas: Vec<PeerQuota>,
    },
    Denied,
    Unavailable,
    Incompatible,
    Legacy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareableModel {
    pub model_id: String,
    pub finite_quota_supported: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareableProvider {
    pub provider_id: String,
    pub models: Vec<ShareableModel>,
}

/// `getPeerAccess` result; writes return only `outgoing`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerAccess {
    pub version: u64,
    pub peer_id: String,
    pub outgoing: Outgoing,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incoming: Option<Incoming>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shareable_catalog: Option<Vec<ShareableProvider>>,
}

/// `querymt/mesh/peerAccessChanged` params.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct AccessChanged {
    pub version: u64,
    pub peer_id: String,
    /// `outgoing` or `incoming`.
    pub direction: String,
    /// `policy`, `quota`, `availability` or `catalog`.
    pub reason: String,
}

#[derive(Serialize)]
struct GetRequest<'a> {
    version: u64,
    peer_id: &'a str,
}

#[derive(Serialize)]
struct SetAccessRequest<'a> {
    version: u64,
    peer_id: &'a str,
    expected_revision: &'a str,
    grant: &'a PeerGrant,
}

#[derive(Serialize)]
struct SetQuotaRequest<'a> {
    version: u64,
    peer_id: &'a str,
    expected_revision: &'a str,
    scope: &'a QuotaScope,
    operation: QuotaOperation,
    limit: &'a QuotaLimit,
}

#[derive(Serialize)]
struct ResetQuotaRequest<'a> {
    version: u64,
    peer_id: &'a str,
    expected_revision: &'a str,
    scope: &'a QuotaScope,
}

/// One write to a peer's outgoing access.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Write {
    Grant(PeerGrant),
    Quota {
        scope: QuotaScope,
        operation: QuotaOperation,
        limit: QuotaLimit,
    },
    Reset(QuotaScope),
}

impl Write {
    /// Method (without the `_` extension prefix) and params.
    pub fn request(&self, peer_id: &str, revision: &str) -> (&'static str, Value) {
        let to_value = |v: Result<Value, serde_json::Error>| v.unwrap_or(Value::Null);
        match self {
            Self::Grant(grant) => (
                SET_ACCESS,
                to_value(serde_json::to_value(SetAccessRequest {
                    version: VERSION,
                    peer_id,
                    expected_revision: revision,
                    grant,
                })),
            ),
            Self::Quota {
                scope,
                operation,
                limit,
            } => (
                SET_QUOTA,
                to_value(serde_json::to_value(SetQuotaRequest {
                    version: VERSION,
                    peer_id,
                    expected_revision: revision,
                    scope,
                    operation: *operation,
                    limit,
                })),
            ),
            Self::Reset(scope) => (
                RESET_QUOTA,
                to_value(serde_json::to_value(ResetQuotaRequest {
                    version: VERSION,
                    peer_id,
                    expected_revision: revision,
                    scope,
                })),
            ),
        }
    }
}

fn get_params(peer_id: &str) -> Value {
    serde_json::to_value(GetRequest {
        version: VERSION,
        peer_id,
    })
    .unwrap_or(Value::Null)
}

/// The `_querymt/...` extension method name ACP clients send.
fn ext(method: &str) -> String {
    format!("_{method}")
}

/// The agent advertises mesh access v1 with its read method.
pub fn supported(rt: &AgentRuntime) -> bool {
    rt.capabilities
        .as_ref()
        .and_then(|c| c.get("mesh_access_version"))
        .and_then(Value::as_u64)
        == Some(VERSION)
        && rt.supports_method(GET)
}

// ---------------------------------------------------------------------------
// Errors

/// A refused or failed sharing request, by `data.code` (never message text).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccessError {
    /// Stale `expected_revision`: someone else changed the peer first.
    Conflict,
    /// Reset or reconfigure while an attempt is outstanding at `scope`.
    InFlight(Option<QuotaScope>),
    /// An amount-only edit cannot change the kind or window.
    ReconfigureRequired,
    Denied,
    /// No working local sharing store.
    Unavailable,
    Invalid,
    Unsupported,
    /// Anything else (transport failure, unexpected reply): the text.
    Other(String),
}

impl AccessError {
    pub fn from_rpc(err: &RpcError) -> Self {
        let Some(raw) = err.raw() else {
            return Self::Other(err.to_string());
        };
        let data = raw.get("data");
        match data.and_then(|d| d.get("code")).and_then(Value::as_str) {
            Some("mesh_access_conflict") => Self::Conflict,
            Some("mesh_quota_in_flight") => Self::InFlight(
                data.and_then(|d| d.get("scope"))
                    .and_then(|s| serde_json::from_value(s.clone()).ok()),
            ),
            Some("mesh_quota_reconfigure_required") => Self::ReconfigureRequired,
            Some("mesh_access_denied") => Self::Denied,
            Some("mesh_access_unavailable") => Self::Unavailable,
            Some("mesh_access_invalid_params") => Self::Invalid,
            _ if raw.get("code").and_then(Value::as_i64) == Some(-32601) => Self::Unsupported,
            _ => Self::Other(err.to_string()),
        }
    }

    /// What to tell the owner, next to the control that failed.
    pub fn message(&self) -> String {
        match self {
            Self::Conflict => "Sharing for this peer changed elsewhere, so nothing was saved. \
                 The latest settings are shown."
                .into(),
            Self::InFlight(scope) => format!(
                "A request is still using {} right now. Try again once it finishes.",
                scope.as_ref().map_or_else(
                    || "this limit".to_string(),
                    |s| format!("the {} limit", s.label())
                )
            ),
            Self::ReconfigureRequired => {
                "This limit changed elsewhere; check it and save again.".into()
            }
            Self::Denied => "The agent did not allow this change.".into(),
            Self::Unavailable => "The agent's sharing settings are unavailable right now.".into(),
            Self::Invalid => "The agent rejected this value.".into(),
            Self::Unsupported => "This agent does not support this sharing control.".into(),
            Self::Other(text) => text.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Presentation helpers

fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Trim `1.50` to `1.5` and `2.00` to `2`.
fn trim_decimal(text: String) -> String {
    if !text.contains('.') {
        return text;
    }
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// `1 token`, `2,604 tokens`, `0.13 Mtok`, `5 Mtok`.
pub fn tokens_label(n: u64) -> String {
    match n {
        1 => "1 token".into(),
        0..100_000 => format!("{} tokens", group_digits(n)),
        _ => format!(
            "{} Mtok",
            trim_decimal(format!("{:.2}", n as f64 / 1_000_000.))
        ),
    }
}

/// Exact token count, for tooltips.
pub fn exact_tokens(n: u64) -> String {
    if n == 1 {
        "1 token".into()
    } else {
        format!("{} tokens", group_digits(n))
    }
}

/// Exact Mtok text of a token count (the editor's amount): `2.000001`.
pub fn mtok_text(tokens: u64) -> String {
    let (whole, frac) = (tokens / 1_000_000, tokens % 1_000_000);
    if frac == 0 {
        whole.to_string()
    } else {
        trim_decimal(format!("{whole}.{frac:06}"))
    }
}

/// A positive amount in Mtok (up to six decimals, so whole tokens) to tokens.
pub fn parse_mtok(text: &str) -> Result<u64, &'static str> {
    const ERR: &str = "Enter a positive amount in Mtok, e.g. 2.5 (up to 6 decimals).";
    let text = text.trim();
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let (whole, frac) = text
        .split_once('.')
        .map_or((text, None), |(w, f)| (w, Some(f)));
    let whole = if whole.is_empty() && frac.is_some() {
        "0"
    } else {
        whole
    };
    if !digits(whole) {
        return Err(ERR);
    }
    let mut tokens = whole
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(1_000_000))
        .ok_or(ERR)?;
    if let Some(frac) = frac {
        if frac.len() > 6 || !digits(frac) {
            return Err(ERR);
        }
        let part = frac.parse::<u64>().map_err(|_| ERR)? * 10u64.pow(6 - frac.len() as u32);
        tokens = tokens.checked_add(part).ok_or(ERR)?;
    }
    if tokens == 0 {
        return Err(ERR);
    }
    Ok(tokens)
}

/// `update` when only the amount of an existing finite quota changes, else
/// `reconfigure` (first finite cap, new window, or back to unlimited).
pub fn quota_operation(existing: Option<&QuotaLimit>, new: &QuotaLimit) -> QuotaOperation {
    match (existing, new) {
        (Some(QuotaLimit::Finite { window: old, .. }), QuotaLimit::Finite { window: new, .. })
            if old == new =>
        {
            QuotaOperation::Update
        }
        _ => QuotaOperation::Reconfigure,
    }
}

/// `in 45m`, `in 5h`, `in 2d`.
fn relative(at_ms: f64, now_ms: f64) -> String {
    let secs = ((at_ms - now_ms) / 1000.).max(0.) as u64;
    match secs {
        0..60 => "in under a minute".into(),
        60..3600 => format!("in {}m", secs / 60),
        3600..172_800 => format!("in {}h", secs / 3600),
        _ => format!("in {}d", secs / 86400),
    }
}

/// `Resets in 5h (Nov 25, 2026, 00:00 UTC)`.
pub fn reset_label(at: &str, now_ms: f64) -> String {
    match crate::util::parse_rfc3339_ms(at) {
        Some(ms) if ms > now_ms => format!(
            "Resets {} ({})",
            relative(ms, now_ms),
            crate::util::absolute_time(at).unwrap_or_else(|| at.to_string())
        ),
        Some(_) => "Resets now".into(),
        None => format!("Resets at {at}"),
    }
}

/// Usage of one quota in a line: used, reserved, left, reset.
pub fn usage_line(quota: &PeerQuota, now_ms: f64) -> String {
    let u = &quota.usage;
    let mut parts = vec![format!("{} used", tokens_label(u.used()))];
    if let QuotaLimit::Finite { window, .. } = quota.limit {
        if let Some(reserved) = u.reserved_tokens.filter(|r| *r > 0) {
            parts.push(format!("{} reserved", tokens_label(reserved)));
        }
        if let Some(left) = u.available_tokens {
            parts.push(format!("{} left", tokens_label(left)));
        }
        parts.push(match u.next_reset_at.as_deref() {
            Some(at) => reset_label(at, now_ms),
            None if window == QuotaWindow::Manual => "Resets manually".into(),
            None => String::new(),
        });
    }
    if u.estimated_tokens > 0 {
        parts.push(format!("{} estimated", tokens_label(u.estimated_tokens)));
    }
    if u.has_unknown_usage {
        parts.push("some usage unknown".into());
    }
    parts.retain(|p| !p.is_empty());
    parts.join(" · ")
}

/// Share of a finite limit that is used or reserved, 0..=1.
pub fn usage_fraction(quota: &PeerQuota) -> Option<f32> {
    let QuotaLimit::Finite { limit_tokens, .. } = quota.limit else {
        return None;
    };
    let taken = quota
        .usage
        .used()
        .saturating_add(quota.usage.reserved_tokens.unwrap_or(0));
    Some(if limit_tokens == 0 {
        1.
    } else {
        (taken as f64 / limit_tokens as f64).min(1.) as f32
    })
}

fn models_label(models: &ModelSelection) -> String {
    match models {
        ModelSelection::All => "all models".into(),
        ModelSelection::Selected { model_ids } if model_ids.len() == 1 => model_ids[0].clone(),
        ModelSelection::Selected { model_ids } => format!("{} models", model_ids.len()),
    }
}

/// `Sessions, files, codex (all models)`.
fn custom_summary(sessions: bool, filesystem: bool, providers: &[ProviderGrant]) -> String {
    let mut parts = Vec::new();
    if sessions {
        parts.push(if filesystem {
            "Sessions and files".to_string()
        } else {
            "Sessions".to_string()
        });
    }
    match providers {
        [] => {}
        [one] => parts.push(format!(
            "{} ({})",
            one.provider_id,
            models_label(&one.models)
        )),
        many => parts.push(format!("{} providers", many.len())),
    }
    if parts.is_empty() {
        "Nothing yet".into()
    } else {
        parts.join(", ")
    }
}

/// What this machine shares with the peer, at a glance.
pub fn grant_summary(grant: &PeerGrant) -> String {
    match grant {
        PeerGrant::None => "Nothing".into(),
        PeerGrant::Full => "Full access".into(),
        PeerGrant::Custom {
            sessions,
            filesystem,
            providers,
        } => custom_summary(*sessions, *filesystem, providers),
    }
}

/// What the peer shares with this machine, at a glance.
pub fn incoming_summary(incoming: Option<&Incoming>) -> String {
    match incoming {
        None => "Unknown".into(),
        Some(Incoming::Granted { effective, .. }) => match effective.mode {
            AccessMode::Full => "Full access".into(),
            AccessMode::Custom => custom_summary(
                effective.sessions,
                effective.filesystem,
                &effective.providers,
            ),
        },
        Some(Incoming::Denied) => "Nothing".into(),
        Some(Incoming::Unavailable) => "Unknown while unreachable".into(),
        Some(Incoming::Incompatible) => "Unknown (older QueryMT)".into(),
        Some(Incoming::Legacy) => "Legacy sharing".into(),
    }
}

fn mode_name(grant: &PeerGrant) -> &'static str {
    match grant {
        PeerGrant::None => "No access",
        PeerGrant::Full => "Full access",
        PeerGrant::Custom { .. } => "Custom",
    }
}

/// What changed between two grants, in words (for a stale edit).
pub fn grant_changes(old: &PeerGrant, new: &PeerGrant) -> Vec<String> {
    let (
        PeerGrant::Custom {
            sessions: s0,
            filesystem: f0,
            providers: p0,
        },
        PeerGrant::Custom {
            sessions: s1,
            filesystem: f1,
            providers: p1,
        },
    ) = (old, new)
    else {
        return if old == new {
            Vec::new()
        } else {
            vec![format!(
                "Access changed from {} to {}.",
                mode_name(old),
                mode_name(new)
            )]
        };
    };
    let on_off = |on: bool| if on { "on" } else { "off" };
    let mut out = Vec::new();
    if s0 != s1 {
        out.push(format!("Sessions turned {}.", on_off(*s1)));
    }
    if f0 != f1 {
        out.push(format!("Files turned {}.", on_off(*f1)));
    }
    let ids: BTreeSet<&str> = p0
        .iter()
        .chain(p1.iter())
        .map(|p| p.provider_id.as_str())
        .collect();
    for id in ids {
        let find = |list: &[ProviderGrant]| list.iter().find(|p| p.provider_id == id).cloned();
        match (find(p0), find(p1)) {
            (None, Some(p)) => out.push(format!("Now shares {id} ({}).", models_label(&p.models))),
            (Some(_), None) => out.push(format!("Stopped sharing {id}.")),
            (Some(a), Some(b)) if a.models != b.models => {
                out.push(format!("{id} now shares {}.", models_label(&b.models)))
            }
            _ => {}
        }
    }
    out
}

/// What changed between two quota lists, in words.
pub fn quota_changes(old: &[PeerQuota], new: &[PeerQuota]) -> Vec<String> {
    let scopes: BTreeSet<&QuotaScope> = old.iter().chain(new.iter()).map(|q| &q.scope).collect();
    let mut out = Vec::new();
    for scope in scopes {
        let find = |list: &[PeerQuota]| list.iter().find(|q| &q.scope == scope).cloned();
        let label = scope.label();
        match (find(old), find(new)) {
            (None, Some(q)) if q.limit != QuotaLimit::Unlimited => {
                out.push(format!("Limit for {label} set to {}.", q.limit.label()))
            }
            (Some(a), Some(b)) if a.limit != b.limit => out.push(format!(
                "Limit for {label} changed from {} to {}.",
                a.limit.label(),
                b.limit.label()
            )),
            (Some(a), Some(b))
                if a.usage.window_started_at != b.usage.window_started_at
                    && b.usage.window_started_at.is_some() =>
            {
                out.push(format!("Limit for {label} was reset."))
            }
            (Some(a), None) if a.limit != QuotaLimit::Unlimited => {
                out.push(format!("Limit for {label} removed."))
            }
            _ => {}
        }
    }
    out
}

/// Everything that changed in a peer's outgoing access.
pub fn outgoing_changes(old: &Outgoing, new: &Outgoing) -> Vec<String> {
    let mut out = grant_changes(&old.grant, &new.grant);
    out.extend(quota_changes(&old.quotas, &new.quotas));
    out
}

// ---------------------------------------------------------------------------
// Editing a grant

/// An unsaved grant edit, based on the outgoing state it started from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantDraft {
    pub base_revision: String,
    pub base: PeerGrant,
    pub grant: PeerGrant,
    /// Custom settings kept while No access or Full is picked, so switching
    /// back does not lose them.
    custom: PeerGrant,
    /// The owner confirmed that Full includes the host shell.
    pub full_ack: bool,
    /// The saved grant changed elsewhere since the draft started: what
    /// changed. Saving waits until the owner keeps or drops the draft.
    pub changed_elsewhere: Vec<String>,
}

fn empty_custom() -> PeerGrant {
    PeerGrant::Custom {
        sessions: false,
        filesystem: false,
        providers: Vec::new(),
    }
}

impl GrantDraft {
    pub fn new(outgoing: &Outgoing) -> Self {
        let custom = match &outgoing.grant {
            g @ PeerGrant::Custom { .. } => g.clone(),
            _ => empty_custom(),
        };
        Self {
            base_revision: outgoing.revision.clone(),
            base: outgoing.grant.clone(),
            grant: outgoing.grant.clone(),
            custom,
            full_ack: false,
            changed_elsewhere: Vec::new(),
        }
    }

    pub fn dirty(&self) -> bool {
        self.grant != self.base
    }

    /// Pick No access, Custom or Full.
    pub fn set_mode(&mut self, mode: &str) {
        if let g @ PeerGrant::Custom { .. } = &self.grant {
            self.custom = g.clone();
        }
        self.grant = match mode {
            "full" => PeerGrant::Full,
            "custom" => self.custom.clone(),
            _ => PeerGrant::None,
        };
        if mode != "full" {
            self.full_ack = false;
        }
    }

    fn custom_mut(&mut self) -> Option<(&mut bool, &mut bool, &mut Vec<ProviderGrant>)> {
        match &mut self.grant {
            PeerGrant::Custom {
                sessions,
                filesystem,
                providers,
            } => Some((sessions, filesystem, providers)),
            _ => None,
        }
    }

    /// Files need sessions: turning sessions off turns files off.
    pub fn set_sessions(&mut self, on: bool) {
        if let Some((sessions, filesystem, _)) = self.custom_mut() {
            *sessions = on;
            if !on {
                *filesystem = false;
            }
        }
    }

    pub fn set_filesystem(&mut self, on: bool) {
        if let Some((sessions, filesystem, _)) = self.custom_mut() {
            *filesystem = on && *sessions;
        }
    }

    /// Share a provider (all models) or stop sharing it; `order` keeps the
    /// catalog order.
    pub fn set_provider(&mut self, provider_id: &str, on: bool, order: &[String]) {
        let Some((_, _, providers)) = self.custom_mut() else {
            return;
        };
        providers.retain(|p| p.provider_id != provider_id);
        if on {
            providers.push(ProviderGrant {
                provider_id: provider_id.into(),
                models: ModelSelection::All,
            });
            let rank = |id: &str| order.iter().position(|o| o == id).unwrap_or(usize::MAX);
            providers.sort_by_key(|p| rank(&p.provider_id));
        }
    }

    /// All models of a shared provider, or a chosen list (started with
    /// `initial` when switching to a chosen list).
    pub fn set_all_models(&mut self, provider_id: &str, all: bool, initial: &[String]) {
        let Some((_, _, providers)) = self.custom_mut() else {
            return;
        };
        if let Some(p) = providers.iter_mut().find(|p| p.provider_id == provider_id) {
            p.models = if all {
                ModelSelection::All
            } else {
                match &p.models {
                    s @ ModelSelection::Selected { .. } => s.clone(),
                    ModelSelection::All => ModelSelection::Selected {
                        model_ids: initial.to_vec(),
                    },
                }
            };
        }
    }

    pub fn toggle_model(&mut self, provider_id: &str, model_id: &str) {
        let Some((_, _, providers)) = self.custom_mut() else {
            return;
        };
        if let Some(ProviderGrant {
            models: ModelSelection::Selected { model_ids },
            ..
        }) = providers.iter_mut().find(|p| p.provider_id == provider_id)
        {
            if let Some(ix) = model_ids.iter().position(|m| m == model_id) {
                model_ids.remove(ix);
            } else {
                model_ids.push(model_id.into());
            }
        }
    }

    /// Why the draft cannot be saved as it is.
    pub fn problem(&self) -> Option<String> {
        match &self.grant {
            PeerGrant::Full if !self.full_ack => {
                Some("Confirm that you trust this peer with your shell.".into())
            }
            PeerGrant::Custom { providers, .. } => providers
                .iter()
                .find(|p| {
                    matches!(&p.models, ModelSelection::Selected { model_ids } if model_ids.is_empty())
                })
                .map(|p| format!("Choose at least one {} model, or share all of them.", p.provider_id)),
            _ => None,
        }
    }

    pub fn can_save(&self) -> bool {
        self.dirty() && self.changed_elsewhere.is_empty() && self.problem().is_none()
    }

    /// The agent's state moved on (a hint, a stale write, our own quota
    /// write). A quota-only change rebases silently: saving the grant
    /// cannot overwrite it. A grant change elsewhere is shown and blocks
    /// saving until the owner decides.
    pub fn follow(&mut self, server: &Outgoing) {
        if server.revision == self.base_revision {
            return;
        }
        if server.grant == self.base {
            self.base_revision = server.revision.clone();
            return;
        }
        self.changed_elsewhere = grant_changes(&self.base, &server.grant);
        if self.changed_elsewhere.is_empty() {
            self.changed_elsewhere = vec!["The saved access changed.".into()];
        }
    }

    /// Keep my edits on top of the new saved state (still to be saved).
    pub fn keep_mine(&mut self, server: &Outgoing) {
        self.base = server.grant.clone();
        self.base_revision = server.revision.clone();
        self.changed_elsewhere.clear();
    }
}

// ---------------------------------------------------------------------------
// Store state

/// Sharing state of one peer on one agent.
#[derive(Clone, Debug, Default)]
pub struct PeerSharing {
    /// Last read; writes update `outgoing` in place.
    pub access: Option<PeerAccess>,
    pub loading: bool,
    /// Why the last read failed (the last good state stays shown).
    pub error: Option<String>,
    /// Another read was asked for while one ran.
    reread: bool,
    /// Completed writes: a read that started before one is outdated.
    writes: u64,
    refresh_scheduled: bool,
}

impl PeerSharing {
    /// An applied finite quota has no capacity left.
    pub fn limit_reached(&self) -> bool {
        self.access
            .as_ref()
            .is_some_and(|a| a.outgoing.quotas.iter().any(|q| q.active && q.exhausted()))
    }
}

/// Saved peer IDs (`managed_peer_ids`) of a `querymt/mesh/nodes` reply.
pub fn managed_peers(nodes: &Value) -> Vec<String> {
    nodes
        .get("managed_peer_ids")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect()
}

fn parse_access(value: Value) -> Result<PeerAccess, String> {
    let access: PeerAccess =
        serde_json::from_value(value).map_err(|e| format!("Unexpected sharing reply: {e}"))?;
    if access.version != VERSION {
        return Err(format!(
            "Unsupported sharing reply version {}",
            access.version
        ));
    }
    Ok(access)
}

/// Apply a write's confirmed `outgoing`; the incoming side and catalog stay.
fn apply_outgoing(sharing: &mut PeerSharing, peer_id: &str, outgoing: Outgoing) {
    sharing.writes += 1;
    match &mut sharing.access {
        Some(access) => access.outgoing = outgoing,
        None => {
            sharing.access = Some(PeerAccess {
                version: VERSION,
                peer_id: peer_id.into(),
                outgoing,
                incoming: None,
                shareable_catalog: None,
            })
        }
    }
}

impl AgentsStore {
    /// Sharing state of `peer` on agent `id`.
    pub fn peer_sharing(&self, id: &str, peer: &str) -> Option<&PeerSharing> {
        self.runtime.get(id)?.sharing.get(peer)
    }

    /// Whether agent `id` offers peer sharing controls.
    pub fn mesh_access_supported(&self, id: &str) -> bool {
        self.runtime.get(id).is_some_and(supported)
    }

    /// Read every known peer: online nodes and offline peers with a saved policy.
    pub(super) fn load_all_peer_sharing(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(rt) = self.runtime.get(id) else {
            return;
        };
        if !supported(rt) {
            return;
        }
        let mut peers: Vec<String> = rt.mesh_nodes.iter().map(|n| n.id.clone()).collect();
        for p in &rt.managed_peers {
            if !peers.contains(p) {
                peers.push(p.clone());
            }
        }
        for peer in peers {
            self.load_peer_sharing(id, &peer, cx);
        }
    }

    /// Read one peer's sharing (`getPeerAccess`); one read at a time per peer.
    pub fn load_peer_sharing(&mut self, id: &str, peer: &str, cx: &mut Context<Self>) {
        let Some(rt) = self.runtime.get_mut(id) else {
            return;
        };
        let Some(rpc) = rt
            .rpc
            .clone()
            .filter(|_| rt.is_connected() && supported(rt))
        else {
            return;
        };
        let generation = rt.generation;
        let sharing = rt.sharing.entry(peer.to_string()).or_default();
        if sharing.loading {
            sharing.reread = true;
            return;
        }
        sharing.loading = true;
        let writes = sharing.writes;
        cx.notify();
        let request = rpc.request_detailed(&ext(GET), get_params(peer));
        let (id, peer) = (id.to_string(), peer.to_string());
        let task = cx.spawn(async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |s, cx| {
                let Some(rt) = s.live(&id, generation) else {
                    return;
                };
                let sharing = rt.sharing.entry(peer.clone()).or_default();
                sharing.loading = false;
                let outdated = sharing.writes != writes;
                match result
                    .map_err(|e| AccessError::from_rpc(&e).message())
                    .and_then(parse_access)
                {
                    Ok(access) if !outdated => {
                        sharing.access = Some(access);
                        sharing.error = None;
                    }
                    Ok(_) => sharing.reread = true,
                    Err(err) => sharing.error = Some(err),
                }
                let again = std::mem::take(&mut sharing.reread);
                cx.notify();
                if again {
                    s.load_peer_sharing(&id, &peer, cx);
                }
            });
        });
        rt.tasks.push(task);
        rt.tasks.retain(|t| !t.is_ready());
    }

    /// Write `write` against the latest known revision. The confirmed state
    /// replaces `outgoing`; a refusal leaves it and a conflict re-reads the
    /// peer. The task tells the caller how it went.
    pub fn write_peer_sharing(
        &mut self,
        id: &str,
        peer: &str,
        write: Write,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), AccessError>> {
        let Some(rt) = self.runtime.get_mut(id) else {
            return Task::ready(Err(AccessError::Other(
                "The agent is not connected.".into(),
            )));
        };
        let Some(rpc) = rt.rpc.clone().filter(|_| rt.is_connected()) else {
            return Task::ready(Err(AccessError::Other(
                "The agent is not connected.".into(),
            )));
        };
        let Some(revision) = rt
            .sharing
            .get(peer)
            .and_then(|s| s.access.as_ref())
            .map(|a| a.outgoing.revision.clone())
        else {
            return Task::ready(Err(AccessError::Other(
                "This peer's sharing has not loaded yet.".into(),
            )));
        };
        let generation = rt.generation;
        let (method, params) = write.request(peer, &revision);
        let request = rpc.request_detailed(&ext(method), params);
        let (id, peer) = (id.to_string(), peer.to_string());
        cx.spawn(async move |this, cx| {
            let result = request.await;
            this.update(cx, |s, cx| {
                let outcome = match result {
                    Ok(value) => value
                        .get("outgoing")
                        .cloned()
                        .ok_or_else(|| "missing outgoing".to_string())
                        .and_then(|o| {
                            serde_json::from_value::<Outgoing>(o).map_err(|e| e.to_string())
                        })
                        .map_err(|e| {
                            // The write may have happened: read what is saved.
                            AccessError::Other(format!(
                                "Unexpected reply ({e}); reloaded the saved state."
                            ))
                        }),
                    Err(err) => Err(AccessError::from_rpc(&err)),
                };
                match &outcome {
                    Ok(outgoing) => {
                        if let Some(rt) = s.live(&id, generation) {
                            let sharing = rt.sharing.entry(peer.clone()).or_default();
                            apply_outgoing(sharing, &peer, outgoing.clone());
                            if !rt.managed_peers.contains(&peer)
                                && outgoing.grant != PeerGrant::None
                            {
                                rt.managed_peers.push(peer.clone());
                            }
                        }
                    }
                    // Stale or unknown outcome: what is saved now?
                    Err(AccessError::Conflict | AccessError::Other(_)) => {
                        s.load_peer_sharing(&id, &peer, cx)
                    }
                    Err(_) => {}
                }
                cx.notify();
                outcome.map(|_| ())
            })
            .unwrap_or_else(|_| Err(AccessError::Other("The app closed.".into())))
        })
    }

    /// `querymt/mesh/peerAccessChanged`: re-read that peer, debounced.
    pub(super) fn on_peer_access_changed(
        &mut self,
        id: &str,
        params: &Value,
        cx: &mut Context<Self>,
    ) {
        let Ok(hint) = serde_json::from_value::<AccessChanged>(params.clone()) else {
            return;
        };
        if hint.version != VERSION {
            return;
        }
        let Some(rt) = self.runtime.get_mut(id) else {
            return;
        };
        if !rt.is_connected() || !supported(rt) {
            return;
        }
        // A policy saved for a peer the list did not know yet.
        let known = rt.mesh_nodes.iter().any(|n| n.id == hint.peer_id)
            || rt.managed_peers.contains(&hint.peer_id);
        if !known && hint.direction == "outgoing" {
            rt.managed_peers.push(hint.peer_id.clone());
        }
        let sharing = rt.sharing.entry(hint.peer_id.clone()).or_default();
        if sharing.refresh_scheduled {
            return;
        }
        sharing.refresh_scheduled = true;
        let (generation, id, peer) = (rt.generation, id.to_string(), hint.peer_id);
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CHANGE_DEBOUNCE).await;
            let _ = this.update(cx, |s, cx| {
                let Some(rt) = s.live(&id, generation) else {
                    return;
                };
                if let Some(sharing) = rt.sharing.get_mut(&peer) {
                    sharing.refresh_scheduled = false;
                }
                s.load_peer_sharing(&id, &peer, cx);
            });
        });
        rt.tasks.push(task);
        rt.tasks.retain(|t| !t.is_ready());
    }
}

/// Peers of the Mesh page: online nodes (by label), then offline peers
/// with a saved policy.
pub fn peer_ids(rt: &AgentRuntime) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = rt.mesh_nodes.iter().map(|n| (n.id.clone(), true)).collect();
    for p in &rt.managed_peers {
        if !out.iter().any(|(id, _)| id == p) {
            out.push((p.clone(), false));
        }
    }
    out
}

/// Peer -> sharing, the per-agent map.
pub type SharingMap = HashMap<String, PeerSharing>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A `getPeerAccess` reply in the shape querymt sends (see
    /// `ext_mesh.rs` `catalog_withdrawal_refreshes_acp_and_v1_peer_access`
    /// and `typed_peer_access_preserves_the_flat_wire_and_exact_tokens`).
    fn reply() -> Value {
        json!({
            "version": 1,
            "peer_id": "12D3KooWPeer",
            "outgoing": {
                "revision": "6f1c0a0e-0000-4000-8000-000000000001",
                "grant": { "mode": "custom", "sessions": true, "filesystem": false, "providers": [
                    { "provider_id": "codex", "models": { "mode": "selected", "model_ids": ["gpt-6.2-sol"] } }
                ] },
                "quotas": [{
                    "scope": { "provider_id": "codex", "model_id": "gpt-6.2-sol" },
                    "limit": { "kind": "finite", "limit_tokens": "9007199254740993", "window": "daily" },
                    "active": true,
                    "usage": {
                        "known_tokens": "2604", "estimated_tokens": "0", "has_unknown_usage": false,
                        "reserved_tokens": "128089", "available_tokens": "0",
                        "window_started_at": "2026-11-22T00:00:00Z",
                        "next_reset_at": "2026-11-23T00:00:00Z"
                    }
                }, {
                    "scope": { "provider_id": "codex" },
                    "limit": { "kind": "unlimited" },
                    "active": true,
                    "usage": { "known_tokens": "2604", "estimated_tokens": "0", "has_unknown_usage": false }
                }]
            },
            "incoming": { "status": "granted", "effective": {
                "mode": "custom", "sessions": false, "filesystem": false, "shell": false,
                "providers": [{ "provider_id": "openai", "models": { "mode": "all" } }]
            }, "quotas": [] },
            "shareable_catalog": [{ "provider_id": "codex", "models": [
                { "model_id": "gpt-6.2-sol", "finite_quota_supported": true }
            ] }]
        })
    }

    #[test]
    fn a_peer_access_reply_round_trips_exactly() {
        let wire = reply();
        let access: PeerAccess = serde_json::from_value(wire.clone()).unwrap();
        // Exact tokens beyond f64 precision.
        let QuotaLimit::Finite {
            limit_tokens,
            window,
        } = access.outgoing.quotas[0].limit
        else {
            panic!("finite");
        };
        assert_eq!(limit_tokens, 9_007_199_254_740_993);
        assert_eq!(window, QuotaWindow::Daily);
        assert_eq!(serde_json::to_value(&access).unwrap(), wire);
    }

    #[test]
    fn every_incoming_status_parses() {
        for status in ["denied", "unavailable", "incompatible", "legacy"] {
            let incoming: Incoming = serde_json::from_value(json!({ "status": status })).unwrap();
            assert_eq!(serde_json::to_value(incoming).unwrap()["status"], status);
        }
        // An omitted window means manual, as the agent reads it.
        let limit: QuotaLimit =
            serde_json::from_value(json!({ "kind": "finite", "limit_tokens": "5" })).unwrap();
        assert_eq!(
            limit,
            QuotaLimit::Finite {
                limit_tokens: 5,
                window: QuotaWindow::Manual
            }
        );
    }

    #[test]
    fn writes_send_the_v1_requests() {
        let (method, params) = Write::Grant(PeerGrant::None).request("peer", "rev-1");
        assert_eq!(method, SET_ACCESS);
        assert_eq!(
            params,
            json!({ "version": 1, "peer_id": "peer", "expected_revision": "rev-1",
                    "grant": { "mode": "none" } })
        );
        let (method, params) = Write::Quota {
            scope: QuotaScope::model("codex", "gpt-6.2-sol"),
            operation: QuotaOperation::Reconfigure,
            limit: QuotaLimit::Finite {
                limit_tokens: 2_000_001,
                window: QuotaWindow::Weekly,
            },
        }
        .request("peer", "rev-2");
        assert_eq!(method, SET_QUOTA);
        assert_eq!(
            params,
            json!({ "version": 1, "peer_id": "peer", "expected_revision": "rev-2",
                    "scope": { "provider_id": "codex", "model_id": "gpt-6.2-sol" },
                    "operation": "reconfigure",
                    "limit": { "kind": "finite", "limit_tokens": "2000001", "window": "weekly" } })
        );
        let (method, params) = Write::Reset(QuotaScope::provider("codex")).request("peer", "r");
        assert_eq!(method, RESET_QUOTA);
        assert_eq!(
            params,
            json!({ "version": 1, "peer_id": "peer", "expected_revision": "r",
                    "scope": { "provider_id": "codex" } })
        );
        let (_, params) = Write::Grant(PeerGrant::Custom {
            sessions: true,
            filesystem: true,
            providers: vec![ProviderGrant {
                provider_id: "codex".into(),
                models: ModelSelection::All,
            }],
        })
        .request("peer", "r");
        assert_eq!(
            params["grant"],
            json!({ "mode": "custom", "sessions": true, "filesystem": true,
                    "providers": [{ "provider_id": "codex", "models": { "mode": "all" } }] })
        );
    }

    #[test]
    fn mtok_amounts_are_exact() {
        assert_eq!(parse_mtok("2.000001"), Ok(2_000_001));
        assert_eq!(parse_mtok(" 5 "), Ok(5_000_000));
        assert_eq!(parse_mtok(".5"), Ok(500_000));
        assert_eq!(parse_mtok("0.000001"), Ok(1));
        for bad in [
            "",
            "0",
            "0.0",
            "-1",
            "1.0000001",
            "1e3",
            "abc",
            "18446744073710",
        ] {
            assert!(parse_mtok(bad).is_err(), "{bad}");
        }
        for n in [1, 2_000_001, 5_000_000, 123_456_789] {
            assert_eq!(parse_mtok(&mtok_text(n)), Ok(n));
        }
        assert_eq!(tokens_label(1), "1 token");
        assert_eq!(tokens_label(2604), "2,604 tokens");
        assert_eq!(tokens_label(2_500_000), "2.5 Mtok");
        assert_eq!(tokens_label(128_089), "0.13 Mtok");
    }

    #[test]
    fn amount_only_edits_update_and_others_reconfigure() {
        let daily = |n| QuotaLimit::Finite {
            limit_tokens: n,
            window: QuotaWindow::Daily,
        };
        let weekly = QuotaLimit::Finite {
            limit_tokens: 1,
            window: QuotaWindow::Weekly,
        };
        use QuotaOperation::*;
        assert_eq!(quota_operation(Some(&daily(1)), &daily(2)), Update);
        assert_eq!(quota_operation(Some(&daily(1)), &weekly), Reconfigure);
        assert_eq!(quota_operation(None, &daily(1)), Reconfigure);
        assert_eq!(
            quota_operation(Some(&QuotaLimit::Unlimited), &daily(1)),
            Reconfigure
        );
        assert_eq!(
            quota_operation(Some(&daily(1)), &QuotaLimit::Unlimited),
            Reconfigure
        );
    }

    fn rpc_error(error: Value) -> RpcError {
        // Through the real client so `raw()` is what the agent sent.
        let (tx, _rx) = async_channel::unbounded();
        let rpc = crate::rpc::RpcClient::new(tx);
        let request = rpc.request_detailed("x", json!({}));
        rpc.handle(&json!({ "id": 1, "error": error }).to_string());
        futures::executor::block_on(request).unwrap_err()
    }

    #[test]
    fn errors_map_by_code() {
        let conflict = rpc_error(json!({ "code": -32000, "message": "grant revision changed",
            "data": { "code": "mesh_access_conflict", "expected_revision": "a", "actual_revision": "b" } }));
        assert_eq!(AccessError::from_rpc(&conflict), AccessError::Conflict);
        let busy = rpc_error(
            json!({ "code": -32000, "message": "quota has outstanding attempts",
            "data": { "code": "mesh_quota_in_flight", "scope": { "provider_id": "codex", "model_id": "m" } } }),
        );
        let mapped = AccessError::from_rpc(&busy);
        assert_eq!(
            mapped,
            AccessError::InFlight(Some(QuotaScope::model("codex", "m")))
        );
        assert!(mapped.message().contains("m (codex)"));
        let invalid = rpc_error(json!({ "code": -32602, "message": "Invalid params",
            "data": { "code": "mesh_access_invalid_params" } }));
        assert_eq!(AccessError::from_rpc(&invalid), AccessError::Invalid);
        let missing = rpc_error(json!({ "code": -32601, "message": "Method not found" }));
        assert_eq!(AccessError::from_rpc(&missing), AccessError::Unsupported);
        // Message text alone never decides.
        let text = rpc_error(json!({ "code": -32000, "message": "mesh_access_conflict" }));
        assert!(matches!(
            AccessError::from_rpc(&text),
            AccessError::Other(_)
        ));
    }

    fn outgoing(revision: &str, grant: PeerGrant) -> Outgoing {
        Outgoing {
            revision: revision.into(),
            grant,
            quotas: Vec::new(),
        }
    }

    #[test]
    fn a_draft_rebases_over_quota_changes_but_not_grant_changes() {
        let base = outgoing("r1", PeerGrant::None);
        let mut draft = GrantDraft::new(&base);
        draft.set_mode("custom");
        draft.set_sessions(true);
        assert!(draft.can_save());
        // Our own (or another client's) quota write: the grant is the same.
        draft.follow(&outgoing("r2", PeerGrant::None));
        assert_eq!(draft.base_revision, "r2");
        assert!(draft.can_save());
        // Another client granted Full meanwhile: show it, never overwrite.
        let theirs = outgoing("r3", PeerGrant::Full);
        draft.follow(&theirs);
        assert_eq!(
            draft.changed_elsewhere,
            vec!["Access changed from No access to Full access."]
        );
        assert!(!draft.can_save());
        assert_eq!(
            draft.base_revision, "r2",
            "still based on what the owner saw"
        );
        draft.keep_mine(&theirs);
        assert!(draft.can_save());
        assert_eq!(draft.base_revision, "r3");
    }

    #[test]
    fn draft_rules() {
        let mut draft = GrantDraft::new(&outgoing("r", PeerGrant::None));
        assert!(!draft.dirty());
        draft.set_mode("custom");
        draft.set_filesystem(true);
        assert!(
            matches!(
                draft.grant,
                PeerGrant::Custom {
                    filesystem: false,
                    ..
                }
            ),
            "files need sessions"
        );
        draft.set_sessions(true);
        draft.set_filesystem(true);
        draft.set_sessions(false);
        assert!(matches!(
            draft.grant,
            PeerGrant::Custom {
                sessions: false,
                filesystem: false,
                ..
            }
        ));
        let order = vec!["anthropic".to_string(), "codex".to_string()];
        draft.set_provider("codex", true, &order);
        draft.set_provider("anthropic", true, &order);
        draft.set_all_models("codex", false, &[]);
        assert!(draft.problem().unwrap().contains("codex"));
        draft.toggle_model("codex", "gpt-6.2-sol");
        assert!(draft.problem().is_none());
        let PeerGrant::Custom { providers, .. } = &draft.grant else {
            panic!()
        };
        assert_eq!(providers[0].provider_id, "anthropic", "catalog order");
        // Full needs an acknowledgement; Custom settings survive a detour.
        let custom = draft.grant.clone();
        draft.set_mode("full");
        assert!(draft.problem().is_some());
        draft.full_ack = true;
        assert!(draft.can_save());
        draft.set_mode("custom");
        assert_eq!(draft.grant, custom);
        assert!(!draft.full_ack);
    }

    #[test]
    fn changes_read_as_sentences() {
        let custom = |sessions, providers: Vec<ProviderGrant>| PeerGrant::Custom {
            sessions,
            filesystem: false,
            providers,
        };
        let all = |id: &str| ProviderGrant {
            provider_id: id.into(),
            models: ModelSelection::All,
        };
        assert_eq!(
            grant_changes(
                &custom(false, vec![all("codex")]),
                &custom(true, vec![all("openai")])
            ),
            vec![
                "Sessions turned on.",
                "Stopped sharing codex.",
                "Now shares openai (all models)."
            ]
        );
        let access: PeerAccess = serde_json::from_value(reply()).unwrap();
        let mut later = access.outgoing.clone();
        later.quotas[0].limit = QuotaLimit::Finite {
            limit_tokens: 5_000_000,
            window: QuotaWindow::Weekly,
        };
        assert_eq!(
            quota_changes(&access.outgoing.quotas, &later.quotas),
            vec![
                "Limit for gpt-6.2-sol (codex) changed from 9007199254.74 Mtok a day \
                 to 5 Mtok a week."
            ]
        );
        let mut reset = access.outgoing.clone();
        reset.quotas[0].usage.window_started_at = Some("2026-11-22T10:00:00Z".into());
        assert_eq!(
            quota_changes(&access.outgoing.quotas, &reset.quotas),
            vec!["Limit for gpt-6.2-sol (codex) was reset."]
        );
    }

    #[test]
    fn summaries_at_a_glance() {
        let access: PeerAccess = serde_json::from_value(reply()).unwrap();
        assert_eq!(
            grant_summary(&access.outgoing.grant),
            "Sessions, codex (gpt-6.2-sol)"
        );
        assert_eq!(
            incoming_summary(access.incoming.as_ref()),
            "openai (all models)"
        );
        assert_eq!(incoming_summary(Some(&Incoming::Denied)), "Nothing");
        assert_eq!(grant_summary(&PeerGrant::Full), "Full access");
        let now = crate::util::parse_rfc3339_ms("2026-11-22T22:30:00Z").unwrap();
        assert_eq!(
            usage_line(&access.outgoing.quotas[0], now),
            "2,604 tokens used · 0.13 Mtok reserved · 0 tokens left · Resets in 1h (Nov 23, 2026, 00:00 UTC)"
        );
        assert!(access.outgoing.quotas[0].exhausted());
        assert_eq!(usage_fraction(&access.outgoing.quotas[1]), None);
    }

    struct Harness {
        store: gpui_kit::Entity<AgentsStore>,
        rpc: std::rc::Rc<crate::rpc::RpcClient>,
        requests: async_channel::Receiver<String>,
        generation: u64,
    }

    impl Harness {
        fn new(cx: &mut gpui_kit::TestAppContext) -> Self {
            use gpui_kit::AppContext as _;
            let mut store = crate::store::launch::peer_fixture();
            let (rpc, requests) = store.rpc_for_test("test-agent");
            let rt = store.runtime_mut_for_test("test-agent");
            rt.capabilities = Some(json!({
                "features": { "mesh": true },
                "mesh_access_version": 1,
                "methods": [GET, SET_ACCESS, SET_QUOTA, RESET_QUOTA],
                "notifications": [CHANGED],
            }));
            let generation = rt.generation;
            Self {
                store: cx.new(|_| store),
                rpc,
                requests,
                generation,
            }
        }

        fn next(&self) -> Value {
            serde_json::from_str(&self.requests.try_recv().expect("a request")).unwrap()
        }

        fn reply(&self, request: &Value, result: Value, cx: &mut gpui_kit::TestAppContext) {
            self.rpc
                .handle(&json!({ "id": request["id"], "result": result }).to_string());
            cx.run_until_parked();
        }

        fn fail(&self, request: &Value, error: Value, cx: &mut gpui_kit::TestAppContext) {
            self.rpc
                .handle(&json!({ "id": request["id"], "error": error }).to_string());
            cx.run_until_parked();
        }

        fn notify(&self, params: Value, cx: &mut gpui_kit::TestAppContext) {
            let generation = self.generation;
            self.store.update(cx, |s, cx| {
                s.on_incoming(
                    "test-agent",
                    generation,
                    crate::rpc::Incoming::Notification {
                        method: CHANGED.into(),
                        params,
                    },
                    cx,
                )
            });
        }

        fn sharing(&self, cx: &mut gpui_kit::TestAppContext) -> PeerSharing {
            cx.update(|cx| {
                self.store
                    .read(cx)
                    .peer_sharing("test-agent", "12D3KooWPeer")
                    .cloned()
                    .unwrap_or_default()
            })
        }

        fn load(&self, cx: &mut gpui_kit::TestAppContext) {
            self.store.update(cx, |s, cx| {
                s.load_peer_sharing("test-agent", "12D3KooWPeer", cx)
            });
            cx.run_until_parked();
            let read = self.next();
            assert_eq!(read["method"], "_querymt/mesh/getPeerAccess");
            assert_eq!(
                read["params"],
                json!({ "version": 1, "peer_id": "12D3KooWPeer" })
            );
            self.reply(&read, reply(), cx);
        }
    }

    #[gpui_kit::test]
    fn a_change_hint_re_reads_the_peer_once(cx: &mut gpui_kit::TestAppContext) {
        let h = Harness::new(cx);
        let hint = json!({ "version": 1, "peer_id": "12D3KooWPeer",
                           "direction": "outgoing", "reason": "policy" });
        h.notify(hint.clone(), cx);
        h.notify(hint, cx);
        cx.run_until_parked();
        assert!(h.requests.try_recv().is_err(), "debounced");
        cx.executor().advance_clock(CHANGE_DEBOUNCE);
        cx.run_until_parked();
        let read = h.next();
        assert_eq!(read["method"], "_querymt/mesh/getPeerAccess");
        assert!(h.requests.try_recv().is_err(), "one burst, one read");
        h.reply(&read, reply(), cx);
        let sharing = h.sharing(cx);
        assert!(!sharing.loading);
        assert_eq!(
            sharing.access.unwrap().outgoing.revision,
            "6f1c0a0e-0000-4000-8000-000000000001"
        );
        // A saved policy for a peer the list does not show: listed as offline.
        h.notify(
            json!({ "version": 1, "peer_id": "new-peer", "direction": "outgoing", "reason": "policy" }),
            cx,
        );
        cx.update(|cx| {
            let s = h.store.read(cx);
            assert!(
                s.runtime["test-agent"]
                    .managed_peers
                    .contains(&"new-peer".to_string())
            );
        });
    }

    #[gpui_kit::test]
    fn a_write_applies_the_confirmed_state(cx: &mut gpui_kit::TestAppContext) {
        let h = Harness::new(cx);
        h.load(cx);
        let task = h.store.update(cx, |s, cx| {
            s.write_peer_sharing(
                "test-agent",
                "12D3KooWPeer",
                Write::Grant(PeerGrant::None),
                cx,
            )
        });
        cx.run_until_parked();
        let write = h.next();
        assert_eq!(write["method"], "_querymt/mesh/setPeerAccess");
        assert_eq!(
            write["params"]["expected_revision"],
            "6f1c0a0e-0000-4000-8000-000000000001"
        );
        h.reply(
            &write,
            json!({ "version": 1, "peer_id": "12D3KooWPeer", "outgoing": {
                "revision": "r2", "grant": { "mode": "none" }, "quotas": [] } }),
            cx,
        );
        assert_eq!(futures::executor::block_on(task), Ok(()));
        let access = h.sharing(cx).access.unwrap();
        assert_eq!(access.outgoing.revision, "r2");
        assert_eq!(access.outgoing.grant, PeerGrant::None);
        assert!(access.incoming.is_some(), "the incoming side stays");
        assert!(access.shareable_catalog.is_some(), "the catalog stays");
    }

    #[gpui_kit::test]
    fn a_stale_write_is_refused_and_re_read(cx: &mut gpui_kit::TestAppContext) {
        let h = Harness::new(cx);
        h.load(cx);
        let task = h.store.update(cx, |s, cx| {
            s.write_peer_sharing(
                "test-agent",
                "12D3KooWPeer",
                Write::Reset(QuotaScope::model("codex", "gpt-6.2-sol")),
                cx,
            )
        });
        cx.run_until_parked();
        let write = h.next();
        assert_eq!(write["method"], "_querymt/mesh/resetPeerQuota");
        h.fail(
            &write,
            json!({ "code": -32000, "message": "grant revision changed", "data": {
                "code": "mesh_access_conflict",
                "expected_revision": "6f1c0a0e-0000-4000-8000-000000000001",
                "actual_revision": "r9" } }),
            cx,
        );
        assert_eq!(
            futures::executor::block_on(task),
            Err(AccessError::Conflict)
        );
        // Never retried: the saved state is read instead.
        let read = h.next();
        assert_eq!(read["method"], "_querymt/mesh/getPeerAccess");
        let mut theirs = reply();
        theirs["outgoing"]["revision"] = json!("r9");
        theirs["outgoing"]["grant"] = json!({ "mode": "full" });
        h.reply(&read, theirs, cx);
        let outgoing = h.sharing(cx).access.unwrap().outgoing;
        assert_eq!(outgoing.revision, "r9");
        assert_eq!(outgoing.grant, PeerGrant::Full);
        assert!(h.requests.try_recv().is_err());
    }

    #[gpui_kit::test]
    fn a_read_overtaken_by_a_write_reads_again(cx: &mut gpui_kit::TestAppContext) {
        let h = Harness::new(cx);
        h.load(cx);
        // A read starts, then a write lands before its reply.
        h.store.update(cx, |s, cx| {
            s.load_peer_sharing("test-agent", "12D3KooWPeer", cx)
        });
        let _task = h.store.update(cx, |s, cx| {
            s.write_peer_sharing(
                "test-agent",
                "12D3KooWPeer",
                Write::Grant(PeerGrant::None),
                cx,
            )
        });
        cx.run_until_parked();
        let (read, write) = (h.next(), h.next());
        h.reply(
            &write,
            json!({ "version": 1, "peer_id": "12D3KooWPeer", "outgoing": {
                "revision": "r2", "grant": { "mode": "none" }, "quotas": [] } }),
            cx,
        );
        // The old read's state predates the write: dropped, read again.
        h.reply(&read, reply(), cx);
        assert_eq!(h.sharing(cx).access.unwrap().outgoing.revision, "r2");
        let again = h.next();
        assert_eq!(again["method"], "_querymt/mesh/getPeerAccess");
    }

    #[gpui_kit::test]
    fn without_mesh_access_v1_nothing_is_asked(cx: &mut gpui_kit::TestAppContext) {
        let h = Harness::new(cx);
        h.store.update(cx, |s, _| {
            s.runtime_mut_for_test("test-agent").capabilities =
                Some(json!({ "features": { "mesh": true }, "methods": [GET] }));
        });
        h.store.update(cx, |s, cx| {
            s.load_peer_sharing("test-agent", "12D3KooWPeer", cx)
        });
        cx.run_until_parked();
        assert!(h.requests.try_recv().is_err());
        cx.update(|cx| assert!(!h.store.read(cx).mesh_access_supported("test-agent")));
    }

    #[test]
    fn nodes_list_saved_offline_peers() {
        let nodes = json!({ "nodes": [{ "id": "a" }], "managed_peer_ids": ["a", "b"] });
        assert_eq!(managed_peers(&nodes), vec!["a", "b"]);
        let rt = AgentRuntime {
            mesh_nodes: vec![super::super::MeshNode {
                id: "a".into(),
                label: "alpha".into(),
                active_sessions: 0,
                capabilities: Vec::new(),
                transport: String::new(),
            }],
            managed_peers: vec!["a".into(), "b".into()],
            ..Default::default()
        };
        assert_eq!(peer_ids(&rt), vec![("a".into(), true), ("b".into(), false)]);
    }
}
