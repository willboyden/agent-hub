//! Port of backend/tests/adapters/test_hermes.py (adapters/hermes.rs render/discover/verify, and deploy/hermes-deliver.sh
//! run for real with a fake `docker` on PATH), plus the hermes golden cases recorded from Python
//! (tests/golden/adapters.json: 10 renders, 7 discovers, caps/default_config/docs). Owner: WS-A.
//! The deliver-script tests keep testing that (unported, bash) script after pytest is gone at cutover.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod adapter_fakes;

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use adapter_fakes::*;
use agent_hub::adapters::_util as u;
use agent_hub::adapters::base::{ClientAdapter, ClientConfig, Expected, RenderResult};
use agent_hub::adapters::hermes::{BASELINE_DISABLED, HermesAdapter, listed_names};
use hub_core::value::PyValue;
use serde_json::json;

const STAGE: &str = "/w/stage";
const DEST: &str = "/data/skills/hub";

fn adapter() -> HermesAdapter {
    HermesAdapter
}

fn cfg() -> ClientConfig {
    let mut c = adapter().default_config();
    c.roots = roots(&[("stage", STAGE)]);
    set_param(&mut c, "compose_project", s("myproj"));
    set_param(&mut c, "docker_context", s("myctx"));
    set_param(&mut c, "exec_user", s("agent"));
    c
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn deliver() -> PathBuf {
    repo().join("deploy").join("hermes-deliver.sh")
}

fn policy(res: &RenderResult) -> PyValue {
    yaml_load(&art(res, "config.policy.fragment.yaml").content)
}

fn strs(v: Option<&PyValue>) -> Vec<String> {
    v.and_then(PyValue::as_list)
        .unwrap_or_default()
        .iter()
        .map(PyValue::py_str)
        .collect()
}

#[test]
fn test_default_config_is_neutral_placeholders() {
    let d = HermesAdapter.default_config();
    assert!(d.id == "hermes" && d.adapter == "hermes-agent");
    assert_eq!(d.param("compose_project"), Some(&s("CHANGEME")));
    assert_eq!(d.param("source_dirs"), Some(&PyValue::List(vec![])));
    assert!(d.roots["stage"].contains("~/path/to"));
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/adapters");
    let names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert!(!names.iter().any(|n| n.contains("comfy")));
}

#[test]
fn test_golden() {
    let (n, bad) = render_golden("hermes/sample");
    assert_none("hermes sample", n, &bad);
}

#[test]
fn golden_render_cases_equal_python() {
    let (n, bad) = render_golden("hermes/");
    assert_none("hermes render", n, &bad);
}

#[test]
fn golden_discover_cases_equal_python() {
    let (n, bad) = discover_golden("hermes/");
    assert_none("hermes discover", n, &bad);
}

#[test]
fn golden_meta_equals_python() {
    assert_eq!(meta_golden("hermes-agent"), Vec::<String>::new());
}

#[test]
fn test_deterministic() {
    let ctx = sample_ctx(&cfg());
    assert_eq!(
        adapter().render(&ctx).unwrap(),
        adapter().render(&shuffled(&ctx)).unwrap()
    );
}

#[test]
fn test_stage_layout_and_advisory_fragments() {
    let res = adapter().render(&sample_ctx(&cfg())).unwrap();
    assert!(res.artifacts.iter().any(|a| a.path == "skills/example-skill/SKILL.md"));
    assert!(res.artifacts.iter().any(|a| a.path == "AGENTS.md"));
    let pol = art(&res, "config.policy.fragment.yaml");
    assert!(pol.merge == "yaml_keys" && pol.managed_keys == ["agent.disabled_toolsets", "approvals.deny"]);
    let doc = policy(&res);
    let disabled = strs(doc.get("agent").and_then(|a| a.get("disabled_toolsets")));
    assert!(BASELINE_DISABLED.iter().all(|b| disabled.iter().any(|d| d == b)));
    let deny = strs(doc.get("approvals").and_then(|a| a.get("deny")));
    assert!(!deny.contains(&"*sudo *".to_string())); // ask-rule is not a deny
    assert!(!deny.contains(&"***/private-keys/**".to_string()) && deny.contains(&"**/private-keys/**".to_string()));
    let mcp = yaml_load(&art(&res, "config.mcp.fragment.yaml").content);
    let servers = mcp.get("mcp_servers").unwrap();
    assert_eq!(
        enc(servers.get("serena").unwrap()),
        json!({"command": "serena", "args": ["start-mcp-server"]})
    );
    assert_eq!(
        enc(servers.get("docs").unwrap()),
        json!({"url": "http://mcpo:8000/docs"})
    );
    assert!(codes(&res).contains("env_names_omitted"));
    assert!(res.artifacts.iter().all(|a| a.root == "stage"));
}

#[test]
fn test_agents_and_memory_unsupported() {
    let res = adapter().render(&sample_ctx(&cfg())).unwrap();
    let n = res
        .diagnostics
        .iter()
        .filter(|d| d.code == "unsupported_capability" && d.severity == "error")
        .count();
    assert_eq!(n, 2);
}

#[test]
fn test_baseline_floor_is_the_expected_data() {
    let set: BTreeSet<&str> = BASELINE_DISABLED.iter().copied().collect();
    assert!(set.len() == BASELINE_DISABLED.len() && set.len() == 14);
    for t in [
        "web",
        "search",
        "browser",
        "computer_use",
        "image_gen",
        "cronjob",
        "code_execution",
        "desktop_ui",
        "tts",
    ] {
        assert!(set.contains(t));
    }
    let golden: Vec<String> = GOLDEN["constants"]["baseline_disabled"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect();
    assert_eq!(golden, BASELINE_DISABLED.to_vec());
}

#[test]
fn test_weakened_floor_param_is_error_and_render_still_full() {
    let mut c = cfg();
    set_param(&mut c, "disabled_toolsets", list(&["web", "cronjob"]));
    let res = adapter().render(&sample_ctx(&c)).unwrap();
    let d: Vec<_> = res.diagnostics.iter().filter(|x| x.code == "floor_weakened").collect();
    assert!(!d.is_empty() && d[0].severity == "error" && d[0].message.contains("browser"));
    let disabled = strs(policy(&res).get("agent").and_then(|a| a.get("disabled_toolsets")));
    assert!(BASELINE_DISABLED.iter().all(|b| disabled.iter().any(|x| x == b)));
}

#[test]
fn test_extra_disabled_toolsets_allowed_and_sorted() {
    let mut c = cfg();
    let mut l: Vec<&str> = BASELINE_DISABLED.to_vec();
    l.extend(["zzz", "aaa"]);
    set_param(&mut c, "disabled_toolsets", list(&l));
    let res = adapter().render(&sample_ctx(&c)).unwrap();
    assert!(!codes(&res).contains("floor_weakened"));
    let ts = strs(policy(&res).get("agent").and_then(|a| a.get("disabled_toolsets")));
    let mut sorted = ts.clone();
    sorted.sort();
    assert!(ts == sorted && ts.contains(&"aaa".to_string()) && ts.contains(&"zzz".to_string()));
}

#[test]
fn test_allow_rule_against_floor_toolset_is_error() {
    let mut ctx = sample_ctx(&cfg());
    ctx.permissions = rules(&[("tool", "web_search", "allow")]);
    assert!(has_diag(
        &adapter().render(&ctx).unwrap(),
        "floor_conflict",
        Some("error"),
        None
    ));
}

#[test]
fn test_policy_fragment_rendered_even_with_no_rules() {
    let res = adapter().render(&empty_ctx(&cfg())).unwrap();
    assert_eq!(
        res.artifacts.iter().map(|a| a.path.as_str()).collect::<Vec<_>>(),
        vec!["config.policy.fragment.yaml"]
    );
}

#[test]
fn test_hostile_skill_names() {
    let mut ctx = sample_ctx(&cfg());
    ctx.skills = vec![skill("../../x"), skill_with("ok", DESC, &[("a/../../b", b"x")])];
    assert!(
        !adapter()
            .render(&ctx)
            .unwrap()
            .artifacts
            .iter()
            .any(|a| a.kind == "skill")
    );
}

// ---- verify --------------------------------------------------------------------------------------------------------------
fn skill_expected(res: &RenderResult) -> Vec<Expected> {
    res.artifacts
        .iter()
        .filter(|a| a.kind == "skill")
        .map(expected)
        .collect()
}

fn staged(res: &RenderResult) -> FakeFs {
    FakeFs::new(all_files(res, STAGE))
}

#[test]
fn test_verify_container_not_running_never_claims_ok() {
    let res = adapter().render(&sample_ctx(&cfg())).unwrap();
    let run = FakeRunner::new(&[(0, "", "")]);
    let checks = adapter()
        .verify(&cfg(), &skill_expected(&res), &staged(&res), &run)
        .unwrap();
    let ask = checks.last().unwrap();
    assert!(ask.name == "hermes skills list" && !ask.ok && ask.detail == "container not running: unverified");
    assert!(checks[..checks.len() - 1].iter().all(|c| c.ok)); // staged files are fine
    let calls = run.calls();
    assert_eq!(calls[0][..3], ["docker", "--context", "myctx"]);
    assert!(calls[0].contains(&"label=com.docker.compose.project=myproj".to_string()));
    assert_eq!(*run.timeouts.lock().unwrap(), vec![15.0]);
}

#[test]
fn test_verify_asks_container_and_checks_names() {
    let res = adapter().render(&sample_ctx(&cfg())).unwrap();
    let run = FakeRunner::new(&[
        (0, "ctr1\n", ""),
        (0, "hub/example-skill\nhub/example-other-skill\n", ""),
    ]);
    let checks = adapter()
        .verify(&cfg(), &skill_expected(&res), &staged(&res), &run)
        .unwrap();
    assert!(checks.last().unwrap().ok);
    assert_eq!(
        run.calls()[1],
        [
            "docker",
            "--context",
            "myctx",
            "exec",
            "--user",
            "agent",
            "ctr1",
            "hermes",
            "skills",
            "list"
        ]
    );
    assert_eq!(*run.timeouts.lock().unwrap(), vec![15.0, 60.0]);
    let run2 = FakeRunner::new(&[(0, "ctr1\n", ""), (0, "hub/example-skill\n", "")]);
    let bad = adapter()
        .verify(&cfg(), &skill_expected(&res), &staged(&res), &run2)
        .unwrap();
    let last = bad.last().unwrap();
    assert!(!last.ok && last.detail.contains("example-other-skill"));
}

#[test]
fn test_verify_exec_failure_and_no_docker() {
    let res = adapter().render(&sample_ctx(&cfg())).unwrap();
    let r = FakeRunner::new(&[(0, "c\n", ""), (127, "", "not found")]);
    let bad = adapter()
        .verify(&cfg(), &skill_expected(&res), &staged(&res), &r)
        .unwrap();
    assert!(!bad.last().unwrap().ok && bad.last().unwrap().detail.contains("127"));
    let nodock = adapter()
        .verify(&cfg(), &skill_expected(&res), &staged(&res), &FakeRunner::raising())
        .unwrap();
    assert!(!nodock.last().unwrap().ok && nodock.last().unwrap().detail.contains("unverified"));
}

#[test]
fn test_verify_missing_staged_file() {
    let res = adapter().render(&sample_ctx(&cfg())).unwrap();
    let checks = adapter()
        .verify(
            &cfg(),
            &skill_expected(&res),
            &FakeFs::default(),
            &FakeRunner::default(),
        )
        .unwrap();
    assert!(!checks.iter().any(|c| c.ok));
}

#[test]
fn test_discover_reads_stage() {
    let res = adapter().render(&sample_ctx(&cfg())).unwrap();
    let d = adapter().discover(&cfg(), &staged(&res)).unwrap();
    let mut names: Vec<&str> = d.skills.iter().map(|x| x.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["example-other-skill", "example-skill"]);
    assert!(d.skills[0].files.contains_key("SKILL.md"));
}

// ---- deploy/hermes-deliver.sh with a fake docker on PATH ------------------------------------------------------------------
struct Deliver {
    tmp: tempfile::TempDir,
}

impl Deliver {
    fn path(&self) -> &Path {
        self.tmp.path()
    }
    fn args(&self) -> Option<Vec<String>> {
        let raw = std::fs::read(self.path().join("docker.args")).ok()?;
        Some(String::from_utf8_lossy(&raw).split('\0').map(str::to_string).collect())
    }
}

struct DeliverOpts<'a> {
    link: bool,
    docker_rc: i32,
    env_over: &'a [(&'a str, Option<&'a str>)],
    compose_name: &'a str,
}

impl Default for DeliverOpts<'_> {
    fn default() -> Self {
        Self {
            link: false,
            docker_rc: 0,
            env_over: &[],
            compose_name: "docker-compose.yml",
        }
    }
}

