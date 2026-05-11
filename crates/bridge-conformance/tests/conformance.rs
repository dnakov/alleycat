//! Conformance test entry points. Each `#[ignore]`d test drives one target
//! through the canonical scenario; the aggregate `conformance_diff` runs all
//! bridge targets and diffs each bridge against the codex reference.
//!
//! Run with:
//!   cargo test -p alleycat-bridge-conformance -- --ignored --nocapture
//!
//! Without prereqs (no codex, no pi/claude/opencode/droid CLIs, no API keys) the
//! suite still passes — each test prints `skipped: <reason>` and exits.

use std::path::PathBuf;
use std::process::Command as StdCommand;

use alleycat_bridge_conformance::{
    TargetId, Transcript, cache,
    diff::{self, ConformanceReport, Finding},
    prereq::{self, Prereq, SkipReason},
    scenario, streaming,
    targets::{self, TargetSpawn},
};

#[tokio::test]
#[ignore = "live conformance — requires `codex` CLI on PATH"]
async fn conformance_codex() {
    run_target(TargetId::Codex).await;
}

#[tokio::test]
#[ignore = "live conformance — requires pi-coding-agent on PATH"]
async fn conformance_pi() {
    run_target(TargetId::Pi).await;
}

#[tokio::test]
#[ignore = "live conformance — requires `amp` CLI"]
async fn conformance_amp() {
    run_target(TargetId::Amp).await;
}

#[tokio::test]
#[ignore = "live conformance — requires `claude` CLI on PATH"]
async fn conformance_claude() {
    run_target(TargetId::Claude).await;
}

#[tokio::test]
#[ignore = "live conformance — requires `opencode` CLI"]
async fn conformance_opencode() {
    run_target(TargetId::Opencode).await;
}

#[tokio::test]
#[ignore = "live conformance — requires `droid` CLI"]
async fn conformance_droid() {
    run_target(TargetId::Droid).await;
}

#[tokio::test]
#[ignore = "live conformance — requires `hermes` CLI or gateway"]
async fn conformance_hermes() {
    run_target(TargetId::Hermes).await;
}

/// Aggregate test: capture transcripts from every available target and
/// diff each non-codex target against codex (the reference). Skips any
/// target whose prereqs aren't met. Skips entirely if codex itself is
/// unavailable.
#[tokio::test]
#[ignore = "live conformance — runs all targets and diffs them"]
async fn conformance_diff_all_against_codex() {
    let codex_transcript = match drive_fresh(TargetId::Codex).await {
        DriveOutcome::Ran(t) => t,
        DriveOutcome::Skipped(reason) => {
            eprintln!("conformance_diff: codex skipped, no reference; aborting: {reason}");
            return;
        }
        DriveOutcome::Failed(err) => {
            panic!("codex (reference) target failed: {err:#}");
        }
    };
    eprintln!(
        "conformance_diff: codex captured {} frames",
        codex_transcript.frames.len()
    );

    let mut had_findings = false;
    for target in [
        TargetId::Pi,
        TargetId::Amp,
        TargetId::Claude,
        TargetId::Opencode,
        TargetId::Droid,
        TargetId::Hermes,
    ] {
        match drive_fresh(target).await {
            DriveOutcome::Ran(t) => {
                eprintln!(
                    "conformance_diff: {target} captured {} frames",
                    t.frames.len()
                );
                let report = diff::compare(&codex_transcript, &t);
                if !report.is_clean() {
                    had_findings = true;
                    eprintln!("{report}");
                } else {
                    eprintln!("conformance_diff: {target} clean");
                }
            }
            DriveOutcome::Skipped(reason) => {
                eprintln!("conformance_diff: {target} skipped: {reason}");
            }
            DriveOutcome::Failed(err) => {
                eprintln!("conformance_diff: {target} target failed: {err:#}");
                had_findings = true;
            }
        }
    }
    assert!(
        !had_findings,
        "one or more bridges diverged from codex (see findings above)"
    );
}

// ============================================================================
// helpers
// ============================================================================

enum DriveOutcome {
    Ran(Transcript),
    Skipped(String),
    Failed(anyhow::Error),
}

async fn run_target(target: TargetId) {
    match drive(target).await {
        DriveOutcome::Ran(t) => {
            eprintln!("conformance({target}): {} frames", t.frames.len());
            let mut findings = diff::schema_only(&t);
            findings.extend(streaming::check(&t));
            if !findings.is_empty() {
                let report = ConformanceReport { target, findings };
                panic!("schema/streaming findings:\n{report}");
            }
        }
        DriveOutcome::Skipped(reason) => {
            eprintln!("conformance({target}): skipped — {reason}");
        }
        DriveOutcome::Failed(err) => {
            panic!("conformance({target}) target failed: {err:#}");
        }
    }
}

async fn drive(target: TargetId) -> DriveOutcome {
    drive_with_options(target, true, true).await
}

async fn drive_fresh(target: TargetId) -> DriveOutcome {
    drive_with_options(target, false, false).await
}

