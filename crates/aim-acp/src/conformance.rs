//! Live conformance evidence. Advertised ACP capabilities alone do not establish tool authority
//! or private storage behavior. Witnesses are bound to one running adapter connection.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aim_proto::conversation::StopReason;

use crate::client::{AcpClient, AgentInfo, Shared};
use crate::error::AcpError;
use crate::events::{AcpEvent, Update};
use crate::options::{McpServerSpec, SessionOptions, ToolAuthority};
use crate::probe::ProbeReport;

/// Evidence from the positive and negative transcript controls.
#[derive(Clone)]
pub struct PrivateModeEvidence {
    /// Adapter name and version that were exercised.
    pub agent: AgentInfo,
    /// Advertised and probed capabilities of the same connection.
    pub probe: ProbeReport,
    /// A persisted control prompt created a transcript in the inspected store.
    pub persisted_control_found: bool,
    /// The unpersisted prompt left no files in its project directory.
    pub private_files_absent: bool,
    /// A project directory may still expose the private cwd's basename.
    pub cwd_directory_remained: bool,
    /// Time spent on the checks.
    pub elapsed_ms: u64,
}

impl core::fmt::Debug for PrivateModeEvidence {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PrivateModeEvidence")
            .field("agent_version", &"***")
            .field("persisted_control_found", &self.persisted_control_found)
            .field("private_files_absent", &self.private_files_absent)
            .field("cwd_directory_remained", &self.cwd_directory_remained)
            .field("elapsed_ms", &self.elapsed_ms)
            .finish_non_exhaustive()
    }
}

/// Same-process proof that `persistSession:false` left no transcript after a real prompt.
pub struct VerifiedPrivateMode {
    connection: Weak<Shared>,
    evidence: PrivateModeEvidence,
}

impl VerifiedPrivateMode {
    /// The observed conformance evidence. Provider-side retention is outside this check.
    #[must_use]
    pub fn evidence(&self) -> &PrivateModeEvidence {
        &self.evidence
    }

    pub(crate) fn matches(&self, client: &AcpClient) -> bool {
        Weak::ptr_eq(&self.connection, &Arc::downgrade(&client.shared)) && self.evidence.agent == *client.agent()
    }
}

impl core::fmt::Debug for VerifiedPrivateMode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VerifiedPrivateMode").field("evidence", &self.evidence).finish_non_exhaustive()
    }
}

/// Evidence that a live local adapter routed one challenge tool call through aim's MCP server.
#[derive(Clone)]
pub struct AimAuthorityEvidence {
    /// Adapter name and version.
    pub agent: AgentInfo,
    /// Probe of the exact strict session configuration.
    pub probe: ProbeReport,
    /// The aim MCP tool observed in a completed call.
    pub observed_tool: String,
    /// The challenged response contained the nonce returned by that tool.
    pub response_confirmed: bool,
}

impl core::fmt::Debug for AimAuthorityEvidence {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AimAuthorityEvidence")
            .field("agent_version", &"***")
            .field("observed_tool", &"***")
            .field("response_confirmed", &self.response_confirmed)
            .finish_non_exhaustive()
    }
}

/// Same-process local aim-tools conformance. This is insufficient for SSH authority.
pub struct VerifiedAimAuthority {
    connection: Weak<Shared>,
    relay: McpServerSpec,
    evidence: AimAuthorityEvidence,
}

impl VerifiedAimAuthority {
    /// What the local live challenge established.
    #[must_use]
    pub fn evidence(&self) -> &AimAuthorityEvidence {
        &self.evidence
    }

    pub(crate) fn matches(&self, client: &AcpClient, options: &SessionOptions) -> bool {
        Weak::ptr_eq(&self.connection, &Arc::downgrade(&client.shared))
            && self.evidence.agent == *client.agent()
            && options.persist
            && matches!(options.tool_authority, ToolAuthority::Aim)
            && options.mcp_servers.as_slice() == [self.relay.clone()]
            && options.validate_authority().is_ok()
    }
}

impl core::fmt::Debug for VerifiedAimAuthority {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VerifiedAimAuthority").field("evidence", &self.evidence).finish_non_exhaustive()
    }
}

