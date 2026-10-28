//! cli.py: `hubctl`, the operator CLI. Same services as the API, no HTTP; it needs an admin key too (HUB_API_KEY from the
//! environment, else the env file, else the 0600 bootstrap key file, else a hidden TTY prompt), except for `init`,
//! `serve` and `retire-bootstrap-key`. Exit codes: 0 ok, 1 error, 2 the plan has errors/floor violations (and argparse
//! usage errors); `evaluate-skill(s)`: 0 passed, 2 failed or incomplete, 1 could not evaluate.
//! Errors print `error: <code>: <detail>` (+ `fix: ...`) to stderr. Stdout formats (text and `--json`) are compared by
//! the parity harness. Owner: WS-C.
//!
//! Usage/help/parse-error TEXT comes from clap (deviation D-CLI-USAGE); the exit codes are argparse's (2 for a usage
//! error, 0 for `--help`).

use std::collections::HashMap;
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use hub_core::py::json::{Opts, dumps};
use hub_core::py::text::{strip, take_chars};
use hub_core::value::PyValue;
use hub_core::{HubResult, Problem};
use serde_json::{Value, json};

use crate::config::{Overrides, load_settings};
use crate::services::auth::BOOTSTRAP_KEY_FILE;
use crate::services::hub::{Hub, HubOptions};
use crate::services::skilleval::INSTALL_HINT;

pub const ACTOR: &str = "hubctl";
const PROMPT: &str = "Hub admin key (input hidden): ";

#[derive(Parser, Debug)]
#[command(name = "hubctl", about = "Agent Hub operator CLI", disable_version_flag = true)]
pub struct Cli {
    /// machine-readable output
    #[arg(long)]
    pub json: bool,
    /// override HUB_CONTENT_DIR
    #[arg(long = "content-dir")]
    pub content_dir: Option<String>,
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Args, Debug)]
pub struct ClientsArg {
    /// limit to a client (repeatable)
    #[arg(long = "client")]
    pub client: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// create the content repo (git init) and seed it
    Init,
    /// validate every content file (working tree)
    Validate,
    Plan(ClientsArg),
    Apply {
        #[command(flatten)]
        clients: ClientsArg,
        /// adopt a conflicting path: root:path
        #[arg(long = "adopt")]
        adopt: Vec<String>,
        /// confirm (apply never runs without it)
        #[arg(long)]
        yes: bool,
    },
    Verify(ClientsArg),
    Drift(ClientsArg),
    Audit(ClientsArg),
    /// import what a client already has into the content working tree
    Import {
        #[arg(long, required = true)]
        client: String,
        /// import everything discovered
        #[arg(long)]
        all: bool,
        /// kind:name (repeatable)
        #[arg(long = "item")]
        item: Vec<String>,
        /// default: link identical items, skip differing ones
        #[arg(long = "on-conflict", value_parser = ["skip", "rename", "replace", "link"])]
        on_conflict: Option<String>,
        /// let a skill the evaluation gate BLOCKED through (10..500 characters; audited)
        #[arg(long = "override-reason", value_name = "TEXT")]
        override_reason: Option<String>,
    },
    /// read what a client really has (read-only) and store the inventory
    Observe {
        #[arg(long, required = true)]
        client: String,
    },
    /// import skills from a client's stored observation (through the evaluation gate)
    ImportObserved {
        #[arg(long, required = true)]
        client: String,
        /// observed skill key (repeatable)
        #[arg(long = "key", required = true)]
        key: Vec<String>,
        /// final skill name (only with exactly one --key)
        #[arg(long)]
        name: Option<String>,
        #[arg(long = "on-conflict", default_value = "skip", value_parser = ["skip", "rename", "replace", "link"])]
        on_conflict: String,
        /// also enable the imported skills for this client (default: off)
        #[arg(long)]
        enable: bool,
        #[arg(long = "override-reason", value_name = "TEXT")]
        override_reason: Option<String>,
    },
    /// evaluate hub and/or observed skills with SkillEvaluator (advisory; feeds the import gate)
    Evaluate {
        /// hub | <client> | all (default hub)
        #[arg(long, default_value = "hub")]
        source: String,
        /// which skills of --source (default none_or_stale)
        #[arg(long, value_parser = ["none_or_stale", "all"], conflicts_with = "subject")]
        select: Option<String>,
        /// hub:<skill> or <client>:<key> (repeatable)
        #[arg(long = "subject", value_name = "S")]
        subject: Vec<String>,
        #[arg(long, default_value = "static", value_parser = ["static", "tier1", "tier1+2"])]
        tiers: String,
        /// re-run even when this exact content was evaluated before
        #[arg(long)]
        force: bool,
        /// run now, in this process, under the evaluation lock
        #[arg(long)]
        wait: bool,
    },
    /// show pending (uncommitted) content changes
    Status,
    /// approve pending content changes (git commit with the hub's fixed identity)
    Commit {
        #[arg(short = 'm', long = "message", required = true)]
        message: String,
    },
    /// undo an apply (default: the latest) using its recorded backups
    Rollback {
        #[arg(long, required = true)]
        client: String,
        #[arg(long = "apply-id")]
        apply_id: Option<String>,
        /// show the diff, change nothing
        #[arg(long = "dry-run")]
        dry_run: bool,
        #[arg(long)]
        yes: bool,
        /// restore even if edited since
        #[arg(long = "force-paths", value_name = "ROOT:PATH")]
        force_paths: Vec<String>,
    },
    /// list recorded applies for a client
    Applies {
        #[arg(long, required = true)]
        client: String,
    },
    /// move the content repo to a private location outside the source tree
    MigrateContent {
        #[arg(long = "to", required = true, value_name = "PATH")]
        to: String,
        /// do it (without it: show what would move)
        #[arg(long)]
        yes: bool,
    },
    /// record that you HAVE scanned an MCP server (binds `clean` to its current fields)
    AttestMcp {
        name: String,
        /// confirm that your MCP scanner was run and came back clean
        #[arg(long)]
        yes: bool,
    },
    /// verify the admin key, then wipe and remove the bootstrap key file (terminal only)
    RetireBootstrapKey,
    /// read-only report of how exposed the hub's secrets and policy are on this host
    Doctor {
        #[arg(long = "json")]
        doctor_json: bool,
    },
    /// evaluate one skill of the working tree with SkillEvaluator (advisory)
    EvaluateSkill {
        name: String,
        /// static: no model calls (default); tier1: adds the LLM judge; tier1+2: also Tier 2 deduplication
        #[arg(long, default_value = "static", value_parser = ["static", "tier1", "tier1+2"])]
        tiers: String,
    },
    /// evaluate every skill of the working tree; prints a table (advisory)
    EvaluateSkills {
        #[arg(long, default_value = "static", value_parser = ["static", "tier1", "tier1+2"])]
        tiers: String,
    },
    /// list installed adapters
    Adapters,
    /// run the API + UI
    Serve {
        #[arg(long)]
        port: Option<i64>,
    },
}

