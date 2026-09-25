//! What the TUI offers and derives when the user switches (ADR 0074): the session `/new`,
//! `/clear` and `/provider` create, which efforts `/effort` sends, and the values `/model`,
//! `/effort` and `/provider` complete. The decisions are the kernel's (`aim_kernel::switch`); this
//! module only maps strings to the ids it decides over and back.

use aim_kernel::switch::{self, EffortRequest, Shape, Switch};
use aim_proto::daemon::{AUTO_EFFORT, ChoiceValue, Location, Persistence, SessionSpec};

use super::app::SessionView;
use super::complete::Hint;

/// Collision-free ids for the values one decision involves: equal values get equal ids.
struct Registry<T> {
    values: Vec<T>,
}

impl<T: PartialEq + Clone> Registry<T> {
    fn new() -> Self {
        Self { values: Vec::new() }
    }

    fn id(&mut self, value: &T) -> u64 {
        let index = self.values.iter().position(|v| v == value).unwrap_or_else(|| {
            self.values.push(value.clone());
            self.values.len() - 1
        });
        // A registry never holds more than a handful of values.
        u64::try_from(index).unwrap_or(u64::MAX)
    }

    fn value(&self, id: u64) -> Option<T> {
        self.values.get(usize::try_from(id).ok()?).cloned()
    }
}

/// A switch the user asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SwitchTo {
    /// `/new`.
    New,
    /// `/clear`.
    Clear,
    /// `/provider <id>`.
    Provider(String),
}

/// The spec of the session a switch creates, from the attached session (else the configured spec),
/// or `None` when the switch changes nothing (`/provider` naming the provider in use). A provider
/// change never carries the model or effort (`aim_kernel::switch::provider_switch_resets_model_and_effort`).
pub fn derive_spec(attached: Option<&SessionView>, configured: &SessionSpec, to: &SwitchTo) -> Option<SessionSpec> {
    let mut text = Registry::<String>::new();
    let mut places = Registry::<Location>::new();
    let mut shape = |provider: &str, location: &Location, workspace: &str, persistence, model: Option<&str>, effort: Option<&str>| Shape {
        provider: text.id(&provider.to_owned()),
        location: places.id(location),
        workspace: text.id(&workspace.to_owned()),
        persistent: persistence == Persistence::Persistent,
        model: model.map(|m| text.id(&m.to_owned())),
        effort: effort.map(|e| text.id(&e.to_owned())),
    };
    let attached = attached.map(|s| {
        // An agent that reports no model (an empty id) has none to carry.
        let model = Some(s.model.as_str()).filter(|m| !m.is_empty());
        shape(&s.provider, &s.location, &s.workspace, s.persistence, model, s.effort.as_deref())
    });
    let configured_shape = shape(
        &configured.provider,
        &configured.location,
        &configured.workspace,
        configured.persistence,
        configured.model.as_deref(),
        configured.effort.as_deref(),
    );
    let switch = match to {
        SwitchTo::New => Switch::New,
        SwitchTo::Clear => Switch::Clear,
        SwitchTo::Provider(provider) => Switch::Provider(text.id(provider)),
    };
    let out = switch::derive(attached, configured_shape, switch)?;
    Some(SessionSpec {
        workspace: text.value(out.workspace)?,
        location: places.value(out.location)?,
        provider: text.value(out.provider)?,
        model: match out.model {
            Some(id) => Some(text.value(id)?),
            None => None,
        },
        effort: match out.effort {
            Some(id) => Some(text.value(id)?),
            None => None,
        },
        agent: configured.agent.clone(),
        persistence: if out.persistent { Persistence::Persistent } else { Persistence::Ephemeral },
    })
}

