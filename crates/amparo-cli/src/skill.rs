//! The `amparo skill` subcommand — operator-side skill management (M6c).
//!
//! A skill is a named procedure: preconditions, ordered tool-call steps with
//! fixed arguments, and an expected outcome. Skills come from two places —
//! authored directly by the operator (`add`), or distilled from the notebook
//! as inert candidates (`propose`). Either way nothing executes until
//! [`adopt`], which runs the same gate chain as a tool call: a policy check
//! on `use_skill` **and** human approval showing the full step plan (the
//! adoption record is the audit row, I4).
//!
//! Skill *management* needs no `--growth` flag; skill *execution* does — the
//! `run` loop registers `use_skill` only under `--growth`.
//!
//! Exit codes follow the `amparo run` contract: usage problems exit 2,
//! runtime failures (unreadable candidate, policy refusal, approval denial)
//! exit 1.

use amparo_agent::{ApprovalGate, ApprovalRequest, AutoApprove, AutoDeny};
use amparo_notebook::{
    append_event, append_proposals, append_recheck, check_skill_drift, notebook_dir, read_log,
    read_rechecks, read_uses, retirement_reason, skills_dir, summarize, CheckKind, CheckOutcome,
    CheckRecord, Proposer, RetirementThreshold, SkillLogEvent, SkillSet, RECHECKS_FILE,
};
use amparo_policy::{
    wire::WirePolicyEngine, AllowAllPolicyEngine, DenyAllPolicyEngine, PolicyEngine, PolicyVerdict,
};
use amparo_tools::{
    default_registry, PathPolicy, SkillLibrary, SkillSpec, ToolTrustTier, USE_SKILL,
};
use std::path::Path;
use std::sync::Arc;

use crate::approve::InteractiveApprovalGate;

pub const SKILL_USAGE: &str = "\
amparo skill — manage skills (named procedures adopted behind policy + approval)

USAGE:
  amparo skill add <file.toml> [--workspace DIR]
  amparo skill propose [--min-runs N] [--min-verified-rate P]
             [--workspace DIR] [--tenant T]
  amparo skill list [--workspace DIR] [--tenant T]
  amparo skill show <name> [--workspace DIR] [--tenant T]
  amparo skill adopt <name> [--workspace DIR] [--tenant T]
             [--policy-url URL | --allow-all]
             [--auto-approve | --auto-deny]
  amparo skill check [--workspace DIR] [--tenant T]
             [--policy-url URL | --allow-all]
             [--min-verified-rate P] [--window N] [--dry-run]
             [--trust-ceiling T]
  amparo skill retire <name> [--reason ...] [--workspace DIR] [--tenant T]

COMMANDS:
  add       parse a candidate TOML, validate it against the tool registry,
            and write it to <workspace>/.amparo/skills/candidates/<name>.toml.
            Does not adopt — adoption is a separate, gated step.
  propose   distill recurring high-VERIFIED tool sequences from the notebook
            (<workspace>/.amparo/notebook/records.jsonl) into inert
            candidates. The findings and a TOML template print on stdout;
            proposals are logged to proposals.jsonl. Explicit and opt-in:
            proposals never adopt themselves.
  list      print this tenant's adopted skills (name — description).
  show      render one adopted skill: preconditions, every step with its
            arguments, expected outcome, origin and provenance.
  adopt     adopt a candidate for this tenant: the spec is validated, the
            policy engine judges use_skill <name>, and a human approves the
            full rendered step plan. Approved adoptions append to
            adopted.jsonl (the audit log — last event per name wins).
  check     re-check every adopted skill (M6d): policy drift first (the
            step plan dry-runs through the gate chain — nothing executes,
            no approval), then performance (VERIFIED rate over the last
            --window uses). Retirements append Retire events + re-check
            rows; --dry-run reports only and writes nothing. Exit 0 even
            when retirements fire (cron-able).
  retire    retire an adopted skill for this tenant (the operator lever:
            disable + notify, never delete — the audit log keeps every
            row). Refuses when the skill is not currently adopted.