impl Cmd {
    /// argparse's `dest="cmd"` value.
    pub fn name(&self) -> &'static str {
        match self {
            Cmd::Init => "init",
            Cmd::Validate => "validate",
            Cmd::Plan(_) => "plan",
            Cmd::Apply { .. } => "apply",
            Cmd::Verify(_) => "verify",
            Cmd::Drift(_) => "drift",
            Cmd::Audit(_) => "audit",
            Cmd::Import { .. } => "import",
            Cmd::Observe { .. } => "observe",
            Cmd::ImportObserved { .. } => "import-observed",
            Cmd::Evaluate { .. } => "evaluate",
            Cmd::Status => "status",
            Cmd::Commit { .. } => "commit",
            Cmd::Rollback { .. } => "rollback",
            Cmd::Applies { .. } => "applies",
            Cmd::MigrateContent { .. } => "migrate-content",
            Cmd::AttestMcp { .. } => "attest-mcp",
            Cmd::RetireBootstrapKey => "retire-bootstrap-key",
            Cmd::Doctor { .. } => "doctor",
            Cmd::EvaluateSkill { .. } => "evaluate-skill",
            Cmd::EvaluateSkills { .. } => "evaluate-skills",
            Cmd::Adapters => "adapters",
            Cmd::Serve { .. } => "serve",
        }
    }
}

/// `getpass.getpass(prompt)`: None when nothing could be read.
pub type Prompt = Box<dyn FnMut(&str) -> Option<String>>;

/// Where `hubctl` reads its key from and how it talks to the terminal. `run` uses the real process; tests inject theirs
/// (Python's tests monkeypatch `os.environ`, `config.ENV_FILE` and `sys.stdin`).
pub struct CliEnv {
    /// the process environment (`HUB_API_KEY`)
    pub environ: HashMap<String, String>,
    /// `config.ENV_FILE`
    pub env_file: PathBuf,
    /// `sys.stdin.isatty()`
    pub stdin_tty: bool,
    /// the hidden prompt
    pub prompt: Prompt,
}

impl CliEnv {
    /// The real process: its environment, `ENV_FILE`, its stdin and a hidden terminal prompt.
    pub fn process() -> Self {
        let environ: HashMap<String, String> = std::env::vars().collect();
        let env_file = crate::config::env_file_from(&environ);
        Self {
            environ,
            env_file,
            stdin_tty: std::io::stdin().is_terminal(),
            prompt: Box::new(getpass),
        }
    }
}

/// `hubctl` main: parse (usage errors exit 2 like argparse), build the Hub, authenticate, dispatch.
pub fn run(argv: &[String]) -> i32 {
    let mut env = CliEnv::process();
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    run_with(argv, None, &mut env, &mut out, &mut err)
}

/// `run(argv, hub=None)` with an injectable environment and output streams.
pub fn run_with(
    argv: &[String],
    hub: Option<Arc<Hub>>,
    env: &mut CliEnv,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let cli = match Cli::try_parse_from(std::iter::once("hubctl".to_string()).chain(argv.iter().cloned())) {
        Ok(c) => c,
        Err(e) => {
            let text = e.render().to_string();
            if e.use_stderr() {
                let _ = err.write_all(text.as_bytes());
                return 2;
            }
            let _ = out.write_all(text.as_bytes());
            return 0;
        }
    };
    let hub = match hub {
        Some(h) => h,
        None => {
            let mut overrides: Overrides = Vec::new();
            if let Some(d) = cli.content_dir.as_deref().filter(|d| !d.is_empty()) {
                overrides.push(("content_dir".into(), PyValue::str(d)));
            }
            let cfg = match load_settings(&overrides) {
                Ok(c) => c,
                Err(e) => {
                    let _ = writeln!(err, "{e}");
                    return 1;
                }
            };
            match Hub::new(cfg, HubOptions::default()) {
                Ok(h) => h,
                Err(p) => return report_to(&p, err),
            }
        }
    };
    let res = (|| -> HubResult<i32> {
        if !matches!(cli.cmd, Cmd::Init | Cmd::Serve { .. } | Cmd::RetireBootstrapKey) {
            authenticate(&hub, env)?;
        }
        dispatch_io(&cli, &hub, env, out, err)
    })();
    match res {
        Ok(code) => code,
        Err(p) => report_to(&p, err),
    }
}

/// `error: <code>: <detail>` (+ `fix:` when the problem carries one) on stderr; exit 1.
pub fn report(p: &Problem) -> i32 {
    report_to(p, &mut std::io::stderr())
}

fn report_to(p: &Problem, err: &mut dyn Write) -> i32 {
    let _ = writeln!(err, "error: {}: {}", p.code, p.detail);
    if let Some(fix) = p.extra.get("fix").and_then(Value::as_str) {
        let _ = writeln!(err, "fix: {fix}");
    }
    1
}