fn run_deliver(stage_files: &[(&str, &str)], o: &DeliverOpts<'_>) -> (Output, Deliver) {
    let d = Deliver {
        tmp: tempfile::tempdir().unwrap(),
    };
    let tmp = d.path();
    let stage = tmp.join("stage").join("skills");
    std::fs::create_dir_all(&stage).unwrap();
    for (rel, txt) in stage_files {
        let p = stage.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, txt).unwrap();
    }
    if o.link {
        symlink("/etc/hostname", stage.join("evil")).unwrap();
    }
    let compose = tmp.join(o.compose_name);
    std::fs::write(&compose, "services: {}\n").unwrap();
    let bindir = tmp.join("bin");
    std::fs::create_dir(&bindir).unwrap();
    let shim = bindir.join("docker");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nprintf \"%s\\0\" \"$@\" > \"{t}/docker.args\"\ncat > \"{t}/docker.stdin\"\nexit {rc}\n",
            t = tmp.display(),
            rc = o.docker_rc
        ),
    )
    .unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut env: BTreeMap<String, String> = [
        ("PATH", format!("{}:/usr/bin:/bin", bindir.display())),
        ("HOME", tmp.display().to_string()),
        ("HERMES_STAGE_DIR", stage.display().to_string()),
        ("HERMES_COMPOSE_FILE", compose.display().to_string()),
        ("HERMES_SERVICE", "agentsvc".into()),
        ("HERMES_SKILLS_DEST", DEST.into()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    for (k, v) in o.env_over {
        match v {
            None => env.remove(*k),
            Some(v) => env.insert((*k).to_string(), (*v).to_string()),
        };
    }
    let out = Command::new("bash")
        .arg(deliver())
        .env_clear()
        .envs(&env)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (out, d)
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn test_deliver_streams_skills_via_compose_run() {
    let (r, d) = run_deliver(
        &[
            ("s1/SKILL.md", "---\nname: s1\n---\n"),
            ("s1/__pycache__/x.pyc", "junk"),
        ],
        &DeliverOpts {
            env_over: &[
                ("HERMES_DOCKER_CONTEXT", Some("myctx")),
                ("HERMES_BIN", Some("hermes-x")),
                ("HERMES_PATH_PREFIX", Some("/opt/h/bin")),
            ],
            ..DeliverOpts::default()
        },
    );
    assert_eq!(r.status.code(), Some(0), "{}", stderr(&r));
    let args = d.args().unwrap();
    assert_eq!(args[..3], ["--context", "myctx", "compose"]);
    for a in ["run", "--rm", "-T", "--no-deps", "agentsvc"] {
        assert!(args.iter().any(|x| x == a), "{a}");
    }
    let ep = args.iter().position(|x| x == "--entrypoint").unwrap();
    assert_eq!(args[ep + 1], "/bin/sh");
    let i = args.iter().position(|x| x == "-c").unwrap();
    assert_eq!(args[i + 2..i + 6], ["sh", DEST, "hermes-x", "/opt/h/bin"]); // destination passed as data
    assert!(!args[i + 1].contains(DEST) && args[i + 1].contains("mv ") && args[i + 1].contains("skills list"));
    let listing = Command::new("tar")
        .arg("-tf")
        .arg(d.path().join("docker.stdin"))
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&listing.stdout);
    assert!(listing.contains("s1/SKILL.md") && !listing.contains("__pycache__"));
}

#[test]
fn test_deliver_without_context_omits_it() {
    let (r, d) = run_deliver(&[("s/SKILL.md", "x")], &DeliverOpts::default());
    assert_eq!(r.status.code(), Some(0), "{}", stderr(&r));
    assert_eq!(d.args().unwrap()[..2], ["compose", "-f"]);
}

#[test]
fn test_deliver_refuses_when_required_env_unset() {
    for var in [
        "HERMES_STAGE_DIR",
        "HERMES_COMPOSE_FILE",
        "HERMES_SERVICE",
        "HERMES_SKILLS_DEST",
    ] {
        let (r, d) = run_deliver(
            &[("s/SKILL.md", "x")],
            &DeliverOpts {
                env_over: &[(var, None)],
                ..DeliverOpts::default()
            },
        );
        assert_eq!(r.status.code(), Some(2), "{var}");
        assert!(stderr(&r).contains(var) && d.args().is_none(), "{var}");
    }
}

#[test]
fn test_deliver_refuses_unsafe_values() {
    for (var, val) in [
        ("HERMES_SKILLS_DEST", "/"),
        ("HERMES_SKILLS_DEST", "/skills"),
        ("HERMES_SKILLS_DEST", "/a/../b/c"),
        ("HERMES_SKILLS_DEST", "rel/path"),
        ("HERMES_SKILLS_DEST", "/a/b; rm -rf /"),
        ("HERMES_SERVICE", "x; y"),
        ("HERMES_BIN", "h; id"),
        ("HERMES_PATH_PREFIX", "/a b"),
    ] {
        let (r, d) = run_deliver(
            &[("s/SKILL.md", "x")],
            &DeliverOpts {
                env_over: &[(var, Some(val))],
                ..DeliverOpts::default()
            },
        );
        assert_eq!(r.status.code(), Some(2), "{var}={val}");
        assert!(d.args().is_none(), "{var}={val}");
    }
}

#[test]
fn test_deliver_only_accepts_compose_named_files() {
    let (r, d) = run_deliver(
        &[("s/SKILL.md", "x")],
        &DeliverOpts {
            compose_name: "passwd.yml",
            ..DeliverOpts::default()
        },
    );
    assert_eq!(r.status.code(), Some(2));
    assert!(stderr(&r).contains("must be named") && d.args().is_none());
}

#[test]
fn test_deliver_refuses_compose_symlink() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    std::fs::write(t.join("real.yml"), "services: {}\n").unwrap();
    symlink(t.join("real.yml"), t.join("docker-compose.yml")).unwrap();
    std::fs::create_dir_all(t.join("stage/s")).unwrap();
    std::fs::write(t.join("stage/s/SKILL.md"), "x").unwrap();
    let r = Command::new("bash")
        .arg(deliver())
        .env_clear()
        .envs([
            ("PATH", "/usr/bin:/bin".to_string()),
            ("HOME", t.display().to_string()),
            ("HERMES_STAGE_DIR", t.join("stage").display().to_string()),
            ("HERMES_SERVICE", "svc".into()),
            (
                "HERMES_COMPOSE_FILE",
                t.join("docker-compose.yml").display().to_string(),
            ),
            ("HERMES_SKILLS_DEST", DEST.into()),
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(r.status.code(), Some(1));
    assert!(stderr(&r).contains("symlink"));
}

#[test]
fn test_deliver_refuses_empty_stage_and_never_calls_docker() {
    let (r, d) = run_deliver(&[], &DeliverOpts::default());
    assert_eq!(r.status.code(), Some(1));
    assert!(stderr(&r).contains("no SKILL.md") && d.args().is_none());
}

#[test]
fn test_deliver_refuses_symlink_and_propagates_failure() {
    let (r, _d) = run_deliver(
        &[("s/SKILL.md", "x")],
        &DeliverOpts {
            link: true,
            ..DeliverOpts::default()
        },
    );
    assert_eq!(r.status.code(), Some(1));
    assert!(stderr(&r).contains("symlink"));
    let (r2, _d2) = run_deliver(
        &[("s/SKILL.md", "x")],
        &DeliverOpts {
            docker_rc: 3,
            ..DeliverOpts::default()
        },
    );
    assert_eq!(r2.status.code(), Some(1));
    assert!(stderr(&r2).contains("FAILED"));
}

fn container_body() -> String {
    let t = std::fs::read_to_string(deliver()).unwrap();
    let start = t.find("-c '").unwrap() + 4;
    let end = t.find("  ' sh ").unwrap();
    t[start..end].to_string()
}

#[test]
fn test_deliver_script_hygiene() {
    let t = std::fs::read_to_string(deliver()).unwrap();
    assert!(t.contains("set -euo pipefail"));
    assert!(!container_body().contains('\'')); // an apostrophe would end the sh -c quote early
    assert!(!t.contains("printenv") && !t.contains("env |"));
    assert!(std::fs::metadata(deliver()).unwrap().permissions().mode() & 0o100 != 0);
    assert!(
        Command::new("bash")
            .arg("-n")
            .arg(deliver())
            .status()
            .unwrap()
            .success()
    );
    let sc = Command::new("shellcheck")
        .arg("-S")
        .arg("style")
        .arg(deliver())
        .status();
    if let Ok(st) = sc {
        assert!(st.success()); // like the Python test: only when shellcheck is installed
    }
}

/// Run the in-container script for real (sh) against a temp dir, with a fake `hermes` that prints `listing`.
fn run_body(tmp: &Path, skills: &[&str], listing: &str, preexisting: &[(&str, &str)]) -> (Output, PathBuf) {
    let dest = tmp.join("data").join("skills").join("hub");
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    for (rel, txt) in preexisting {
        let p = dest.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, txt).unwrap();
    }
    let bindir = tmp.join("bin");
    std::fs::create_dir(&bindir).unwrap();
    let fake = bindir.join("hermes");
    std::fs::write(&fake, format!("#!/bin/sh\ncat <<'EOF'\n{listing}\nEOF\n")).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let src = tmp.join("src");
    for n in skills {
        std::fs::create_dir_all(src.join(n)).unwrap();
        std::fs::write(src.join(n).join("SKILL.md"), format!("new-{n}")).unwrap();
    }
    let tar = if skills.is_empty() {
        Vec::new()
    } else {
        Command::new("tar")
            .arg("-C")
            .arg(&src)
            .args(["-cf", "-", "."])
            .output()
            .unwrap()
            .stdout
    };
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(container_body())
        .arg("sh")
        .arg(&dest)
        .arg("hermes")
        .arg(&bindir)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        let _ = stdin.write_all(&tar);
    }
    let out = child.wait_with_output().unwrap();
    (out, dest.parent().unwrap().to_path_buf())
}

