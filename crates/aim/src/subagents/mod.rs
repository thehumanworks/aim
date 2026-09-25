//! Native child sessions, admitted under the parent's tool ceiling and turn budget (ADR 0070).

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use aim_kernel::subagents::{DEFAULT_CHILD_TURN_CAP, admit_child, charge_child_turn};
use aim_proto::conversation::{Item, Part, StopReason};
use aim_proto::daemon::{SessionSpec, SessionUpdate};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::event::{SubagentParent, SubagentStatus};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolResult, ToolSpec};
use futures_util::StreamExt as _;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::ToolHost;
use crate::agent::tools::{BoxFuture, ToolCallContext};
use crate::host::{SessionClient, SessionHost, ToolsFactory};
use crate::resources::agents::ToolPolicy;

const RESULT_CHARS: usize = 8_000;

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The budget and ancestry of a child being started by the host.
#[derive(Clone)]
pub struct ChildStart {
    /// The call that created the child.
    pub parent: SubagentParent,
    /// The parent's effective tool ceiling.
    pub ceiling: ToolPolicy,
    /// The child's depth (root is zero).
    pub depth: u32,
    /// The remaining turn quotas of every ancestor, followed by the child.
    pub quotas: Vec<Arc<Mutex<u32>>>,
    /// The parent's model when the child definition names none.
    pub parent_model: String,
}

/// The session-bound information a tools factory needs to offer child sessions.
#[derive(Clone)]
pub struct SpawnContext {
    /// The owning session host.
    pub host: SessionHost,
    /// The parent session id.
    pub session_id: String,
    /// The parent's session spec.
    pub spec: SessionSpec,
    /// The effective ceiling after its own definition and ancestors.
    pub ceiling: ToolPolicy,
    /// Current session depth.
    pub depth: u32,
    /// Turn quotas of ancestors and this session.
    pub quotas: Vec<Arc<Mutex<u32>>>,
    /// Children currently executing calls for this session.
    pub active: Arc<AtomicU32>,
    /// The model actually in force.
    pub model: String,
    /// The spawning parent's model, used when a definition supplies none.
    pub parent_model: Option<String>,
}

impl SpawnContext {
    /// A root session starts with its own shared child-turn budget.
    #[must_use]
    pub fn root(host: SessionHost, session_id: String, spec: SessionSpec) -> Self {
        Self {
            host,
            session_id,
            spec,
            ceiling: ToolPolicy::default(),
            depth: 0,
            quotas: vec![Arc::new(Mutex::new(DEFAULT_CHILD_TURN_CAP))],
            active: Arc::new(AtomicU32::new(0)),
            model: String::new(),
            parent_model: None,
        }
    }

    /// Inherits the parent chain for a newly created child.
    #[must_use]
    pub fn child(host: SessionHost, session_id: String, spec: SessionSpec, start: &ChildStart) -> Self {
        Self {
            host,
            session_id,
            spec,
            ceiling: start.ceiling.clone(),
            depth: start.depth,
            quotas: start.quotas.clone(),
            active: Arc::new(AtomicU32::new(0)),
            model: String::new(),
            parent_model: Some(start.parent_model.clone()),
        }
    }

    /// Applies the named definition's policy and the model the backend selected.
    #[must_use]
    pub fn effective(mut self, policy: &ToolPolicy, model: String, root: String) -> Self {
        self.ceiling = self.ceiling.intersect(policy);
        self.model = model;
        self.spec.workspace = root;
        self
    }
}

