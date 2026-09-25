//! `aim login codex | claude` (docs/architecture.md §6.6).
//!
//! - **codex** signs in to `ChatGPT` with the browser flow (PKCE on a loopback callback) or the
//!   device-code flow, and stores aim's own credentials (`~/.aim/auth/codex.json`, 0600). It never
//!   touches the Codex CLI's `~/.codex/auth.json`, which aim only ever reads.
//! - **claude** runs Claude Code's own terminal login through `claude-agent-acp`: it asks the
//!   adapter which login methods it offers and runs the chosen one in this terminal.

use std::process::Stdio;

use aim_acp::{AcpAgentConfig, AcpClient};
use aim_llm_codex::CodexConfig;
use aim_llm_codex::auth::{begin_browser_login, begin_device_login, finish_browser_login, finish_device_login};

/// Signs in to `ChatGPT` for the codex provider.
///
/// # Errors
/// A message for the user when the login fails or cannot be stored.
pub async fn codex(device: bool, say: &mut dyn FnMut(&str)) -> Result<(), String> {
    let provider = crate::providers::codex()?;
    let config = CodexConfig::default();
    let client = config.http_client().map_err(|e| e.to_string())?;
    let credentials = if device {
        let challenge = begin_device_login(&client, &config).await.map_err(|e| e.to_string())?;
        say(&format!("Open {} and enter the code {}", challenge.verification_url, challenge.user_code));
        say("Waiting for approval (up to 15 minutes)…");
        finish_device_login(&client, &challenge).await.map_err(|e| e.to_string())?
    } else {
        let challenge = begin_browser_login(&config).await.map_err(|e| e.to_string())?;
        say("Opening your browser to sign in. If it does not open, visit:");
        say(&format!("  {}", challenge.url));
        open_browser(&challenge.url);
        finish_browser_login(&client, challenge).await.map_err(|e| e.to_string())?
    };
    provider.auth().save(credentials).await.map_err(|e| e.to_string())?;
    say("Signed in to ChatGPT. aim's codex credentials are stored under ~/.aim/auth/.");
    Ok(())
}

fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    // Best effort: the URL is printed too.
    let spawned = std::process::Command::new(opener).arg(url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    if let Ok(mut child) = spawned {
        std::thread::spawn(move || {
            let _reaped = child.wait();
        });
    }
}

/// Signs in to Claude Code through its terminal login. `method` picks one of the adapter's login
/// methods by id; by default the first terminal method.
///
/// # Errors
/// A message for the user: the adapter is missing, offers no terminal login, or the login failed.
pub async fn claude(method: Option<&str>, say: &mut dyn FnMut(&str)) -> Result<(), String> {
    let client = AcpClient::spawn(AcpAgentConfig::claude()).await.map_err(|e| e.to_string())?;
    let methods: Vec<_> = client.auth_methods().iter().filter(|m| m.is_terminal()).cloned().collect();
    let chosen = match method {
        Some(id) => methods.iter().find(|m| m.id == id),
        None => methods.first(),
    };
    let Some(chosen) = chosen else {
        let ids: Vec<&str> = methods.iter().map(|m| m.id.as_str()).collect();
        return Err(format!(
            "no matching terminal login method (offered: {})",
            if ids.is_empty() { "none".to_owned() } else { ids.join(", ") }
        ));
    };
    if methods.len() > 1 {
        let others: Vec<&str> = methods.iter().filter(|m| m.id != chosen.id).map(|m| m.id.as_str()).collect();
        say(&format!("Using login method `{}` (others: {}; pick with --method).", chosen.id, others.join(", ")));
    }
    let command = client.login_command(&chosen.id).map_err(|e| e.to_string())?;
    client.shutdown(std::time::Duration::from_secs(2)).await;
    let status = tokio::process::Command::new(&command.program)
        .args(&command.args)
        .envs(&command.env)
        .status()
        .await
        .map_err(|e| format!("could not run the Claude login: {e}"))?;
    if status.success() {
        say("Signed in to Claude Code.");
        Ok(())
    } else {
        Err(format!("the Claude login exited with {status}"))
    }
}
