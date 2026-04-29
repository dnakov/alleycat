//! Compare a target transcript against the codex (reference) transcript.
//!
//! Three layers of comparison:
//!  - Schema: every frame must round-trip through the typed `codex-proto`
//!    structs (delegated to [`crate::schema`]).
//!  - Method-presence: methods that succeed on codex must succeed on the
//!    target — except those documented in [`KnownDivergence`].
//!  - Notification pattern: for each step, the *kinds* of notifications
//!    emitted by the target must match codex's (modulo allowlist), and
//!    where both emit a given kind, their key-fingerprints must match.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::schema;
use crate::{Frame, FrameKind, TargetId, Transcript};

/// Documented gaps where a target legitimately diverges from codex.
///
/// Bootstrapped from the manual exploration of each bridge's dispatcher.
/// Adding support to a bridge means *removing* the corresponding entry —
/// silent passes are not allowed.
#[derive(Debug, Clone)]
pub struct KnownDivergence {
    pub target: TargetId,
    /// Methods that may return an error (any code) on this target but
    /// succeed on codex. Also skips key-fingerprint comparison entirely
    /// for these methods — use only for methods whose response shape is
    /// fundamentally different (e.g., bridges proxy a different agent's
    /// config tree).
    pub skipped_methods: &'static [&'static str],
    /// Notification kinds whose presence/absence/shape may differ in
    /// either direction.
    pub skipped_notifications: &'static [&'static str],
    /// Per-method field-path allowlist: paths in this list are excluded
    /// from the missing/extra key fingerprint diff. Use this for fields
    /// like `permissionProfile`, `agentNickname`, `phase` that bridges
    /// don't populate but the rest of the response shape *does* match
    /// codex — `skipped_methods` would silence too much.
    pub field_path_divergences: &'static [(&'static str, &'static [&'static str])],
}

