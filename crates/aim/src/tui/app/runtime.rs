//! Runtime command lifecycle: one active action, bound to its original session/attach attempt.
use aim_kernel::slash::{Delivery, delivery};

use super::{App, Effect, Level, SessionSpec};
use crate::tui::runtime::{Action, ResultText, bounded, render};
use crate::tui::settings::Output;

#[derive(Debug)]
pub(super) struct Pending {
    id: u64,
    attempt: u64,
    session: String,
    name: String,
    output: Output,
    template: Option<String>,
    args: String,
}

impl App {
    pub(super) fn run_command(&mut self, name: &str, output: Output, template: Option<String>, args: &str, action: Action) -> Vec<Effect> {
        if self.command_pending.is_some() {
            self.notice(Level::Error, "a slash command is running; /cancel stops it");
            return Vec::new();
        }
        let Some(session) = self.session.as_ref().filter(|_| !self.waiting_for_session()) else {
            self.notice(Level::Error, "wait for the session to attach before running a slash command");
            return Vec::new();
        };
        let spec = SessionSpec {
            workspace: session.workspace.clone(),
            location: session.location.clone(),
            provider: session.provider.clone(),
            model: Some(session.model.clone()),
            effort: session.effort.clone(),
            persistence: session.persistence,
            agent: None,
            code_mode: None,
        };
        self.next_command = self.next_command.saturating_add(1);
        let id = self.next_command;
        self.command_pending = Some(Pending {
            id,
            attempt: self.attempt,
            session: session.id.clone(),
            name: name.to_owned(),
            output,
            template,
            args: args.to_owned(),
        });
        self.composer.clear();
        let mut effects = Vec::new();
        self.close_popup(&mut effects);
        self.notice(Level::Info, format!("/{name}: running… (/cancel to stop)"));
        effects.push(Effect::RunCommand { id, action, spec: Box::new(spec) });
        effects
    }

    pub(super) fn cancel_command(&mut self) -> Option<Effect> {
        self.command_pending.take().map(|pending| Effect::CancelCommand(pending.id))
    }

    pub(super) fn command_done(&mut self, id: u64, result: Result<ResultText, String>) -> Vec<Effect> {
        if self.command_pending.as_ref().is_none_or(|pending| pending.id != id) {
            return Vec::new();
        }
        let Some(pending) = self.command_pending.take() else { return Vec::new() };
        let current = pending.attempt == self.attempt
            && !self.waiting_for_session()
            && self.session.as_ref().is_some_and(|s| s.id == pending.session);
        let result = result.and_then(|mut value| {
            value.text = bounded(render(pending.template.as_deref(), &pending.args, &value.text))?;
            Ok(value)
        });
        let route = delivery(pending.output == Output::Agent, current, result.is_ok());
        match (route, result) {
            (Delivery::Discard, _) => Vec::new(),
            (Delivery::Agent, Ok(value)) if !value.text.trim().is_empty() => self.send(value.text),
            (Delivery::User, Ok(value)) => {
                if value.limits.is_some() && self.session.as_ref().is_some_and(|s| s.provider == "codex") {
                    self.limits = value.limits;
                }
                self.notice(Level::Info, value.text);
                Vec::new()
            }
            (_, Err(error)) => {
                self.notice(Level::Error, format!("/{}: {error}", pending.name));
                Vec::new()
            }
            _ => {
                self.notice(Level::Info, format!("/{} completed with no output", pending.name));
                Vec::new()
            }
        }
    }
}
