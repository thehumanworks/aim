//! `AIM_CODE_MODE` (ADR 0076): what the environment asks for, and what a session is offered.
//!
//! The decision itself is the verified [`aim_kernel::code_mode`]; this module parses the request,
//! names the tools the compact set hides, maps tool names to the kernel's ids and logs fallbacks.

use std::collections::BTreeMap;

pub use aim_kernel::code_mode::{CodeModeRequest, DEFAULT_MODE, Direct, Exposure, Fallback, Mode};
use aim_proto::tool::ToolSpec;

/// The environment variable that selects code mode: `off`, `on` or `only` (also `0`, `1`,
/// `false`, `true`), in any case. Unset means [`DEFAULT_MODE`]; an invalid value means `off`.
pub const ENV: &str = "AIM_CODE_MODE";

/// Tools a model reaches only inside a cell when code mode is `on` (ADR 0056's compact set).
pub const COMPACT_HIDDEN: [&str; 5] = ["Glob", "Grep", "KillShell", "search_sessions", "read_session"];

/// The code tools' names: `run_code`, codex's `exec`/`wait`, and `run_program`, which runs a
/// saved program in a cell.
pub const CELL_TOOLS: [&str; 4] = ["run_code", "exec", "wait", "run_program"];

/// Parses a setting: `off|on|only` or `0|1|false|true`, in any case, surrounding space ignored.
#[must_use]
pub fn parse(value: &str) -> Option<Mode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" | "0" | "false" => Some(Mode::Off),
        "on" | "1" | "true" => Some(Mode::On),
        "only" => Some(Mode::Only),
        _ => None,
    }
}

/// The setting's name, as `AIM_CODE_MODE` and `aim code-mcp --code-mode` spell it.
#[must_use]
pub const fn label(mode: Mode) -> &'static str {
    match mode {
        Mode::Off => "off",
        Mode::On => "on",
        Mode::Only => "only",
    }
}

/// What `value` (an `AIM_CODE_MODE` value, if set) requests. An invalid value fails closed: code
/// mode is off (ADR 0076).
#[must_use]
pub fn requested(value: Option<&str>) -> CodeModeRequest {
    match value.map(parse) {
        None => CodeModeRequest::Unset,
        Some(None) => CodeModeRequest::Invalid,
        Some(Some(mode)) => CodeModeRequest::Set(mode),
    }
}

/// The warning an invalid `value` deserves, if it is invalid.
#[must_use]
pub fn invalid_warning(value: Option<&str>) -> Option<String> {
    let value = value?;
    (requested(Some(value)) == CodeModeRequest::Invalid).then(|| {
        // Environment content is echoed only when it is short and plain (codex re-check N1).
        let plain = value.len() <= 16 && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
        let shown = if plain { format!("{ENV}=\"{value}\"") } else { format!("{ENV} holds an unrecognized value, which") };
        format!("{shown} is not off, on or only (or 0, 1, false, true); code mode is off")
    })
}

fn env_value() -> Option<String> {
    std::env::var_os(ENV).map(|value| value.to_string_lossy().into_owned())
}

/// What this process's environment requests ([`requested`]). An invalid value is logged once.
#[must_use]
pub fn requested_from_env() -> CodeModeRequest {
    static LOGGED: std::sync::Once = std::sync::Once::new();
    let value = env_value();
    if let Some(warning) = invalid_warning(value.as_deref()) {
        LOGGED.call_once(|| tracing::warn!("{warning}"));
    }
    requested(value.as_deref())
}

/// The warning an invalid `AIM_CODE_MODE` in this process's environment deserves. The CLI says it
/// on stderr, once, since it has no log subscriber and a log warning alone would go unseen.
#[must_use]
pub fn invalid_env_warning() -> Option<String> {
    invalid_warning(env_value().as_deref())
}

/// Why code mode fell back to off, for the user.
#[must_use]
pub const fn reason(fallback: Fallback) -> &'static str {
    match fallback {
        Fallback::PlatformUnsupported => "code cells run sandboxed on macOS only",
        Fallback::WorkerMissing => "the aim-coderun worker was not found (set AIM_CODERUN, or install it next to aim)",
        Fallback::NotPermitted => "the session's tools do not permit run_code",
    }
}

/// Asks the kernel what a session is offered ([`aim_kernel::code_mode::decide`], with
/// [`DEFAULT_MODE`]) and logs a fallback: as a warning when the mode was asked for
/// explicitly, else at debug level (an unset request on a machine without the worker is normal,
/// and so is an agent whose ceiling leaves out `run_code`).
#[must_use]
pub fn decide(requested: CodeModeRequest, worker: bool, platform: bool, permitted: bool) -> Exposure {
    let exposure = aim_kernel::code_mode::decide(requested, DEFAULT_MODE, worker, platform, permitted);
    if let Some(fallback) = exposure.fallback {
        let (explicit, wanted) = match requested {
            CodeModeRequest::Set(mode) => (true, label(mode)),
            _ => (false, label(DEFAULT_MODE)),
        };
        if explicit && fallback != Fallback::NotPermitted {
            tracing::warn!("code mode `{wanted}` is off: {}", reason(fallback));
        } else {
            tracing::debug!("code mode `{wanted}` (the default) is off: {}", reason(fallback));
        }
    }
    exposure
}