FLAGS:
  --workspace DIR         workspace root (sets AMPARO_WORKSPACE); skills live
                          under <workspace>/.amparo/skills/
  --tenant T              tenant id (default \"cli\")
  --policy-url URL        wire a remote policy engine for adopt (deny-all
                          default: adoption is refused without --policy-url
                          or --allow-all)
  --allow-all             adopt without policy checks (explicit opt-in)
  --auto-approve          approve the adoption without a human
  --auto-deny             deny the adoption without asking
  --min-runs N            propose: minimum VERIFIED runs per sequence
                          (default 3)
  --min-verified-rate P   propose: minimum verified fraction, 0-1
                          (default 0.8); check: the retirement threshold,
                          0-1 (default 0.5)
  --window N              check: evaluate the last N uses (default 20)
  --dry-run               check: report retirements without writing
                          anything (no Retire events, no re-check rows)
  --trust-ceiling T       check: observational | network | local_mutating |
                          external_effector | system_control (default
                          system_control) — mirror the deployment ceiling
                          for the drift dry-run
  --reason ...            retire: the retirement reason (default
                          \"operator retired\")";

// ─────────────────────────────────────────────── Parsing ─────────────────────

/// The parsed `amparo skill` command.
#[derive(Debug)]
enum Command {
    Add { file: String, flags: SkillFlags },
    Propose(SkillFlags),
    List(SkillFlags),
    Show { name: String, flags: SkillFlags },
    Adopt { name: String, flags: SkillFlags },
    Check(SkillFlags),
    Retire { name: String, flags: SkillFlags },
}

/// Outcome of parsing: print usage (exit 0), a usage error (exit 2), or a
/// command to execute.
#[derive(Debug)]
enum ParsedSkill {
    Help,
    Error(String),
    Run(Command),
}

/// Parsed `amparo skill` flags. `tenant` defaults to `\"cli\"` at use;
/// `min_runs`/`min_verified_rate` default in the proposer (0.8) and the
/// checker (0.5); `window`/`trust_ceiling` default in the checker (20 /
/// system_control).
#[derive(Debug)]
struct SkillFlags {
    workspace: Option<String>,
    tenant: Option<String>,
    policy_url: Option<String>,
    allow_all: bool,
    auto_approve: bool,
    auto_deny: bool,
    min_runs: Option<usize>,
    min_verified_rate: Option<f64>,
    window: Option<usize>,
    dry_run: bool,
    trust_ceiling: ToolTrustTier,
    reason: Option<String>,
}

impl Default for SkillFlags {
    fn default() -> Self {
        Self {
            workspace: None,
            tenant: None,
            policy_url: None,
            allow_all: false,
            auto_approve: false,
            auto_deny: false,
            min_runs: None,
            min_verified_rate: None,
            window: None,
            dry_run: false,
            trust_ceiling: ToolTrustTier::SystemControl,
            reason: None,
        }
    }
}

impl SkillFlags {
    fn tenant(&self) -> &str {
        self.tenant.as_deref().unwrap_or("cli")
    }
}

/// Which subcommand-only flags [`parse_flags`] accepts.
#[derive(Clone, Copy, PartialEq)]
enum FlagExtras {
    /// No subcommand-only flags.
    None,
    /// `--min-runs`, `--min-verified-rate`.
    Propose,
    /// `--min-verified-rate`, `--window`, `--dry-run`, `--trust-ceiling`.
    Check,
    /// `--reason`.
    Retire,
}

/// Parse the flags every skill subcommand shares (`extras` enables the
/// subcommand-only flags). `--help`/`-h` are handled before this runs.
fn parse_flags(args: Vec<String>, extras: FlagExtras) -> Result<(SkillFlags, Vec<String>), String> {
    let mut flags = SkillFlags::default();
    let mut positional = Vec::new();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--workspace" => match iter.next() {
                Some(dir) => flags.workspace = Some(dir),
                None => return Err("--workspace requires a directory".into()),
            },
            "--tenant" => match iter.next() {
                Some(tenant) => flags.tenant = Some(tenant),
                None => return Err("--tenant requires a tenant id".into()),
            },
            "--policy-url" => match iter.next() {
                Some(url) => flags.policy_url = Some(url),
                None => return Err("--policy-url requires a URL".into()),
            },
            "--allow-all" => flags.allow_all = true,
            "--auto-approve" => flags.auto_approve = true,
            "--auto-deny" => flags.auto_deny = true,
            "--min-runs" if extras == FlagExtras::Propose => match iter.next() {
                Some(n) => match n.parse::<usize>() {
                    Ok(runs) if runs > 0 => flags.min_runs = Some(runs),
                    _ => return Err(format!("--min-runs must be a positive integer, got '{n}'")),
                },
                None => return Err("--min-runs requires a number".into()),
            },
            "--min-verified-rate"
                if extras == FlagExtras::Propose || extras == FlagExtras::Check =>
            {
                match iter.next() {
                    Some(p) => match p.parse::<f64>() {
                        Ok(rate) if (0.0..=1.0).contains(&rate) => {
                            flags.min_verified_rate = Some(rate)
                        }
                        _ => {
                            return Err(format!(
                                "--min-verified-rate must be between 0 and 1, got '{p}'"
                            ))
                        }
                    },
                    None => return Err("--min-verified-rate requires a fraction".into()),
                }
            }
            "--window" if extras == FlagExtras::Check => match iter.next() {
                Some(n) => match n.parse::<usize>() {
                    Ok(window) if window > 0 => flags.window = Some(window),
                    _ => return Err(format!("--window must be a positive integer, got '{n}'")),
                },
                None => return Err("--window requires a number".into()),
            },
            "--dry-run" if extras == FlagExtras::Check => flags.dry_run = true,
            "--trust-ceiling" if extras == FlagExtras::Check => match iter.next() {
                Some(tier) => match tier.as_str() {
                    "observational" => flags.trust_ceiling = ToolTrustTier::Observational,
                    "network" => flags.trust_ceiling = ToolTrustTier::Network,
                    "local_mutating" => flags.trust_ceiling = ToolTrustTier::LocalMutating,
                    "external_effector" => flags.trust_ceiling = ToolTrustTier::ExternalEffector,
                    "system_control" => flags.trust_ceiling = ToolTrustTier::SystemControl,
                    other => return Err(format!("unknown trust tier {other}")),
                },
                None => return Err("--trust-ceiling requires a tier".into()),
            },
            "--reason" if extras == FlagExtras::Retire => match iter.next() {
                Some(reason) => flags.reason = Some(reason),
                None => return Err("--reason requires a value".into()),
            },
            other if other.starts_with('-') => {
                return Err(format!("unknown flag {other}; see `amparo skill --help`"))
            }
            other => positional.push(other.to_string()),
        }
    }
    if flags.policy_url.is_some() && flags.allow_all {
        return Err("--policy-url and --allow-all are mutually exclusive".into());
    }
    if flags.auto_approve && flags.auto_deny {
        return Err("--auto-approve and --auto-deny are mutually exclusive".into());
    }
    Ok((flags, positional))
}