impl KnownDivergence {
    pub fn for_target(target: TargetId) -> Self {
        // Bridges proxy *different* coding agents than codex. Methods listed
        // here legitimately diverge in response shape (codex populates
        // fields the underlying agent has no equivalent for) but are not
        // bugs:
        //
        //  - `account/read` — codex tracks a chatgpt account; bridges don't.
        //  - `collaborationMode/list`, `experimentalFeature/list`,
        //    `mcpServerStatus/list` — codex-specific feature surfaces.
        //  - `config/read` — each agent has its own (much smaller) config
        //    tree.
        //  - `model/list`, `skills/list` — codex enriches each entry with
        //    upgrade/availability/icon metadata the bridges don't track.
        //  - `thread/start` / `thread/list` / `thread/read` — bridges
        //    don't synthesize codex-specific defaults like
        //    `permissionProfile`, `serviceTier`, `reasoningEffort` when
        //    the upstream agent doesn't expose them.
        // Methods whose entire response is fundamentally different between
        // codex and the bridge (different agent's config tree, codex-only
        // marketplaces/feature flags, etc.). thread/start/read/list/resume
        // are *not* here — those are field-level allowlisted below so we
        // still compare the parts that do match.
        // `account/read` and `model/list` were previously here but are now
        // handled by the bridges (Account::ApiKey and per-provider model
        // catalogs respectively); they're compared field-by-field instead.
        const SHAPE_DIVERGENT_RESPONSES: &[&str] = &[
            "collaborationMode/list",
            "experimentalFeature/list",
            "mcpServerStatus/list",
            "config/read",
            "skills/list",
        ];

        // Field paths bridges may legitimately not populate. Applied to
        // missing/extra both ways. Keys derive from `schema::fingerprint`
        // (dotted, with `[]` for array elements).
        //
        // Common across all three bridges:
        //  - `permissionProfile`/`reasoningEffort`/`serviceTier` are
        //    Option<T> on `ThreadResumeResponse` etc.; bridges return None
        //    when the underlying agent has no equivalent.
        //  - `thread.agentNickname` / `thread.agentRole` are codex-only.
        //  - `thread.turns[].items[].phase` is codex's per-message phase
        //    metadata; bridges don't track it.
        //  - `thread.turns[].items[].summary` is bridge reasoning content
        //    serialized as a list (matching codex's Reasoning shape) but
        //    populated where codex emits empty.
        //  - `thread.turns[].durationMs` may be absent if the bridge can't
        //    reconstruct it from persisted state.
        //  - `thread.turns[].items[].content[].text_elements` — codex's
        //    snake_case `text_elements`; bridges that synthesize turn
        //    history from persisted state may omit empty arrays.
        // Field paths bridges legitimately have empty/null content for —
        // codex populates them from its own (chatgpt-only) feature surfaces.
        // The fingerprint walker treats `null` and missing identically (it
        // skips both), so these are *content* divergences, not schema
        // divergences. The fields ARE present on every bridge response,
        // emitted as `null`; codex just fills them with content.
        //
        //  - `permissionProfile` / `permissionProfile.type`: codex has named
        //    permission profiles ("disabled", per-tool overrides) the bridges
        //    don't model.
        //  - `reasoningEffort`: codex defaults "high" on every thread; the
        //    underlying agents (pi/claude/opencode) expose this differently
        //    (or not at all) and bridges don't synthesize a default.
        //  - `serviceTier`: codex's chatgpt-tier metadata.
        const COMMON_FIELD_DIVERGENCES: &[(&str, &[&str])] = &[
            (
                "thread/start",
                &[
                    //opencode auto-generates a thread title from the model
                    // output; codex never names a thread on creation.
                    "thread.name",
                    // Same CommandExecution shape divergence as thread/read
                    // (see comment there) — applies whenever a turn with a
                    // tool call is replayed back in a thread/start response.
                    "thread.turns[].items[].command",
                    "thread.turns[].items[].cwd",
                    "thread.turns[].items[].source",
                    "thread.turns[].items[].status",
                    "thread.turns[].items[].aggregatedOutput",
                    "thread.turns[].items[].commandActions",
                    "thread.turns[].items[].commandActions[]",
                    "thread.turns[].items[].phase",
                    "thread.turns[].items[].summary",
                    "thread.turns[].items[].summary[]",
                ],
            ),
            (
                "thread/resume",
                &[
                    // `serviceTier` is the OpenAI account tier (upstream
                    // schema: `"fast" | "flex" | null`). Codex emits the
                    // user's actual tier; bridges legitimately have no
                    // concept and emit null. Real content gap, valid
                    // wire shape (won't fail upstream-schema check).
                    "serviceTier",
                    //codex's per-message phase metadata; bridges have no
                    // analogue. claude's stream-json doesn't carry a
                    // turn-internal phase field; pi/opencode similarly.
                    "thread.turns[].items[].phase",
                    // pi/opencode emit Reasoning items with empty `summary`
                    // when their underlying model emits a thinking block;
                    // codex's reference response for "Reply with OK" doesn't
                    // reason. Different prompts → different reasoning, not
                    // a wire-shape bug.
                    "thread.turns[].items[].summary",
                    "thread.turns[].items[].summary[]",
                    // Same CommandExecution divergence as thread/read.
                    "thread.turns[].items[].command",
                    "thread.turns[].items[].cwd",
                    "thread.turns[].items[].source",
                    "thread.turns[].items[].status",
                    "thread.turns[].items[].aggregatedOutput",
                    "thread.turns[].items[].commandActions",
                    "thread.turns[].items[].commandActions[]",
                    "thread.name",
                ],
            ),
            (
                "thread/read",
                &[
                    "thread.turns[].items[].phase",
                    "thread.turns[].items[].summary",
                    "thread.turns[].items[].summary[]",
                    "thread.name",
                    // codex's gpt-5.5 reads files via "unifiedExecStartup"
                    // (codex-internal sandbox exec, not an agent tool call),
                    // and codex prunes those CommandExecution items from
                    // turn history. Bridges proxy models that issue
                    // traditional `Bash` tool calls — those land in turn
                    // history with the canonical CommandExecution shape.
                    // The shape itself matches codex when codex DOES persist
                    // a CommandExecution; this allowlist only acknowledges
                    // the per-model decision of whether to include them at
                    // all. Same fields apply on `thread/start` and
                    // `thread/resume` since both can replay a tool turn.
                    "thread.turns[].items[].command",
                    "thread.turns[].items[].cwd",
                    "thread.turns[].items[].source",
                    "thread.turns[].items[].status",
                    "thread.turns[].items[].aggregatedOutput",
                    "thread.turns[].items[].commandActions",
                    "thread.turns[].items[].commandActions[]",
                ],
            ),
            (
                "account/read",
                &[
                    // Populated only on the `Chatgpt` Account variant. Bridges
                    // return `Account::ApiKey {}` which carries no identity
                    // metadata.
                    "account.email",
                    "account.planType",
                ],
            ),
            (
                "model/list",
                &[
                    // codex announces newly-shipped models via these fields
                    // (e.g. the GPT-5.5 availability nux, upgrade
                    // suggestions for older models). Bridges proxy other
                    // agents that don't ship marketing copy.
                    "data[].availabilityNux",
                    "data[].availabilityNux.message",
                    "data[].upgrade",
                    "data[].upgradeInfo",
                    "data[].upgradeInfo.model",
                    "data[].upgradeInfo.upgradeCopy",
                    "data[].upgradeInfo.modelLink",
                    "data[].upgradeInfo.migrationMarkdown",
                ],
            ),
            (
                "thread/list",
                &[
                    "data[].agentNickname",
                    "data[].agentRole",
                    // gitInfo is populated by codex (it shells out to
                    // `git`); bridges don't extract repo metadata.
                    "data[].gitInfo",
                    "data[].gitInfo.branch",
                    "data[].gitInfo.originUrl",
                    "data[].gitInfo.sha",
                    // codex paginates at 25 entries; bridges return all
                    // matching threads in one page so the cursors are null
                    // (and `skip_serializing_if`-omitted from the wire).
                    "nextCursor",
                    "backwardsCursor",
                ],
            ),
        ];
        // codex emits these notifications around its own MCP/account/
        // session lifecycle; the bridges have no equivalent so they never
        // fire (or have nothing to report when they do).
        const SHAPE_DIVERGENT_NOTIFICATIONS: &[&str] = &[
            "mcpServer/startupStatus/updated",
            "account/rateLimits/updated",
            // `tokenUsage` differs because bridges report their own
            // token counts which often lack `modelContextWindow`.
            "thread/tokenUsage/updated",
            // `item/started`/`item/completed` payload shapes diverge for
            // items codex enriches (e.g., `item.phase`); the streaming
            // check in `crate::streaming` still validates the lifecycle.
            "item/started",
            "item/completed",
            // codex emits `thread/status/changed` whenever it transitions
            // the session between idle and active; bridges that don't
            // model that state machine just stay implicit.
            "thread/status/changed",
            // codex surfaces startup warnings from its own MCP/sandbox
            // boot path; bridges don't have these warning sources.
            "warning",
            // claude/pi stream bash stdout incrementally as
            // `item/commandExecution/outputDelta`; codex's unifiedExec path
            // emits begin+end with the full aggregatedOutput on the end
            // event (no deltas). Per-byte vs final-blob is a streaming
            // mechanism choice, not a wire-shape bug.
            "item/commandExecution/outputDelta",
            // pi/opencode stream model reasoning incrementally; codex's
            // gpt-5.5 doesn't reason (or reasoning is invisible). Whether
            // a reasoning event fires is a per-model decision, not a
            // wire-shape bug.
            "item/reasoning/textDelta",
            "item/reasoning/summaryTextDelta",
            "item/reasoning/summaryPartAdded",
        ];

        match target {
            TargetId::Codex => Self {
                target,
                skipped_methods: &[],
                skipped_notifications: &[],
                field_path_divergences: &[],
            },
            TargetId::Pi => Self {
                target,
                // Pi: review/start unimplemented + all architectural
                // divergences from the const lists above.
                skipped_methods: concat_static(&["review/start"], SHAPE_DIVERGENT_RESPONSES),
                // Pi additionally emits a one-off `configWarning` advising
                // clients that pi-bridge v1 doesn't proxy MCP servers; codex
                // never emits this and there's no equivalent on the codex
                // side, so allowlist the extra.
                skipped_notifications: concat_static(
                    &["configWarning"],
                    SHAPE_DIVERGENT_NOTIFICATIONS,
                ),
                field_path_divergences: COMMON_FIELD_DIVERGENCES,
            },
            TargetId::Claude => Self {
                target,
                skipped_methods: SHAPE_DIVERGENT_RESPONSES,
                skipped_notifications: SHAPE_DIVERGENT_NOTIFICATIONS,
                field_path_divergences: COMMON_FIELD_DIVERGENCES,
            },
            TargetId::Opencode => Self {
                target,
                skipped_methods: concat_static(
                    &[
                        "account/login/start",
                        "account/login/cancel",
                        "account/logout",
                        "mcpServer/oauth/login",
                        "skills/config/write",
                        "configRequirements/read",
                        "thread/turns/list",
                        "review/start",
                    ],
                    SHAPE_DIVERGENT_RESPONSES,
                ),
                skipped_notifications: concat_static(
                    &[
                        "thread/started",
                        "thread/closed",
                        "skills/changed",
                        "turn/diff/updated",
                        "turn/plan/updated",
                        // Opencode reasoning is wired (see translate/events.rs)
                        // but only fires when the underlying model reasons.
                        // Codex's call may not reason in the same turn, so
                        // textDelta on opencode and not codex is fine.
                        "item/reasoning/textDelta",
                        "item/reasoning/summaryTextDelta",
                        "item/reasoning/summaryPartAdded",
                        "item/mcpToolCall/progress",
                        "item/dynamicToolCall/argumentsDelta",
                        // Opencode auto-generates a thread title during the
                        // first turn; codex never does.
                        "thread/name/updated",
                        "model/rerouted",
                        "configWarning",
                        "deprecationNotice",
                        "serverRequest/resolved",
                    ],
                    SHAPE_DIVERGENT_NOTIFICATIONS,
                ),
                field_path_divergences: COMMON_FIELD_DIVERGENCES,
            },
        }
    }
}

