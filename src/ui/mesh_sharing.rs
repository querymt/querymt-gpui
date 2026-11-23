//! Peer sharing on the Mesh page: every peer row says what this machine
//! shares with the peer and what the peer shares back; the row opens (like a
//! provider in Settings) into the editor for this machine's side: No access,
//! Custom (sessions, files, providers and models) or Full, plus token limits
//! per provider and model. The peer's side is read-only.
//!
//! Grant edits are a draft saved with the revision they started from; a
//! change made elsewhere meanwhile is shown, never overwritten. Limits,
//! resets and "Stop sharing" write at once, against the latest revision.

use std::collections::{HashMap, HashSet};

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Disableable as _, Icon, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::kit::ButtonExt as _;
use super::mesh::short_peer;
use super::{QmtApp, kit};
use crate::store::mesh_access::{
    self as access, AccessError, GrantDraft, Incoming, ModelSelection, Outgoing, PeerAccess,
    PeerGrant, PeerQuota, PeerSharing, QuotaLimit, QuotaScope, QuotaWindow, ShareableProvider,
    TokenUnit, Write,
};
use crate::theme::{Palette, palette};
use crate::util::now_ms;

/// One write the page started, by what it changes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Op {
    Grant,
    Stop,
    Quota(QuotaScope),
    Reset(QuotaScope),
}

/// The limit being edited.
struct QuotaEdit {
    peer: String,
    scope: QuotaScope,
    window: QuotaWindow,
    /// Unit of the amount field.
    unit: TokenUnit,
    /// The saved limit when the editor opened.
    existing: Option<QuotaLimit>,
}

pub(super) struct SharingPage {
    /// The expanded peer.
    open: Option<String>,
    drafts: HashMap<String, GrantDraft>,
    busy: HashSet<(String, Op)>,
    errors: HashMap<(String, Op), String>,
    /// Refused as stale: the outgoing state the write was based on, until
    /// the re-read shows what changed instead.
    stale: HashMap<(String, Op), Outgoing>,
    /// Refused while a request was using the limit: its settled usage then.
    /// Once that changes the request is over and the notice goes.
    busy_refusals: HashMap<(String, Op), (u64, Option<String>)>,
    edit: Option<QuotaEdit>,
    amount: Entity<InputState>,
    /// The model picker that is open: `(peer, provider)`.
    models_modal: Option<(String, String)>,
    models_query: Entity<InputState>,
}

impl SharingPage {
    pub(super) fn new(window: &mut Window, cx: &mut Context<QmtApp>) -> Self {
        let amount = cx.new(|cx| InputState::new(window, cx).placeholder("e.g. 2.5"));
        let models_query = cx.new(|cx| InputState::new(window, cx).placeholder("Search models"));
        Self {
            open: None,
            drafts: HashMap::new(),
            busy: HashSet::new(),
            errors: HashMap::new(),
            stale: HashMap::new(),
            busy_refusals: HashMap::new(),
            edit: None,
            amount,
            models_modal: None,
            models_query,
        }
    }

    /// Enter saves the limit being edited; the model search filters as typed.
    pub(super) fn subscribe(
        &self,
        window: &mut Window,
        cx: &mut Context<QmtApp>,
    ) -> Vec<Subscription> {
        vec![
            cx.subscribe_in(
                &self.amount,
                window,
                |this, _, ev: &InputEvent, _, cx| match ev {
                    InputEvent::PressEnter { .. } => this.save_quota(cx),
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &self.models_query,
                window,
                |_, _, ev: &InputEvent, _, cx| {
                    if matches!(ev, InputEvent::Change) {
                        cx.notify()
                    }
                },
            ),
        ]
    }

    /// Escape: close the model picker, else the limit editor.
    pub(super) fn close_editor(&mut self) -> bool {
        self.models_modal.take().is_some() || self.edit.take().is_some()
    }

    /// Follow what the agent now has saved: drafts rebase or show a change
    /// made elsewhere; stale writes say what changed.
    fn sync(&mut self, sharing: &HashMap<String, PeerSharing>) {
        for (peer, draft) in &mut self.drafts {
            if let Some(a) = sharing.get(peer).and_then(|s| s.access.as_ref()) {
                draft.follow(&a.outgoing);
            }
        }
        self.drafts
            .retain(|_, d| d.dirty() || !d.changed_elsewhere.is_empty());
        let mut done = Vec::new();
        for ((peer, op), before) in &self.stale {
            let Some(now) = sharing.get(peer).and_then(|s| s.access.as_ref()) else {
                continue;
            };
            if now.outgoing.revision == before.revision {
                continue;
            }
            let changes = access::outgoing_changes(before, &now.outgoing);
            let mut text = "Not saved: sharing for this peer changed elsewhere.".to_string();
            if !changes.is_empty() {
                text = format!("{text} {}", changes.join(" "));
            }
            if *op != Op::Grant {
                text.push_str(" Check the latest settings and try again.");
            }
            self.errors.insert((peer.clone(), op.clone()), text);
            done.push((peer.clone(), op.clone()));
        }
        for key in done {
            self.stale.remove(&key);
        }
        let settled = |peer: &str, op: &Op| {
            let scope = match op {
                Op::Quota(s) | Op::Reset(s) => s,
                _ => return None,
            };
            let q = sharing.get(peer)?.access.as_ref()?.outgoing.quota(scope)?;
            Some((q.usage.used(), q.usage.window_started_at.clone()))
        };
        let over: Vec<(String, Op)> = self
            .busy_refusals
            .iter()
            .filter(|((peer, op), then)| settled(peer, op).is_some_and(|now| &now != *then))
            .map(|(k, _)| k.clone())
            .collect();
        for key in over {
            self.busy_refusals.remove(&key);
            self.errors.remove(&key);
        }
    }
}

/// Everything the page shows of one peer.
#[derive(Clone)]
struct PeerView {
    id: String,
    name: String,
    online: bool,
    active_sessions: u64,
    sharing: PeerSharing,
}

fn mode_of(grant: &PeerGrant) -> &'static str {
    match grant {
        PeerGrant::None => "none",
        PeerGrant::Full => "full",
        PeerGrant::Custom { .. } => "custom",
    }
}

fn shared<'a>(grant: &'a PeerGrant, provider: &str) -> Option<&'a ModelSelection> {
    match grant {
        PeerGrant::Custom { providers, .. } => providers
            .iter()
            .find(|p| p.provider_id == provider)
            .map(|p| &p.models),
        _ => None,
    }
}

/// Width of the scope column (provider, or one of its models) of a
/// provider's limit lines, so every usage bar starts at the same x.
const SCOPE_W: f32 = 200.;

fn subtle_text(p: &Palette) -> Div {
    div().text_xs().text_color(p.muted)
}

