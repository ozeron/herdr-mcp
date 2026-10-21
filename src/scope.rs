//! One call's view of Herdr: the client, the caller, and guarded targets.
//!
//! A target request covers send, interrupt, close, mail send and wait's target path.
//! `guard_pane` covers a pane that is already known: wait's turn path.
//! `place`'s existing-pane path builds a scope over the caller it already resolved
//! (`Scope::with_caller`) and makes a target request.

use std::sync::Arc;

use crate::caller::{Ancestry, CallerEnv, Resolution, resolve_caller};
use crate::guard::{self, Resolved, TargetKind};
use crate::herdr::{AgentInfo, Client, HerdrError, SessionSnapshot};
use crate::server::Inner;

/// The scope owns the client it opened, or borrows one a caller already holds.
pub enum ClientRef<'a> {
    Owned(Client),
    Borrowed(&'a Client),
}

impl From<Client> for ClientRef<'_> {
    fn from(client: Client) -> Self {
        Self::Owned(client)
    }
}

impl<'a> From<&'a Client> for ClientRef<'a> {
    fn from(client: &'a Client) -> Self {
        Self::Borrowed(client)
    }
}

impl std::ops::Deref for ClientRef<'_> {
    type Target = Client;

    fn deref(&self) -> &Client {
        match self {
            Self::Owned(client) => client,
            Self::Borrowed(client) => client,
        }
    }
}

pub struct Scope<'a> {
    client: ClientRef<'a>,
    env: CallerEnv,
    ancestry: Arc<dyn Ancestry>,
    resolution: Option<Resolution>,
}

/// What a tool wants of its target, as data instead of code in the tool.
#[derive(Debug, Clone)]
pub struct TargetRequest<'a> {
    target: &'a str,
    accept: &'static [TargetKind],
    refusal_code: &'static str,
    refusal_message: &'a str,
    allow_self: bool,
    provenance: Option<bool>,
    tolerant: bool,
    self_guard_first: bool,
}

impl<'a> TargetRequest<'a> {
    /// `refusal_message` is the error for a target whose kind is not in `accept`.
    pub fn new(target: &'a str, accept: &'static [TargetKind], refusal_message: &'a str) -> Self {
        Self {
            target,
            accept,
            refusal_code: "unsupported_target",
            refusal_message,
            allow_self: false,
            provenance: None,
            tolerant: false,
            self_guard_first: false,
        }
    }

    pub fn refusal_code(mut self, code: &'static str) -> Self {
        self.refusal_code = code;
        self
    }

    pub fn allow_self(mut self, allow: bool) -> Self {
        self.allow_self = allow;
        self
    }

    /// Refuse panes no bridge launched, unless `force`.
    pub fn provenance(mut self, force: bool) -> Self {
        self.provenance = Some(force);
        self
    }

    /// An unknown name that does not look like a Herdr ID resolves to no target.
    pub fn tolerant(mut self) -> Self {
        self.tolerant = true;
        self
    }

    /// Check the self-guard before the kind. The default checks the kind first.
    pub fn self_guard_first(mut self) -> Self {
        self.self_guard_first = true;
        self
    }
}

/// A guarded target, with the caller and the snapshot it was resolved in.
#[derive(Debug)]
pub struct Scoped {
    pub resolution: Resolution,
    pub snapshot: SessionSnapshot,
    /// `None` only for a tolerant request whose name matched nothing.
    pub target: Option<Resolved>,
}

impl Scoped {
    /// The target of a request that is not tolerant, which always resolves one.
    pub fn required(&self) -> &Resolved {
        self.target.as_ref().expect("target is required")
    }

    /// The agent in the target pane, when the target is a pane with one.
    pub fn agent(&self) -> Option<&AgentInfo> {
        let target = self.target.as_ref()?;
        if target.kind != TargetKind::Pane {
            return None;
        }
        self.snapshot
            .agents
            .iter()
            .find(|agent| agent.pane_id == target.id)
    }
}

#[derive(Debug)]
pub struct PaneGuard {
    pub resolution: Resolution,
    /// Taken only when a target name had to be checked.
    pub snapshot: Option<SessionSnapshot>,
}

impl<'a> Scope<'a> {
    pub fn open(inner: &Inner) -> Result<Self, HerdrError> {
        Ok(Self {
            client: inner.client()?.into(),
            env: inner.caller_env.clone(),
            ancestry: Arc::clone(&inner.ancestry),
            resolution: None,
        })
    }