/// Charges one child turn to the child and every ancestor before the model runs.
///
/// # Errors
/// Refuses a turn when any participating quota is exhausted.
pub fn charge_turn(quotas: &[Arc<Mutex<u32>>]) -> Result<(), ProtoError> {
    let mut guards: Vec<_> = quotas.iter().map(|quota| locked(quota)).collect();
    if guards.iter().any(|left| **left == 0) {
        return Err(ProtoError::new(ErrorCode::LimitExceeded, "subagent turn budget exhausted"));
    }
    for left in &mut guards {
        let Some((after, _)) = charge_child_turn(**left, **left) else {
            return Err(ProtoError::new(ErrorCode::LimitExceeded, "subagent turn budget exhausted"));
        };
        **left = after;
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentArgs {
    prompt: String,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    description: String,
}

struct ActiveChild(Arc<AtomicU32>);

impl Drop for ActiveChild {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct ChildRegistration {
    host: SessionHost,
    parent: SubagentParent,
    id: String,
    description: String,
    events: tokio::sync::mpsc::UnboundedSender<SessionUpdate>,
    stopped: bool,
}

impl ChildRegistration {
    fn stop(&mut self, status: SubagentStatus) {
        let _ignored = self.events.send(SessionUpdate::SubagentStopped {
            parent_session: self.parent.session.clone(),
            call_id: self.parent.call_id.clone(),
            child_session: self.id.clone(),
            description: self.description.clone(),
            status,
        });
        self.stopped = true;
    }
}

impl Drop for ChildRegistration {
    fn drop(&mut self) {
        if !self.stopped {
            self.stop(SubagentStatus::Cancelled);
        }
        self.host.unregister_child(&self.parent.session, &self.id);
        let host = self.host.clone();
        let id = self.id.clone();
        tokio::spawn(async move {
            let _ignored = host.close(id).await;
        });
    }
}

fn reserve(context: &SpawnContext) -> Result<(ActiveChild, u32), ProtoError> {
    loop {
        let active = context.active.load(Ordering::Acquire);
        let remaining = context.quotas.last().map(|quota| locked(quota));
        let child = admit_child(context.depth, active, remaining.as_ref().map_or(0, |left| **left))
            .map_err(|reason| ProtoError::new(ErrorCode::LimitExceeded, format!("subagent spawn refused: {reason:?}")))?;
        if context.active.compare_exchange(active, active + 1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            return Ok((ActiveChild(Arc::clone(&context.active)), child.turns_left));
        }
    }
}

struct AgentTools(SpawnContext);

impl ToolHost for AgentTools {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "agent".to_owned(),
            description: "Run a child agent in this workspace. It inherits your tool ceiling and remaining turn budget. The result includes its session id for read_session.".to_owned(),
            input_schema: json!({"type":"object","additionalProperties":false,"required":["prompt","description"],"properties":{"prompt":{"type":"string"},"agent":{"type":"string"},"model":{"type":"string"},"effort":{"type":"string"},"description":{"type":"string"}}}),
            input: aim_proto::tool::ToolInput::default(),
            annotations: aim_proto::tool::ToolAnnotations::default(),
        }]
    }

    fn call(&self, name: String, arguments: Value, _key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        let context = self.0.clone();
        Box::pin(async move {
            if name != "agent" {
                return Err(ProtoError::new(ErrorCode::NotFound, "no such agent tool"));
            }
            if context.spec.provider.starts_with("acp:") {
                return Err(ProtoError::new(ErrorCode::Unavailable, "ACP parents cannot spawn native subagents"));
            }
            let call = ToolCallContext::current()
                .ok_or_else(|| ProtoError::new(ErrorCode::Unavailable, "agent tool requires a running native tool call"))?;
            let args: AgentArgs = serde_json::from_value(arguments)
                .map_err(|_| ProtoError::new(ErrorCode::InvalidParams, "agent requires prompt and description"))?;
            if args.prompt.trim().is_empty()
                || args.description.trim().is_empty()
                || args.prompt.len() > 65_536
                || args.description.len() > 256
            {
                return Err(ProtoError::new(ErrorCode::InvalidParams, "agent prompt or description is empty or too long"));
            }
            let (slot, turns_left) = reserve(&context)?;
            run_child(context, call, args, turns_left, slot).await
        })
    }
}

async fn run_child(
    context: SpawnContext,
    call: ToolCallContext,
    args: AgentArgs,
    turns_left: u32,
    _slot: ActiveChild,
) -> Result<ToolResult, ProtoError> {
    let mut spec = context.spec.clone();
    spec.agent = args.agent;
    spec.model = args.model;
    spec.effort = args.effort;
    let mut quotas = context.quotas.clone();
    quotas.push(Arc::new(Mutex::new(turns_left)));
    let parent = SubagentParent { session: context.session_id.clone(), call_id: call.call_id.clone() };
    let model = context.host.current_model(&context.session_id).unwrap_or(context.model);
    let start = ChildStart { parent: parent.clone(), ceiling: context.ceiling, depth: context.depth + 1, quotas, parent_model: model };
    // Host startup opens a workspace and a store. Keep it running if the parent tool future is
    // dropped; a completed but unclaimed child is closed instead of leaking a live session.
    let host = context.host.clone();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let created = host.create_child(spec, start).await;
        if let Err(Ok(orphan)) = sender.send(created) {
            let _ignored = host.close(orphan.meta.id).await;
        }
    });
    let child = receiver.await.map_err(|_| ProtoError::new(ErrorCode::Unavailable, "child session startup crashed"))??;
    let id = child.meta.id;
    let description = args.description;
    context.host.register_child(&parent.session, &id);
    let mut registration = ChildRegistration {
        host: context.host.clone(),
        parent: parent.clone(),
        id: id.clone(),
        description: description.clone(),
        events: call.events.clone(),
        stopped: false,
    };
    let _ignored = call.events.send(SessionUpdate::SubagentStarted {
        parent_session: parent.session.clone(),
        call_id: parent.call_id.clone(),
        child_session: id.clone(),
        description: description.clone(),
    });
    let result = run_child_turn(&context.host, &id, args.prompt, &call).await;
    let status = match &result {
        Ok(_) => SubagentStatus::Completed,
        Err(error) if error.code == ErrorCode::Cancelled => SubagentStatus::Cancelled,
        Err(_) => SubagentStatus::Failed,
    };
    registration.stop(status);
    result
}