impl QmtApp {
    /// Write `write` for `peer`; `op` owns the progress and the error.
    fn sharing_write(
        &mut self,
        agent: &str,
        peer: &str,
        op: Op,
        write: Write,
        cx: &mut Context<Self>,
    ) {
        let key = (peer.to_string(), op.clone());
        if self.sharing.busy.contains(&key) {
            return;
        }
        let before = self
            .store
            .read(cx)
            .peer_sharing(agent, peer)
            .and_then(|s| s.access.as_ref())
            .map(|a| a.outgoing.clone());
        self.sharing.busy.insert(key.clone());
        self.sharing.errors.remove(&key);
        self.sharing.busy_refusals.remove(&key);
        let task = self
            .store
            .update(cx, |s, cx| s.write_peer_sharing(agent, peer, write, cx));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.sharing.busy.remove(&key);
                match result {
                    Ok(()) => match &key.1 {
                        Op::Grant | Op::Stop => {
                            this.sharing.drafts.remove(&key.0);
                        }
                        Op::Quota(scope) => {
                            if this
                                .sharing
                                .edit
                                .as_ref()
                                .is_some_and(|e| e.peer == key.0 && &e.scope == scope)
                            {
                                this.sharing.edit = None;
                            }
                        }
                        Op::Reset(_) => {}
                    },
                    Err(AccessError::Conflict) => {
                        this.sharing
                            .errors
                            .insert(key.clone(), AccessError::Conflict.message());
                        if let Some(before) = before {
                            this.sharing.stale.insert(key, before);
                        }
                    }
                    Err(err @ AccessError::InFlight(_)) => {
                        let usage = before.as_ref().and_then(|o| match &key.1 {
                            Op::Quota(s) | Op::Reset(s) => o
                                .quota(s)
                                .map(|q| (q.usage.used(), q.usage.window_started_at.clone())),
                            _ => None,
                        });
                        if let Some(usage) = usage {
                            this.sharing.busy_refusals.insert(key.clone(), usage);
                        }
                        this.sharing.errors.insert(key.clone(), err.message());
                        // Show the reservation that is holding it.
                        if let Some(agent) = this.store.read(cx).primary_agent() {
                            let peer = key.0.clone();
                            this.store
                                .update(cx, |s, cx| s.load_peer_sharing(&agent, &peer, cx));
                        }
                    }
                    Err(err) => {
                        this.sharing.errors.insert(key, err.message());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn sharing_draft(&mut self, peer: &str, cx: &App) -> Option<&mut GrantDraft> {
        let agent = self.store.read(cx).primary_agent()?;
        let outgoing = self
            .store
            .read(cx)
            .peer_sharing(&agent, peer)?
            .access
            .as_ref()?
            .outgoing
            .clone();
        Some(
            self.sharing
                .drafts
                .entry(peer.to_string())
                .or_insert_with(|| GrantDraft::new(&outgoing)),
        )
    }

    /// Edit `peer`'s draft (created from the saved grant on first edit).
    fn edit_draft(&mut self, peer: &str, cx: &mut Context<Self>, f: impl FnOnce(&mut GrantDraft)) {
        if let Some(draft) = self.sharing_draft(peer, cx) {
            f(draft);
        }
        self.sharing.errors.remove(&(peer.to_string(), Op::Grant));
        cx.notify();
    }

    fn save_quota(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.sharing.edit.as_ref() else {
            return;
        };
        let Some(agent) = self.store.read(cx).primary_agent() else {
            return;
        };
        let (peer, scope) = (edit.peer.clone(), edit.scope.clone());
        let key = (peer.clone(), Op::Quota(scope.clone()));
        let tokens = match access::parse_amount(&self.sharing.amount.read(cx).value(), edit.unit) {
            Ok(tokens) => tokens,
            Err(err) => {
                self.sharing.errors.insert(key, err);
                cx.notify();
                return;
            }
        };
        let limit = QuotaLimit::Finite {
            limit_tokens: tokens,
            window: edit.window,
        };
        let operation = access::quota_operation(edit.existing.as_ref(), &limit);
        self.sharing_write(
            &agent,
            &peer,
            Op::Quota(scope.clone()),
            Write::Quota {
                scope,
                operation,
                limit,
            },
            cx,
        );
    }

    fn open_quota_editor(
        &mut self,
        peer: &str,
        scope: QuotaScope,
        existing: Option<QuotaLimit>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (text, win, unit) = match &existing {
            Some(QuotaLimit::Finite {
                limit_tokens,
                window,
            }) => {
                let unit = TokenUnit::for_tokens(*limit_tokens);
                (access::amount_text(*limit_tokens, unit), *window, unit)
            }
            _ => (String::new(), QuotaWindow::Daily, TokenUnit::M),
        };
        self.sharing
            .errors
            .remove(&(peer.to_string(), Op::Quota(scope.clone())));
        self.sharing.edit = Some(QuotaEdit {
            peer: peer.to_string(),
            scope,
            window: win,
            unit,
            existing,
        });
        self.sharing.amount.update(cx, |s, cx| {
            s.set_value(SharedString::from(text), window, cx);
            s.focus(window, cx);
        });
        cx.notify();
    }

    /// The Nodes list with sharing: online peers, then offline peers with
    /// a saved policy.
    pub(super) fn render_sharing_nodes(
        &mut self,
        agent: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = *palette(cx);
        let store = self.store.read(cx);
        let Some(rt) = store.runtime(agent) else {
            return div().into_any_element();
        };
        let peers: Vec<PeerView> = access::peer_ids(rt)
            .into_iter()
            .map(|(id, online)| {
                let node = rt.mesh_nodes.iter().find(|n| n.id == id);
                PeerView {
                    name: rt.peer_name(&id),
                    active_sessions: node.map_or(0, |n| n.active_sessions),
                    sharing: rt.sharing.get(&id).cloned().unwrap_or_default(),
                    online,
                    id,
                }
            })
            .collect();
        let hostname = rt
            .mesh
            .as_ref()
            .and_then(|m| m.hostname.clone())
            .unwrap_or_else(|| "this machine".into());
        let show_remote = store.prefs.show_remote_sessions;
        let remote_peers = store.prefs.remote_session_peers.clone();
        let sharing_map = rt.sharing.clone();
        self.sharing.sync(&sharing_map);

        if peers.is_empty() {
            return kit::card(&p)
                .p_4()
                .text_sm()
                .text_color(p.muted)
                .child("No peers yet. Invite a machine to join this mesh, then choose what it may use here.")
                .into_any_element();
        }
        let count = peers.len();
        let mut rows: Vec<AnyElement> = Vec::new();
        for (ix, peer) in peers.into_iter().enumerate() {
            let open = self.sharing.open.as_deref() == Some(peer.id.as_str());
            let included = remote_peers.contains(&peer.id);
            rows.push(
                self.peer_row(agent, &peer, ix == 0, open, show_remote, included, &p, cx)
                    .into_any_element(),
            );
            if open {
                rows.push(
                    self.sharing_panel(agent, &peer, &hostname, &p, window, cx)
                        .into_any_element(),
                );
            }
        }
        let _ = count;
        kit::card(&p)
            .children(rows)
            .child(kit::card_mask(&p))
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn peer_row(
        &self,
        agent: &str,
        peer: &PeerView,
        first: bool,
        open: bool,
        show_remote: bool,
        included: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let (_, hover) = kit::subtle(p);
        let access = peer.sharing.access.as_ref();
        let unsaved = self.sharing.drafts.contains_key(&peer.id);
        let direction = |icon: IconName, label: &'static str, value: String| {
            h_flex()
                .gap_1()
                .min_w_0()
                .child(kit::icon(icon, 12., p.dim))
                .child(div().text_color(p.dim).child(label))
                .child(div().truncate().text_color(p.muted).child(value))
        };
        let summary = match access {
            Some(a) => h_flex()
                .gap_3()
                .min_w_0()
                .child(direction(
                    IconName::ArrowUpRight,
                    "You share",
                    access::grant_summary(&a.outgoing.grant),
                ))
                .child(direction(
                    IconName::ArrowDownLeft,
                    "They share",
                    access::incoming_summary(a.incoming.as_ref()),
                )),
            None if peer.sharing.error.is_some() => h_flex()
                .gap_1()
                .text_color(p.warn)
                .child(kit::icon(IconName::TriangleAlert, 12., p.warn))
                .child("Could not read sharing"),
            None => h_flex().text_color(p.dim).child("Checking sharing…"),
        };
        let id = peer.id.clone();
        let include_id = peer.id.clone();
        let name = peer.name.clone();
        let a = agent.to_string();
        kit::card_row(first, p)
            .id(kit::keyed("peer", peer.id.clone()))
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .when(open, |el| el.bg(hover))
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.sharing.open.as_deref() == Some(id.as_str()) {
                    this.sharing.open = None;
                } else {
                    this.sharing.open = Some(id.clone());
                    // Fresh state for the editor, the peer's side included.
                    this.store
                        .update(cx, |s, cx| s.load_peer_sharing(&a, &id, cx));
                }
                cx.notify();
            }))
            .child(
                div()
                    .when(!peer.online, |el| el.opacity(0.55))
                    .child(kit::identicon(&peer.id, 22.)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().text_sm().font_semibold().child(peer.name.clone()))
                            .child(if !peer.online {
                                kit::icon_chip(IconName::WifiOff, "Offline", p)
                            } else if peer.active_sessions > 0 {
                                kit::tone_chip(
                                    format!("{} active", peer.active_sessions),
                                    p.success,
                                )
                            } else {
                                kit::chip("idle", p)
                            })
                            .when(
                                access.is_some_and(|a| {
                                    matches!(
                                        a.outgoing.grant,
                                        PeerGrant::Full | PeerGrant::Custom { shell: true, .. }
                                    )
                                }),
                                |el| {
                                    el.child(kit::icon_tone_chip(
                                        IconName::SquareTerminal,
                                        "Shell access",
                                        p.warn,
                                    ))
                                },
                            )
                            .when(peer.sharing.limit_reached(), |el| {
                                el.child(kit::icon_tone_chip(
                                    IconName::Gauge,
                                    "Limit reached",
                                    p.warn,
                                ))
                            })
                            .when(unsaved, |el| el.child(kit::chip("Unsaved changes", p))),
                    )
                    .child(
                        h_flex()
                            .gap_1p5()
                            .min_w_0()
                            .text_xs()
                            .text_color(p.muted)
                            // The title is the ID when no name is known.
                            .when(peer.name != short_peer(&peer.id), |el| {
                                el.child(div().font_family(kit::MONO).child(short_peer(&peer.id)))
                            })
                            .child(kit::copy_button(
                                kit::keyed("copy-node", peer.id.clone()),
                                peer.id.clone(),
                                "Copy peer ID",
                            ))
                            .child("·")
                            .child(summary),
                    ),
            )
            .when(peer.online, |el| {
                el.child(
                    // The switch is its own control: it must not open the row.
                    div()
                        .flex_none()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            Switch::new(kit::keyed("node-include", peer.id.clone()))
                                .when(show_remote, |s| s.cursor_pointer())
                                .checked(included)
                                .small()
                                .label("Include")
                                .disabled(!show_remote)
                                .tooltip(if show_remote {
                                    format!("Show sessions of {name} in Sessions")
                                } else {
                                    "Turn on Show remote sessions first".to_string()
                                })
                                .on_click(cx.listener(move |this, v: &bool, _, cx| {
                                    let v = *v;
                                    cx.stop_propagation();
                                    this.store.update(cx, |s, cx| {
                                        s.set_remote_session_peer(&include_id, v, cx)
                                    });
                                })),
                        ),
                )
            })
            .child(
                div()
                    .size(px(28.))
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .child(kit::icon(
                        if open {
                            IconName::ChevronUp
                        } else {
                            IconName::ChevronDown
                        },
                        15.,
                        p.muted,
                    )),
            )
    }

    /// The opened peer: this machine's side (editable), then the peer's.
    fn sharing_panel(
        &mut self,
        agent: &str,
        peer: &PeerView,
        hostname: &str,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let (divider, surface) = kit::subtle(p);
        let panel = v_flex()
            .gap_4()
            .px(px(14.4))
            .pt_3()
            .pb_4()
            .border_t_1()
            .border_color(divider)
            .bg(surface.opacity(0.35));
        let Some(access) = peer.sharing.access.clone() else {
            let a = agent.to_string();
            let id = peer.id.clone();
            return panel.child(match &peer.sharing.error {
                Some(err) => h_flex()
                    .gap_2()
                    .text_sm()
                    .child(kit::icon(IconName::TriangleAlert, 14., p.warn))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(format!("Could not read sharing: {err}")),
                    )
                    .child(
                        kit::flat_btn(kit::keyed("sharing-retry", peer.id.clone()), true, true, p)
                            .child("Try again")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.store
                                    .update(cx, |s, cx| s.load_peer_sharing(&a, &id, cx))
                            })),
                    ),
                None => h_flex()
                    .gap_2()
                    .text_sm()
                    .text_color(p.muted)
                    .child(kit::spinner(
                        kit::keyed("sharing-spin", peer.id.clone()),
                        13.,
                        p.muted,
                    ))
                    .child("Checking what you share…"),
            });
        };
        panel
            .child(self.outgoing_editor(agent, peer, &access, hostname, p, window, cx))
            .child(kit::divider(p).bg(divider))
            .child(incoming_view(peer, &access, p))
    }

    #[allow(clippy::too_many_arguments)]
    fn outgoing_editor(
        &mut self,
        agent: &str,
        peer: &PeerView,
        access: &PeerAccess,
        hostname: &str,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let saved = &access.outgoing;
        let draft = self.sharing.drafts.get(&peer.id).cloned();
        let grant = draft.as_ref().map_or(&saved.grant, |d| &d.grant).clone();
        let mode = mode_of(&grant);
        let grant_key = (peer.id.clone(), Op::Grant);
        let saving = self.sharing.busy.contains(&grant_key);
        let stop_key = (peer.id.clone(), Op::Stop);
        let stopping = self.sharing.busy.contains(&stop_key);
        let name = peer.name.clone();

        // ---- header: what this is, and the quick way out
        let header = h_flex()
            .gap_3()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .child(format!("What {name} may use on {hostname}")),
                    )
                    .child(subtle_text(p).child(if peer.online {
                        "Saved on this machine by peer ID; changes apply to new requests."
                    } else {
                        "Offline now. The saved setting stays and applies when it reconnects."
                    })),
            )
            .when(saved.grant != PeerGrant::None, |el| {
                let (a, id) = (agent.to_string(), peer.id.clone());
                el.child(
                    kit::flat_btn(
                        kit::keyed("stop-sharing", peer.id.clone()),
                        !stopping,
                        true,
                        p,
                    )
                    .text_color(p.danger)
                    .when(stopping, |el| {
                        el.child(kit::spinner(
                            kit::keyed("stop-spin", peer.id.clone()),
                            13.,
                            p.danger,
                        ))
                    })
                    .when(!stopping, |el| {
                        el.child(kit::icon(IconName::Ban, 14., p.danger))
                    })
                    .child("Stop sharing")
                    .tooltip(|window, cx| {
                        kit::wrap_tip(
                            "Set No access now. Limits stay saved; work already running stops \
                                 at its next step.",
                        )
                        .build(window, cx)
                    })
                    .when(!stopping, |el| {
                        el.on_click(cx.listener(move |this, _, _, cx| {
                            this.sharing.drafts.remove(&id);
                            this.sharing_write(&a, &id, Op::Stop, Write::Grant(PeerGrant::None), cx)
                        }))
                    }),
                )
            });

        // ---- mode
        let segment = |m: &'static str, label: &'static str, icon: IconName, this: &Self| {
            let id = peer.id.clone();
            let _ = this;
            kit::segment(
                kit::keyed("share-mode", format!("{}-{m}", peer.id)),
                mode == m,
                p,
            )
            .icon(icon)
            .label(label)
            .inactive(saving)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.edit_draft(&id, cx, |d| d.set_mode(m));
            }))
        };
        let modes = h_flex()
            .gap_3()
            .child(
                kit::segment_bar(p)
                    .child(segment("none", "No access", IconName::CircleOff, self))
                    .child(segment(
                        "custom",
                        "Custom",
                        IconName::SlidersHorizontal,
                        self,
                    ))
                    .child(segment(
                        "full",
                        "Full access",
                        IconName::SquareTerminal,
                        self,
                    )),
            )
            .child(subtle_text(p).flex_1().min_w_0().child(match mode {
                "none" => format!("{name} can see this machine in the mesh, nothing more."),
                "custom" => "Only what you turn on below.".to_string(),
                _ => String::new(),
            }));

        let body = match &grant {
            PeerGrant::None => None,
            PeerGrant::Full => Some(full_warning(p)),
            PeerGrant::Custom {
                sessions,
                filesystem,
                shell,
                ..
            } => Some(self.custom_editor(
                agent,
                peer,
                access,
                &grant,
                (*sessions, *filesystem, *shell),
                saving,
                p,
                window,
                cx,
            )),
        };

        // ---- changed elsewhere while editing
        let conflict = draft
            .as_ref()
            .filter(|d| !d.changed_elsewhere.is_empty())
            .map(|d| {
                let (keep, theirs) = (peer.id.clone(), peer.id.clone());
                v_flex()
                    .gap_2()
                    .px_3()
                    .py_2p5()
                    .rounded(px(10.))
                    .bg(p.warn.opacity(0.08))
                    .border_1()
                    .border_color(p.warn.opacity(0.25))
                    .child(
                        h_flex()
                            .gap_1p5()
                            .text_sm()
                            .font_semibold()
                            .child(kit::icon(IconName::TriangleAlert, 14., p.warn))
                            .child("Changed elsewhere while you were editing"),
                    )
                    .children(
                        d.changed_elsewhere
                            .iter()
                            .map(|c| subtle_text(p).child(c.clone())),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                kit::flat_btn(
                                    kit::keyed("use-theirs", peer.id.clone()),
                                    true,
                                    true,
                                    p,
                                )
                                .child("Use the saved version")
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.sharing.drafts.remove(&theirs);
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                kit::flat_btn(
                                    kit::keyed("keep-mine", peer.id.clone()),
                                    true,
                                    true,
                                    p,
                                )
                                .child("Keep my changes")
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        let Some(agent) = this.store.read(cx).primary_agent()
                                        else {
                                            return;
                                        };
                                        let saved = this
                                            .store
                                            .read(cx)
                                            .peer_sharing(&agent, &keep)
                                            .and_then(|s| s.access.as_ref())
                                            .map(|a| a.outgoing.clone());
                                        if let (Some(saved), Some(d)) =
                                            (saved, this.sharing.drafts.get_mut(&keep))
                                        {
                                            d.keep_mine(&saved);
                                        }
                                        cx.notify();
                                    },
                                )),
                            ),
                    )
            });

        // ---- save bar
        let footer = draft.as_ref().filter(|d| d.dirty()).map(|d| {
            let problem = d.problem().filter(|_| d.changed_elsewhere.is_empty());
            let can_save = d.can_save() && !saving;
            let full = d.grant == PeerGrant::Full;
            let (discard, save) = (peer.id.clone(), peer.id.clone());
            let a = agent.to_string();
            let grant = d.grant.clone();
            h_flex()
                .gap_2()
                .child(
                    subtle_text(p)
                        .flex_1()
                        .min_w_0()
                        .child(problem.unwrap_or_else(|| {
                            format!("Saved now: {}", access::grant_summary(&saved.grant))
                        })),
                )
                .child(
                    kit::flat_btn(kit::keyed("discard", peer.id.clone()), !saving, true, p)
                        .child("Discard")
                        .when(!saving, |el| {
                            el.on_click(cx.listener(move |this, _, _, cx| {
                                this.sharing.drafts.remove(&discard);
                                this.sharing.errors.remove(&(discard.clone(), Op::Grant));
                                cx.notify();
                            }))
                        }),
                )
                .child(
                    if full {
                        kit::danger_btn(kit::keyed("save-grant", peer.id.clone()), can_save, p)
                    } else {
                        kit::accent_btn(
                            kit::keyed("save-grant", peer.id.clone()),
                            can_save,
                            true,
                            p,
                        )
                    }
                    .when(saving, |el| {
                        el.child(kit::spinner(
                            kit::keyed("save-spin", peer.id.clone()),
                            13.,
                            p.muted,
                        ))
                    })
                    .child(match (saving, full) {
                        (true, _) => "Saving…",
                        (false, true) => "Grant full access",
                        (false, false) => "Save changes",
                    })
                    .when(can_save, |el| {
                        el.on_click(cx.listener(move |this, _, _, cx| {
                            this.sharing_write(
                                &a,
                                &save,
                                Op::Grant,
                                Write::Grant(grant.clone()),
                                cx,
                            )
                        }))
                    }),
                )
        });

        let error = |key: &(String, Op)| {
            self.sharing.errors.get(key).map(|e| {
                h_flex()
                    .gap_1p5()
                    .items_start()
                    .text_xs()
                    .text_color(p.danger)
                    .child(kit::icon(IconName::CircleAlert, 13., p.danger))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_normal()
                            .child(e.clone()),
                    )
            })
        };
        let grant_error = error(&grant_key);
        let stop_error = error(&stop_key);

        // ---- saved limits that do not apply right now
        let idle_limits = self.idle_limits(agent, peer, access, &grant, p, window, cx);

        v_flex()
            .gap_3()
            .child(header)
            .children(stop_error)
            .child(modes)
            .children(body)
            .children(conflict)
            .children(grant_error)
            .children(footer)
            .children(idle_limits)
    }

    /// Sessions, Files and Shell as one row of pills: each needs the one
    /// before it, so they read as a ladder.
    fn capability_row(
        &self,
        peer: &PeerView,
        (sessions, filesystem, shell): (bool, bool, bool),
        saving: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Div {
        let pill = |this: &Self,
                    name: &'static str,
                    icon: IconName,
                    title: &'static str,
                    on: bool,
                    cx: &mut Context<Self>,
                    set: fn(&mut GrantDraft, bool)| {
            let id = peer.id.clone();
            let _ = this;
            kit::toggle_chip(kit::keyed(name, peer.id.clone()), on, !saving, p)
                .child(kit::icon(icon, 13., if on { p.accent } else { p.muted }))
                .child(title)
                .when(on, |el| el.child(kit::icon(IconName::Check, 12., p.accent)))
                .when(!saving, |el| {
                    el.on_click(cx.listener(move |this, _, _, cx| {
                        this.edit_draft(&id, cx, |d| set(d, !on));
                    }))
                })
        };
        let summary = match (sessions, filesystem, shell) {
            (false, _, _) => {
                "Nothing yet: the peer can only use models you share below.".to_string()
            }
            (true, false, _) => format!(
                "{} can list, open, start and prompt sessions here.",
                peer.name
            ),
            (true, true, false) => format!(
                "{} can run sessions and change files inside their workspace.",
                peer.name
            ),
            (true, true, true) => format!(
                "{} can run sessions, change workspace files, and use the unsandboxed shell.",
                peer.name
            ),
        };
        v_flex()
            .gap_1p5()
            .child(
                h_flex()
                    .gap_1p5()
                    .flex_wrap()
                    .child(pill(
                        self,
                        "share-sessions",
                        IconName::MessagesSquare,
                        "Sessions",
                        sessions,
                        cx,
                        GrantDraft::set_sessions,
                    ))
                    .child(pill(
                        self,
                        "share-files",
                        IconName::Folder,
                        "Files",
                        filesystem,
                        cx,
                        GrantDraft::set_filesystem,
                    ))
                    .child(pill(
                        self,
                        "share-shell",
                        IconName::SquareTerminal,
                        "Shell",
                        shell,
                        cx,
                        GrantDraft::set_shell,
                    )),
            )
            .child(subtle_text(p).whitespace_normal().child(summary))
    }

    /// "Add provider": a menu of the catalog providers not shared yet.
    fn add_provider_menu(
        &self,
        peer: &PeerView,
        available: &[&ShareableProvider],
        order: &[String],
        saving: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
        let enabled = !saving && !available.is_empty();
        let trigger = kit::flat_menu_btn(
            kit::keyed("add-provider", peer.id.clone()),
            Some(IconName::Plus),
            "Add provider",
            p,
            cx,
        );
        if !enabled {
            return trigger
                .inactive(true)
                .when(available.is_empty() && !saving, |b| {
                    b.tooltip("Every provider on this machine is shared already.")
                })
                .into_any_element();
        }
        let app = cx.entity().downgrade();
        let peer_id = peer.id.clone();
        let order = order.to_vec();
        let items: Vec<(String, usize)> = available
            .iter()
            .map(|c| (c.provider_id.clone(), c.models.len()))
            .collect();
        trigger
            .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, _, _| {
                for (pid, count) in &items {
                    let (app, peer_id, order, id) =
                        (app.clone(), peer_id.clone(), order.clone(), pid.clone());
                    menu = menu.item(
                        PopupMenuItem::new(format!(
                            "{pid}  ·  {count} model{}",
                            if *count == 1 { "" } else { "s" }
                        ))
                        .on_click(move |_, _, cx| {
                            let _ = app.update(cx, |this, cx| {
                                this.edit_draft(&peer_id, cx, |d| {
                                    d.set_provider(&id, true, &order)
                                });
                            });
                        }),
                    );
                }
                menu
            })
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn custom_editor(
        &mut self,
        agent: &str,
        peer: &PeerView,
        access: &PeerAccess,
        grant: &PeerGrant,
        capabilities: (bool, bool, bool),
        saving: bool,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let catalog: Vec<ShareableProvider> = access.shareable_catalog.clone().unwrap_or_default();
        let order: Vec<String> = catalog.iter().map(|c| c.provider_id.clone()).collect();
        let caps = self.capability_row(peer, capabilities, saving, p, cx);

        let shared_ids: Vec<String> = match grant {
            PeerGrant::Custom { providers, .. } => {
                providers.iter().map(|pg| pg.provider_id.clone()).collect()
            }
            _ => Vec::new(),
        };
        // Providers the grant names that this machine no longer offers.
        let missing: Vec<String> = shared_ids
            .iter()
            .filter(|id| !order.contains(id))
            .cloned()
            .collect();
        let available: Vec<&ShareableProvider> = catalog
            .iter()
            .filter(|c| !shared_ids.contains(&c.provider_id))
            .collect();
        let shared: Vec<&ShareableProvider> = catalog
            .iter()
            .filter(|c| shared_ids.contains(&c.provider_id))
            .collect();
        let header = h_flex()
            .gap_3()
            .items_start()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().font_medium().child("Models"))
                    .child(subtle_text(p).child(format!(
                        "Providers set up on this machine that {} may use.",
                        peer.name
                    ))),
            )
            .when(!catalog.is_empty(), |el| {
                el.child(self.add_provider_menu(peer, &available, &order, saving, p, cx))
            });
        let list: AnyElement = if catalog.is_empty() {
            subtle_text(p)
                .px_3()
                .py_2()
                .rounded(px(8.))
                .bg(p.chip)
                .child("This machine has no models to share yet. Set up a provider in Settings → Providers.")
                .into_any_element()
        } else if shared.is_empty() && missing.is_empty() {
            subtle_text(p)
                .px_3()
                .py_2()
                .rounded(px(8.))
                .bg(p.chip)
                .child(format!(
                    "No providers shared yet: {} cannot use any model here. Add one above.",
                    peer.name
                ))
                .into_any_element()
        } else {
            let mut rows: Vec<AnyElement> = Vec::new();
            for provider in &shared {
                rows.push(
                    self.provider_row(
                        agent, peer, access, grant, provider, &order, saving, p, window, cx,
                    )
                    .into_any_element(),
                );
            }
            for id in missing {
                let (pid, order) = (peer.id.clone(), order.clone());
                rows.push(
                    h_flex()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .rounded(px(10.))
                        .border_1()
                        .border_color(p.border)
                        .bg(p.card)
                        .text_xs()
                        .text_color(p.warn)
                        .child(kit::icon(IconName::TriangleAlert, 12., p.warn))
                        .child(div().flex_1().child(format!(
                            "{id} is shared but not available on this machine right now."
                        )))
                        .child(
                            kit::flat_btn(
                                kit::keyed("drop-provider", format!("{pid}-{id}")),
                                true,
                                true,
                                p,
                            )
                            .child("Stop sharing it")
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.edit_draft(&pid, cx, |d| {
                                        d.set_provider(&id, false, &order)
                                    });
                                },
                            )),
                        )
                        .into_any_element(),
                );
            }
            // One block per provider: easier to tell apart than rows of a
            // single card once several carry model limits.
            v_flex().gap_2().children(rows).into_any_element()
        };
        v_flex()
            .gap_4()
            .child(caps)
            .child(v_flex().gap_2().child(header).child(list))
            .into_any_element()
    }

    /// One shared provider: its name, which models, its limit, and the
    /// limits of single models underneath.
    #[allow(clippy::too_many_arguments)]
    fn provider_row(
        &mut self,
        agent: &str,
        peer: &PeerView,
        access: &PeerAccess,
        grant: &PeerGrant,
        provider: &ShareableProvider,
        order: &[String],
        saving: bool,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
        let (divider, _) = kit::subtle(p);
        let pid = provider.provider_id.clone();
        let selection = shared(grant, &pid).cloned().unwrap_or(ModelSelection::All);
        let count = provider.models.len();
        let all = matches!(selection, ModelSelection::All);
        let picked: Vec<String> = match &selection {
            ModelSelection::All => provider.models.iter().map(|m| m.model_id.clone()).collect(),
            ModelSelection::Selected { model_ids } => model_ids.clone(),
        };
        let models_label = match &selection {
            ModelSelection::All => "All models".to_string(),
            ModelSelection::Selected { model_ids } => {
                format!("{} of {count} models", model_ids.len())
            }
        };
        let row_key = format!("{}/{pid}", peer.id);

        // Header: who, which models, adding a model limit, and stopping
        // the share, kept apart from the limit lines' own actions.
        let (m_peer, m_pid) = (peer.id.clone(), pid.clone());
        let models_btn = kit::flat_btn(
            kit::keyed("provider-models", row_key.clone()),
            !saving,
            true,
            p,
        )
        .child(
            div()
                .text_color(if all { p.text } else { p.accent })
                .child(models_label),
        )
        .child(kit::icon(IconName::ChevronDown, 12., p.dim))
        .when(!saving, |el| {
            el.on_click(cx.listener(move |this, _, window, cx| {
                this.open_models_modal(&m_peer, &m_pid, window, cx);
            }))
        });
        // Models of this provider that have a limit of their own.
        let limited = |model: &str| {
            access
                .outgoing
                .quota(&QuotaScope::model(&pid, model))
                .is_some_and(|q| q.limit != QuotaLimit::Unlimited)
        };
        // Add a limit for any other model the peer may use.
        let limitable: Vec<String> = provider
            .models
            .iter()
            .filter(|m| {
                m.finite_quota_supported && picked.contains(&m.model_id) && !limited(&m.model_id)
            })
            .map(|m| m.model_id.clone())
            .collect();
        let add_limit = (!limitable.is_empty() && !saving).then(|| {
            let app = cx.entity().downgrade();
            let (peer_id, pid) = (peer.id.clone(), pid.clone());
            kit::flat_menu_btn(
                kit::keyed("limit-model", row_key.clone()),
                Some(IconName::Plus),
                "Limit a model",
                p,
                cx,
            )
            .dropdown_menu_with_anchor(Anchor::TopLeft, move |mut menu, _, _| {
                for model in &limitable {
                    let (app, peer_id, scope) =
                        (app.clone(), peer_id.clone(), QuotaScope::model(&pid, model));
                    menu = menu.item(PopupMenuItem::new(model.clone()).on_click(
                        move |_, window, cx| {
                            let _ = app.update(cx, |this, cx| {
                                this.open_quota_editor(&peer_id, scope.clone(), None, window, cx)
                            });
                        },
                    ));
                }
                menu
            })
        });
        let (r_peer, r_pid, r_order) = (peer.id.clone(), pid.clone(), order.to_vec());
        let stop = kit::row_icon_button(
            kit::keyed("provider-remove", row_key.clone()),
            IconName::Trash,
            format!("Stop sharing {pid} with {}", peer.name),
            p,
            cx,
        )
        .label("Stop sharing")
        .px_2()
        .disabled(saving)
        .on_click(cx.listener(move |this, _, _, cx| {
            this.edit_draft(&r_peer, cx, |d| d.set_provider(&r_pid, false, &r_order));
        }));
        let header = h_flex()
            .gap_2()
            .px_3()
            .py_1p5()
            .bg(p.card_hover)
            .border_b_1()
            .border_color(divider)
            .child(kit::provider_icon(&pid, 16., p.text))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .font_semibold()
                    .text_color(p.text)
                    .child(pid.clone()),
            )
            .child(models_btn)
            .children(add_limit)
            .child(div().flex_1())
            .child(stop);

        // Body: one line per limit, all the same shape (scope, usage,
        // actions). The provider's own limit comes first.
        let scope_label = |text: String, mono: bool, color: Hsla| {
            div()
                .w(px(SCOPE_W))
                .flex_none()
                .min_w_0()
                .truncate()
                .text_xs()
                .text_color(color)
                .map(|el| {
                    if mono {
                        el.font_family(kit::MONO)
                    } else {
                        el.font_medium()
                    }
                })
                .child(text)
        };
        let mut lines: Vec<AnyElement> = vec![
            self.quota_line(
                agent,
                peer,
                &access.outgoing,
                QuotaScope::provider(&pid),
                true,
                scope_label(
                    if all {
                        "All models"
                    } else {
                        "All shared models"
                    }
                    .to_string(),
                    false,
                    p.text,
                ),
                p,
                window,
                cx,
            ),
        ];

        // Single-model limits: only the models that have one (or whose
        // limit is being set now).
        let editing_model = self.sharing.edit.as_ref().and_then(|e| {
            (e.peer == peer.id && e.scope.provider_id == pid)
                .then(|| e.scope.model_id.clone())
                .flatten()
        });
        for m in &provider.models {
            if !limited(&m.model_id) && editing_model.as_deref() != Some(m.model_id.as_str()) {
                continue;
            }
            let color = if picked.contains(&m.model_id) {
                p.text
            } else {
                p.muted
            };
            lines.push(self.quota_line(
                agent,
                peer,
                &access.outgoing,
                QuotaScope::model(&pid, &m.model_id),
                m.finite_quota_supported,
                scope_label(m.model_id.clone(), true, color),
                p,
                window,
                cx,
            ));
        }

        // Chosen models this machine no longer lists.
        let gone: Vec<String> = match &selection {
            ModelSelection::Selected { model_ids } => model_ids
                .iter()
                .filter(|s| !provider.models.iter().any(|m| &m.model_id == *s))
                .cloned()
                .collect(),
            ModelSelection::All => Vec::new(),
        };
        for g in gone {
            let (t_peer, t_pid, t_model) = (peer.id.clone(), pid.clone(), g.clone());
            lines.push(
                h_flex()
                    .id(kit::keyed("model-gone", format!("{row_key}/{g}")))
                    .min_h(px(28.))
                    .gap_2()
                    .text_xs()
                    .cursor_pointer()
                    .child(kit::icon(IconName::TriangleAlert, 12., p.warn))
                    .child(div().font_family(kit::MONO).child(g))
                    .child(
                        div()
                            .text_color(p.warn)
                            .child("not available here now · click to drop"),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.edit_draft(&t_peer, cx, |d| d.toggle_model(&t_pid, &t_model));
                    }))
                    .into_any_element(),
            );
        }

        let body = v_flex().children(lines.into_iter().enumerate().map(|(ix, line)| {
            div()
                .px_3()
                .py_2()
                .when(ix > 0, |el| el.border_t_1().border_color(divider))
                .child(line)
        }));
        v_flex()
            .rounded(px(10.))
            .overflow_hidden()
            .border_1()
            .border_color(p.border)
            .bg(p.card)
            .child(header)
            .child(body)
    }

    fn open_models_modal(
        &mut self,
        peer: &str,
        provider: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sharing.models_modal = Some((peer.to_string(), provider.to_string()));
        self.sharing.models_query.update(cx, |s, cx| {
            s.set_value(SharedString::default(), window, cx);
            s.focus(window, cx);
        });
        cx.notify();
    }

    /// The model picker of one shared provider: search, "All models", and
    /// one checkbox row per model. Changes go into the peer's draft.
    pub(super) fn render_share_models_modal(
        &mut self,
        radii: Corners<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (peer_id, pid) = self.sharing.models_modal.clone()?;
        let p = *palette(cx);
        let store = self.store.read(cx);
        let agent = store.primary_agent()?;
        let access = store.peer_sharing(&agent, &peer_id)?.access.clone()?;
        let peer_name = store
            .runtime(&agent)
            .map(|rt| rt.peer_name(&peer_id))
            .unwrap_or_else(|| short_peer(&peer_id));
        let provider = access
            .shareable_catalog
            .as_ref()
            .and_then(|c| c.iter().find(|c| c.provider_id == pid))
            .cloned();
        let grant = self
            .sharing
            .drafts
            .get(&peer_id)
            .map(|d| d.grant.clone())
            .unwrap_or_else(|| access.outgoing.grant.clone());
        let selection = shared(&grant, &pid).cloned();
        let all = matches!(selection, Some(ModelSelection::All));
        let picked: Vec<String> = match &selection {
            Some(ModelSelection::Selected { model_ids }) => model_ids.clone(),
            _ => Vec::new(),
        };
        let models: Vec<(String, bool)> = provider
            .as_ref()
            .map(|c| {
                c.models
                    .iter()
                    .map(|m| (m.model_id.clone(), m.finite_quota_supported))
                    .collect()
            })
            .unwrap_or_default();
        let count = models.len();
        let query = self
            .sharing
            .models_query
            .read(cx)
            .value()
            .trim()
            .to_lowercase();
        let shown: Vec<(String, bool)> = models
            .into_iter()
            .filter(|(m, _)| query.is_empty() || m.to_lowercase().contains(&query))
            .collect();

        let close = |this: &mut Self, cx: &mut Context<Self>| {
            this.sharing.models_modal = None;
            cx.notify();
        };
        let (a_peer, a_pid) = (peer_id.clone(), pid.clone());
        let all_row = h_flex()
            .id("share-models-all")
            .gap_3()
            .px_3()
            .py_2()
            .rounded(px(8.))
            .bg(p.card)
            .cursor_pointer()
            .child(
                v_flex()
                    .flex_1()
                    .child(div().text_sm().font_medium().child("All models"))
                    .child(subtle_text(&p).child(format!(
                        "Every one of the {count}, and any {pid} adds later."
                    ))),
            )
            .child(
                kit::switch("share-models-all-switch")
                    .checked(all)
                    .small()
                    .on_click(cx.listener({
                        let (peer_id, pid) = (peer_id.clone(), pid.clone());
                        move |this, v: &bool, _, cx| {
                            let v = *v;
                            this.edit_draft(&peer_id, cx, |d| d.set_all_models(&pid, v, &[]));
                        }
                    })),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.edit_draft(&a_peer, cx, |d| d.set_all_models(&a_pid, !all, &[]));
            }));
        let hover = p.card;
        let rows: Vec<AnyElement> = shown
            .iter()
            .map(|(m, _)| {
                let on = all || picked.contains(m);
                let (t_peer, t_pid, t_model) = (peer_id.clone(), pid.clone(), m.clone());
                h_flex()
                    .id(kit::keyed("share-model", format!("{peer_id}/{pid}/{m}")))
                    .gap_2p5()
                    .px_3()
                    .py_1p5()
                    .rounded(px(8.))
                    .when(!all, |el| {
                        el.cursor_pointer()
                            .hover(move |s| s.bg(hover))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.edit_draft(&t_peer, cx, |d| d.toggle_model(&t_pid, &t_model));
                            }))
                    })
                    .child(kit::icon(
                        if on {
                            IconName::SquareCheckBig
                        } else {
                            IconName::Square
                        },
                        15.,
                        if all {
                            p.dim
                        } else if on {
                            p.accent
                        } else {
                            p.muted
                        },
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(kit::MONO)
                            .text_sm()
                            .text_color(if all {
                                p.muted
                            } else if on {
                                p.text
                            } else {
                                p.muted
                            })
                            .child(m.clone()),
                    )
                    .into_any_element()
            })
            .collect();
        let footer_text = if all {
            format!("All {count} models")
        } else {
            format!("{} of {count} models", picked.len())
        };
        let search = h_flex()
            .pl_2p5()
            .pr_1()
            .h(px(34.))
            .rounded(px(10.))
            .bg(p.card)
            .border_1()
            .border_color(p.border)
            .child(kit::icon(IconName::Search, 15., p.dim))
            .child(
                Input::new(&self.sharing.models_query)
                    .appearance(false)
                    .flex_1(),
            );
        let card = v_flex()
            .id("share-models-card")
            .w(px(520.))
            .max_h(px(600.))
            .rounded(px(16.))
            .bg(p.surface)
            .border_1()
            .border_color(p.border)
            .shadow_lg()
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                h_flex()
                    .px_4()
                    .pt_4()
                    .pb_3()
                    .gap_3()
                    .child(
                        v_flex()
                            .flex_1()
                            .child(div().font_semibold().child(format!("{pid} models for {peer_name}")))
                            .child(div().text_xs().text_color(p.dim).child(
                                "Which models the peer may use. Limits are set on the provider row.",
                            )),
                    )
                    .child(
                        kit::icon_button("share-models-close", IconName::X, "Close")
                            .on_click(cx.listener(move |this, _, _, cx| close(this, cx))),
                    ),
            )
            .child(v_flex().px_4().gap_2().child(all_row).child(search))
            .child(
                v_flex()
                    .id("share-models-list")
                    .flex_1()
                    .min_h(px(220.))
                    .overflow_y_scroll()
                    .px_2()
                    .py_2()
                    .children(rows)
                    .when(shown.is_empty(), |el| {
                        el.child(
                            div()
                                .p_6()
                                .text_sm()
                                .text_color(p.dim)
                                .text_center()
                                .child(if count == 0 {
                                    "This provider lists no models right now."
                                } else {
                                    "No models match your search."
                                }),
                        )
                    })
                    .map(|el| self.smooth_scroll.region("share-models-list", el)),
            )
            .child(
                h_flex()
                    .px_4()
                    .py_3()
                    .gap_3()
                    .border_t_1()
                    .border_color(p.border)
                    .child(subtle_text(&p).flex_1().child(footer_text))
                    .child(
                        kit::flat_btn("share-models-done", true, true, &p)
                            .child("Done")
                            .on_click(cx.listener(move |this, _, _, cx| close(this, cx))),
                    ),
            );
        Some(
            div()
                .id("scrim-share-models")
                .absolute()
                .inset_0()
                .rounded_tl(radii.top_left)
                .rounded_tr(radii.top_right)
                .rounded_bl(radii.bottom_left)
                .rounded_br(radii.bottom_right)
                .bg(black().opacity(if p.dark { 0.55 } else { 0.28 }))
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .on_click(cx.listener(move |this, _, _, cx| close(this, cx)))
                .child(card)
                .into_any_element(),
        )
    }

    /// One limit: its amount, usage and actions, or its editor.
    #[allow(clippy::too_many_arguments)]
    fn quota_line(
        &mut self,
        agent: &str,
        peer: &PeerView,
        outgoing: &Outgoing,
        scope: QuotaScope,
        finite_supported: bool,
        label: impl IntoElement,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let quota = outgoing.quota(&scope).cloned();
        let set_key = (peer.id.clone(), Op::Quota(scope.clone()));
        let reset_key = (peer.id.clone(), Op::Reset(scope.clone()));
        let busy = self.sharing.busy.contains(&set_key) || self.sharing.busy.contains(&reset_key);
        let error = self
            .sharing
            .errors
            .get(&set_key)
            .or_else(|| self.sharing.errors.get(&reset_key))
            .cloned();
        let editing = self
            .sharing
            .edit
            .as_ref()
            .is_some_and(|e| e.peer == peer.id && e.scope == scope);
        let finite = quota
            .as_ref()
            .is_some_and(|q| matches!(q.limit, QuotaLimit::Finite { .. }));
        // Busy is not a failure: a calm note, not red.
        let waiting = self.sharing.busy_refusals.contains_key(&set_key)
            || self.sharing.busy_refusals.contains_key(&reset_key);
        let tone = if waiting { p.warn } else { p.danger };
        let error_el = error.map(|e| {
            h_flex()
                .gap_1p5()
                .items_start()
                .text_xs()
                .text_color(tone)
                .child(kit::icon(
                    if waiting {
                        IconName::Hourglass
                    } else {
                        IconName::CircleAlert
                    },
                    12.,
                    tone,
                ))
                .child(div().flex_1().min_w_0().whitespace_normal().child(e))
        });
        if editing {
            // Keep the scope label so the editor stays in the value column.
            return h_flex()
                .items_start()
                .gap_2()
                .child(label)
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(self.quota_editor(agent, peer, &scope, busy, p, window, cx))
                        .children(error_el),
                )
                .into_any_element();
        }
        let key = scope.key();
        let line = h_flex()
            .gap_2()
            .min_h(px(28.))
            .child(label)
            .child(match &quota {
                Some(q) if finite => {
                    let fraction = access::usage_fraction(q).unwrap_or(0.);
                    let bar_color = if q.exhausted() { p.warn } else { p.accent };
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .text_xs()
                                        .font_semibold()
                                        .child(q.limit.label())
                                        .id(kit::keyed("limit-exact", format!("{}-{key}", peer.id)))
                                        .tooltip({
                                            let exact = match q.limit {
                                                QuotaLimit::Finite { limit_tokens, .. } => {
                                                    access::exact_tokens(limit_tokens)
                                                }
                                                QuotaLimit::Unlimited => String::new(),
                                            };
                                            move |window, cx| {
                                                kit::wrap_tip(exact.clone()).build(window, cx)
                                            }
                                        }),
                                )
                                .when(q.exhausted(), |el| {
                                    el.child(kit::tone_chip("used up", p.warn))
                                })
                                .when(!q.active, |el| el.child(kit::chip("not applied", p))),
                        )
                        .child(div().h(px(4.)).w_full().rounded_full().bg(p.chip).child(
                            div().h_full().rounded_full().bg(bar_color).w(relative(
                                fraction.max(if fraction > 0. { 0.02 } else { 0. }),
                            )),
                        ))
                        .child(subtle_text(p).child(access::usage_line(q, now_ms())))
                }
                Some(q) => h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .text_xs()
                    .text_color(p.muted)
                    .child("No limit")
                    .when(q.usage.used() > 0, |el| {
                        el.child("·").child(access::usage_line(q, now_ms()))
                    }),
                None => h_flex()
                    .flex_1()
                    .text_xs()
                    .text_color(p.muted)
                    .child("No limit"),
            })
            .when(busy, |el| {
                el.child(kit::spinner(
                    kit::keyed("quota-spin", format!("{}-{key}", peer.id)),
                    13.,
                    p.muted,
                ))
            })
            .when(!busy && finite_supported, |el| {
                let (peer_id, s, existing) = (
                    peer.id.clone(),
                    scope.clone(),
                    quota.as_ref().map(|q| q.limit.clone()),
                );
                el.child(
                    kit::flat_btn(
                        kit::keyed("quota-edit", format!("{}-{key}", peer.id)),
                        true,
                        true,
                        p,
                    )
                    .child(if finite { "Change" } else { "Set limit" })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_quota_editor(&peer_id, s.clone(), existing.clone(), window, cx)
                    })),
                )
            })
            // Less frequent actions of a limit in one menu; both apply now.
            .when(!busy && finite, |el| {
                use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
                let app = cx.entity().downgrade();
                let (a, peer_id, s) = (agent.to_string(), peer.id.clone(), scope.clone());
                el.child(
                    kit::row_icon_button(
                        kit::keyed("quota-menu", format!("{}-{key}", peer.id)),
                        IconName::Ellipsis,
                        "Limit actions",
                        p,
                        cx,
                    )
                    .dropdown_menu_with_anchor(
                        Anchor::TopRight,
                        move |menu, _, _| {
                            let (app1, a1, p1, s1) =
                                (app.clone(), a.clone(), peer_id.clone(), s.clone());
                            let (app2, a2, p2, s2) =
                                (app.clone(), a.clone(), peer_id.clone(), s.clone());
                            menu.item(
                                PopupMenuItem::new("Reset usage now")
                                    .icon(Icon::new(IconName::RotateCcw))
                                    .on_click(move |_, _, cx| {
                                        let _ = app1.update(cx, |this, cx| {
                                            this.sharing_write(
                                                &a1,
                                                &p1,
                                                Op::Reset(s1.clone()),
                                                Write::Reset(s1.clone()),
                                                cx,
                                            )
                                        });
                                    }),
                            )
                            .separator()
                            .item(
                                PopupMenuItem::new("Remove limit")
                                    .icon(Icon::new(IconName::X))
                                    .on_click(move |_, _, cx| {
                                        let _ = app2.update(cx, |this, cx| {
                                            this.sharing_write(
                                                &a2,
                                                &p2,
                                                Op::Quota(s2.clone()),
                                                Write::Quota {
                                                    scope: s2.clone(),
                                                    operation: access::QuotaOperation::Reconfigure,
                                                    limit: QuotaLimit::Unlimited,
                                                },
                                                cx,
                                            )
                                        });
                                    }),
                            )
                        },
                    ),
                )
            });
        v_flex()
            .gap_1()
            .child(line)
            .children(error_el)
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn quota_editor(
        &self,
        _agent: &str,
        peer: &PeerView,
        scope: &QuotaScope,
        busy: bool,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let Some(edit) = self.sharing.edit.as_ref() else {
            return div();
        };
        let key = format!("{}-{}", peer.id, scope.key());
        let text = self.sharing.amount.read(cx).value().to_string();
        let parsed = access::parse_amount(&text, edit.unit);
        let new_limit = parsed.as_ref().ok().map(|&tokens| QuotaLimit::Finite {
            limit_tokens: tokens,
            window: edit.window,
        });
        let restarts = match (&edit.existing, &new_limit) {
            (Some(QuotaLimit::Finite { .. }), Some(limit)) => {
                access::quota_operation(edit.existing.as_ref(), limit)
                    == access::QuotaOperation::Reconfigure
            }
            _ => false,
        };
        let focused = self
            .sharing
            .amount
            .read(cx)
            .focus_handle(cx)
            .is_focused(window);
        let windows = kit::segment_bar(p).children(QuotaWindow::ALL.into_iter().map(|w| {
            kit::segment(
                kit::keyed("quota-window", format!("{key}-{}", w.label())),
                edit.window == w,
                p,
            )
            .label(w.label())
            .inactive(busy)
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(e) = this.sharing.edit.as_mut() {
                    e.window = w;
                }
                cx.notify();
            }))
        }));
        let hint = match &parsed {
            Ok(tokens) => format!(
                "{} · {}",
                access::exact_tokens(*tokens),
                edit.window.describe()
            ),
            Err(_) if text.trim().is_empty() => edit.window.describe().to_string(),
            Err(err) => err.clone(),
        };
        let units = kit::segment_bar(p).children(TokenUnit::ALL.into_iter().map(|u| {
            kit::segment(
                kit::keyed("quota-unit", format!("{key}-{}", u.label())),
                edit.unit == u,
                p,
            )
            .label(u.label())
            .inactive(busy)
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(e) = this.sharing.edit.as_mut() {
                    e.unit = u;
                }
                cx.notify();
            }))
        }));
        v_flex()
            .gap_2()
            .px_3()
            .py_2p5()
            .rounded(px(8.))
            .bg(p.chip)
            .child(
                div()
                    .text_xs()
                    .font_semibold()
                    .child(format!("Limit for {}", scope.label())),
            )
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        kit::field_shell(focused, p).w(px(120.)).h(px(32.)).child(
                            Input::new(&self.sharing.amount)
                                .appearance(false)
                                .aria_label("Limit amount")
                                .disabled(busy)
                                .flex_1(),
                        ),
                    )
                    .child(units)
                    .child(windows)
                    .child(div().flex_1())
                    .child(
                        kit::flat_btn(kit::keyed("quota-cancel", key.clone()), !busy, true, p)
                            .child("Cancel")
                            .when(!busy, |el| {
                                el.on_click(cx.listener(|this, _, _, cx| {
                                    this.sharing.edit = None;
                                    cx.notify();
                                }))
                            }),
                    )
                    .child(
                        kit::soft_accent_btn(kit::keyed("quota-save", key.clone()), !busy && parsed.is_ok(), p)
                            .child(if busy { "Saving…" } else { "Save limit" })
                            .when(!busy && parsed.is_ok(), |el| {
                                el.on_click(cx.listener(|this, _, _, cx| this.save_quota(cx)))
                            }),
                    ),
            )
            .child(
                subtle_text(p)
                    .text_color(if parsed.is_err() && !text.trim().is_empty() {
                        p.warn
                    } else {
                        p.muted
                    })
                    .child(hint),
            )
            .when(restarts, |el| {
                el.child(subtle_text(p).child(
                    "Changing how it resets starts a new period now; usage so far stays in the old one.",
                ))
            })
    }

    /// Saved limits a Custom grant does not apply (the provider or model is
    /// not shared). Hidden under No access and Full access, where limits do
    /// not apply at all and the list would only be noise; they come back
    /// with Custom.
    #[allow(clippy::too_many_arguments)]
    fn idle_limits(
        &mut self,
        agent: &str,
        peer: &PeerView,
        access: &PeerAccess,
        grant: &PeerGrant,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !matches!(grant, PeerGrant::Custom { .. }) {
            return None;
        }
        let shown = |q: &PeerQuota| match &q.scope.model_id {
            None => shared(grant, &q.scope.provider_id).is_some(),
            Some(model) => match shared(grant, &q.scope.provider_id) {
                Some(ModelSelection::All) => true,
                Some(ModelSelection::Selected { model_ids }) => model_ids.contains(model),
                None => false,
            },
        };
        let idle: Vec<PeerQuota> = access
            .outgoing
            .quotas
            .iter()
            .filter(|q| q.limit != QuotaLimit::Unlimited && !shown(q))
            .cloned()
            .collect();
        if idle.is_empty() {
            return None;
        }
        let why = "They apply once you share these providers or models.";
        let mut rows = Vec::new();
        for q in idle {
            let label = div()
                .w(px(SCOPE_W))
                .flex_none()
                .truncate()
                .text_xs()
                .text_color(p.muted)
                .child(q.scope.label());
            rows.push(self.quota_line(
                agent,
                peer,
                &access.outgoing,
                q.scope,
                true,
                label,
                p,
                window,
                cx,
            ));
        }
        Some(
            v_flex()
                .gap_2()
                .child(
                    v_flex()
                        .child(div().text_sm().font_medium().child("Saved limits"))
                        .child(subtle_text(p).child(why)),
                )
                .children(rows)
                .into_any_element(),
        )
    }
}