/// `_authenticate(hub)`: hubctl needs an admin key too (HUB_API_KEY, else the env file, else the 0600 bootstrap file,
/// else a hidden prompt on a terminal), so a shell-capable agent cannot drive the hub just because it can run a local
/// command. The key is never printed, logged or stored.
pub fn authenticate(hub: &Hub, env: &mut CliEnv) -> HubResult<()> {
    let mut key = strip(env.environ.get("HUB_API_KEY").map_or("", String::as_str)).to_string();
    if key.is_empty()
        && env.env_file.is_file()
        && let Ok(text) = std::fs::read_to_string(&env.env_file)
    {
        let vals = hub_core::dotenv::values(&text, &env.environ);
        let v = vals
            .iter()
            .rev()
            .find(|(k, _)| k == "HUB_API_KEY")
            .and_then(|(_, v)| v.clone())
            .unwrap_or_default();
        key = strip(&v).to_string();
    }
    if key.is_empty()
        && let Ok(text) = std::fs::read_to_string(hub.cfg().data_dir.join(BOOTSTRAP_KEY_FILE))
    {
        key = strip(&text).to_string();
    }
    if key.is_empty() && env.stdin_tty {
        key = strip(&(env.prompt)(PROMPT).unwrap_or_default()).to_string();
    }
    let p = if key.is_empty() {
        None
    } else {
        hub.auth.authenticate(&key)
    };
    match p {
        Some(p) if p.role == "admin" => Ok(()),
        _ => Err(Problem::unauthorized(
            "an admin key is required: set HUB_API_KEY (environment or the env file), or run from a terminal to be prompted for it",
        )),
    }
}

/// `getpass.getpass(prompt)`: the prompt and the input go through /dev/tty with echo off (termios, `TCSAFLUSH`, restored
/// afterwards, like getpass's `unix_getpass`); when echo cannot be controlled, the input is read with getpass's warning.
fn getpass(prompt: &str) -> Option<String> {
    let Ok(mut tty) = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty") else {
        let mut e = std::io::stderr();
        let _ = write!(e, "Warning: Password input may be echoed.\n{prompt}");
        let _ = e.flush();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok()?;
        return Some(line.trim_end_matches(['\r', '\n']).to_string());
    };
    use nix::sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr};
    let old = tcgetattr(&tty).ok();
    let hidden = old.as_ref().is_some_and(|old| {
        let mut quiet = old.clone();
        quiet.local_flags.remove(LocalFlags::ECHO);
        tcsetattr(&tty, SetArg::TCSAFLUSH, &quiet).is_ok()
    });
    if !hidden {
        let _ = tty.write_all(b"Warning: Password input may be echoed.\n");
    }
    let _ = tty.write_all(prompt.as_bytes());
    let _ = tty.flush();
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while let Ok(1) = tty.read(&mut byte) {
        if byte[0] == b'\n' || buf.len() > 64 * 1024 {
            break;
        }
        buf.push(byte[0]);
    }
    if hidden && let Some(old) = &old {
        let _ = tcsetattr(&tty, SetArg::TCSAFLUSH, old);
    }
    let _ = tty.write_all(b"\n");
    let _ = tty.flush();
    Some(String::from_utf8_lossy(&buf).trim_end_matches('\r').to_string())
}

// ---- output helpers -------------------------------------------------------------------------------------------------

/// `str(x)` of a JSON value.
pub fn py_str(v: &Value) -> String {
    PyValue::from_json(v).py_str()
}

/// Python truthiness of a JSON value (absent = None).
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(m)) => !m.is_empty(),
    }
}

fn get<'a>(v: &'a Value, k: &str) -> &'a Value {
    v.get(k).unwrap_or(&Value::Null)
}

fn s(v: &Value, k: &str) -> String {
    py_str(get(v, k))
}

fn arr(v: &Value) -> &[Value] {
    v.as_array().map_or(&[], Vec::as_slice)
}

fn len_of(v: &Value) -> usize {
    match v {
        Value::Array(a) => a.len(),
        Value::Object(m) => m.len(),
        Value::String(s) => s.chars().count(),
        _ => 0,
    }
}

/// `format(x, '.1f')` (Rust's exact-decimal rounding equals CPython's for finite values).
fn f1(x: f64) -> String {
    if x.is_nan() {
        "nan".into()
    } else if x.is_infinite() {
        if x > 0.0 { "inf".into() } else { "-inf".into() }
    } else {
        format!("{x:.1}")
    }
}

/// `_fmt(v, spec)`: "-" for None, else `format(v, spec)` (spec "" or ".1f").
pub fn fmt_opt(v: Option<&Value>, one_decimal: bool) -> String {
    match v {
        None | Some(Value::Null) => "-".into(),
        Some(Value::Number(n)) if one_decimal => f1(n.as_f64().unwrap_or(0.0)),
        Some(other) => py_str(other),
    }
}

/// `f"{x:<w}"` / `f"{x:w}"` for a str: pad to a width in code points.
fn ljust(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(w - n))
    }
}

/// `f"{x:>w}"`.
fn rjust(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w {
        s.to_string()
    } else {
        format!("{}{s}", " ".repeat(w - n))
    }
}

/// `json.dumps(payload, indent=2, default=str)`.
pub fn json_indent2(v: &Value) -> String {
    dumps(
        v,
        &Opts {
            indent: Some(2),
            ..Opts::default()
        },
    )
}

/// `_emit(args, payload, text)`.
fn emit(cli: &Cli, out: &mut dyn Write, payload: &Value, text: &str) {
    if cli.json {
        let _ = writeln!(out, "{}", json_indent2(payload));
    } else {
        let _ = writeln!(out, "{text}");
    }
}

