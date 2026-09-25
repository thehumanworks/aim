//! Scoped board tools for a model session. Claim secrets stay in the host, never in tool input.

use aim_proto::board::{ArtifactInput, CompleteParams, JobSpec, ListParams, MessageParams, PostParams, ReviewParams};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolAnnotations, ToolInput, ToolLocation, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::ToolHost;
use crate::agent::tools::BoxFuture;

use super::Board;

struct AttemptAccess {
    job_id: String,
    attempt_id: String,
    token: String,
}

/// A board tool host scoped to one run and either a worker attempt or a reviewer.
pub struct BoardTools {
    board: Board,
    run_id: String,
    actor: String,
    attempt: Option<AttemptAccess>,
    reviewer: bool,
}

impl BoardTools {
    /// Binds a worker's model to its own fenced attempt. The token is never advertised.
    #[must_use]
    pub fn worker(board: Board, run_id: String, actor: String, job_id: String, attempt_id: String, token: String) -> Self {
        Self { board, run_id, actor, attempt: Some(AttemptAccess { job_id, attempt_id, token }), reviewer: false }
    }

    /// Gives a reviewer read, post and review tools within one run.
    #[must_use]
    pub fn reviewer(board: Board, run_id: String, actor: String) -> Self {
        Self { board, run_id, actor, attempt: None, reviewer: true }
    }
}

fn spec(name: &str, description: &str, schema: Value, read_only: bool) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        input_schema: schema,
        input: ToolInput::Json,
        annotations: ToolAnnotations { read_only, location: ToolLocation::LocalService, ..ToolAnnotations::default() },
    }
}

fn object(properties: &Value, required: &[&str]) -> Value {
    json!({"type":"object","additionalProperties":false,"properties":properties,"required":required})
}

#[derive(Deserialize)]
struct ShowArgs {
    job_id: String,
}

#[derive(Deserialize)]
struct ListArgs {
    limit: Option<u32>,
    after_job: Option<String>,
}

#[derive(Deserialize)]
struct MessageArgs {
    recipient: String,
    kind: String,
    body: String,
}

#[derive(Deserialize)]
struct CompleteArgs {
    artifacts: Vec<ArtifactInput>,
}

#[derive(Deserialize)]
struct ReviewArgs {
    job_id: String,
    attempt_id: String,
    accepted: bool,
    evidence: Vec<String>,
}

fn invalid(message: impl Into<String>) -> ProtoError {
    ProtoError::new(ErrorCode::InvalidParams, message)
}

fn decode<T: serde::de::DeserializeOwned>(arguments: Value) -> Result<T, ProtoError> {
    serde_json::from_value(arguments).map_err(|err| invalid(format!("invalid board arguments: {err}")))
}

fn result<T: serde::Serialize>(value: &T) -> Result<ToolResult, ProtoError> {
    serde_json::to_string(value).map(ToolResult::text).map_err(|err| ProtoError::new(ErrorCode::Internal, err.to_string()))
}