fn hub_leftovers(skills: &Path) -> bool {
    std::fs::read_dir(skills)
        .unwrap()
        .any(|e| e.unwrap().file_name().to_string_lossy().starts_with(".hub."))
}

#[test]
fn test_body_exact_name_match_and_rollback() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a");
    std::fs::create_dir(&a).unwrap();
    let (r, skills) = run_body(
        &a,
        &["video"],
        "hub/video-improve-loop  ok\n",
        &[("old/SKILL.md", "OLD")],
    );
    assert_eq!(r.status.code(), Some(1));
    assert!(stderr(&r).contains("does not list skill video"));
    assert_eq!(std::fs::read_to_string(skills.join("hub/old/SKILL.md")).unwrap(), "OLD"); // rolled back
    assert!(!hub_leftovers(&skills));
    let b = tmp.path().join("b");
    std::fs::create_dir(&b).unwrap();
    let (r2, skills2) = run_body(
        &b,
        &["video", "video-improve-loop"],
        "hub/video-improve-loop\nhub/video\n",
        &[("old/SKILL.md", "OLD")],
    );
    assert_eq!(r2.status.code(), Some(0), "{}", stderr(&r2));
    assert_eq!(
        std::fs::read_to_string(skills2.join("hub/video/SKILL.md")).unwrap(),
        "new-video"
    );
    assert!(!skills2.join("hub/old").exists() && !hub_leftovers(&skills2));
}