async fn run_child_turn(host: &SessionHost, id: &str, prompt: String, call: &ToolCallContext) -> Result<ToolResult, ProtoError> {
    let (_, mut updates) = host.attach(id.to_owned()).await?;
    host.prompt(id.to_owned(), vec![Part::Text { text: prompt }]).await?;
    let mut final_text = String::new();
    loop {
        tokio::select! {
            () = call.cancel.cancelled() => {
                let _ignored = host.cancel(id.to_owned()).await;
                return Err(ProtoError::new(ErrorCode::Cancelled, "parent turn cancelled"));
            }
            update = updates.next() => match update {
                Some(SessionUpdate::ItemAdded { item: Item::Assistant { parts, .. } }) => {
                    final_text = parts.into_iter().filter_map(|part| match part { Part::Text { text } => Some(text), Part::Image { .. } => None }).collect::<Vec<_>>().join("\n");
                }
                Some(SessionUpdate::TurnEnded { stop: StopReason::EndTurn }) => {
                    let mut bounded: String = final_text.chars().take(RESULT_CHARS).collect();
                    if final_text.chars().count() > RESULT_CHARS { bounded.push_str("\n[response truncated]"); }
                    let _ignored = write!(bounded, "\n[child session: {id}; use read_session to recover the full transcript]");
                    return Ok(ToolResult::text(bounded));
                }
                Some(SessionUpdate::TurnEnded { stop }) => return Err(ProtoError::new(ErrorCode::Unavailable, format!("child turn ended: {stop:?}; session {id}"))),
                Some(SessionUpdate::TurnFailed { message }) => return Err(ProtoError::new(ErrorCode::Unavailable, format!("child failed: {message}; session {id}"))),
                Some(_) => {}
                None => return Err(ProtoError::new(ErrorCode::Unavailable, format!("child session {id} closed before completion"))),
            }
        }
    }
}

/// A `ToolsFactory` entry for every native session.
#[must_use]
pub fn tools_factory() -> ToolsFactory {
    Arc::new(|_spec, context| Box::pin(async move { context.map(|context| Arc::new(AgentTools(context)) as Arc<dyn ToolHost>) }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::HostConfig;
    use crate::store::MemoryStore;

    fn context() -> SpawnContext {
        let host = SessionHost::new(HostConfig {
            store: Arc::new(MemoryStore::default()),
            backends: Arc::new(|_| Box::pin(async { Err(ProtoError::new(ErrorCode::Unavailable, "unused")) })),
            update_capacity: 16,
        });
        let spec = serde_json::from_value(json!({"workspace":"/w","provider":"scripted"})).unwrap();
        SpawnContext::root(host, "parent".to_owned(), spec)
    }

    #[test]
    fn depth_and_concurrent_children_are_bounded() {
        let mut context = context();
        let slots: Vec<_> = (0..4).map(|_| reserve(&context).unwrap().0).collect();
        assert_eq!(reserve(&context).err().map(|error| error.code), Some(ErrorCode::LimitExceeded));
        drop(slots);
        assert!(reserve(&context).is_ok());
        context.depth = 2;
        assert_eq!(reserve(&context).err().map(|error| error.code), Some(ErrorCode::LimitExceeded));
    }

    #[test]
    fn a_grandchild_charges_every_ancestor() {
        let quotas: Vec<_> = [3, 2, 1].into_iter().map(|left| Arc::new(Mutex::new(left))).collect();
        charge_turn(&quotas).unwrap();
        assert_eq!(quotas.iter().map(|quota| *locked(quota)).collect::<Vec<_>>(), [2, 1, 0]);
        assert_eq!(charge_turn(&quotas).err().map(|error| error.code), Some(ErrorCode::LimitExceeded));
        assert_eq!(quotas.iter().map(|quota| *locked(quota)).collect::<Vec<_>>(), [2, 1, 0]);
    }
}
