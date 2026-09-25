# Tier 3 design (not run in W22)

ADR 0024 reserves public-suite runs for releases. Use Harbor with a pinned version and an
installed-agent adapter that launches `aim run` inside each Harbor task container. The adapter
must write ATIF events for turns, tools, result, and provider usage. A sidecar recording proxy
receives the agent's Responses or Chat Completions traffic and attaches numeric usage to the
trajectory; ATIF usage copied from aim's own log is only a cross-check.

Run a predeclared stratified subset of Terminal-Bench 4.0, SWE-Atlas QnA, DeepSWE 1.1, and
SWE-bench Verified. Keep task IDs, container images, evaluator revisions, model, effort, tool
permissions, network policy, and timeout in a release manifest. Run the same images and proxy
upstream for aim and Harbor's Codex/Pi adapters. Preserve each suite's independent grader and
unmodified task data. Rotate agent order, use fresh containers and caches, and bound concurrency
to three. Capture every task outcome, error, provider usage, wall time, peak memory, image digest,
binary hash, and ATIF artifact.

Report paired task flips and confidence intervals before ITE or cost per passed task. A failed
or ungraded task still contributes its usage to the numerator. Refuse a headline comparison if
the model or upstream differs, provider usage is missing, or a task grader cannot be reproduced.
Measure WebSocket transport in a separate cohort from common HTTP/SSE.

No public suite was downloaded or run for W22. The implementation pattern is documented in
`docs/research/landscape.md` §I1 and the pinned Unreal Harbor adapter reference named there.