/// Whether `/effort <requested>` is sent for a model whose ladder is `ladder` (empty: not known,
/// the session decides). `auto` always is (`aim_kernel::switch::effort_sent`).
pub fn effort_offered(ladder: &[ChoiceValue], requested: &str) -> bool {
    let mut text = Registry::<String>::new();
    let ids: Vec<u64> = ladder.iter().map(|c| text.id(&c.value)).collect();
    let request = if requested == AUTO_EFFORT { EffortRequest::Auto } else { EffortRequest::Level(text.id(&requested.to_owned())) };
    switch::effort_sent(&ids, request)
}

/// What the popup says about a choice: its name and description, without repeating the value
/// (`low`, "Low") or a name the description starts with ("Fable 5.1", "Fable 5.1 · Most capable…").
fn detail(choice: &ChoiceValue) -> String {
    let description = choice.description.as_deref().filter(|d| !d.is_empty());
    let name = choice
        .name
        .as_deref()
        .filter(|name| !name.is_empty() && !name.eq_ignore_ascii_case(&choice.value))
        .filter(|name| description.is_none_or(|d| !d.starts_with(name)));
    [name, description].into_iter().flatten().collect::<Vec<_>>().join(" · ")
}

/// `/effort` candidates: the ladder's levels (or, before the session said, values seen for its
/// provider), each once, and `auto` (`aim_kernel::switch::effort_candidates`).
pub fn effort_hints(ladder: Option<&[ChoiceValue]>, seen: &[String]) -> Vec<Hint> {
    let choices: Vec<ChoiceValue> = match ladder {
        Some(ladder) => ladder.to_vec(),
        None => seen.iter().map(|value| ChoiceValue { value: value.clone(), name: None, description: None }).collect(),
    };
    let mut text = Registry::<String>::new();
    let ids: Vec<u64> = choices.iter().map(|c| text.id(&c.value)).collect();
    let auto = text.id(&AUTO_EFFORT.to_owned());
    switch::effort_candidates(&ids, auto)
        .into_iter()
        .filter_map(|id| {
            let value = text.value(id)?;
            let detail = match choices.iter().find(|c| c.value == value) {
                Some(choice) => detail(choice),
                None => "let aim choose the effort".to_owned(),
            };
            Some(Hint { value, detail })
        })
        .collect()
}

/// `/model` candidates: the session's models, or (before it said) models seen for its provider.
pub fn model_hints(models: Option<&[ChoiceValue]>, seen: &[String]) -> Vec<Hint> {
    match models {
        Some(models) => models.iter().map(|c| Hint { value: c.value.clone(), detail: detail(c) }).collect(),
        None => seen.iter().map(|value| Hint { value: value.clone(), detail: String::new() }).collect(),
    }
}

/// `/provider` candidates: every provider this build knows, with a line about each.
pub fn provider_hints() -> Vec<Hint> {
    crate::providers::KNOWN
        .iter()
        .map(|id| Hint { value: (*id).to_owned(), detail: crate::providers::summary(id).unwrap_or_default().to_owned() })
        .collect()
}

#[cfg(test)]
mod tests {
    use aim_proto::daemon::SessionState;

    use super::*;

    fn view(provider: &str, model: &str, effort: Option<&str>) -> SessionView {
        SessionView {
            id: "s1".into(),
            workspace: "/srv/app".into(),
            provider: provider.into(),
            location: Location::Ssh { destination: "box".into() },
            model: model.into(),
            effort: effort.map(Into::into),
            state: SessionState::Idle,
            persistence: Persistence::Ephemeral,
            options: None,
            stale_efforts: false,
        }
    }

    fn configured() -> SessionSpec {
        SessionSpec {
            workspace: "/w".into(),
            location: Location::Local,
            provider: "codex".into(),
            model: Some("gpt-6-sol".into()),
            effort: Some("high".into()),
            agent: Some("reader".into()),
            persistence: Persistence::Persistent,
        }
    }

    fn choice(value: &str) -> ChoiceValue {
        ChoiceValue { value: value.into(), name: None, description: None }
    }