/// Const-friendly concatenation of two `&[&'static str]` slices. Returns a
/// leaked static slice so the result satisfies `&'static [&'static str]`. The
/// allocation happens once per process.
fn concat_static(
    a: &'static [&'static str],
    b: &'static [&'static str],
) -> &'static [&'static str] {
    // SAFETY: leaking a Vec produces a slice that lives for the rest of the
    // process — exactly what we want for these allowlist tables.
    let mut out = Vec::with_capacity(a.len() + b.len());
    out.extend_from_slice(a);
    out.extend_from_slice(b);
    Box::leak(out.into_boxed_slice())
}

#[derive(Debug, Clone)]
pub struct ConformanceReport {
    pub target: TargetId,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone)]
pub enum Finding {
    /// Frame failed typed deserialize.
    SchemaError {
        step: String,
        method: String,
        kind: FrameKind,
        message: String,
    },
    /// Frame failed validation against the upstream codex-rs JSON schema.
    /// Independent verification: a violation here is a real wire-spec gap,
    /// not just a drift between bridge output and our `codex-proto` mirror.
    UpstreamSchemaError {
        step: String,
        method: String,
        kind: FrameKind,
        message: String,
    },
    /// Method returned an error on the target but succeeded on codex (and the
    /// method is not in the per-target allowlist).
    UnexpectedError {
        step: String,
        method: String,
        code: i64,
        message: String,
    },
    /// Codex emitted a notification kind during a step that the target did
    /// not emit.
    MissingNotification { step: String, method: String },
    /// Target emitted a notification kind that codex did not emit during the
    /// same step.
    ExtraNotification { step: String, method: String },
    /// Both codex and target emitted a frame for the same step+method but
    /// the populated key set differs. `missing` is keys present in codex but
    /// absent on the target; `extra` is keys present on the target but
    /// absent on codex.
    KeyDifference {
        step: String,
        method: String,
        kind: FrameKind,
        missing: BTreeSet<String>,
        extra: BTreeSet<String>,
    },
}