/// The peer's side: what it lets this machine use. Read-only.
fn incoming_view(peer: &PeerView, access: &PeerAccess, p: &Palette) -> Div {
    let name = peer.name.clone();
    let title = div()
        .text_sm()
        .font_semibold()
        .child(format!("What {name} shares with you"));
    let body = match &access.incoming {
        Some(Incoming::Granted { effective, quotas }) => {
            let mut can: Vec<&str> = Vec::new();
            if effective.sessions {
                can.push("Sessions");
            }
            if effective.filesystem {
                can.push("Files");
            }
            if effective.shell {
                can.push("Shell");
            }
            let lead = match effective.mode {
                access::AccessMode::Full => "Full access.".to_string(),
                access::AccessMode::Custom if can.is_empty() && effective.providers.is_empty() => {
                    "Nothing yet.".to_string()
                }
                access::AccessMode::Custom => String::new(),
            };
            v_flex()
                .gap_1p5()
                .when(!lead.is_empty(), |el| el.child(subtle_text(p).child(lead)))
                .when(!can.is_empty(), |el| {
                    el.child(
                        h_flex()
                            .gap_1()
                            .children(can.into_iter().map(|c| kit::chip(c, p))),
                    )
                })
                .children(effective.providers.iter().map(|pg| {
                    let limits: Vec<&PeerQuota> = quotas
                        .iter()
                        .filter(|q| q.scope.provider_id == pg.provider_id)
                        .collect();
                    v_flex()
                        .gap_0p5()
                        .child(
                            h_flex()
                                .gap_2()
                                .text_xs()
                                .child(div().font_semibold().child(pg.provider_id.clone()))
                                .child(div().text_color(p.muted).child(match &pg.models {
                                    ModelSelection::All => "all models".to_string(),
                                    ModelSelection::Selected { model_ids } => model_ids.join(", "),
                                })),
                        )
                        .children(
                            limits
                                .into_iter()
                                .filter(|q| q.limit != QuotaLimit::Unlimited)
                                .map(|q| {
                                    subtle_text(p).child(format!(
                                        "{}: {} · {}",
                                        q.scope
                                            .model_id
                                            .clone()
                                            .unwrap_or_else(|| "All models".into()),
                                        q.limit.label(),
                                        access::usage_line(q, now_ms())
                                    ))
                                }),
                        )
                }))
        }
        Some(Incoming::Denied) => v_flex().child(subtle_text(p).child(format!(
            "Nothing. {name} has not shared anything with this machine."
        ))),
        Some(Incoming::Unavailable) | None => {
            v_flex().child(subtle_text(p).child(if peer.online {
                format!("Could not ask {name} right now.")
            } else {
                format!("Unknown while {name} is offline.")
            }))
        }
        Some(Incoming::Incompatible) => v_flex().child(subtle_text(p).child(format!(
            "{name} runs an older QueryMT that cannot report what it shares."
        ))),
        Some(Incoming::Legacy) => v_flex().child(subtle_text(p).child(format!(
            "{name} uses legacy sharing: it shares without per-peer settings."
        ))),
    };
    v_flex().gap_2().child(title).child(body)
}