async fn drive_with_options(
    target: TargetId,
    reuse_thread_cache: bool,
    use_stable_cwd: bool,
) -> DriveOutcome {
    let prereq = match prereq::check(target).await {
        Ok(p) => p,
        Err(SkipReason::Reason(s)) => return DriveOutcome::Skipped(s),
    };

    let spawn_opts = match build_spawn(target, &prereq, use_stable_cwd) {
        Ok(s) => s,
        Err(err) => return DriveOutcome::Failed(err),
    };

    let mut handle = match targets::spawn(spawn_opts.clone()).await {
        Ok(h) => h,
        Err(err) => return DriveOutcome::Failed(err),
    };
    let mut cfg = scenario::ScenarioConfig::for_target(target, spawn_opts.cwd.clone());
    cfg.reuse_thread_cache = reuse_thread_cache;
    let transcript = match scenario::run(&mut handle.client, &cfg, target).await {
        Ok(t) => t,
        Err(err) => return DriveOutcome::Failed(err),
    };
    if let Some(dir) = std::env::var_os("BRIDGE_CONFORMANCE_DUMP_DIR") {
        if let Err(err) = dump_transcript(std::path::Path::new(&dir), &transcript) {
            eprintln!("conformance({target}): dump failed: {err:#}");
        }
    }
    DriveOutcome::Ran(transcript)
}

fn dump_transcript(
    dir: &std::path::Path,
    transcript: &alleycat_bridge_conformance::Transcript,
) -> anyhow::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.jsonl", transcript.target));
    let mut f = std::fs::File::create(&path)?;
    for frame in &transcript.frames {
        let line = serde_json::to_string(frame)?;
        writeln!(f, "{line}")?;
    }
    Ok(())
}

fn build_spawn(
    target: TargetId,
    prereq: &Prereq,
    use_stable_cwd: bool,
) -> anyhow::Result<TargetSpawn> {
    // Stable cwd lives at ~/.cache/alleycat-bridge-conformance/cwd/. Reusing
    // it run-to-run lets us also reuse the per-target thread id (cwd is part
    // of every bridge's thread-id binding). On a machine without $HOME we
    // fall back to a tempdir, which means thread ids won't be reusable —
    // acceptable degradation.
    let cwd = if use_stable_cwd {
        match cache::stable_cwd() {
            Ok(p) => p,
            Err(_) => tempfile::TempDir::new()?.keep(),
        }
    } else {
        tempfile::TempDir::new()?.keep()
    };
    Ok(match (target, prereq) {
        (TargetId::Codex, Prereq::Codex { bin }) => TargetSpawn {
            target,
            bridge_bin: None,
            backend_bin: Some(bin.clone()),
            cwd,
        },
        (TargetId::Pi, Prereq::Pi { bin }) => TargetSpawn {
            target,
            bridge_bin: Some(workspace_bin("alleycat-pi-bridge")?),
            backend_bin: Some(bin.clone()),
            cwd,
        },
        (TargetId::Amp, Prereq::Amp { bin }) => TargetSpawn {
            target,
            bridge_bin: Some(workspace_bin("alleycat-amp-bridge")?),
            backend_bin: Some(bin.clone()),
            cwd,
        },
        (TargetId::Claude, Prereq::Claude { bin }) => TargetSpawn {
            target,
            bridge_bin: Some(workspace_bin("alleycat-claude-bridge")?),
            backend_bin: Some(bin.clone()),
            cwd,
        },
        (TargetId::Opencode, Prereq::Opencode { bin }) => TargetSpawn {
            target,
            bridge_bin: Some(workspace_bin("alleycat-opencode-bridge")?),
            backend_bin: Some(bin.clone()),
            cwd,
        },
        (TargetId::Droid, Prereq::Droid { bin }) => TargetSpawn {
            target,
            bridge_bin: Some(workspace_bin("alleycat-droid-bridge")?),
            backend_bin: Some(bin.clone()),
            cwd,
        },
        (TargetId::Hermes, Prereq::Hermes { bin }) => TargetSpawn {
            target,
            bridge_bin: Some(workspace_bin("alleycat-hermes-bridge")?),
            backend_bin: Some(bin.clone()),
            cwd,
        },
        (t, p) => anyhow::bail!("prereq {p:?} doesn't match target {t:?}"),
    })
}

/// Resolve a workspace-sibling binary by building it via cargo. We do this
/// at test time because `CARGO_BIN_EXE_<name>` is only set for binaries
/// belonging to the *same* package as the integration test. The build is
/// idempotent — cargo no-ops when already up to date.
fn workspace_bin(name: &str) -> anyhow::Result<PathBuf> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .ancestors()
        .find(|p| p.join("Cargo.lock").is_file())
        .unwrap_or(&manifest_dir)
        .to_path_buf();

    let status = StdCommand::new(&cargo)
        .arg("build")
        .arg("--package")
        .arg(name)
        .arg("--bin")
        .arg(name)
        .current_dir(&workspace_root)
        .status()?;
    if !status.success() {
        anyhow::bail!("cargo build --bin {name} failed");
    }

    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root.join("target"));
    // Conformance tests build in dev profile.
    let candidate = target_dir
        .join("debug")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    if !candidate.is_file() {
        anyhow::bail!("expected binary at {} after build", candidate.display());
    }
    Ok(candidate)
}

// Suppress warnings if the test binary is built without one of the targets
// having any prereq path resolved.
#[allow(dead_code)]
fn _unused_imports_silencer(_f: &Finding) {}