#[test]
fn test_body_failed_extract_keeps_previous_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let (r, skills) = run_body(tmp.path(), &[], "", &[("old/SKILL.md", "OLD")]); // empty stdin: nothing extracted
    assert_eq!(r.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(skills.join("hub/old/SKILL.md")).unwrap(), "OLD");
}

#[test]
fn test_registry_loads_generic_adapter_ids() {
    let reg = agent_hub::adapters::load_registry();
    let ids: BTreeSet<String> = reg.ids().into_iter().collect();
    for id in ["claude-code", "opencode", "hermes-agent", "generic"] {
        assert!(ids.contains(id), "{id}");
    }
    for m in ["claude_code", "opencode", "hermes", "generic"] {
        assert!(!reg.load_errors.contains_key(m), "{m}");
    }
}

const LAB: &str = "/src";

fn lab_cfg(dirs: PyValue) -> ClientConfig {
    let mut c = adapter().default_config();
    set_param(&mut c, "source_root", s(LAB));
    set_param(&mut c, "source_dirs", dirs);
    c.roots = roots(&[("stage", STAGE)]);
    c
}

fn src(kind: &str, path: &str) -> PyValue {
    dict(vec![("kind", s(kind)), ("path", s(path))])
}

#[test]
fn test_discover_skills_via_source_dirs() {
    let fs = FakeFs::new([
        (
            format!("{LAB}/skills-src/assemble-video/SKILL.md"),
            b"---\nname: assemble-video\ndescription: Assemble clips.\n---\n\nbody".to_vec(),
        ),
        (format!("{LAB}/skills-src/assemble-video/tool.py"), b"print(1)".to_vec()),
        (
            format!("{LAB}/skills-src/assemble-video/__pycache__/tool.pyc"),
            b"junk".to_vec(),
        ),
        (format!("{LAB}/skills-src/loose-file.md"), b"x".to_vec()),
    ]);
    let d = adapter()
        .discover(&lab_cfg(PyValue::List(vec![src("skills", "skills-src")])), &fs)
        .unwrap();
    assert_eq!(
        d.skills.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(),
        vec!["assemble-video"]
    );
    assert_eq!(
        d.skills[0].files.keys().collect::<BTreeSet<_>>(),
        ["SKILL.md", "tool.py"]
            .iter()
            .map(|x| (*x).to_string())
            .collect::<BTreeSet<_>>()
            .iter()
            .collect()
    );
    assert_eq!(d.skills[0].description, "Assemble clips.");
}