/// Full, said plainly.
fn full_warning(p: &Palette) -> AnyElement {
    h_flex()
        .gap_1p5()
        .px_3()
        .py_2()
        .rounded(px(10.))
        .bg(p.warn.opacity(0.08))
        .border_1()
        .border_color(p.warn.opacity(0.3))
        .text_sm()
        .child(kit::icon(IconName::ShieldAlert, 15., p.warn))
        .child("Full: everything, including the unsandboxed shell, no limits.")
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::super::{Page, QmtApp};
    use super::Op;
    use crate::store::mesh_access::{GET, PeerGrant, SET_ACCESS, Write};

    struct Harness {
        handle: gpui_kit::AnyWindowHandle,
        app: gpui_kit::Entity<QmtApp>,
        rpc: std::rc::Rc<crate::rpc::RpcClient>,
        requests: async_channel::Receiver<String>,
    }

    impl Harness {
        fn next(&self) -> Value {
            serde_json::from_str(&self.requests.try_recv().expect("a request")).unwrap()
        }

        fn reply(&self, request: &Value, body: Value, cx: &mut gpui_kit::TestAppContext) {
            let mut message = json!({ "id": request["id"] });
            if body.get("code").is_some() {
                message["error"] = body;
            } else {
                message["result"] = body;
            }
            self.rpc.handle(&message.to_string());
            cx.run_until_parked();
        }

        fn render(&self, cx: &mut gpui_kit::TestAppContext) {
            use gpui_kit::AppContext as _;
            use gpui_kit::test::TestWindowExt as _;
            cx.update_window(self.handle, |_, window, cx| window.render_frame(cx))
                .unwrap();
        }
    }

    fn access(revision: &str, grant: Value) -> Value {
        json!({
            "version": 1, "peer_id": "peer-a-id",
            "outgoing": { "revision": revision, "grant": grant, "quotas": [] },
            "incoming": { "status": "unavailable" },
            "shareable_catalog": [{ "provider_id": "codex", "models": [
                { "model_id": "gpt-6.2-sol", "finite_quota_supported": true } ] }]
        })
    }

    /// The Mesh page of an agent with mesh access v1 and one peer, `peer-a`,
    /// whose sharing has loaded (No access, revision `r1`) and is open.
    fn harness(cx: &mut gpui_kit::TestAppContext) -> Harness {
        use gpui_kit::AppContext as _;
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let mut store = crate::store::launch::peer_fixture();
        let (rpc, requests) = store.rpc_for_test("test-agent");
        // A peer with a saved policy, so its row and panel render.
        store.runtime_mut_for_test("test-agent").managed_peers = vec!["peer-a-id".into()];
        store.runtime_mut_for_test("test-agent").capabilities = Some(json!({
            "features": { "mesh": true },
            "mesh_access_version": 1,
            "methods": [GET, SET_ACCESS],
        }));
        let (handle, app) = cx.update(|cx| {
            let store = cx.new(|_| store);
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| QmtApp::new(store, window, cx))
            })
            .unwrap()
        });
        let h = Harness {
            handle,
            app,
            rpc,
            requests,
        };
        cx.update(|cx| {
            h.app.update(cx, |app, cx| {
                app.page = Page::Mesh;
                app.sharing.open = Some("peer-a-id".into());
                app.store.update(cx, |s, cx| {
                    s.load_peer_sharing("test-agent", "peer-a-id", cx)
                });
            })
        });
        cx.run_until_parked();
        let read = h.next();
        assert_eq!(read["method"], "_querymt/mesh/getPeerAccess");
        h.reply(&read, access("r1", json!({ "mode": "none" })), cx);
        h.render(cx);
        h
    }

    #[gpui_kit::test]
    fn full_access_saves_and_clears_the_draft(cx: &mut gpui_kit::TestAppContext) {
        let h = harness(cx);
        cx.update(|cx| {
            h.app.update(cx, |app, cx| {
                app.edit_draft("peer-a-id", cx, |d| d.set_mode("full"));
                assert!(app.sharing.drafts["peer-a-id"].can_save());
            })
        });
        h.render(cx);
        cx.update(|cx| {
            h.app.update(cx, |app, cx| {
                let grant = app.sharing.drafts["peer-a-id"].grant.clone();
                app.sharing_write(
                    "test-agent",
                    "peer-a-id",
                    Op::Grant,
                    Write::Grant(grant),
                    cx,
                );
            })
        });
        cx.run_until_parked();
        let write = h.next();
        assert_eq!(write["method"], "_querymt/mesh/setPeerAccess");
        assert_eq!(write["params"]["expected_revision"], "r1");
        assert_eq!(write["params"]["grant"], json!({ "mode": "full" }));
        h.reply(
            &write,
            json!({ "version": 1, "peer_id": "peer-a-id", "outgoing": {
                "revision": "r2", "grant": { "mode": "full" }, "quotas": [] } }),
            cx,
        );
        h.render(cx);
        cx.update(|cx| {
            let app = h.app.read(cx);
            assert!(app.sharing.drafts.is_empty(), "saved: the draft is gone");
            let saved = app
                .store
                .read(cx)
                .peer_sharing("test-agent", "peer-a-id")
                .and_then(|s| s.access.clone())
                .unwrap();
            assert_eq!(saved.outgoing.grant, PeerGrant::Full);
        });
    }

    #[gpui_kit::test]
    fn a_stale_save_shows_what_changed(cx: &mut gpui_kit::TestAppContext) {
        let h = harness(cx);
        cx.update(|cx| {
            h.app.update(cx, |app, cx| {
                app.edit_draft("peer-a-id", cx, |d| d.set_mode("custom"));
                app.edit_draft("peer-a-id", cx, |d| d.set_sessions(true));
                let grant = app.sharing.drafts["peer-a-id"].grant.clone();
                app.sharing_write(
                    "test-agent",
                    "peer-a-id",
                    Op::Grant,
                    Write::Grant(grant),
                    cx,
                );
            })
        });
        cx.run_until_parked();
        let write = h.next();
        h.reply(
            &write,
            json!({ "code": -32000, "message": "grant revision changed", "data": {
                "code": "mesh_access_conflict", "expected_revision": "r1", "actual_revision": "r9" } }),
            cx,
        );
        // Another client granted Full meanwhile: read back, never retried.
        let read = h.next();
        assert_eq!(read["method"], "_querymt/mesh/getPeerAccess");
        h.reply(&read, access("r9", json!({ "mode": "full" })), cx);
        h.render(cx);
        assert!(h.requests.try_recv().is_err(), "no automatic retry");
        cx.update(|cx| {
            let app = h.app.read(cx);
            let draft = &app.sharing.drafts["peer-a-id"];
            assert_eq!(
                draft.changed_elsewhere,
                vec!["Access changed from No access to Full access."]
            );
            assert!(!draft.can_save(), "the owner decides first");
            let error = &app.sharing.errors[&("peer-a-id".to_string(), Op::Grant)];
            assert!(error.starts_with("Not saved: sharing for this peer changed elsewhere."));
            assert!(error.contains("No access to Full access"));
        });
    }

    #[gpui_kit::test]
    fn providers_are_added_and_models_picked_in_the_modal(cx: &mut gpui_kit::TestAppContext) {
        let h = harness(cx);
        cx.update(|cx| {
            h.app.update(cx, |app, cx| {
                app.edit_draft("peer-a-id", cx, |d| d.set_mode("custom"));
                // Custom starts with no provider: the list shows only what is shared.
                assert!(matches!(
                    &app.sharing.drafts["peer-a-id"].grant,
                    PeerGrant::Custom { providers, .. } if providers.is_empty()
                ));
                app.edit_draft("peer-a-id", cx, |d| {
                    d.set_provider("codex", true, &["codex".to_string()])
                });
            })
        });
        h.render(cx);
        cx.update(|cx| {
            h.app.update(cx, |app, cx| {
                app.sharing.models_modal = Some(("peer-a-id".into(), "codex".into()));
                cx.notify();
            })
        });
        h.render(cx);
        cx.update(|cx| {
            h.app.update(cx, |app, cx| {
                app.edit_draft("peer-a-id", cx, |d| d.set_all_models("codex", false, &[]));
                app.edit_draft("peer-a-id", cx, |d| d.toggle_model("codex", "gpt-6.2-sol"));
                assert!(app.sharing.close_editor(), "Escape closes the picker");
                assert!(app.sharing.models_modal.is_none());
            })
        });
        h.render(cx);
        cx.update(|cx| {
            let app = h.app.read(cx);
            assert_eq!(
                crate::store::mesh_access::grant_summary(&app.sharing.drafts["peer-a-id"].grant),
                "codex (gpt-6.2-sol)"
            );
        });
    }

    #[gpui_kit::test]
    fn the_shell_switch_is_saved_and_read_back(cx: &mut gpui_kit::TestAppContext) {
        let h = harness(cx);
        cx.update(|cx| {
            h.app.update(cx, |app, cx| {
                app.edit_draft("peer-a-id", cx, |d| d.set_mode("custom"));
                app.edit_draft("peer-a-id", cx, |d| d.set_shell(true));
                let grant = app.sharing.drafts["peer-a-id"].grant.clone();
                app.sharing_write(
                    "test-agent",
                    "peer-a-id",
                    Op::Grant,
                    Write::Grant(grant),
                    cx,
                );
            })
        });
        h.render(cx);
        cx.run_until_parked();
        let write = h.next();
        let grant = json!({ "mode": "custom", "sessions": true, "filesystem": true,
                            "shell": true, "providers": [] });
        assert_eq!(write["params"]["grant"], grant);
        h.reply(
            &write,
            json!({ "version": 1, "peer_id": "peer-a-id", "outgoing": {
                "revision": "r2", "grant": grant, "quotas": [] } }),
            cx,
        );
        h.render(cx);
        cx.update(|cx| {
            let app = h.app.read(cx);
            assert!(app.sharing.drafts.is_empty(), "saved: the draft is gone");
            let saved = app
                .store
                .read(cx)
                .peer_sharing("test-agent", "peer-a-id")
                .and_then(|s| s.access.clone())
                .unwrap();
            assert_eq!(
                crate::store::mesh_access::grant_summary(&saved.outgoing.grant),
                "Sessions, files, shell"
            );
        });
    }
}