/// Parse `amparo skill` arguments. Never panics and never exits.
fn parse(args: Vec<String>) -> ParsedSkill {
    if args.is_empty() {
        return ParsedSkill::Error("missing subcommand; see `amparo skill --help`".into());
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return ParsedSkill::Help;
    }
    let mut iter = args.into_iter();
    let command = iter.next().expect("checked non-empty");
    let rest: Vec<String> = iter.collect();
    let result = match command.as_str() {
        "add" => {
            parse_flags(rest, FlagExtras::None).and_then(|(flags, positional)| {
                match positional.len() {
                    1 => Ok(Command::Add {
                        file: positional.into_iter().next().expect("one positional"),
                        flags,
                    }),
                    _ => Err("amparo skill add requires exactly one candidate file".into()),
                }
            })
        }
        "propose" => parse_flags(rest, FlagExtras::Propose).and_then(|(flags, positional)| {
            if positional.is_empty() {
                Ok(Command::Propose(flags))
            } else {
                Err("amparo skill propose takes no positional arguments".into())
            }
        }),
        "list" => parse_flags(rest, FlagExtras::None).and_then(|(flags, positional)| {
            if positional.is_empty() {
                Ok(Command::List(flags))
            } else {
                Err("amparo skill list takes no positional arguments".into())
            }
        }),
        "show" => parse_flags(rest, FlagExtras::None).and_then(|(flags, positional)| {
            match positional.len() {
                1 => Ok(Command::Show {
                    name: positional.into_iter().next().expect("one positional"),
                    flags,
                }),
                _ => Err("amparo skill show requires a skill name".into()),
            }
        }),
        "adopt" => parse_flags(rest, FlagExtras::None).and_then(|(flags, positional)| {
            match positional.len() {
                1 => Ok(Command::Adopt {
                    name: positional.into_iter().next().expect("one positional"),
                    flags,
                }),
                _ => Err("amparo skill adopt requires a skill name".into()),
            }
        }),
        "check" => parse_flags(rest, FlagExtras::Check).and_then(|(flags, positional)| {
            if positional.is_empty() {
                Ok(Command::Check(flags))
            } else {
                Err("amparo skill check takes no positional arguments".into())
            }
        }),
        "retire" => parse_flags(rest, FlagExtras::Retire).and_then(|(flags, positional)| {
            match positional.len() {
                1 => Ok(Command::Retire {
                    name: positional.into_iter().next().expect("one positional"),
                    flags,
                }),
                _ => Err("amparo skill retire requires a skill name".into()),
            }
        }),
        other => Err(format!(
            "unknown skill subcommand {other}; see `amparo skill --help`"
        )),
    };
    match result {
        Ok(command) => ParsedSkill::Run(command),
        Err(message) => ParsedSkill::Error(message),
    }
}

// ─────────────────────────────────────────────── Dispatch ────────────────────