#[test]
fn test_discover_agent_workspace_instruction_and_config_without_secrets() {
    let config: &[u8] = concat!(
        "_config_version: 34\nmodel:\n  default: __MODEL__\n  api_key: brokered-placeholder\n",
        "custom_providers:\n  - models:\n      __MODEL__:\n        context_length: __CTX__\n",
        "agent:\n  disabled_toolsets: [web, browser]\n",
        "approvals:\n  mode: manual\n  deny:\n    - '*hermes update*'\n    - '*secret-key*'\n",
        "mcp_servers:\n  serena:\n    command: /usr/local/bin/serena\n    args: [start-mcp-server, --transport, stdio]\n",
        "    env:\n      SERENA_HOME: /opt/serena-home\n      TOKEN: super-secret-value\n",
        "  remote:\n    url: http://x:1/mcp\n"
    )
    .as_bytes();
    let fs = FakeFs::new([
        (format!("{LAB}/workspace-AGENTS.md"), b"# Workspace\n\nrules\n".to_vec()),
        (format!("{LAB}/agent.config.yaml"), config.to_vec()),
    ]);
    let c = lab_cfg(PyValue::List(vec![
        src("instruction", "workspace-AGENTS.md"),
        src("config", "agent.config.yaml"),
    ]));
    let d = adapter().discover(&c, &fs).unwrap();
    assert_eq!(d.instructions[0].body, "# Workspace\n\nrules\n");
    let by = |n: &str| d.mcp_servers.iter().find(|m| m.name == n).unwrap().clone();
    assert_eq!(by("serena").env_names, vec!["SERENA_HOME", "TOKEN"]);
    assert_eq!(by("serena").scan_status, "unscanned");
    assert_eq!(by("remote").transport, "http");
    let dump = format!("{d:?}");
    assert!(!dump.contains("super-secret-value") && !dump.contains("/opt/serena-home"));
    let kinds: BTreeSet<(String, String)> = d
        .permissions
        .as_ref()
        .unwrap()
        .rules
        .iter()
        .map(|r| (r.kind.clone(), r.decision.clone()))
        .collect();
    assert_eq!(kinds, [("command".to_string(), "deny".to_string())].into());
    assert!(
        d.notes
            .iter()
            .any(|n| n.contains("disabled_toolsets") && n.contains("not imported"))
    );
}