impl ConformanceReport {
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

impl fmt::Display for ConformanceReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "[conformance:{}] {} finding(s)",
            self.target,
            self.findings.len()
        )?;
        for (i, finding) in self.findings.iter().enumerate() {
            write!(f, "  #{i}: ")?;
            match finding {
                Finding::SchemaError {
                    step,
                    method,
                    kind,
                    message,
                } => {
                    writeln!(
                        f,
                        "schema error in {kind:?} {method} (step={step}): {message}"
                    )?;
                }
                Finding::UpstreamSchemaError {
                    step,
                    method,
                    kind,
                    message,
                } => {
                    writeln!(
                        f,
                        "upstream-schema violation in {kind:?} {method} (step={step}): {message}"
                    )?;
                }
                Finding::UnexpectedError {
                    step,
                    method,
                    code,
                    message,
                } => {
                    writeln!(
                        f,
                        "unexpected error response for {method} (step={step}, code={code}): {message}"
                    )?;
                }
                Finding::MissingNotification { step, method } => {
                    writeln!(f, "missing notification {method} in step {step}")?;
                }
                Finding::ExtraNotification { step, method } => {
                    writeln!(f, "unexpected extra notification {method} in step {step}")?;
                }
                Finding::KeyDifference {
                    step,
                    method,
                    kind,
                    missing,
                    extra,
                } => {
                    writeln!(
                        f,
                        "key fingerprint diff for {kind:?} {method} (step={step}):"
                    )?;
                    if !missing.is_empty() {
                        writeln!(f, "      missing on target: {missing:?}")?;
                    }
                    if !extra.is_empty() {
                        writeln!(f, "      extra on target:   {extra:?}")?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Run all three layers of the conformance check on `target` against
/// `reference`.
pub fn compare(reference: &Transcript, target: &Transcript) -> ConformanceReport {
    let mut report = ConformanceReport {
        target: target.target,
        findings: Vec::new(),
    };
    let div = KnownDivergence::for_target(target.target);

    // Layer 1: schema for every frame on the target.
    for frame in &target.frames {
        let chk = schema::check(frame);
        // Schema deserialize errors are always findings, regardless of the
        // divergence allowlist — a stub returning `null` is the bug we want
        // to surface.
        if let Some(err) = chk.deserialize_error.clone() {
            report.findings.push(Finding::SchemaError {
                step: frame.step.clone(),
                method: frame.method.clone(),
                kind: frame.kind,
                message: err,
            });
        }
        // Independent layer: validate against the upstream codex-rs JSON
        // schemas (skipped silently when the schema dir isn't present).
        // Error frames are exempt — their `result` is null and the schema
        // expects a populated payload.
        if !chk.is_error_response()
            && let Err(err) = crate::upstream_schema::validate(frame)
        {
            report.findings.push(Finding::UpstreamSchemaError {
                step: frame.step.clone(),
                method: frame.method.clone(),
                kind: frame.kind,
                message: err,
            });
        }
        // Error responses for non-allowlisted methods → UnexpectedError.
        if frame.kind == FrameKind::Response {
            if let (Some(code), Some(msg)) = (chk.error_code, chk.error_message) {
                if !div.skipped_methods.contains(&frame.method.as_str()) {
                    report.findings.push(Finding::UnexpectedError {
                        step: frame.step.clone(),
                        method: frame.method.clone(),
                        code,
                        message: msg,
                    });
                }
            }
        }
    }

    // Layer 2 + 3: per-step comparison against codex.
    let ref_by_step = group_by_step(reference);
    let tgt_by_step = group_by_step(target);
    let all_steps: BTreeSet<&str> = ref_by_step
        .keys()
        .chain(tgt_by_step.keys())
        .copied()
        .collect();
    for step in all_steps {
        let ref_frames = ref_by_step.get(step).copied().unwrap_or(&[][..]);
        let tgt_frames = tgt_by_step.get(step).copied().unwrap_or(&[][..]);

        // Notifications: which kinds appeared on each side?
        let ref_notif_kinds: BTreeSet<&str> = ref_frames
            .iter()
            .filter(|f| f.kind == FrameKind::Notification)
            .map(|f| f.method.as_str())
            .collect();
        let tgt_notif_kinds: BTreeSet<&str> = tgt_frames
            .iter()
            .filter(|f| f.kind == FrameKind::Notification)
            .map(|f| f.method.as_str())
            .collect();
        for kind in ref_notif_kinds.difference(&tgt_notif_kinds) {
            if !div.skipped_notifications.contains(kind) {
                report.findings.push(Finding::MissingNotification {
                    step: step.to_string(),
                    method: kind.to_string(),
                });
            }
        }
        for kind in tgt_notif_kinds.difference(&ref_notif_kinds) {
            // `skipped_notifications` documents notification kinds whose
            // presence/absence is allowed to differ in either direction
            // — codex may emit them and the bridge not, or vice versa.
            // Extras outside the allowlist are still surfaced.
            if div.skipped_notifications.contains(kind) {
                continue;
            }
            report.findings.push(Finding::ExtraNotification {
                step: step.to_string(),
                method: kind.to_string(),
            });
        }

        // Per-method/per-kind key fingerprint comparison. We union all
        // fingerprints for a given (method, FrameKind) within the step on
        // each side and compare the unions — captures optional fields that
        // appear on only some entries.
        let ref_fp = group_fingerprints(ref_frames);
        let tgt_fp = group_fingerprints(tgt_frames);
        let mut keys: BTreeSet<&(String, FrameKind)> = BTreeSet::new();
        keys.extend(ref_fp.keys());
        keys.extend(tgt_fp.keys());
        for key in keys {
            let (method, kind) = key.clone();
            let r = ref_fp.get(&key.clone()).cloned().unwrap_or_default();
            let t = tgt_fp.get(&key.clone()).cloned().unwrap_or_default();
            if r.is_empty() && t.is_empty() {
                continue;
            }
            // Skip key-set comparison entirely when:
            //  - the method is in the per-target skipped_methods list
            //    (response frames) — we already either filtered it via
            //    UnexpectedError or accepted it.
            //  - the method (for notifications) is in skipped_notifications.
            let allow_skip = match kind {
                FrameKind::Response => div.skipped_methods.contains(&method.as_str()),
                FrameKind::Notification => div.skipped_notifications.contains(&method.as_str()),
            };
            if allow_skip {
                continue;
            }
            // Field paths the divergence allowlist marks as expected for
            // this method (codex-specific fields the bridge doesn't carry,
            // or vice versa). Subtract from both sides before reporting.
            let allowed_paths: BTreeSet<&str> = div
                .field_path_divergences
                .iter()
                .filter(|(m, _)| *m == method.as_str())
                .flat_map(|(_, paths)| paths.iter().copied())
                .collect();
            let missing: BTreeSet<String> = r
                .difference(&t)
                .filter(|p| !allowed_paths.contains(p.as_str()))
                .cloned()
                .collect();
            let extra: BTreeSet<String> = t
                .difference(&r)
                .filter(|p| !allowed_paths.contains(p.as_str()))
                .cloned()
                .collect();
            if !missing.is_empty() || !extra.is_empty() {
                report.findings.push(Finding::KeyDifference {
                    step: step.to_string(),
                    method: method.clone(),
                    kind,
                    missing,
                    extra,
                });
            }
        }
    }

    report
}

fn group_by_step(t: &Transcript) -> BTreeMap<&str, &[Frame]> {
    let mut out = BTreeMap::new();
    let mut current: &str = "";
    let mut start = 0usize;
    for (i, f) in t.frames.iter().enumerate() {
        if f.step != current {
            if !current.is_empty() {
                out.insert(current, &t.frames[start..i]);
            }
            current = f.step.as_str();
            start = i;
        }
    }
    if !current.is_empty() {
        out.insert(current, &t.frames[start..]);
    }
    out
}

fn group_fingerprints(frames: &[Frame]) -> BTreeMap<(String, FrameKind), BTreeSet<String>> {
    let mut out: BTreeMap<(String, FrameKind), BTreeSet<String>> = BTreeMap::new();
    for f in frames {
        let chk = schema::check(f);
        if !chk.fingerprint.is_empty() {
            let key = (f.method.clone(), f.kind);
            out.entry(key).or_default().extend(chk.fingerprint);
        }
    }
    out
}

/// Run schema validation in isolation (no reference required). Used when a
/// target is the only one available — still surfaces deserialize failures.
pub fn schema_only(transcript: &Transcript) -> Vec<Finding> {
    let mut findings = Vec::new();
    let div = KnownDivergence::for_target(transcript.target);
    for frame in &transcript.frames {
        let chk = schema::check(frame);
        if let Some(err) = chk.deserialize_error.clone() {
            findings.push(Finding::SchemaError {
                step: frame.step.clone(),
                method: frame.method.clone(),
                kind: frame.kind,
                message: err,
            });
        }
        if !chk.is_error_response()
            && let Err(err) = crate::upstream_schema::validate(frame)
        {
            findings.push(Finding::UpstreamSchemaError {
                step: frame.step.clone(),
                method: frame.method.clone(),
                kind: frame.kind,
                message: err,
            });
        }
        if frame.kind == FrameKind::Response {
            if let (Some(code), Some(msg)) = (chk.error_code, chk.error_message.clone()) {
                if !div.skipped_methods.contains(&frame.method.as_str()) {
                    findings.push(Finding::UnexpectedError {
                        step: frame.step.clone(),
                        method: frame.method.clone(),
                        code,
                        message: msg,
                    });
                }
            }
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frame(step: &str, method: &str, kind: FrameKind, raw: serde_json::Value) -> Frame {
        Frame {
            step: step.to_string(),
            method: method.to_string(),
            kind,
            raw,
        }
    }

    #[test]
    fn identical_transcripts_have_no_findings() {
        let mut a = Transcript::new(TargetId::Codex);
        let mut b = Transcript::new(TargetId::Pi);
        let resp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "userAgent": "x", "codexHome": "/tmp",
                "platformFamily": "unix", "platformOs": "linux"
            }
        });
        a.push(frame(
            "initialize",
            "initialize",
            FrameKind::Response,
            resp.clone(),
        ));
        b.push(frame("initialize", "initialize", FrameKind::Response, resp));
        let report = compare(&a, &b);
        assert!(report.is_clean(), "{report}");
    }

    #[test]
    fn missing_field_is_flagged() {
        let mut a = Transcript::new(TargetId::Codex);
        let mut b = Transcript::new(TargetId::Pi);
        a.push(frame("step", "initialize", FrameKind::Response, json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"userAgent": "x", "codexHome": "/h", "platformFamily": "u", "platformOs": "l"}
        })));
        b.push(frame(
            "step",
            "initialize",
            FrameKind::Response,
            json!({
                "jsonrpc": "2.0", "id": 1,
                "result": {"userAgent": "x", "platformFamily": "u", "platformOs": "l"}
            }),
        ));
        let report = compare(&a, &b);
        // Pi's response is missing "codexHome" -> fingerprint diff *and*
        // schema (typed) failure.
        assert!(!report.is_clean());
        let has_key_diff = report.findings.iter().any(|f| {
            matches!(
                f, Finding::KeyDifference { missing, .. } if missing.contains("codexHome")
            )
        });
        assert!(has_key_diff, "{report}");
    }

    #[test]
    fn opencode_known_divergence_is_quiet() {
        let mut a = Transcript::new(TargetId::Codex);
        let b = Transcript::new(TargetId::Opencode);
        a.push(frame(
            "turn/start",
            "thread/started",
            FrameKind::Notification,
            json!({
                "jsonrpc": "2.0", "method": "thread/started",
                "params": {"thread": {}}
            }),
        ));
        // Opencode does not emit thread/started — that's allowlisted.
        let report = compare(&a, &b);
        // We do still get a SchemaError for codex's empty thread struct, but
        // crucially no MissingNotification on opencode.
        assert!(!report
            .findings
            .iter()
            .any(|f| matches!(f, Finding::MissingNotification { method, .. } if method == "thread/started")));
    }
}