    /// A scope whose caller is already resolved, so a target request takes only a snapshot.
    pub fn with_caller(client: impl Into<ClientRef<'a>>, resolution: Resolution) -> Self {
        Self {
            client: client.into(),
            env: CallerEnv::default(),
            ancestry: Arc::new(crate::caller::StaticAncestry(Vec::new())),
            resolution: Some(resolution),
        }
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub async fn caller(&mut self) -> Result<&Resolution, HerdrError> {
        if self.resolution.is_none() {
            let resolution =
                resolve_caller(&self.client, &self.env, self.ancestry.as_ref()).await?;
            self.resolution = Some(resolution);
        }
        Ok(self.resolution.as_ref().expect("resolved above"))
    }

    /// The snapshot and the caller, fetched together (the first error wins).
    pub async fn snapshot_and_caller(
        &mut self,
    ) -> Result<(SessionSnapshot, Resolution), HerdrError> {
        match self.resolution.clone() {
            Some(resolution) => Ok((self.client.session_snapshot().await?, resolution)),
            None => {
                let (snapshot, resolution) = tokio::try_join!(
                    self.client.session_snapshot(),
                    resolve_caller(&self.client, &self.env, self.ancestry.as_ref()),
                )?;
                self.resolution = Some(resolution.clone());
                Ok((snapshot, resolution))
            }
        }
    }

    /// Fetches the snapshot and the caller together (the first error wins), then resolves and
    /// guards the target.
    pub async fn target(&mut self, request: TargetRequest<'_>) -> Result<Scoped, HerdrError> {
        let (snapshot, resolution) = self.snapshot_and_caller().await?;

        let target = match guard::resolve_target(&self.client, &snapshot, request.target).await {
            Ok(target) => Some(target),
            Err(err) if request.tolerant && tolerated(&err, request.target) => None,
            Err(err) => return Err(err),
        };
        if let Some(target) = &target {
            let guard_pane = resolution.guard_target.as_deref();
            let kind = || {
                if request.accept.contains(&target.kind) {
                    Ok(())
                } else {
                    Err(HerdrError::tool(
                        request.refusal_code,
                        request.refusal_message,
                    ))
                }
            };
            let own = || guard::self_guard(guard_pane, target, request.allow_self);
            if request.self_guard_first {
                own()?;
                kind()?;
            } else {
                kind()?;
                own()?;
            }
            if let Some(force) = request.provenance {
                guard::provenance_guard(&snapshot, target, force)?;
            }
        }
        Ok(Scoped {
            resolution,
            snapshot,
            target,
        })
    }

    /// Guards a pane the call already knows, such as the one a turn token names. With `named`,
    /// the caller and snapshot run at once, a caller error is reported first, and `named` must
    /// resolve to `pane`. Without it, only the caller is resolved.
    pub async fn guard_pane(
        &mut self,
        pane: &str,
        allow_self: bool,
        named: Option<&str>,
    ) -> Result<PaneGuard, HerdrError> {
        let mut snapshot = None;
        if let Some(named) = named {
            let (resolution, taken) = match self.resolution.clone() {
                Some(resolution) => (Ok(resolution), self.client.session_snapshot().await),
                None => {
                    tokio::join!(
                        resolve_caller(&self.client, &self.env, self.ancestry.as_ref()),
                        self.client.session_snapshot(),
                    )
                }
            };
            let (resolution, taken) = (resolution?, taken?);
            self.resolution = Some(resolution);
            let resolved = guard::resolve_target(&self.client, &taken, named).await?;
            if resolved.id != pane {
                return Err(HerdrError::invalid_params(
                    "target is not the pane this turn token belongs to",
                ));
            }
            snapshot = Some(taken);
        }
        let resolution = self.caller().await?.clone();
        let own = Resolved {
            kind: TargetKind::Pane,
            id: pane.into(),
            pane_ids: vec![pane.into()],
        };
        guard::self_guard(resolution.guard_target.as_deref(), &own, allow_self)?;
        Ok(PaneGuard {
            resolution,
            snapshot,
        })
    }
}

fn tolerated(err: &HerdrError, name: &str) -> bool {
    err.code() == "target_not_found" && !looks_like_herdr_id(name)
}

/// Whether `raw` has the shape of a Herdr pane, tab or workspace ID (`w1:p2`, `w1:t2`, `w1`), so
/// a miss is a wrong ID, not an agent name that is not running.
fn looks_like_herdr_id(raw: &str) -> bool {
    let digits = |text: &str| !text.is_empty() && text.chars().all(|c| c.is_ascii_digit());
    match raw.split_once(':') {
        Some((scope, rest)) => {
            !scope.is_empty()
                && !scope.contains(char::is_whitespace)
                && rest
                    .strip_prefix('p')
                    .or_else(|| rest.strip_prefix('t'))
                    .is_some_and(digits)
        }
        None => raw.strip_prefix('w').is_some_and(digits),
    }
}