#[test]
fn test_discover_missing_and_malformed_sources_only_note() {
    let mut c = adapter().default_config();
    c.roots = roots(&[("stage", STAGE)]);
    set_param(
        &mut c,
        "source_dirs",
        PyValue::List(vec![
            src("skills", "../x"),
            dict(vec![("kind", s("nope"))]),
            src("config", "gone.yaml"),
        ]),
    );
    let d = adapter().discover(&c, &FakeFs::default()).unwrap();
    assert!(d.skills.is_empty() && d.notes.len() >= 3);
}

#[test]
fn test_verify_uses_exact_names_not_substrings() {
    let want: BTreeSet<String> = ["hub", "video-improve-loop", "ok", "video-x"]
        .iter()
        .map(|x| (*x).to_string())
        .collect();
    assert_eq!(
        listed_names("hub/video-improve-loop  ok\n| video-x |")
            .into_iter()
            .collect::<BTreeSet<_>>(),
        want
    );
    let mut ctx = sample_ctx(&cfg());
    ctx.skills = vec![skill("video"), skill("video-improve-loop")];
    let res = adapter().render(&ctx).unwrap();
    let run = FakeRunner::new(&[(0, "c\n", ""), (0, "hub/video-improve-loop\n", "")]);
    let bad = adapter()
        .verify(&cfg(), &skill_expected(&res), &staged(&res), &run)
        .unwrap();
    let last = bad.last().unwrap();
    assert!(!last.ok && last.detail.contains("video"));
    assert!(!last.detail.rsplit(':').next().unwrap().contains("video-improve-loop"));
}