    #[test]
    fn new_and_clear_copy_the_attached_session_and_provider_resets_the_model() {
        let attached = view("openrouter", "openai/gpt-4.1-mini", Some("low"));
        for to in [SwitchTo::New, SwitchTo::Clear] {
            let spec = derive_spec(Some(&attached), &configured(), &to).unwrap();
            assert_eq!(spec.provider, "openrouter");
            assert_eq!((spec.model.as_deref(), spec.effort.as_deref()), (Some("openai/gpt-4.1-mini"), Some("low")));
            assert_eq!((spec.workspace.as_str(), &spec.location), ("/srv/app", &Location::Ssh { destination: "box".into() }));
            assert_eq!(spec.persistence, Persistence::Ephemeral, "as private as the attached session");
            assert_eq!(spec.agent.as_deref(), Some("reader"));
        }
        let spec = derive_spec(Some(&attached), &configured(), &SwitchTo::Provider("codex".into())).unwrap();
        assert_eq!(spec.provider, "codex");
        assert_eq!((spec.model, spec.effort), (None, None), "never a model chosen under another provider");
        assert_eq!(spec.workspace, "/srv/app");
        assert_eq!(derive_spec(Some(&attached), &configured(), &SwitchTo::Provider("openrouter".into())), None);
        // Without an attached session the configured spec is the base.
        let spec = derive_spec(None, &configured(), &SwitchTo::Provider("openrouter".into())).unwrap();
        assert_eq!((spec.provider.as_str(), spec.model, spec.persistence), ("openrouter", None, Persistence::Persistent));
        assert_eq!(derive_spec(None, &configured(), &SwitchTo::New), Some(configured()));
        // An agent that reports no model carries none.
        let spec = derive_spec(Some(&view("acp:claude", "", None)), &configured(), &SwitchTo::New).unwrap();
        assert_eq!(spec.model, None);
    }

    #[test]
    fn efforts_are_the_ladder_and_auto_and_only_offered_ones_are_sent() {
        let ladder = [choice("low"), choice("high"), choice("low")];
        let values: Vec<String> = effort_hints(Some(&ladder), &[]).into_iter().map(|h| h.value).collect();
        assert_eq!(values, ["low", "high", "auto"]);
        let seen: Vec<String> = effort_hints(None, &["auto".into(), "max".into()]).into_iter().map(|h| h.value).collect();
        assert_eq!(seen, ["auto", "max"], "auto once");
        assert!(effort_offered(&ladder, "high"));
        assert!(effort_offered(&ladder, AUTO_EFFORT));
        assert!(!effort_offered(&ladder, "ultra"));
        assert!(effort_offered(&[], "ultra"), "an unknown ladder leaves the decision to the session");
    }

    #[test]
    fn details_do_not_repeat_the_value_or_the_name() {
        let with = |value: &str, name: Option<&str>, description: Option<&str>| {
            detail(&ChoiceValue { value: value.into(), name: name.map(Into::into), description: description.map(Into::into) })
        };
        assert_eq!(with("low", Some("Low"), None), "");
        assert_eq!(with("claude-fable-5-1[1m]", Some("Fable 5.1"), Some("Fable 5.1 · Most capable")), "Fable 5.1 · Most capable");
        assert_eq!(with("opus[1m]", Some("Opus 5.5"), Some("Opus 5.5 with 1M context")), "Opus 5.5 with 1M context");
        assert_eq!(with("default", Some("Default (recommended)"), Some("Opus (1M context)")), "Default (recommended) · Opus (1M context)");
        assert_eq!(with("gpt-6-sol", Some("GPT-6-Sol"), Some("272k context")), "272k context", "a name that only differs in case");
    }

    #[test]
    fn providers_complete_with_a_line_each() {
        let hints = provider_hints();
        assert_eq!(hints.len(), crate::providers::KNOWN.len());
        assert!(hints.iter().all(|h| !h.detail.is_empty()), "{hints:?}");
    }
}