/// `", ".join(f"{v} {k}" for k, v in sorted(d.items()))`.
fn sorted_pairs(d: &Value, value_first: bool) -> String {
    let mut pairs: Vec<(String, String)> = d
        .as_object()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), py_str(v))).collect())
        .unwrap_or_default();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| {
            if value_first {
                format!("{v} {k}")
            } else {
                format!("{k} {v}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `_plan_text(plan)`.
pub fn plan_text(plan: &Value) -> String {
    let hash = s(plan, "content_hash");
    let mut lines = vec![format!("plan {}  content {}", s(plan, "id"), take_chars(&hash, 12))];
    if truthy(plan.get("pending_changes")) {
        lines.push(format!(
            "  note: {} uncommitted content change(s) are NOT in this plan",
            s(plan, "pending_changes")
        ));
    }
    for cp in arr(get(plan, "clients")) {
        let summ = sorted_pairs(get(cp, "summary"), true);
        let summ = if summ.is_empty() {
            "nothing to do".to_string()
        } else {
            summ
        };
        lines.push(format!(
            "- {}: {summ}{}",
            s(cp, "client"),
            if truthy(cp.get("blocked")) { "  [BLOCKED]" } else { "" }
        ));
        for f in arr(get(cp, "files")) {
            if s(f, "action") != "unchanged" {
                let reason = if truthy(f.get("reason")) {
                    format!("  ({})", s(f, "reason"))
                } else {
                    String::new()
                };
                lines.push(format!(
                    "    {} {}:{}{reason}",
                    ljust(&s(f, "action"), 9),
                    s(f, "root"),
                    s(f, "path")
                ));
            }
        }
        for r in arr(get(cp, "blocked_reasons")) {
            lines.push(format!("    ! {}", py_str(r)));
        }
    }
    lines.join("\n")
}

/// `rec.get(k) or {}`.
fn dict_or_empty<'a>(v: Option<&'a Value>, empty: &'a Value) -> &'a Value {
    v.filter(|x| truthy(Some(x))).unwrap_or(empty)
}

/// `_eval_text(rec)`.
pub fn eval_text(rec: &Value) -> String {
    let empty = json!({});
    let t1 = dict_or_empty(rec.get("tier1"), &empty);
    let sev = dict_or_empty(t1.get("severity"), &empty);
    let duration = rec.get("duration_ms").and_then(Value::as_f64).unwrap_or(0.0);
    let mut first = format!(
        "{}: {}  (tiers {}, run {}, {} s)",
        s(rec, "skill"),
        s(rec, "verdict"),
        s(rec, "tiers"),
        s(rec, "run_id"),
        f1(duration / 1000.0)
    );
    if truthy(rec.get("reused_from")) {
        first.push_str(&format!("  [reused {}]", s(get(rec, "reused_from"), "run_id")));
    }
    let mut lines = vec![first];
    if truthy(Some(t1)) {
        lines.push(format!(
            "  quality {} grade {}  judge {}  errors {} warnings {}  critical {} high {} medium {}",
            fmt_opt(t1.get("quality_score"), true),
            fmt_opt(t1.get("grade"), false),
            fmt_opt(t1.get("judge_score"), true),
            fmt_opt(t1.get("errors"), false),
            fmt_opt(t1.get("warnings"), false),
            fmt_opt(sev.get("critical"), false),
            fmt_opt(sev.get("high"), false),
            fmt_opt(sev.get("medium"), false),
        ));
        if truthy(t1.get("incomplete")) {
            lines.push(format!(
                "  incomplete scans: {}",
                arr(get(t1, "incomplete"))
                    .iter()
                    .map(py_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    if truthy(rec.get("tier2")) {
        lines.push(format!("  tier2: {}", s(get(rec, "tier2"), "status")));
    }
    for f in arr(get(rec, "findings")) {
        lines.push(format!(
            "  [{}] {}: {}",
            s(f, "severity").to_uppercase(),
            s(f, "validator"),
            s(f, "message")
        ));
    }
    if truthy(rec.get("error")) {
        lines.push(format!("  error: {}", s(rec, "error")));
    }
    lines.push("  advisory only: plan and apply are not affected".into());
    lines.join("\n")
}

/// `_eval_table(recs)`.
pub fn eval_table(recs: &[Value]) -> String {
    let mut rows = vec![format!(
        "{} {} {} {} {} {} {}",
        ljust("skill", 40),
        ljust("verdict", 10),
        rjust("quality", 7),
        rjust("grade", 5),
        rjust("judge", 6),
        rjust("errors", 6),
        rjust("high", 4)
    )];
    let empty = json!({});
    for r in recs {
        let t1 = dict_or_empty(r.get("tier1"), &empty);
        let sev = dict_or_empty(t1.get("severity"), &empty);
        let source = get(r, "source");
        let label = if !source.is_null() && source.as_str() != Some("hub") {
            s(r, "subject")
        } else {
            s(r, "skill")
        };
        let mut row = format!(
            "{} {} {} {} {} {} {}",
            ljust(take_chars(&label, 40), 40),
            ljust(&s(r, "verdict"), 10),
            rjust(&fmt_opt(t1.get("quality_score"), true), 7),
            rjust(&fmt_opt(t1.get("grade"), false), 5),
            rjust(&fmt_opt(t1.get("judge_score"), true), 6),
            rjust(&fmt_opt(t1.get("errors"), false), 6),
            rjust(&fmt_opt(sev.get("high"), false), 4)
        );
        if r.get("verdict").and_then(Value::as_str) == Some("error") && truthy(r.get("error")) {
            row.push_str(&format!("  {}", take_chars(&s(r, "error"), 80)));
        }
        rows.push(row);
    }
    rows.join("\n")
}

/// `EVAL_EXIT.get(str(verdict), 1)`: 0 passed, 2 failed/incomplete, 1 error or anything else.
pub fn eval_exit(verdict: &Value) -> i32 {
    match py_str(verdict).as_str() {
        "passed" => 0,
        "failed" | "incomplete" => 2,
        _ => 1,
    }
}

/// evaluate / evaluate-skills: 1 if any error, 2 if any failed/incomplete, else 0.
fn batch_exit(recs: &[Value]) -> i32 {
    let verdicts: Vec<String> = recs.iter().map(|r| s(r, "verdict")).collect();
    if verdicts.iter().any(|v| v == "error") {
        1
    } else if verdicts.iter().any(|v| v == "failed" || v == "incomplete") {
        2
    } else {
        0
    }
}

fn evaluator_missing() -> Problem {
    Problem::unavailable(format!(
        "SkillEvaluator is not installed; install it with: {INSTALL_HINT}"
    ))
    .code("evaluator_missing")
}

/// `_clients(hub, wanted)`: `hub.approved()` first (its errors surface even with --client), then the named clients,
/// else every client of the approved (HEAD) content.
fn clients_of(hub: &Hub, wanted: &[String]) -> HubResult<Vec<String>> {
    let (_, store) = hub.approved()?;
    if !wanted.is_empty() {
        return Ok(wanted.to_vec());
    }
    let mut ids: Vec<String> = store.catalog()?.clients.keys().cloned().collect();
    ids.sort();
    Ok(ids)
}

/// `_dispatch(args, hub)` with the process's stdout/stderr and environment.
pub fn dispatch(cli: &Cli, hub: &Hub) -> HubResult<i32> {
    let mut env = CliEnv::process();
    dispatch_io(cli, hub, &mut env, &mut std::io::stdout(), &mut std::io::stderr())
}

/// `_dispatch(args, hub)`.
#[allow(clippy::too_many_lines)]
pub fn dispatch_io(cli: &Cli, hub: &Hub, env: &mut CliEnv, out: &mut dyn Write, err: &mut dyn Write) -> HubResult<i32> {
    match &cli.cmd {
        Cmd::Init => {
            let r = hub.init_content()?;
            hub.auth.ensure_bootstrap_key(&hub.cfg().data_dir)?;
            let text = format!(
                "content repo ready at {} ({} file(s) seeded)",
                s(&r, "content_dir"),
                len_of(get(&r, "seeded"))
            );
            emit(cli, out, &r, &text);
            Ok(0)
        }
        Cmd::Status => {
            hub.require_repo()?;
            let ch = hub.changes()?;
            let text = arr(get(&ch, "items"))
                .iter()
                .map(|c| format!("{} {}", ljust(&s(c, "status"), 9), s(c, "path")))
                .collect::<Vec<_>>()
                .join("\n");
            let text = if text.is_empty() {
                "no pending changes".to_string()
            } else {
                text
            };
            emit(cli, out, &ch, &text);
            Ok(0)
        }
        Cmd::Commit { message } => {
            let r = hub.commit(message, ACTOR)?;
            let text = format!("committed {}", take_chars(&s(&r, "commit"), 12));
            emit(cli, out, &r, &text);
            Ok(0)
        }
        Cmd::Applies { client } => {
            let r = hub.list_applies(client)?;
            let text = arr(get(&r, "items"))
                .iter()
                .map(|a| {
                    format!(
                        "{}  {}{}",
                        s(a, "apply_id"),
                        s(a, "counts"),
                        if truthy(a.get("rolled_back_at")) {
                            "  [rolled back]"
                        } else {
                            ""
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            let text = if text.is_empty() {
                "no applies recorded".to_string()
            } else {
                text
            };
            emit(cli, out, &r, &text);
            Ok(0)
        }
        Cmd::Rollback {
            client,
            apply_id,
            dry_run,
            yes,
            force_paths,
        } => {
            let o = hub.rollback(client, apply_id.as_deref(), *yes, *dry_run, force_paths, ACTOR)?;
            let mut lines = vec![format!("{}: apply {}", s(&o, "status"), s(&o, "apply_id"))];
            for c in arr(get(&o, "changed")) {
                lines.push(format!(
                    "  {} {}:{}",
                    ljust(&s(c, "action"), 8),
                    s(c, "root"),
                    s(c, "path")
                ));
            }
            for c in arr(get(&o, "conflicts")) {
                lines.push(format!(
                    "  CONFLICT {}:{}: {} (skipped; --force-paths to override)",
                    s(c, "root"),
                    s(c, "path"),
                    s(c, "reason")
                ));
            }
            if truthy(o.get("message")) {
                lines.push(s(&o, "message"));
            }
            if *dry_run {
                lines.extend(arr(get(&o, "changed")).iter().map(|c| s(c, "diff")));
            }
            emit(cli, out, &o, &lines.join("\n"));
            Ok(i32::from(truthy(o.get("conflicts")) && !*dry_run))
        }
        Cmd::MigrateContent { to, yes } => {
            let r = hub.migrate_content(to, *yes)?;
            if truthy(r.get("dry_run")) {
                let text = format!("would move {} -> {}; re-run with --yes", s(&r, "from"), s(&r, "to"));
                emit(cli, out, &r, &text);
                return Ok(1);
            }
            let text = format!(
                "content moved to {} (HEAD {}). Set HUB_CONTENT_DIR={} (not needed if that is <data_dir>/content).",
                s(&r, "to"),
                take_chars(&s(&r, "head"), 12),
                s(&r, "to")
            );
            emit(cli, out, &r, &text);
            Ok(0)
        }
        Cmd::RetireBootstrapKey => Ok(retire_bootstrap_key(hub, env, out, err)),
        Cmd::Doctor { doctor_json } => {
            let ks = knowledge_summary(hub);
            let rep = hub.doctor(ks.as_ref())?;
            if cli.json || *doctor_json {
                let _ = writeln!(out, "{}", json_indent2(&rep));
            } else {
                let counts = get(&rep, "counts");
                let _ = writeln!(
                    out,
                    "status: {}  ({} crit, {} warn, {} info)",
                    s(&rep, "status"),
                    s(counts, "crit"),
                    s(counts, "warn"),
                    s(counts, "info")
                );
                for f in arr(get(&rep, "findings")) {
                    if s(f, "severity") == "ok" {
                        continue;
                    }
                    let _ = writeln!(
                        out,
                        "[{}] {}",
                        ljust(&s(f, "severity").to_uppercase(), 4),
                        s(f, "title")
                    );
                    if truthy(f.get("detail")) {
                        let _ = writeln!(out, "       {}", s(f, "detail"));
                    }
                    if truthy(f.get("fix")) {
                        let _ = writeln!(out, "       fix: {}", s(f, "fix").replace('\n', "\n            "));
                    }
                }
            }
            Ok(if truthy(get(&rep, "counts").get("crit")) { 2 } else { 0 })
        }
        Cmd::AttestMcp { name, yes } => {
            let reference = "your MCP scanner";
            if !*yes {
                let _ = writeln!(
                    err,
                    "Run the scanner yourself first ({reference}); this command does NOT run it. Then re-run with --yes to attest {} as clean for its current command/args/url/pinned_ref/transport.",
                    hub_core::py::repr(name)
                );
                return Ok(1);
            }
            let item = {
                let _g = hub.mutating()?;
                hub.work.attest_mcp(name)?
            };
            let payload = json!({"name": item.name, "scan_status": "clean"});
            let text = format!(
                "attested {} (uncommitted; commit it to approve). Scanner: {reference}",
                item.name
            );
            emit(cli, out, &payload, &text);
            Ok(0)
        }
        Cmd::EvaluateSkill { name, tiers } => {
            let rec = hub.skilleval.evaluate(name, tiers, ACTOR)?;
            emit(cli, out, &rec, &eval_text(&rec));
            Ok(eval_exit(get(&rec, "verdict")))
        }
        Cmd::EvaluateSkills { tiers } => {
            let names = hub.work.names("skill")?;
            if names.is_empty() {
                emit(cli, out, &json!({"items": []}), "no skills in the working tree");
                return Ok(0);
            }
            if hub.skilleval.find_launcher().is_none() {
                return Err(evaluator_missing());
            }
            let recs = hub.skilleval.evaluate_many(&names, tiers, ACTOR)?;
            emit(cli, out, &json!({"items": recs}), &eval_table(&recs));
            Ok(batch_exit(&recs))
        }
        Cmd::Adapters => {
            let views = hub.adapters_view();
            let text = views
                .iter()
                .map(|v| format!("{} {}", ljust(&s(v, "id"), 14), s(v, "display_name")))
                .collect::<Vec<_>>()
                .join("\n");
            let text = if text.is_empty() {
                "no adapters installed".to_string()
            } else {
                text
            };
            let load_errors: serde_json::Map<String, Value> = hub
                .registry
                .load_errors
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect();
            emit(cli, out, &json!({"adapters": views, "load_errors": load_errors}), &text);
            Ok(0)
        }
        Cmd::Validate => {
            hub.require_repo()?;
            let r = hub.validate_content()?;
            let ok = truthy(r.get("ok"));
            let text = if ok {
                "content is valid".to_string()
            } else {
                arr(get(&r, "problems"))
                    .iter()
                    .map(|p| format!("{}: {}", s(p, "path"), s(p, "message")))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            emit(cli, out, &r, &text);
            Ok(i32::from(!ok))
        }
        Cmd::Plan(c) => {
            let plan = plan_value(hub, &c.client)?;
            emit(cli, out, &plan, &plan_text(&plan));
            Ok(if any_blocked(&plan) { 2 } else { 0 })
        }
        Cmd::Apply { clients, adopt, yes } => {
            let plan = plan_value(hub, &clients.client)?;
            if any_blocked(&plan) {
                emit(cli, out, &plan, &plan_text(&plan));
                return Ok(2);
            }
            if !*yes {
                let text = format!(
                    "{}\n\nnothing applied: re-run with --yes to apply this plan",
                    plan_text(&plan)
                );
                emit(cli, out, &plan, &text);
                return Ok(1);
            }
            let res = hub.apply(&s(&plan, "id"), true, adopt, ACTOR)?;
            let results = arr(get(&res, "results"));
            let bad = results.iter().any(|r| {
                r.get("status").and_then(Value::as_str) == Some("failed")
                    || r.get("verify_ok") == Some(&Value::Bool(false))
            });
            let text = results
                .iter()
                .map(|r| {
                    let verify = get(r, "verify_ok");
                    let v = if verify.is_null() {
                        String::new()
                    } else {
                        format!(", verify {}", if truthy(Some(verify)) { "ok" } else { "FAILED" })
                    };
                    format!(
                        "{}: {}, wrote {}, removed {}, {} conflict(s) skipped{v}",
                        s(r, "client"),
                        s(r, "status"),
                        len_of(get(r, "written")),
                        len_of(get(r, "removed")),
                        len_of(get(r, "skipped_conflicts"))
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            emit(cli, out, &res, &text);
            Ok(i32::from(bad))
        }
        Cmd::Verify(c) | Cmd::Drift(c) | Cmd::Audit(c) => {
            let name = cli.cmd.name();
            let mut results = Vec::new();
            let mut code = 0;
            for cid in clients_of(hub, &c.client)? {
                let r = match name {
                    "audit" => hub.audit_client(&cid)?,
                    "verify" => hub.verify(&cid)?,
                    _ => hub.drift(&cid)?,
                };
                if name != "drift" && !truthy(r.get("ok")) {
                    code = if name == "audit" { 2 } else { 1 };
                }
                results.push(r);
            }
            let text = results
                .iter()
                .map(|r| take_chars(&dumps(r, &Opts::default()), 600).to_string())
                .collect::<Vec<_>>()
                .join("\n");
            emit(cli, out, &Value::Array(results), &text);
            Ok(code)
        }
        Cmd::Import {
            client,
            all,
            item,
            on_conflict,
            override_reason,
        } => {
            let selection: Option<Vec<(String, String)>> = if *all {
                None
            } else {
                let mut sel = Vec::new();
                for i in item {
                    let Some((k, n)) = i.split_once(':') else {
                        // Python builds a 1-tuple here and dies unpacking it (traceback, exit 1).
                        let _ = writeln!(err, "error: --item must be kind:name, got {}", hub_core::py::repr(i));
                        return Ok(1);
                    };
                    sel.push((k.to_string(), n.to_string()));
                }
                Some(sel)
            };
            if !*all && selection.as_ref().is_none_or(Vec::is_empty) {
                let _ = writeln!(err, "error: pass --all or --item kind:name");
                return Ok(1);
            }
            let r = hub.import_items(
                client,
                selection.as_deref(),
                on_conflict.as_deref(),
                true,
                &indexmap::IndexMap::new(),
                override_reason.as_deref(),
                ACTOR,
            )?;
            let rows = arr(get(&r, "results"));
            let text = rows
                .iter()
                .map(|x| {
                    format!(
                        "{} {}:{} {}{}",
                        ljust(&s(x, "status"), 10),
                        s(x, "kind"),
                        s(x, "final_name"),
                        s(x, "detail"),
                        gate_suffix(x)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            emit(cli, out, &r, &text);
            Ok(refused_or_error(rows))
        }
        Cmd::Observe { client } => {
            let r = hub.observe(client, ACTOR)?;
            let bs = get(&r, "by_hub_status");
            let notes: String = arr(get(&r, "notes"))
                .iter()
                .map(|n| format!("\n  note: {}", py_str(n)))
                .collect();
            let text = format!(
                "{client}: {} skill(s) observed{}; not in hub {}, differs {}, same {}; origins {}{notes}",
                s(&r, "total"),
                if truthy(r.get("complete")) { "" } else { " (INCOMPLETE)" },
                s(bs, "not_in_hub"),
                s(bs, "in_hub_differs"),
                s(bs, "in_hub_same"),
                sorted_pairs(get(&r, "by_origin"), false)
            );
            emit(cli, out, &r, &text);
            Ok(0)
        }
        Cmd::ImportObserved {
            client,
            key,
            name,
            on_conflict,
            enable,
            override_reason,
        } => {
            if name.as_deref().is_some_and(|n| !n.is_empty()) && key.len() != 1 {
                let _ = writeln!(err, "error: --name needs exactly one --key");
                return Ok(1);
            }
            let items: Vec<Value> = key.iter().map(|k| json!({"key": k, "name": name})).collect();
            let r = hub.import_observed(client, &items, on_conflict, *enable, override_reason.as_deref(), ACTOR)?;
            let rows = arr(get(&r, "results"));
            let text = rows
                .iter()
                .map(|x| {
                    format!(
                        "{} {} -> {} {}{}",
                        ljust(&s(x, "status"), 10),
                        s(x, "key"),
                        s(x, "final_name"),
                        s(x, "detail"),
                        gate_suffix(x)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            emit(cli, out, &r, &text);
            Ok(refused_or_error(rows))
        }
        Cmd::Evaluate {
            source,
            select,
            subject,
            tiers,
            force,
            wait,
        } => {
            let subjects = if subject.is_empty() {
                hub.eval_select(source, select.as_deref().unwrap_or("none_or_stale"))?
            } else {
                subject.clone()
            };
            if subjects.is_empty() {
                emit(cli, out, &json!({"items": []}), "nothing to evaluate");
                return Ok(0);
            }
            if !*wait {
                // A queue lives in the memory of the hub SERVER; this process exits when done. Say so instead of
                // dropping the jobs.
                let listed = subjects
                    .iter()
                    .take(50)
                    .map(|x| format!("  {x}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                let text = format!(
                    "{} subject(s) selected; nothing was run. Add --wait to evaluate them here, or queue them in the running hub (POST /api/v1/skill-evaluations/run).\n{listed}",
                    subjects.len()
                );
                emit(cli, out, &json!({"subjects": subjects, "queued": false}), &text);
                return Ok(0);
            }
            if hub.skilleval.find_launcher().is_none() {
                return Err(evaluator_missing());
            }
            let recs = hub.skilleval.evaluate_subjects(&subjects, tiers, ACTOR, *force)?;
            emit(cli, out, &json!({"items": recs}), &eval_table(&recs));
            Ok(batch_exit(&recs))
        }
        Cmd::Serve { port } => serve(hub, *port, out, err),
    }
}

fn gate_suffix(x: &Value) -> String {
    if truthy(x.get("gate")) {
        format!("  [gate {}]", s(x, "gate"))
    } else {
        String::new()
    }
}

fn refused_or_error(rows: &[Value]) -> i32 {
    i32::from(
        rows.iter()
            .any(|x| matches!(x.get("status").and_then(Value::as_str), Some("error" | "refused"))),
    )
}

/// `hub.plan(args.client).model_dump(mode="json")`.
fn plan_value(hub: &Hub, clients: &[String]) -> HubResult<Value> {
    let wanted = if clients.is_empty() { None } else { Some(clients) };
    let plan = hub.plan(wanted, "committed")?;
    serde_json::to_value(plan).map_err(|_| Problem::internal())
}

fn any_blocked(plan: &Value) -> bool {
    arr(get(plan, "clients")).iter().any(|c| truthy(c.get("blocked")))
}

/// `asyncio.run(asyncio.wait_for(hub.knowledge.summary(), 4))`, every failure suppressed (None).
fn knowledge_summary(hub: &Hub) -> Option<Value> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(4), hub.knowledge.summary())
            .await
            .ok()
    })
}

/// `_retire_bootstrap_key(hub)`: remove the admin key from disk once the operator has it elsewhere. TTY only; the key is
/// verified BEFORE anything is touched; the file is overwritten with zeros, then unlinked (best effort on
/// journaling/CoW filesystems).
fn retire_bootstrap_key(hub: &Hub, env: &mut CliEnv, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let f = hub.cfg().data_dir.join(BOOTSTRAP_KEY_FILE);
    if std::fs::symlink_metadata(&f).is_err() {
        let _ = writeln!(out, "the bootstrap key file is already gone; nothing to retire");
        return 0;
    }
    if !env.stdin_tty {
        let _ = writeln!(
            err,
            "error: retire-bootstrap-key needs an interactive terminal (it asks for the key)"
        );
        return 1;
    }
    let key = strip(&(env.prompt)(PROMPT).unwrap_or_default()).to_string();
    let p = if key.is_empty() {
        None
    } else {
        hub.auth.authenticate(&key)
    };
    if !p.is_some_and(|p| p.role == "admin") {
        let _ = writeln!(
            err,
            "error: that key did not verify as an admin key; the file was NOT touched"
        );
        return 1;
    }
    if !std::fs::symlink_metadata(&f).is_ok_and(|m| m.file_type().is_file()) {
        let _ = writeln!(
            err,
            "error: the bootstrap key path is not a regular file; refusing to touch it"
        );
        return 1;
    }
    if let Ok(m) = std::fs::metadata(&f)
        && let Ok(mut fh) = std::fs::OpenOptions::new().read(true).write(true).open(&f)
    {
        let size = usize::try_from(m.len()).unwrap_or(0);
        let _ = fh.write_all(&vec![0u8; size]);
        let _ = fh.flush();
        let _ = fh.sync_all();
    }
    if let Err(e) = std::fs::remove_file(&f) {
        // Python lets the OSError escape (traceback, exit 1).
        let _ = writeln!(
            err,
            "error: {}",
            hub_core::py::os::error_str(&e, Some(&f.to_string_lossy()))
        );
        return 1;
    }
    let _ = writeln!(
        out,
        "bootstrap key file removed. Keep the admin key in a password manager: hubctl and the UI will now prompt for it."
    );
    0
}

/// `hubctl serve [--port N]`: loopback-only bind, the bootstrap key on first start, then the API + UI.
fn serve(hub: &Hub, port: Option<i64>, out: &mut dyn Write, err: &mut dyn Write) -> HubResult<i32> {
    let host = hub.cfg().host;
    crate::config::check_bind(&host).map_err(|e| Problem::unavailable(e).code("nonloopback_bind_refused"))?;
    let port = port.filter(|p| *p != 0).unwrap_or(hub.cfg().port);
    {
        // the Host allowlist follows the effective bind
        let mut c = hub.cfg.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        c.port = port;
    }
    let data = hub.cfg().data_dir;
    let kf = hub.auth.ensure_bootstrap_key(&data)?;
    let keyfile = kf.unwrap_or_else(|| data.join(BOOTSTRAP_KEY_FILE));
    if keyfile.exists() {
        let k = keyfile.display();
        let _ = writeln!(
            out,
            "admin key file: {k} (0600). Use it: export HUB_API_KEY=$(cat {k})  # the UI asks for it on first load\nBetter: store the key in a password manager, then run `hubctl retire-bootstrap-key` so no key sits on disk."
        );
    } else {
        let _ = writeln!(
            out,
            "no bootstrap key file on disk (retired): hubctl and the UI prompt for the admin key"
        );
    }
    if hub.cfg().trust_loopback {
        let _ = writeln!(
            err,
            "WARNING: HUB_TRUST_LOOPBACK is on: local processes are admin without a key"
        );
    }
    let _ = out.flush();
    // `dispatch` gets `&Hub`; the server needs the Arc (the services hold a Weak to it).
    let Some(arc) = hub.skilleval.hub.upgrade() else {
        return Ok(1);
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_writer(std::io::stderr)
        .try_init();
    let cfg = hub.cfg();
    let Ok(rt) = tokio::runtime::Builder::new_multi_thread().enable_all().build() else {
        return Ok(1);
    };
    Ok(rt.block_on(async move {
        let listener = match crate::server::bind(&cfg).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(
                    "error while attempting to bind on address ({}, {}): {e}",
                    cfg.host,
                    cfg.port
                );
                return 1;
            }
        };
        match crate::server::serve(crate::server::create_app(arc), listener).await {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_text_and_table_formats() {
        let rec = json!({"skill": "alpha", "verdict": "incomplete", "tiers": "static", "run_id": "r1", "duration_ms": 1250,
            "tier1": {"quality_score": 83.0, "grade": "B", "judge_score": null, "errors": 2, "warnings": 23,
                      "severity": {"critical": 0, "high": 1, "medium": 6, "low": 15}, "incomplete": ["skillspector"]},
            "findings": [{"severity": "high", "validator": "Schema & Repository Governance", "message": "Author not specified"}],
            "error": null, "reused_from": {"subject": "hub:x", "run_id": "r0"}});
        assert_eq!(
            eval_text(&rec),
            "alpha: incomplete  (tiers static, run r1, 1.2 s)  [reused r0]\n  quality 83.0 grade B  judge -  errors 2 warnings 23  critical 0 high 1 medium 6\n  incomplete scans: skillspector\n  [HIGH] Schema & Repository Governance: Author not specified\n  advisory only: plan and apply are not affected"
        );
        let err_row = json!({"skill": "obs1:k", "subject": "obs1:k", "tiers": "static", "verdict": "error", "error": "boom", "run_id": null});
        let table = eval_table(&[rec, err_row]);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(
            lines[0],
            format!(
                "{:<40} {:<10} {:>7} {:>5} {:>6} {:>6} {:>4}",
                "skill", "verdict", "quality", "grade", "judge", "errors", "high"
            )
        );
        assert_eq!(
            lines[1].split_whitespace().collect::<Vec<_>>(),
            ["alpha", "incomplete", "83.0", "B", "-", "2", "1"]
        );
        assert!(lines[2].starts_with("obs1:k") && lines[2].ends_with("  boom"));
        let observed = json!({"skill": "k", "subject": "obs1:k", "source": "obs1", "verdict": "passed"});
        assert!(
            eval_table(&[observed])
                .lines()
                .nth(1)
                .is_some_and(|l| l.starts_with("obs1:k "))
        );
    }

    #[test]
    fn plan_text_format() {
        let plan = json!({"id": "plan_1", "content_hash": "0123456789abcdef", "pending_changes": 2, "clients": [
            {"client": "c1", "summary": {"write": 1, "delete": 2}, "blocked": true,
             "files": [{"action": "write", "root": "home", "path": "a", "reason": ""},
                       {"action": "unchanged", "root": "home", "path": "b", "reason": ""},
                       {"action": "conflict", "root": "home", "path": "c", "reason": "edited"}],
             "blocked_reasons": ["floor says no"]},
            {"client": "c2", "summary": {}, "blocked": false, "files": [], "blocked_reasons": []}]});
        assert_eq!(
            plan_text(&plan),
            "plan plan_1  content 0123456789ab\n  note: 2 uncommitted content change(s) are NOT in this plan\n- c1: 2 delete, 1 write  [BLOCKED]\n    write     home:a\n    conflict  home:c  (edited)\n    ! floor says no\n- c2: nothing to do"
        );
    }

    #[test]
    fn py_str_and_exit_codes() {
        assert_eq!(
            py_str(&json!({"written": 2, "x": null, "f": 1.0})),
            "{'written': 2, 'x': None, 'f': 1.0}"
        );
        assert_eq!(eval_exit(&json!("passed")), 0);
        assert_eq!(eval_exit(&json!("failed")), 2);
        assert_eq!(eval_exit(&json!("incomplete")), 2);
        assert_eq!(eval_exit(&json!("error")), 1);
        assert_eq!(eval_exit(&Value::Null), 1);
        assert_eq!(fmt_opt(Some(&json!(83)), true), "83.0");
        assert_eq!(fmt_opt(None, false), "-");
        assert_eq!(
            json_indent2(&json!({"a": [1, "é"]})),
            "{\n  \"a\": [\n    1,\n    \"\\u00e9\"\n  ]\n}"
        );
    }
}