/// Entry point for `amparo skill` (exit codes: 0 ok/help, 2 usage, 1 runtime).
pub async fn dispatch(args: impl Iterator<Item = String>) {
    match parse(args.collect()) {
        ParsedSkill::Help => println!("{SKILL_USAGE}"),
        ParsedSkill::Error(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
        ParsedSkill::Run(command) => {
            if let Err(message) = execute(command).await {
                eprintln!("amparo skill: {message}");
                std::process::exit(1);
            }
        }
    }
}

/// Run one parsed command. `Err` is a runtime failure (exit 1).
async fn execute(command: Command) -> Result<(), String> {
    match command {
        Command::Add { file, flags } => add(&file, &flags),
        Command::Propose(flags) => propose(&flags),
        Command::List(flags) => list(&flags),
        Command::Show { name, flags } => show(&name, &flags),
        Command::Adopt { name, flags } => adopt(&name, &flags).await,
        Command::Check(flags) => check(&flags).await,
        Command::Retire { name, flags } => retire(&name, &flags),
    }
}

/// Apply `--workspace` (process-wide, the run.rs pattern) and return the
/// workspace root.
fn setup_workspace(flags: &SkillFlags) -> std::path::PathBuf {
    if let Some(dir) = &flags.workspace {
        std::env::set_var("AMPARO_WORKSPACE", dir);
    }
    PathPolicy::from_env().workspace_root
}

// ─────────────────────────────────────────────── Commands ────────────────────

/// `skill add <file.toml>` — parse and validate a candidate, write it into
/// `candidates/<name>.toml`. Does not adopt.
fn add(file: &str, flags: &SkillFlags) -> Result<(), String> {
    setup_workspace(flags);
    let text = std::fs::read_to_string(file).map_err(|e| format!("cannot read {file}: {e}"))?;
    let spec: SkillSpec =
        toml::from_str(&text).map_err(|e| format!("cannot parse {file} as a skill TOML: {e}"))?;
    spec.validate(&default_registry())?;
    let candidates = skills_dir(&PathPolicy::from_env().workspace_root).join("candidates");
    std::fs::create_dir_all(&candidates)
        .map_err(|e| format!("cannot create {}: {e}", candidates.display()))?;
    let path = candidates.join(format!("{}.toml", spec.name));
    std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    eprintln!(
        "[skills] candidate {} written — `amparo skill adopt {}` to adopt (policy + approval)",
        path.display(),
        spec.name
    );
    Ok(())
}

/// `skill propose` — distill recurring high-VERIFIED sequences from the
/// notebook into inert candidates; append new proposals to
/// `proposals.jsonl` and print the findings plus a TOML template.
fn propose(flags: &SkillFlags) -> Result<(), String> {
    let workspace = setup_workspace(flags);
    let records = notebook_dir(&workspace).join("records.jsonl");
    if !records.exists() {
        eprintln!(
            "[skills] no notebook records at {} — nothing to propose \
             (amparo run --growth writes records)",
            records.display()
        );
        return Ok(());
    }
    let tenant = flags.tenant();
    let proposer = Proposer {
        min_runs: flags.min_runs.unwrap_or(3),
        min_verified_rate: flags.min_verified_rate.unwrap_or(0.8),
    };
    let proposals = proposer.propose(&records, tenant)?;
    if proposals.is_empty() {
        eprintln!(
            "[skills] no recurring tool sequences met the bar for tenant {tenant} \
             (at least {} VERIFIED run(s), {}% verified rate) — nothing to propose",
            proposer.min_runs,
            (proposer.min_verified_rate * 100.0).round() as u32
        );
        return Ok(());
    }
    let proposals_path = skills_dir(&workspace).join("proposals.jsonl");
    let written = append_proposals(&proposals_path, &proposals)?;
    for proposal in &proposals {
        println!(
            "sequence {} ({}): {} run(s), {} VERIFIED ({:.0}%), examples: {}\n{}",
            proposal.tool_names.join(" → "),
            proposal.suggested_name,
            proposal.total_runs,
            proposal.verified_runs,
            proposal.verified_rate * 100.0,
            proposal.example_run_ids.join(", "),
            template(
                &proposal.tool_names,
                &proposal.suggested_name,
                &proposal.example_run_ids
            )
        );
    }
    eprintln!(
        "[skills] {} proposal(s) for tenant {tenant} recorded in {} — candidates are \
         inert; review every step and fill in the arguments before adopting",
        written,
        proposals_path.display()
    );
    Ok(())
}

/// The inert candidate TOML template for a distilled sequence. Steps carry
/// empty `arguments = {}` — the notebook stores targets, not arguments, so
/// the operator must fill them in; adoption re-validates as a backstop.
fn template(tool_names: &[String], suggested_name: &str, examples: &[String]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# candidate distilled from the notebook — INERT until adopted\n\
         # every step runs through the gate chain individually, even after adoption\n\
         name = \"{suggested_name}\"\n\
         description = \"fill in a one-line description\"\n\
         preconditions = []\n\
         origin = \"distilled\"\n\
         source_run_ids = [{}]\n\
         expected_outcome = \"fill in what the procedure should achieve\"\n\n",
        examples
            .iter()
            .map(|e| format!("\"{e}\""))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    for tool in tool_names {
        out.push_str(&format!(
            "[[steps]]\n\
             tool = \"{tool}\"\n\
             # arguments were not recorded with the run — review and fill in\n\
             arguments = {{}}\n\n"
        ));
    }
    out
}

/// `skill list` — this tenant's adopted skills, name and description each,
/// with a compact metrics tail (M6d) when the notebook has uses.
fn list(flags: &SkillFlags) -> Result<(), String> {
    setup_workspace(flags);
    let tenant = flags.tenant();
    let workspace_root = PathPolicy::from_env().workspace_root;
    let set = load_adopted(&workspace_root, tenant);
    if set.names().is_empty() {
        println!("no adopted skills for tenant {tenant}");
        return Ok(());
    }
    let records = notebook_dir(&workspace_root).join("records.jsonl");
    for name in set.names() {
        let spec = set.get(&name).expect("names come from the set");
        let uses = read_uses(&records, tenant, &name).unwrap_or_default();
        if uses.is_empty() {
            println!("{name} — {}", spec.description);
        } else {
            let metrics = summarize(&uses);
            println!(
                "{name} — {} — uses: {}, VERIFIED: {:.0}%",
                spec.description,
                metrics.uses,
                metrics.verified_rate * 100.0
            );
        }
    }
    Ok(())
}

/// `skill show <name>` — the full adopted skill, including the step plan
/// the operator approved.
fn show(name: &str, flags: &SkillFlags) -> Result<(), String> {
    setup_workspace(flags);
    let tenant = flags.tenant();
    let log = skills_dir(&PathPolicy::from_env().workspace_root).join("adopted.jsonl");
    let record = read_log(&log)
        .into_iter()
        .rev()
        .find(|r| {
            matches!(r, SkillLogEvent::Adopt { .. }) && r.tenant_id() == tenant && r.name() == name
        })
        .ok_or_else(|| format!("no adopted skill {name:?} for tenant {tenant}"))?;
    let SkillLogEvent::Adopt {
        spec, adopted_at, ..
    } = record
    else {
        unreachable!("the find predicate matches only Adopt variants")
    };
    let spec = &spec;
    println!("name: {}", spec.name);
    println!("description: {}", spec.description);
    println!(
        "preconditions: {}",
        if spec.preconditions.is_empty() {
            "none".to_string()
        } else {
            spec.preconditions.join(", ")
        }
    );
    for (i, step) in spec.steps.iter().enumerate() {
        println!(
            "step {}: {} {}",
            i + 1,
            step.tool,
            serde_json::to_string(&step.arguments).unwrap_or_default()
        );
    }
    println!("expected outcome: {}", spec.expected_outcome);
    println!(
        "origin: {}",
        match spec.origin {
            amparo_tools::SkillOrigin::Operator => "operator",
            amparo_tools::SkillOrigin::Distilled => "distilled",
        }
    );
    if !spec.source_run_ids.is_empty() {
        println!("source runs: {}", spec.source_run_ids.join(", "));
    }
    println!("adopted: {}", adopted_at);
    // M6d: status, metrics and re-check history. Existence keys on the
    // last Adopt event above, so retired skills stay inspectable.
    let latest_retire = read_log(&log).into_iter().rev().find(|r| {
        matches!(r, SkillLogEvent::Retire { .. }) && r.tenant_id() == tenant && r.name() == name
    });
    match latest_retire {
        Some(SkillLogEvent::Retire {
            retired_at, reason, ..
        }) => println!("status: retired ({reason}, {retired_at})"),
        _ => println!("status: adopted"),
    }
    let records = notebook_dir(&PathPolicy::from_env().workspace_root).join("records.jsonl");
    let metrics = summarize(&read_uses(&records, tenant, name)?);
    println!("uses: {}", metrics.uses);
    if metrics.uses > 0 {
        println!(
            "VERIFIED rate: {:.0}% ({}/{})",
            metrics.verified_rate * 100.0,
            metrics.verified_uses,
            metrics.uses
        );
        println!("mean steps: {:.2}", metrics.mean_steps);
        if metrics.denials.is_empty() {
            println!("denials: none");
        } else {
            for (decision, count) in &metrics.denials {
                println!("denials: {decision} x{count}");
            }
        }
        if let Some(last) = &metrics.last_use {
            println!("last use: {last}");
        }
    }
    let rechecks: Vec<CheckRecord> =
        read_rechecks(&skills_dir(&PathPolicy::from_env().workspace_root).join(RECHECKS_FILE))
            .into_iter()
            .filter(|r| r.tenant_id == tenant && r.name == name)
            .collect();
    match rechecks.last() {
        Some(row) => {
            let kind = match row.kind {
                CheckKind::Drift => "drift",
                CheckKind::Performance => "performance",
            };
            let outcome = match row.outcome {
                CheckOutcome::Ok => "ok",
                CheckOutcome::Retired => "retired",
            };
            println!(
                "last policy re-check: {} ({kind}, {outcome})",
                row.checked_at
            );
        }
        None => println!("last policy re-check: never"),
    }
    let retirements: Vec<SkillLogEvent> = read_log(&log)
        .into_iter()
        .filter(|r| {
            matches!(r, SkillLogEvent::Retire { .. }) && r.tenant_id() == tenant && r.name() == name
        })
        .collect();
    if retirements.is_empty() {
        println!("retirement history: none");
    } else {
        for event in retirements {
            let SkillLogEvent::Retire {
                retired_at, reason, ..
            } = event
            else {
                unreachable!("the filter predicate matches only Retire variants")
            };
            println!("retirement history: {retired_at} — {reason}");
        }
    }
    Ok(())
}

/// The policy engine for a gated skill command — the run.rs match
/// (Wire / AllowAll / DenyAll).
fn policy_engine(flags: &SkillFlags) -> Arc<dyn PolicyEngine> {
    match (&flags.policy_url, flags.allow_all) {
        (Some(url), false) => {
            let api_key = std::env::var("AMPARO_POLICY_KEY").ok();
            Arc::new(WirePolicyEngine::new(url.clone(), api_key))
        }
        (None, true) => Arc::new(AllowAllPolicyEngine),
        (None, false) => Arc::new(DenyAllPolicyEngine::new(
            "no policy configured (--policy-url or --allow-all)",
        )),
        (Some(_), true) => unreachable!("rejected by parse_flags"),
    }
}

/// `skill adopt <name>` — the gated adoption: validate, policy-check
/// `use_skill <name>`, then human approval over the full rendered plan.
/// Approved adoptions append to the audit log.
async fn adopt(name: &str, flags: &SkillFlags) -> Result<(), String> {
    let workspace = setup_workspace(flags);
    let tenant = flags.tenant().to_string();
    let candidate_path = skills_dir(&workspace)
        .join("candidates")
        .join(format!("{name}.toml"));
    let text = std::fs::read_to_string(&candidate_path).map_err(|e| {
        format!(
            "cannot read candidate {}: {e} — `amparo skill add` writes candidates",
            candidate_path.display()
        )
    })?;
    let mut spec: SkillSpec = toml::from_str(&text).map_err(|e| {
        format!(
            "cannot parse {} as a skill TOML: {e}",
            candidate_path.display()
        )
    })?;
    spec.validate(&default_registry())?;

    let policy = policy_engine(flags);
    let decision = policy
        .judge_tool(USE_SKILL, name, &[("tenant", &tenant)])
        .await;
    if decision.verdict == PolicyVerdict::Deny {
        return Err(format!(
            "policy refused to adopt {name:?} for tenant {tenant}: {}",
            decision.fired.join("; ")
        ));
    }

    let approval: Arc<dyn ApprovalGate> = if flags.auto_approve {
        Arc::new(AutoApprove)
    } else if flags.auto_deny {
        Arc::new(AutoDeny)
    } else {
        Arc::new(InteractiveApprovalGate::default())
    };
    let request = ApprovalRequest {
        call_id: format!("skill-adopt:{name}"),
        tool_name: USE_SKILL.to_string(),
        arguments: serde_json::to_value(&spec).map_err(|e| e.to_string())?,
        reasons: vec![render_plan(&spec)],
        // No agent runs here — no preflight classification or delegation
        // label (display-only context; the plan itself is the approver's
        // subject).
        blast_radius: None,
        session_label: None,
        rollback: None,
    };
    if !approval.request(&request).await {
        return Err(format!(
            "adoption of {name:?} for tenant {tenant} was not approved — nothing written"
        ));
    }

    let adopted_at = chrono::Utc::now().to_rfc3339();
    spec.adopted_at = Some(adopted_at.clone());
    let event = SkillLogEvent::adopt(tenant.clone(), name.to_string(), spec, adopted_at);
    append_event(&skills_dir(&workspace).join("adopted.jsonl"), &event)?;
    eprintln!("[skills] adopted {name} for tenant {tenant}");
    Ok(())
}

/// `skill check` — the M6d re-check of every adopted skill: policy drift
/// first (the step plan dry-runs through the gate chain with this task's
/// policy engine + ceiling — nothing executes, no approval), then
/// performance (VERIFIED rate over the last `window` uses). Drift wins
/// over performance; a retirement appends a Retire event + a re-check row
/// (a pass appends only the row). `--dry-run` reports and writes nothing.
/// Exit 0 even when retirements fire (cron-able).
async fn check(flags: &SkillFlags) -> Result<(), String> {
    let workspace = setup_workspace(flags);
    let tenant = flags.tenant();
    let skills_path = skills_dir(&workspace).join("adopted.jsonl");
    let set = SkillSet::load(&skills_path, tenant);
    let names = set.names();
    let total = names.len();
    if names.is_empty() {
        eprintln!("[skills] no adopted skills for tenant {tenant} — nothing to check");
        return Ok(());
    }
    let policy = policy_engine(flags);
    let registry = default_registry();
    let threshold = RetirementThreshold {
        min_verified_rate: flags.min_verified_rate.unwrap_or(0.5),
        window: flags.window.unwrap_or(20),
        ..RetirementThreshold::default()
    };
    let records = notebook_dir(&workspace).join("records.jsonl");
    let rechecks_path = skills_dir(&workspace).join(RECHECKS_FILE);
    let checked_at = chrono::Utc::now().to_rfc3339();
    let mut retired = 0usize;
    for name in names {
        let spec = set.get(&name).expect("names come from the set");
        let (kind, outcome, finding) = if let Some(reason) =
            check_skill_drift(&spec, &registry, flags.trust_ceiling, policy.as_ref()).await
        {
            (CheckKind::Drift, CheckOutcome::Retired, Some(reason))
        } else {
            let uses = read_uses(&records, tenant, &name)?;
            match retirement_reason(&uses, &threshold) {
                Some(reason) => (CheckKind::Performance, CheckOutcome::Retired, Some(reason)),
                None => (CheckKind::Performance, CheckOutcome::Ok, None),
            }
        };
        match &finding {
            Some(reason) => {
                eprintln!("[skills] retired {name} for tenant {tenant}: {reason}");
                retired += 1;
            }
            None => {
                eprintln!("[skills] {name}: ok — no policy drift, performance within the threshold")
            }
        }
        if flags.dry_run {
            // Reports only — no Retire events, no re-check rows.
            continue;
        }
        append_recheck(
            &rechecks_path,
            &CheckRecord {
                checked_at: checked_at.clone(),
                tenant_id: tenant.to_string(),
                name: name.clone(),
                kind,
                outcome,
                reason: finding.clone(),
            },
        )?;
        if let Some(reason) = finding {
            append_event(
                &skills_path,
                &SkillLogEvent::retire(tenant, &name, checked_at.clone(), reason),
            )?;
        }
    }
    eprintln!(
        "[skills] checked {total} skill(s) for tenant {tenant}, {retired} retired{}",
        if flags.dry_run {
            " (dry-run — nothing written)"
        } else {
            ""
        }
    );
    Ok(())
}

/// `skill retire <name>` — the I4 operator lever: append a Retire event
/// (disable + notify, never delete — the audit log keeps every row).
/// Refuses when the folded set lacks the name.
fn retire(name: &str, flags: &SkillFlags) -> Result<(), String> {
    let workspace = setup_workspace(flags);
    let tenant = flags.tenant();
    let skills_path = skills_dir(&workspace).join("adopted.jsonl");
    let set = SkillSet::load(&skills_path, tenant);
    if set.get(name).is_none() {
        return Err(format!(
            "{name:?} is not currently adopted for tenant {tenant} — nothing to retire"
        ));
    }
    let retired_at = chrono::Utc::now().to_rfc3339();
    let reason = flags.reason.as_deref().unwrap_or("operator retired");
    append_event(
        &skills_path,
        &SkillLogEvent::retire(tenant, name, retired_at, reason),
    )?;
    eprintln!("[skills] retired {name} for tenant {tenant}: {reason}");
    Ok(())
}

/// Load the effective (folded) adopted-skill set for a tenant.
fn load_adopted(workspace_root: &Path, tenant: &str) -> SkillSet {
    SkillSet::load(&skills_dir(workspace_root).join("adopted.jsonl"), tenant)
}

/// Render the full step plan for the adoption approval — the operator must
/// see exactly what will execute (I4: the adoption record is the audit row).
fn render_plan(spec: &SkillSpec) -> String {
    let mut plan = format!(
        "adopting skill {:?} for this tenant\n  description: {}\n  preconditions: {}\n  steps ({}):",
        spec.name,
        spec.description,
        if spec.preconditions.is_empty() {
            "none".to_string()
        } else {
            spec.preconditions.join(", ")
        },
        spec.steps.len()
    );
    for (i, step) in spec.steps.iter().enumerate() {
        plan.push_str(&format!(
            "\n    {}. {} {}",
            i + 1,
            step.tool,
            serde_json::to_string(&step.arguments).unwrap_or_default()
        ));
    }
    plan.push_str(&format!("\n  expected outcome: {}", spec.expected_outcome));
    plan
}

// ───────────────────────────────────────────────────────────── Tests ─────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(args: &[&str]) -> Command {
        match parse(args.iter().map(|s| s.to_string()).collect()) {
            ParsedSkill::Run(command) => command,
            ParsedSkill::Help => panic!("expected a command, got help"),
            ParsedSkill::Error(message) => panic!("expected a command, got: {message}"),
        }
    }

    fn parse_error(args: &[&str]) -> String {
        match parse(args.iter().map(|s| s.to_string()).collect()) {
            ParsedSkill::Error(message) => message,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn help_is_recognized_anywhere() {
        assert!(matches!(parse(vec!["--help".into()]), ParsedSkill::Help));
        assert!(matches!(
            parse(vec!["adopt".into(), "x".into(), "-h".into()]),
            ParsedSkill::Help
        ));
        assert!(parse_error(&[]).contains("missing subcommand"));
    }

    #[test]
    fn unknown_subcommand_and_flag_are_usage_errors() {
        assert!(parse_error(&["frobnicate"]).contains("unknown skill subcommand"));
        assert!(parse_error(&["list", "--nonsense"]).contains("unknown flag --nonsense"));
        assert!(parse_error(&["adopt", "--min-runs", "3", "x"]).contains("unknown flag --min-runs"));
    }

    #[test]
    fn add_requires_exactly_one_file() {
        match parse_ok(&["add", "candidate.toml"]) {
            Command::Add { file, flags } => {
                assert_eq!(file, "candidate.toml");
                assert_eq!(flags.tenant(), "cli");
            }
            other => panic!("expected Add, got {other:?}"),
        }
        assert!(parse_error(&["add"]).contains("exactly one candidate file"));
        assert!(parse_error(&["add", "a.toml", "b.toml"]).contains("exactly one"));
    }

    #[test]
    fn propose_parses_thresholds_and_tenant() {
        match parse_ok(&[
            "propose",
            "--min-runs",
            "5",
            "--min-verified-rate",
            "0.9",
            "--tenant",
            "telegram:1",
        ]) {
            Command::Propose(flags) => {
                assert_eq!(flags.min_runs, Some(5));
                assert_eq!(flags.min_verified_rate, Some(0.9));
                assert_eq!(flags.tenant(), "telegram:1");
            }
            other => panic!("expected Propose, got {other:?}"),
        }
        assert!(parse_error(&["propose", "--min-runs", "0"]).contains("positive integer"));
        assert!(parse_error(&["propose", "--min-verified-rate", "1.5"]).contains("between 0 and 1"));
        assert!(parse_error(&["propose", "extra"]).contains("no positional"));
    }

    #[test]
    fn show_and_adopt_require_a_name() {
        match parse_ok(&["show", "greet"]) {
            Command::Show { name, .. } => assert_eq!(name, "greet"),
            other => panic!("expected Show, got {other:?}"),
        }
        assert!(parse_error(&["show"]).contains("requires a skill name"));
        match parse_ok(&["adopt", "greet", "--allow-all", "--auto-approve"]) {
            Command::Adopt { name, flags } => {
                assert_eq!(name, "greet");
                assert!(flags.allow_all);
                assert!(flags.auto_approve);
            }
            other => panic!("expected Adopt, got {other:?}"),
        }
        assert!(parse_error(&["adopt"]).contains("requires a skill name"));
    }

    #[test]
    fn conflicting_modes_are_usage_errors() {
        assert!(
            parse_error(&["adopt", "x", "--policy-url", "http://p", "--allow-all"])
                .contains("mutually exclusive")
        );
        assert!(
            parse_error(&["adopt", "x", "--auto-approve", "--auto-deny"])
                .contains("mutually exclusive")
        );
    }

    #[test]
    fn list_takes_no_positionals() {
        assert!(matches!(
            parse_ok(&["list", "--tenant", "cli"]),
            Command::List(_)
        ));
        assert!(parse_error(&["list", "x"]).contains("no positional"));
    }

    #[test]
    fn check_parses_metrics_flags_with_defaults() {
        match parse_ok(&["check"]) {
            Command::Check(flags) => {
                assert!(flags.min_verified_rate.is_none());
                assert!(flags.window.is_none());
                assert!(!flags.dry_run);
                assert_eq!(flags.trust_ceiling, ToolTrustTier::SystemControl);
            }
            other => panic!("expected Check, got {other:?}"),
        }
        match parse_ok(&[
            "check",
            "--min-verified-rate",
            "0.5",
            "--window",
            "10",
            "--dry-run",
            "--trust-ceiling",
            "external_effector",
        ]) {
            Command::Check(flags) => {
                assert_eq!(flags.min_verified_rate, Some(0.5));
                assert_eq!(flags.window, Some(10));
                assert!(flags.dry_run);
                assert_eq!(flags.trust_ceiling, ToolTrustTier::ExternalEffector);
            }
            other => panic!("expected Check, got {other:?}"),
        }
        assert!(parse_error(&["check", "--window", "0"]).contains("positive integer"));
        assert!(parse_error(&["check", "--window", "nope"]).contains("positive integer"));
        assert!(parse_error(&["check", "--min-verified-rate", "2"]).contains("between 0 and 1"));
        assert!(
            parse_error(&["check", "--trust-ceiling", "nonsense"]).contains("unknown trust tier")
        );
        assert!(parse_error(&["check", "--trust-ceiling"]).contains("requires a tier"));
        assert!(parse_error(&["check", "extra"]).contains("no positional"));
        // The check-only flags are rejected for other subcommands.
        assert!(parse_error(&["list", "--window", "5"]).contains("unknown flag --window"));
        assert!(parse_error(&["list", "--dry-run"]).contains("unknown flag --dry-run"));
        assert!(
            parse_error(&["adopt", "x", "--trust-ceiling", "system_control"])
                .contains("unknown flag --trust-ceiling")
        );
    }

    #[test]
    fn retire_requires_a_name_and_parses_reason() {
        match parse_ok(&["retire", "greet", "--reason", "operator retired"]) {
            Command::Retire { name, flags } => {
                assert_eq!(name, "greet");
                assert_eq!(flags.reason.as_deref(), Some("operator retired"));
            }
            other => panic!("expected Retire, got {other:?}"),
        }
        assert!(parse_error(&["retire"]).contains("requires a skill name"));
        assert!(parse_error(&["retire", "a", "b"]).contains("requires a skill name"));
        assert!(parse_error(&["retire", "greet", "--reason"]).contains("--reason requires a value"));
        // The retire-only flag is rejected for other subcommands.
        assert!(parse_error(&["list", "--reason", "x"]).contains("unknown flag --reason"));
    }

    #[test]
    fn template_renders_inert_skeletons() {
        let text = template(
            &["git_status".to_string(), "git_diff".to_string()],
            "git-status-git-diff",
            &["2026-08-27T10:00:00Z".to_string()],
        );
        assert!(text.contains("name = \"git-status-git-diff\""), "{text}");
        assert!(text.contains("origin = \"distilled\""), "{text}");
        assert!(
            text.contains("source_run_ids = [\"2026-08-27T10:00:00Z\"]"),
            "{text}"
        );
        assert!(text.contains("tool = \"git_status\""), "{text}");
        assert!(text.contains("arguments = {}"), "{text}");
        assert!(text.contains("INERT until adopted"), "{text}");
    }

    #[test]
    fn render_plan_shows_every_step_with_arguments() {
        let spec: SkillSpec = toml::from_str(
            "name = \"greet\"\ndescription = \"greets\"\npreconditions = [\"none\"]\n\
             expected_outcome = \"tests pass\"\n\
             [[steps]]\ntool = \"run_tests\"\narguments = {}\n",
        )
        .unwrap();
        let plan = render_plan(&spec);
        assert!(plan.contains("1. run_tests {}"), "{plan}");
        assert!(plan.contains("expected outcome: tests pass"), "{plan}");
        assert!(plan.contains("steps (1)"), "{plan}");
    }
}