impl ToolHost for BoardTools {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut tools = vec![
            spec(
                "board_post",
                "Post a job in this run.",
                object(
                    &json!({"title":{"type":"string"},"deliverable":{"type":"string"},"acceptance":{"type":"array","items":{"type":"string"}},"depends_on":{"type":"array","items":{"type":"string"}},"max_retries":{"type":"integer","minimum":0},"workspace":{"type":["string","null"]},"work":{"type":["object","null"]}}),
                    &["title", "deliverable", "acceptance", "max_retries"],
                ),
                false,
            ),
            spec(
                "board_list",
                "List jobs in this run.",
                object(&json!({"limit":{"type":"integer","minimum":1,"maximum":200},"after_job":{"type":"string"}}), &[]),
                true,
            ),
            spec("board_show", "Read a job in this run.", object(&json!({"job_id":{"type":"string"}}), &["job_id"]), true),
        ];
        if self.attempt.is_some() {
            tools.push(spec(
                "board_message",
                "Send a bounded message from this attempt.",
                object(
                    &json!({"recipient":{"type":"string"},"kind":{"type":"string"},"body":{"type":"string"}}),
                    &["recipient", "kind", "body"],
                ),
                false,
            ));
            tools.push(spec("board_complete", "Complete this attempt with base64 artifact bytes.", object(&json!({"artifacts":{"type":"array","items":{"type":"object","properties":{"media_type":{"type":"string"},"data":{"type":"string"}},"required":["media_type","data"]}}}), &["artifacts"]), false));
        }
        if self.reviewer {
            tools.push(spec("board_review", "Accept or reject an attempt with artifact IDs.", object(&json!({"job_id":{"type":"string"},"attempt_id":{"type":"string"},"accepted":{"type":"boolean"},"evidence":{"type":"array","items":{"type":"string"}}}), &["job_id", "attempt_id", "accepted", "evidence"]), false));
        }
        tools
    }

    fn call(&self, name: String, arguments: Value, key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        let board = self.board.clone();
        let run_id = self.run_id.clone();
        let actor = self.actor.clone();
        let attempt = self.attempt.as_ref().map(|access| (access.job_id.clone(), access.attempt_id.clone(), access.token.clone()));
        let reviewer = self.reviewer;
        Box::pin(async move {
            match name.as_str() {
                "board_post" => {
                    let mut spec: JobSpec = decode(arguments)?;
                    if let Some((job_id, _, _)) = &attempt {
                        if spec.workspace.is_some() || spec.work.is_some() {
                            return Err(ProtoError::new(ErrorCode::Denied, "worker posts inherit their current workspace policy"));
                        }
                        let parent = board.show(job_id.clone()).await.map_err(|err| invalid(err.to_string()))?;
                        spec.workspace = parent.spec.workspace;
                        spec.work = parent.spec.work;
                    }
                    result(
                        &board
                            .post(PostParams { run_id: Some(run_id), spec, idempotency_key: key.0 })
                            .await
                            .map_err(|err| invalid(err.to_string()))?,
                    )
                }
                "board_list" => {
                    let args: ListArgs = decode(arguments)?;
                    result(
                        &board
                            .list(ListParams { run_id: Some(run_id), limit: args.limit, after_job: args.after_job })
                            .await
                            .map_err(|err| invalid(err.to_string()))?,
                    )
                }
                "board_show" => {
                    let args: ShowArgs = decode(arguments)?;
                    let job = board.show(args.job_id).await.map_err(|err| invalid(err.to_string()))?;
                    if job.run_id != run_id {
                        return Err(ProtoError::new(ErrorCode::Denied, "job is outside this run"));
                    }
                    result(&job)
                }
                "board_message" => {
                    let Some((job_id, attempt_id, claim_token)) = attempt else {
                        return Err(ProtoError::new(ErrorCode::Denied, "not a worker attempt"));
                    };
                    let args: MessageArgs = decode(arguments)?;
                    result(
                        &board
                            .message(MessageParams {
                                job_id,
                                attempt_id,
                                claim_token,
                                recipient: args.recipient,
                                kind: args.kind,
                                body: args.body,
                                idempotency_key: key.0,
                                now_ms: 0,
                            })
                            .await
                            .map_err(|err| invalid(err.to_string()))?,
                    )
                }
                "board_complete" => {
                    let Some((job_id, attempt_id, claim_token)) = attempt else {
                        return Err(ProtoError::new(ErrorCode::Denied, "not a worker attempt"));
                    };
                    let args: CompleteArgs = decode(arguments)?;
                    result(
                        &board
                            .complete(CompleteParams { job_id, attempt_id, claim_token, artifacts: args.artifacts, now_ms: 0 })
                            .await
                            .map_err(|err| invalid(err.to_string()))?,
                    )
                }
                "board_review" if reviewer => {
                    let args: ReviewArgs = decode(arguments)?;
                    let job = board.show(args.job_id.clone()).await.map_err(|err| invalid(err.to_string()))?;
                    if job.run_id != run_id {
                        return Err(ProtoError::new(ErrorCode::Denied, "job is outside this run"));
                    }
                    result(
                        &board
                            .review(ReviewParams {
                                job_id: args.job_id,
                                attempt_id: args.attempt_id,
                                reviewer: actor,
                                accepted: args.accepted,
                                evidence: args.evidence,
                                expected_version: job.version,
                                now_ms: 0,
                            })
                            .await
                            .map_err(|err| invalid(err.to_string()))?,
                    )
                }
                _ => Err(ProtoError::new(ErrorCode::MethodNotFound, "board tool is unavailable")),
            }
        })
    }
}