/// Which of `names` (in order) `direct` offers directly: all, all but `hidden`, or none. The
/// selection is the kernel's [`aim_kernel::code_mode::direct_tools`] over ids assigned here.
fn shown(direct: Direct, names: &[&str], hidden: &[&str]) -> Vec<bool> {
    let mut ids: BTreeMap<&str, u64> = BTreeMap::new();
    for name in names.iter().chain(hidden) {
        let next = u64::try_from(ids.len()).unwrap_or(u64::MAX);
        ids.entry(name).or_insert(next);
    }
    let tools: Vec<u64> = names.iter().map(|name| ids.get(name).copied().unwrap_or(u64::MAX)).collect();
    let hidden: Vec<u64> = hidden.iter().filter_map(|name| ids.get(name).copied()).collect();
    // The kernel's answer is `tools` filtered in order, so one pass pairs it back up.
    let mut kept = aim_kernel::code_mode::direct_tools(direct, &tools, &hidden).into_iter().peekable();
    tools
        .into_iter()
        .map(|id| {
            let keep = kept.peek() == Some(&id);
            if keep {
                kept.next();
            }
            keep
        })
        .collect()
}

/// The names of `names` (in order) that `direct` offers directly, `hidden` naming the compact
/// set's exclusions.
#[must_use]
pub fn direct_names(direct: Direct, names: &[&str], hidden: &[&str]) -> Vec<String> {
    names.iter().zip(shown(direct, names, hidden)).filter(|(_, keep)| *keep).map(|(name, _)| (*name).to_owned()).collect()
}

/// The specs `direct` offers directly, in order ([`direct_names`]).
#[must_use]
pub fn direct_specs(direct: Direct, specs: Vec<ToolSpec>, hidden: &[&str]) -> Vec<ToolSpec> {
    let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
    let keep = shown(direct, &names, hidden);
    specs.into_iter().zip(keep).filter(|(_, keep)| *keep).map(|(spec, _)| spec).collect()
}

#[cfg(test)]
mod tests {
    use aim_proto::tool::{ToolAnnotations, ToolInput, ToolSpec};
    use serde_json::json;

    use super::{
        COMPACT_HIDDEN, CodeModeRequest, DEFAULT_MODE, Direct, Fallback, Mode, decide, direct_specs, invalid_warning, parse, requested,
    };

    #[test]
    fn settings_parse_in_any_case_and_invalid_values_fail_closed() {
        for (value, mode) in [
            ("off", Mode::Off),
            ("OFF", Mode::Off),
            ("0", Mode::Off),
            ("False", Mode::Off),
            ("on", Mode::On),
            (" On ", Mode::On),
            ("1", Mode::On),
            ("TRUE", Mode::On),
            ("only", Mode::Only),
            ("Only", Mode::Only),
        ] {
            assert_eq!(parse(value), Some(mode), "{value}");
        }
        for value in ["", "yes", "2", "onl", "only!", "no"] {
            assert_eq!(parse(value), None, "{value}");
        }
        assert_eq!(requested(None), CodeModeRequest::Unset);
        assert_eq!(requested(Some("sometimes")), CodeModeRequest::Invalid);
        assert_eq!(requested(Some("only")), CodeModeRequest::Set(Mode::Only));
        assert_eq!(DEFAULT_MODE, Mode::On, "the maintainer's default (ADR 0076 §6); AIM_CODE_MODE=off opts out");
        let invalid = decide(requested(Some("onn")), true, true, true);
        assert_eq!((invalid.mode, invalid.code, invalid.fallback), (Mode::Off, false, None), "an invalid value turns code mode off");
        assert_eq!(
            invalid_warning(Some("onn")).as_deref(),
            Some("AIM_CODE_MODE=\"onn\" is not off, on or only (or 0, 1, false, true); code mode is off")
        );
        assert_eq!(invalid_warning(Some("only")), None);
        for hidden in ["sk-live-0123456789abcdef", "on\u{1b}[2J", "o n", "x".repeat(17).as_str()] {
            let warning = invalid_warning(Some(hidden)).expect("invalid");
            assert!(!warning.contains(hidden), "{warning}");
            assert!(warning.starts_with("AIM_CODE_MODE holds an unrecognized value, which is not off"), "{warning}");
        }
        assert_eq!(invalid_warning(None), None);
    }

    #[test]
    fn fallbacks_name_their_cause_and_never_widen_a_ceiling() {
        let only = CodeModeRequest::Set(Mode::Only);
        assert_eq!(decide(only, true, true, true).direct, Direct::Hidden);
        assert_eq!(decide(only, false, true, true).fallback, Some(Fallback::WorkerMissing));
        assert_eq!(decide(only, true, false, true).fallback, Some(Fallback::PlatformUnsupported));
        let denied = decide(only, true, true, false);
        assert_eq!(
            (denied.mode, denied.code, denied.direct, denied.fallback),
            (Mode::Off, false, Direct::Full, Some(Fallback::NotPermitted))
        );
        let off = decide(CodeModeRequest::Set(Mode::Off), false, false, false);
        assert_eq!((off.code, off.fallback), (false, None), "off asked for is not a fallback");
        assert_eq!(decide(CodeModeRequest::Unset, true, true, true).mode, DEFAULT_MODE);
    }

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_owned(),
            description: String::new(),
            input_schema: json!({"type":"object"}),
            input: ToolInput::Json,
            annotations: ToolAnnotations::default(),
        }
    }

    #[test]
    fn direct_sets_keep_order_and_hide_the_compact_names() {
        let names = ["Read", "Glob", "Bash", "Grep", "board_list", "search_sessions"];
        let specs = || names.iter().map(|name| spec(name)).collect::<Vec<_>>();
        let shown = |direct| direct_specs(direct, specs(), &COMPACT_HIDDEN).into_iter().map(|spec| spec.name).collect::<Vec<_>>();
        assert_eq!(shown(Direct::Full), names);
        assert_eq!(shown(Direct::Compact), ["Read", "Bash", "board_list"]);
        assert!(shown(Direct::Hidden).is_empty());
    }
}