#[test]
fn test_verify_unconfigured_project_never_claims_ok() {
    let mut c = adapter().default_config();
    c.roots = roots(&[("stage", STAGE)]);
    let run = FakeRunner::default();
    let ch = adapter().verify(&c, &[], &FakeFs::default(), &run).unwrap();
    assert!(!ch.last().unwrap().ok && ch.last().unwrap().detail.contains("unverified") && run.calls().is_empty());
    set_param(&mut c, "compose_project", s("p"));
    let run2 = FakeRunner::new(&[(0, "", "")]);
    adapter().verify(&c, &[], &FakeFs::default(), &run2).unwrap();
    assert_eq!(run2.calls()[0][..2], ["docker", "ps"]); // no --context unless configured
}

#[test]
fn verify_expected_file_checks_come_first_and_stay_sorted() {
    let res = adapter().render(&sample_ctx(&cfg())).unwrap();
    let all: Vec<Expected> = res.artifacts.iter().map(expected).collect();
    let checks = adapter()
        .verify(&cfg(), &all, &staged(&res), &FakeRunner::default())
        .unwrap();
    let names: Vec<&str> = checks[..checks.len() - 1].iter().map(|c| c.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted);
    assert_eq!(
        u::sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

/// Audit F13: verify applies observe's plain-name rules to every value it puts on the docker command line (the client
/// YAML's project, service, context, user and hermes_bin, and the container name `docker ps` printed); a refused value
/// runs nothing further.
#[test]
fn test_verify_refuses_names_that_are_not_plain() {
    let res = adapter().render(&sample_ctx(&cfg())).unwrap();
    for (key, val) in [
        ("compose_project", "proj --privileged"),
        ("compose_service", "-svc"),
        ("docker_context", "ctx;rm"),
        ("exec_user", "root --privileged"),
        ("hermes_bin", "--version"),
        ("hermes_bin", "hermes; id"),
    ] {
        let mut c = cfg();
        set_param(&mut c, key, s(val));
        let run = FakeRunner::new(&[(0, "ctr1\n", ""), (0, "hub/example-skill\n", "")]);
        let checks = adapter()
            .verify(&c, &skill_expected(&res), &staged(&res), &run)
            .unwrap();
        let last = checks.last().unwrap();
        assert!(
            !last.ok && last.detail.ends_with("unverified"),
            "{key}={val}: {}",
            last.detail
        );
        assert!(run.calls().is_empty(), "{key}={val} ran {:?}", run.calls());
    }
    // a container name that is not a plain name is never exec'ed into
    let run = FakeRunner::new(&[(0, "--privileged\n", ""), (0, "hub/example-skill\n", "")]);
    let checks = adapter()
        .verify(&cfg(), &skill_expected(&res), &staged(&res), &run)
        .unwrap();
    let last = checks.last().unwrap();
    assert!(
        !last.ok && last.detail.contains("container not running"),
        "{}",
        last.detail
    );
    assert_eq!(run.calls().len(), 1);
}