/// SSH tool-authority proof. Intentionally has no public constructor: the SSH integration must
/// add a live remote-write/local-untouched routine before it can create this witness.
pub struct VerifiedSshAuthority {
    connection: Weak<Shared>,
    relay: McpServerSpec,
    agent: AgentInfo,
}

impl VerifiedSshAuthority {
    pub(crate) fn matches(&self, client: &AcpClient, options: &SessionOptions) -> bool {
        Weak::ptr_eq(&self.connection, &Arc::downgrade(&client.shared))
            && self.agent == *client.agent()
            && options.persist
            && matches!(options.tool_authority, ToolAuthority::Aim)
            && options.mcp_servers.as_slice() == [self.relay.clone()]
            && options.validate_authority().is_ok()
    }
}

impl core::fmt::Debug for VerifiedSshAuthority {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VerifiedSshAuthority").field("agent_version", &"***").finish_non_exhaustive()
    }
}

/// One unique scratch cwd and its matching Claude project-store directory.
struct Scratch {
    path: PathBuf,
    basename: String,
    projects: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Result<Self, AcpError> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let basename = format!("aim-acp-conformance-{tag}-{}-{nanos}", std::process::id());
        let path = std::env::temp_dir().join(&basename);
        std::fs::create_dir(&path).map_err(|_| AcpError::InvalidState("cannot create conformance scratch cwd".into()))?;
        let base = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude")));
        let Some(base) = base else {
            drop(std::fs::remove_dir(&path));
            return Err(AcpError::InvalidState("Claude configuration directory is unavailable".into()));
        };
        Ok(Self { path, basename, projects: base.join("projects") })
    }

    fn project_dirs(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(&self.projects) else { return Vec::new() };
        entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().contains(&self.basename)))
            .collect()
    }

    fn has_transcript(&self, session_id: &str) -> bool {
        self.project_dirs().iter().any(|dir| {
            std::fs::read_dir(dir)
                .is_ok_and(|entries| entries.flatten().any(|entry| entry.file_name().to_string_lossy().contains(session_id)))
        })
    }

    fn any_files(&self) -> bool {
        fn contains_file(path: &Path) -> bool {
            let Ok(entries) = std::fs::read_dir(path) else { return false };
            entries.flatten().any(|entry| {
                let path = entry.path();
                if path.is_dir() { contains_file(&path) } else { true }
            })
        }
        self.project_dirs().iter().any(|dir| contains_file(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        for dir in self.project_dirs() {
            drop(std::fs::remove_dir_all(dir));
        }
        drop(std::fs::remove_dir_all(&self.path));
    }
}

impl AcpClient {
    /// Performs live private-mode conformance with a persisted positive control and a real
    /// unpersisted prompt. It inspects the adapter's local Claude project store after close.
    /// Raw wire capture is refused because it would itself persist a private transcript.
    ///
    /// # Errors
    ///
    /// Returns an error if the controls fail or the transcript store cannot be established.
    pub async fn verify_private_mode(&self) -> Result<VerifiedPrivateMode, AcpError> {
        if self.shared.tap_active {
            return Err(AcpError::InvalidState("raw wire capture prevents private-mode conformance".into()));
        }
        let start = Instant::now();
        // A separate process is necessary: the adapter can flush a transcript at process exit.
        // The witness is bound to this caller's connection only after that exit is observed.
        let verifier = AcpClient::spawn(self.config().clone()).await?;
        if verifier.agent() != self.agent() {
            return Err(AcpError::InvalidState("conformance adapter identity differs from the active connection".into()));
        }
        let Some(process) = &verifier.shared.process else {
            return Err(AcpError::InvalidState("private conformance requires a managed adapter process".into()));
        };
        let mut exit = process.exit.subscribe();
        let control = Scratch::new("control")?;
        let private = Scratch::new("private")?;
        let probe = verifier.probe(&private.path).await?;
        if !probe.capabilities.claude_code || !probe.capabilities.sessions.close {
            return Err(AcpError::InvalidState("adapter lacks private-mode prerequisites".into()));
        }
        let mut persisted = verifier.new_session_unchecked(SessionOptions::new(&control.path)).await?;
        let persisted_id = persisted.id().to_owned();
        let turn = persisted.prompt_text("Reply exactly OK").await?;
        drop(turn.collect_all().await?);
        persisted.close().await?;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let persisted_control_found = control.has_transcript(&persisted_id);
        if !persisted_control_found {
            return Err(AcpError::InvalidState("private-mode positive control found no transcript".into()));
        }
        let mut options = SessionOptions::new(&private.path);
        options.persist = false;
        let mut session = verifier.new_session_unchecked(options).await?;
        let turn = session.prompt_text("Reply exactly OK").await?;
        let events = turn.collect_all().await?;
        if !matches!(events.last(), Some(AcpEvent::Stopped(end)) if end.stop == StopReason::EndTurn) {
            return Err(AcpError::InvalidState("private-mode challenge did not finish".into()));
        }
        session.close().await?;
        verifier.shutdown(Duration::from_secs(5)).await;
        tokio::time::timeout(Duration::from_secs(5), exit.wait_for(Option::is_some))
            .await
            .map_err(|_| AcpError::InvalidState("conformance adapter did not exit".into()))?
            .map_err(|_| AcpError::InvalidState("conformance exit status is unavailable".into()))?;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let private_files_absent = !private.any_files();
        let cwd_directory_remained = !private.project_dirs().is_empty();
        if !private_files_absent {
            return Err(AcpError::InvalidState("private-mode challenge left files in the adapter store".into()));
        }
        Ok(VerifiedPrivateMode {
            connection: Arc::downgrade(&self.shared),
            evidence: PrivateModeEvidence {
                agent: self.agent().clone(),
                probe,
                persisted_control_found,
                private_files_absent,
                cwd_directory_remained,
                elapsed_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
            },
        })
    }

    /// Challenges a local strict aim-tools relay with a nonce-bearing MCP call. The returned
    /// witness only permits local aim-tools sessions with the same relay and adapter connection.
    ///
    /// # Errors
    ///
    /// The probe, tool call, or nonce response did not pass.
    pub async fn verify_local_aim_authority(
        &self,
        relay: McpServerSpec,
        expected_tool: &str,
        nonce: &str,
    ) -> Result<VerifiedAimAuthority, AcpError> {
        if !expected_tool.starts_with("mcp__aim__") || nonce.is_empty() {
            return Err(AcpError::InvalidState("invalid aim MCP conformance challenge".into()));
        }
        let scratch = Scratch::new("aim-tools")?;
        let options = SessionOptions::strict_aim(&scratch.path, relay.clone())?;
        let probe = self.probe_with(options.clone()).await?;
        if !probe.supports(&ToolAuthority::Aim) {
            return Err(AcpError::InvalidState("aim-tools capability probe failed".into()));
        }
        let mut session = self.new_session_unchecked(options).await?;
        let turn = session.prompt_text(format!("Call {expected_tool} with text {nonce}. Reply with the returned text.")).await?;
        let events = turn.collect_all().await?;
        session.close().await?;
        let called = events.iter().any(|event| {
            matches!(event, AcpEvent::Update { update: Update::ToolCall(call), .. }
                if call.name.as_deref() == Some(expected_tool) && call.status.is_final())
        });
        let response_confirmed = events.iter().any(|event| {
            matches!(event, AcpEvent::Update { update: Update::AgentMessage(chunk), .. }
                if matches!(&chunk.content, crate::events::ContentPart::Text { text } if text.contains(nonce)))
        });
        if !called || !response_confirmed {
            return Err(AcpError::InvalidState("aim MCP challenge did not prove the expected route".into()));
        }
        Ok(VerifiedAimAuthority {
            connection: Arc::downgrade(&self.shared),
            relay,
            evidence: AimAuthorityEvidence { agent: self.agent().clone(), probe, observed_tool: expected_tool.into(), response_confirmed },
        })
    }

    /// Challenges a strict local relay by reading a file whose contents are withheld from the
    /// agent's prompt. The caller creates the file inside the relay's workspace and removes it
    /// after this method returns.
    ///
    /// # Errors
    /// The adapter did not route a completed aim read or did not report the file's contents.
    pub async fn verify_local_aim_read_authority(
        &self,
        relay: McpServerSpec,
        path: &Path,
        expected_content: &str,
    ) -> Result<VerifiedAimAuthority, AcpError> {
        if expected_content.is_empty() {
            return Err(AcpError::InvalidState("empty aim read challenge".into()));
        }
        let scratch = Scratch::new("aim-read")?;
        let options = SessionOptions::strict_aim(&scratch.path, relay.clone())?;
        let probe = self.probe_with(options.clone()).await?;
        if !probe.supports(&ToolAuthority::Aim) {
            return Err(AcpError::InvalidState("aim-tools capability probe failed".into()));
        }
        let mut session = self.new_session_unchecked(options).await?;
        let prompt = format!("Use the mcp__aim__read tool to read {}. Reply with only the file contents.", path.display());
        let events = session.prompt_text(prompt).await?.collect_all().await?;
        session.close().await?;
        let response_confirmed = events.iter().any(|event| {
            matches!(event,
                AcpEvent::Update { update: Update::ToolCall(call), .. }
                    if call.name.as_deref() == Some("mcp__aim__read")
                        && call.status == crate::events::ToolCallStatus::Completed
                        && call.raw_output.as_ref().is_some_and(|value| value.to_string().contains(expected_content))
            )
        });
        if !response_confirmed {
            return Err(AcpError::InvalidState("aim read challenge did not prove the expected route".into()));
        }
        Ok(VerifiedAimAuthority {
            connection: Arc::downgrade(&self.shared),
            relay,
            evidence: AimAuthorityEvidence {
                agent: self.agent().clone(),
                probe,
                observed_tool: "mcp__aim__read".into(),
                response_confirmed,
            },
        })
    }

    /// Challenges an SSH relay with a completed aim write, then checks the remote file through
    /// the caller's independent harness read while a local sentinel remains unchanged.
    ///
    /// # Errors
    /// The adapter, remote read, or local sentinel failed the challenge.
    pub async fn verify_ssh_aim_authority<F, Fut>(
        &self,
        relay: McpServerSpec,
        remote_path: &str,
        local_sentinel: &Path,
        content: &str,
        verify_remote: F,
    ) -> Result<VerifiedSshAuthority, AcpError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<bool, AcpError>>,
    {
        if remote_path.is_empty() || content.is_empty() {
            return Err(AcpError::InvalidState("invalid SSH authority challenge".into()));
        }
        let before = std::fs::read(local_sentinel).map_err(|_| AcpError::InvalidState("cannot read local authority sentinel".into()))?;
        let scratch = Scratch::new("aim-ssh")?;
        let options = SessionOptions::strict_aim(&scratch.path, relay.clone())?;
        let probe = self.probe_with(options.clone()).await?;
        if !probe.supports(&ToolAuthority::Aim) {
            return Err(AcpError::InvalidState("aim-tools capability probe failed".into()));
        }
        let mut session = self.new_session_unchecked(options).await?;
        let prompt =
            format!("Use mcp__aim__write to create {remote_path} with exactly this content: {content}. Do not use any other tool.");
        let events = session.prompt_text(prompt).await?.collect_all().await?;
        session.close().await?;
        let called = events.iter().any(|event| {
            matches!(event,
                AcpEvent::Update { update: Update::ToolCall(call), .. }
                    if call.name.as_deref() == Some("mcp__aim__write")
                        && call.status == crate::events::ToolCallStatus::Completed
            )
        });
        let local_untouched = std::fs::read(local_sentinel).is_ok_and(|after| after == before);
        if !called || !local_untouched || !verify_remote().await? {
            return Err(AcpError::InvalidState("SSH aim-tools route did not pass remote-write conformance".into()));
        }
        Ok(VerifiedSshAuthority { connection: Arc::downgrade(&self.shared), relay, agent: self.agent().clone() })
    }
}
