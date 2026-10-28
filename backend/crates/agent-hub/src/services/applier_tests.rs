//! Engine-level tests of the delivery pipeline (planner -> applier -> rollback) on real files in temp dirs, driven with
//! a hand-built `Resolved` and the FakeAdapter of backend/tests/core/fakes.py, so they run without the content store,
//! resolver or floor of the other workstreams. The Hub-level ports of test_apply.py / test_rollback.py /
//! test_toctou.py live in tests/ (pending those workstreams); these prove the same properties on the engine now.

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hub_core::models::ClientDoc;
use indexmap::IndexMap;
use serde_json::{Value, json};

use super::*;
use crate::adapters::AdapterRegistry;
use crate::adapters::base::{
    AdapterCaps, AdapterResult, AgentItem, Artifact, ClientAdapter, Diag, DiscoveredContent, FileSystem,
    InstructionItem, McpItem, PermissionSet, RenderContext, RenderResult, Rule, SkillItem,
};
use crate::services::floor::{Floor, load_floor};
use crate::services::merge;
use crate::services::resolver::{Resolved, Resolver};
use crate::services::rollback::Rollbacker;

type R = Result<(), Box<dyn std::error::Error>>;

const BEGIN: &str = "<!-- agent-hub:begin (managed block; edits inside are overwritten) -->";
const END: &str = "<!-- agent-hub:end -->";

/// adapters/_util.py `slice_digest` (verify compares the hub-owned slice of merged files).
fn slice_digest(raw: &[u8], merge_mode: &str, keys: &[String]) -> Option<String> {
    if merge_mode == "own" {
        return Some(hub_core::common::sha256(raw));
    }
    let text = String::from_utf8_lossy(raw).into_owned();
    if merge_mode == "block" {
        let region = merge::find_region(&text, "md")
            .ok()
            .flatten()
            .or_else(|| merge::find_region(&text, "hash").ok().flatten())?;
        let mut r = text[region.0..region.1].to_string();
        if !r.ends_with('\n') {
            r.push('\n');
        }
        return Some(hub_core::common::sha256(r.as_bytes()));
    }
    let doc = merge::load_doc(raw, if merge_mode == "json_keys" { "json" } else { "yaml" }).ok()?;
    let mut ks: Vec<&String> = keys.iter().collect();
    ks.sort();
    let found: Vec<(hub_core::PyValue, hub_core::PyValue)> = ks
        .into_iter()
        .filter_map(|k| {
            merge::get_dotted(&doc, k)
                .filter(|v| !v.is_none())
                .map(|v| (hub_core::PyValue::str(k.as_str()), v.clone()))
        })
        .collect();
    if found.is_empty() {
        None
    } else {
        merge::canon(&hub_core::PyValue::Dict(found)).ok()
    }
}

/// fakes.py `FakeAdapter`: every merge mode (own skills/agents, a CLAUDE.md block, .mcp.json and settings.json keys).
struct FakeAdapter;

fn dumps(v: &Value) -> Vec<u8> {
    hub_core::py::json::dumps(v, &hub_core::py::json::Opts::default()).into_bytes()
}

impl ClientAdapter for FakeAdapter {
    fn id(&self) -> &str {
        "fake"
    }
    fn display_name(&self) -> &str {
        "Fake client"
    }
    fn docs(&self) -> &str {
        ""
    }
    fn caps(&self) -> AdapterCaps {
        AdapterCaps {
            skills: true,
            agents: true,
            instructions: true,
            mcp: true,
            permissions: true,
            egress: true,
            tool_map: IndexMap::from([
                ("read".into(), Some("Read".into())),
                ("shell".into(), Some("Bash".into())),
                ("browser".into(), None),
            ]),
            notes: "fake".into(),
            ..AdapterCaps::default()
        }
    }
    fn default_config(&self) -> ClientConfig {
        ClientConfig::default()
    }
    fn render(&self, ctx: &RenderContext) -> AdapterResult<RenderResult> {
        let mut arts = Vec::new();
        let mut diags = Vec::new();
        for s in &ctx.skills {
            let mut files: Vec<(&String, &Vec<u8>)> = s.files.iter().collect();
            files.sort();
            for (rel, blob) in files {
                let mut a = Artifact::new("home", &format!("skills/{}/{rel}", s.name), blob.clone(), "skill");
                a.source_ids = vec![s.name.clone()];
                arts.push(a);
            }
        }
        for a in &ctx.agents {
            for c in &a.capabilities {
                if matches!(self.caps().tool_map.get(c), Some(None)) {
                    diags.push(Diag {
                        severity: "error".into(),
                        code: "unsupported_capability".into(),
                        message: format!("{c} unsupported"),
                        item: Some(a.name.clone()),
                    });
                }
            }
            let mut art = Artifact::new(
                "home",
                &format!("agents/{}.md", a.name),
                format!("# {}\n{}", a.name, a.body).into_bytes(),
                "agent",
            );
            art.source_ids = vec![a.name.clone()];
            arts.push(art);
        }
        if !ctx.instructions.is_empty() {
            let body: Vec<String> = ctx
                .instructions
                .iter()
                .map(|i| format!("## {}\n\n{}", i.title, i.body.trim()))
                .collect();
            let mut art = Artifact::new(
                "home",
                "CLAUDE.md",
                format!("{BEGIN}\n{}\n{END}\n", body.join("\n\n")).into_bytes(),
                "instruction",
            );
            art.merge = "block".into();
            art.source_ids = ctx.instructions.iter().map(|i| i.id.clone()).collect();
            arts.push(art);
        }
        let mut servers = serde_json::Map::new();
        for m in &ctx.mcp_servers {
            servers.insert(
                m.name.clone(),
                json!({"command": m.command, "args": m.args, "url": m.url}),
            );
        }
        let mut names: Vec<String> = servers.keys().cloned().collect();
        names.sort();
        let mut mcp = Artifact::new("home", ".mcp.json", dumps(&json!({"mcpServers": servers})), "mcp");
        mcp.merge = "json_keys".into();
        mcp.managed_keys = vec!["mcpServers".into()];
        mcp.source_ids = names;
        arts.push(mcp);
        let mut perms = serde_json::Map::from_iter([
            ("deny".to_string(), json!([])),
            ("ask".to_string(), json!([])),
            ("allow".to_string(), json!([])),
        ]);
        for r in &ctx.permissions.rules {
            if let Some(Value::Array(a)) = perms.get_mut(&r.decision) {
                a.push(json!(format!("{}:{}", r.kind, r.matches)));
            }
        }
        let mut st = Artifact::new("home", "settings.json", dumps(&json!({"permissions": perms})), "rule");
        st.merge = "json_keys".into();
        st.managed_keys = vec![
            "permissions.deny".into(),
            "permissions.ask".into(),
            "permissions.allow".into(),
        ];
        st.source_ids = vec!["floor".into()];
        arts.push(st);
        Ok(RenderResult {
            artifacts: arts,
            diagnostics: diags,
        })
    }
    fn discover(&self, _cfg: &ClientConfig, _fs: &dyn FileSystem) -> AdapterResult<DiscoveredContent> {
        Ok(DiscoveredContent::default())
    }
    fn verify(
        &self,
        cfg: &ClientConfig,
        expected: &[Expected],
        fs: &dyn FileSystem,
        run: &dyn CommandRunner,
    ) -> AdapterResult<Vec<Check>> {
        let mut out = Vec::new();
        for e in expected {
            let p = format!("{}/{}", cfg.roots.get(&e.root).map_or("", String::as_str), e.path);
            let ok = fs.exists(&p)
                && fs.read_bytes(&p).ok().is_some_and(|b| {
                    if e.merge == "own" {
                        hub_core::common::sha256(&b) == e.sha256
                    } else {
                        slice_digest(&b, &e.merge, &e.managed_keys).as_deref() == Some(e.sha256.as_str())
                    }
                });
            out.push(Check {
                name: format!("{}:{}", e.root, e.path),
                ok,
                detail: if ok {
                    String::new()
                } else {
                    "missing or hash differs".into()
                },
            });
        }
        let (rc, o, _) =
            run.run(&["fake-client".into(), "--list".into()], 5.0)
                .unwrap_or((127, String::new(), String::new()));
        out.push(Check {
            name: "fake-client --list".into(),
            ok: rc == 0,
            detail: o.trim().to_string(),
        });
        Ok(out)
    }
}

/// fakes.py `FakeRunner`.
#[derive(Default)]
struct FakeRunner {
    rc: Mutex<i32>,
    calls: Mutex<Vec<Vec<String>>>,
}

impl CommandRunner for FakeRunner {
    fn run(&self, argv: &[String], _timeout: f64) -> std::io::Result<(i32, String, String)> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(argv.to_vec());
        Ok((
            *self.rc.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
            "ok".into(),
            String::new(),
        ))
    }
}

/// The content the conftest `seed_content` enables for fake1, as the adapter receives it.
#[derive(Clone)]
struct Content {
    skill_md: String,
    notes: Option<String>,
    agent_body: Option<String>,
    style: Option<String>,
    mcp: Vec<McpItem>,
    rules: Vec<Rule>,
}

impl Default for Content {
    fn default() -> Self {
        Self {
            skill_md: "---\nname: alpha\ndescription: Alpha skill\n---\nDo alpha.\n".into(),
            notes: Some("notes".into()),
            agent_body: Some("Review carefully.\n".into()),
            style: Some("Be concise.\n".into()),
            mcp: Vec::new(),
            rules: vec![Rule {
                kind: "path_read".into(),
                matches: "~/.aws/**".into(),
                decision: "deny".into(),
                reason: None,
            }],
        }
    }
}

struct Rig {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    outside: PathBuf,
    planner: Arc<Planner>,
    locks: Arc<LockStore>,
    log: Arc<ApplyLog>,
    applier: Arc<Applier>,
    rollbacker: Rollbacker,
    runner: Arc<FakeRunner>,
    content: Content,
    manage: IndexMap<String, bool>,
}

impl Rig {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let home = tmp.path().join("client-home");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&home)?;
        std::fs::create_dir(&outside)?;
        let policy = load_floor(&crate::config::app_root().join("policy").join("floor.yaml")).map_err(|p| p.detail)?;
        let resolver = Arc::new(Resolver::new(
            Arc::new(Floor::new(policy)),
            AdapterRegistry::new(),
            None,
            None,
        ));
        let locks = Arc::new(LockStore::new(&tmp.path().join("state/locks")));
        let planner = Arc::new(Planner::new(
            resolver,
            AdapterRegistry::new(),
            locks.clone(),
            8 * 1024 * 1024,
        ));
        let log = Arc::new(ApplyLog::new(&tmp.path().join("state/applies")));
        let runner = Arc::new(FakeRunner::default());
        let applier = Arc::new(Applier::new(
            planner.clone(),
            locks.clone(),
            &tmp.path().join("state/backups"),
            runner.clone(),
            log.clone(),
        ));
        let rollbacker = Rollbacker::new(applier.clone(), log.clone(), locks.clone());
        let manage = IndexMap::from([
            ("skills".to_string(), true),
            ("agents".to_string(), true),
            ("instructions".to_string(), true),
            ("mcp".to_string(), false),
            ("permissions".to_string(), false),
            ("memory".to_string(), false),
        ]);
        Ok(Self {
            _tmp: tmp,
            home,
            outside,
            planner,
            locks,
            log,
            applier,
            rollbacker,
            runner,
            content: Content::default(),
            manage,
        })
    }

    /// `full` fixture: mcp and permissions managed too.
    fn full(mut self) -> Self {
        self.manage.insert("mcp".into(), true);
        self.manage.insert("permissions".into(), true);
        self
    }

    fn cfg(&self) -> ClientConfig {
        ClientConfig {
            id: "fake1".into(),
            adapter: "fake".into(),
            display_name: "Fake 1".into(),
            roots: IndexMap::from([("home".to_string(), self.home.to_string_lossy().to_string())]),
            manage: self.manage.clone(),
            strict: true,
            ..ClientConfig::default()
        }
    }

    fn resolved(&self) -> Resolved {
        let c = &self.content;
        let mut files = IndexMap::new();
        files.insert("SKILL.md".to_string(), c.skill_md.clone().into_bytes());
        if let Some(n) = &c.notes {
            files.insert("ref/notes.md".to_string(), n.clone().into_bytes());
        }
        let ctx = RenderContext {
            client: self.cfg(),
            skills: vec![SkillItem {
                name: "alpha".into(),
                description: "Alpha skill".into(),
                files,
                ..SkillItem::default()
            }],
            agents: c
                .agent_body
                .iter()
                .map(|b| AgentItem {
                    name: "reviewer".into(),
                    description: "Reviews code".into(),
                    capabilities: vec!["read".into()],
                    body: b.clone(),
                    ..AgentItem::default()
                })
                .collect(),
            instructions: c
                .style
                .iter()
                .map(|b| InstructionItem {
                    id: "style".into(),
                    title: "Style".into(),
                    order: 10,
                    body: b.clone(),
                })
                .collect(),
            mcp_servers: c.mcp.clone(),
            memory: Vec::new(),
            permissions: PermissionSet {
                rules: c.rules.clone(),
                ..PermissionSet::default()
            },
            endpoints: IndexMap::new(),
        };
        Resolved {
            client: ClientDoc {
                id: "fake1".into(),
                adapter: "fake".into(),
                strict: true,
                ..ClientDoc::default()
            },
            cfg: Some(self.cfg()),
            adapter: Some(Arc::new(FakeAdapter)),
            manage: self.manage.clone(),
            entries: IndexMap::new(),
            ctx: Some(ctx),
            findings: Vec::new(),
            diagnostics: Vec::new(),
            warnings: Vec::new(),
            kept_rules: Vec::new(),
            kept_sources: Vec::new(),
            allow_hosts: Vec::new(),
            permissions: PermissionSet::default(),
            mcp: Vec::new(),
        }
    }

    fn plan(&self, adopt: &[String]) -> HubResult<ClientPlanInternal> {
        self.planner.plan_resolved(self.resolved(), "fake1", adopt)
    }

    fn acts(&self) -> HubResult<IndexMap<String, String>> {
        Ok(self
            .plan(&[])?
            .plan
            .files
            .iter()
            .map(|f| (format!("{}:{}", f.root, f.path), f.action.clone()))
            .collect())
    }

    /// plan + apply with the digest check, as Hub.apply does for one client.
    fn apply(&self, adopt: &[String]) -> HubResult<ApplyResult> {
        let digest = self.plan(&[])?.plan.digest;
        let res = self.resolved();
        let plan = |a: &[String]| self.planner.plan_resolved(res.clone(), "fake1", a);
        self.applier
            .apply_with(&plan, "tree1", "fake1", Some(&digest), adopt, "plan_0123456789ab")
    }

    fn rollback(&self, apply_id: Option<&str>, dry_run: bool, force: &[&str]) -> HubResult<Value> {
        let force: BTreeSet<String> = force.iter().map(|s| (*s).to_string()).collect();
        self.rollbacker.run(&self.cfg(), "fake1", apply_id, dry_run, &force)
    }
}

fn snapshot(p: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut out = std::collections::BTreeMap::new();
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let path = e.path();
            let Ok(md) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if md.is_dir() {
                stack.push(path);
            } else if md.is_file() {
                let rel = path.strip_prefix(p).unwrap_or(&path).to_string_lossy().to_string();
                out.insert(rel, std::fs::read(&path).unwrap_or_default());
            }
        }
    }
    out
}

fn tmp_files(p: &Path) -> usize {
    let mut n = 0;
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().starts_with(".hub-tmp-") {
                n += 1;
            }
            if e.path().is_dir() && !e.path().is_symlink() {
                stack.push(e.path());
            }
        }
    }
    n
}

fn mode_of(p: &Path) -> u32 {
    std::fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0)
}

// ---- plan / apply ---------------------------------------------------------------------------------------------------

#[test]
fn plan_writes_nothing_and_apply_delivers_files_lock_and_verifies() -> R {
    let rig = Rig::new()?;
    let a = rig.acts()?;
    assert_eq!(a["home:skills/alpha/SKILL.md"], "add");
    assert_eq!(a["home:skills/alpha/ref/notes.md"], "add");
    assert_eq!(a["home:agents/reviewer.md"], "add");
    assert_eq!(a["home:CLAUDE.md"], "add");
    assert_eq!(a["home:.mcp.json"], "advisory");
    assert_eq!(a["home:settings.json"], "advisory");
    assert!(snapshot(&rig.home).is_empty());
    let r = rig.apply(&[])?;
    assert_eq!((r.status.as_str(), r.verify_ok), ("applied", Some(true)));
    assert!(!rig.runner.calls.lock().map_err(|_| "poison")?.is_empty());
    assert!(std::fs::read_to_string(rig.home.join("skills/alpha/SKILL.md"))?.starts_with("---\nname: alpha"));
    assert_eq!(
        std::fs::read_to_string(rig.home.join("agents/reviewer.md"))?,
        "# reviewer\nReview carefully.\n"
    );
    let claude = std::fs::read_to_string(rig.home.join("CLAUDE.md"))?;
    assert!(claude.starts_with(BEGIN) && claude.contains("Be concise.") && claude.trim_end().ends_with(END));
    assert!(!rig.home.join(".mcp.json").exists() && !rig.home.join("settings.json").exists());
    let lock_file = rig.locks.dir.join("fake1.json");
    assert_eq!(mode_of(&lock_file), 0o600);
    let lock: Value = serde_json::from_str(&std::fs::read_to_string(&lock_file)?)?;
    let ent = &lock["files"]["home:skills/alpha/SKILL.md"];
    assert_eq!(
        ent["sha256"],
        json!(hub_core::common::sha256(&std::fs::read(
            rig.home.join("skills/alpha/SKILL.md")
        )?))
    );
    assert_eq!(ent["created"], json!(true));
    assert_eq!(lock["content_hash"], json!("tree1"));
    assert_eq!(tmp_files(&rig.home), 0);
    assert_eq!(mode_of(&rig.home.join("skills")), 0o755 & !current_umask());
    assert_eq!(mode_of(&rig.home.join("skills/alpha/SKILL.md")), 0o644);
    let id = r.apply_id.ok_or("no apply id")?;
    assert!(id.ends_with("-plan_0123456789ab") && id.len() == "20261027T000000Z-plan_0123456789ab".len());
    Ok(())
}

fn current_umask() -> u32 {
    // the mode a fresh mkdir gets tells the umask (mkdir 0777 & ~umask)
    let d = tempfile::tempdir().ok();
    d.as_ref()
        .map(|d| {
            let p = d.path().join("probe");
            let _ = std::fs::create_dir(&p);
            0o777 & !mode_of(&p)
        })
        .unwrap_or(0o022)
}

#[test]
fn apply_is_idempotent() -> R {
    let rig = Rig::new()?;
    rig.apply(&[])?;
    let before = snapshot(&rig.home);
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let m1 = mtime(&rig.home.join("skills/alpha/SKILL.md"));
    let acts = rig.acts()?;
    assert!(acts.values().all(|a| a == "unchanged" || a == "advisory"), "{acts:?}");
    let r = rig.apply(&[])?;
    assert_eq!((r.status.as_str(), r.apply_id.as_deref()), ("nothing", None));
    assert_eq!(snapshot(&rig.home), before);
    assert_eq!(mtime(&rig.home.join("skills/alpha/SKILL.md")), m1);
    Ok(())
}

#[test]
fn edit_shows_change_backs_up_previous_and_removes_dropped_files() -> R {
    let mut rig = Rig::new()?;
    rig.apply(&[])?;
    let old = std::fs::read(rig.home.join("skills/alpha/SKILL.md"))?;
    rig.content.skill_md = "---\nname: alpha\ndescription: Alpha v2\n---\nDo alpha v2.\n".into();
    rig.content.notes = None;
    let ip = rig.plan(&[])?;
    let f = ip
        .plan
        .files
        .iter()
        .find(|f| f.path == "skills/alpha/SKILL.md")
        .ok_or("no file")?;
    assert_eq!(f.action, "change");
    assert!(f.diff.contains("-description: Alpha skill") && f.diff.contains("+description: Alpha v2"));
    let r = rig.apply(&[])?;
    assert!(std::fs::read_to_string(rig.home.join("skills/alpha/SKILL.md"))?.contains("Alpha v2"));
    let bdir = PathBuf::from(r.backup_dir.ok_or("no backup dir")?);
    assert_eq!(std::fs::read(bdir.join("home/skills/alpha/SKILL.md"))?, old);
    assert_eq!(mode_of(&bdir.join("home/skills/alpha/SKILL.md")), 0o600);
    assert!(!rig.home.join("skills/alpha/ref/notes.md").exists() && !rig.home.join("skills/alpha/ref").exists());
    assert!(r.removed.iter().any(|w| w["path"] == "skills/alpha/ref/notes.md"));
    Ok(())
}

#[test]
fn plan_diffs_are_redacted() -> R {
    let mut rig = Rig::new()?;
    rig.content.style = Some("api_key = sk-abcdefghijklmnopqrstuv\n".into());
    let text: String = rig.plan(&[])?.plan.files.iter().map(|f| f.diff.clone()).collect();
    assert!(!text.contains("sk-abcdefghijklmnopqrstuv") && text.contains("[redacted]"));
    Ok(())
}

#[test]
fn plan_goes_stale_when_a_live_file_changes() -> R {
    let mut rig = Rig::new()?;
    rig.apply(&[])?;
    rig.content.style = Some("v2\n".into());
    let digest = rig.plan(&[])?.plan.digest;
    std::fs::write(rig.home.join("agents/reviewer.md"), "edited by a human\n")?;
    let res = rig.resolved();
    let plan = |a: &[String]| rig.planner.plan_resolved(res.clone(), "fake1", a);
    let e = rig
        .applier
        .apply_with(&plan, "t", "fake1", Some(&digest), &[], "p")
        .err()
        .ok_or("applied a stale plan")?;
    assert_eq!((e.status, e.code.as_str()), (409, "plan_stale"));
    Ok(())
}

// ---- all-or-nothing -------------------------------------------------------------------------------------------------

#[test]
fn failure_while_committing_rolls_everything_back() -> R {
    let mut rig = Rig::new()?;
    rig.apply(&[])?;
    rig.content.skill_md = "v2\n".into();
    rig.content.agent_body = Some("v2\n".into());
    rig.content.style = Some("v2\n".into());
    let before = snapshot(&rig.home);
    let lock_before = std::fs::read(rig.locks.dir.join("fake1.json"))?;
    let n2 = Arc::new(Mutex::new(0));
    rig.applier.set_test_hooks(Some(ApplyHooks {
        before_commit: Some(Box::new(move |_| {
            let mut c = n2.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            *c += 1;
            if *c == 3 {
                return Err(OpError::new("OSError", "disk full"));
            }
            Ok(())
        })),
        ..ApplyHooks::default()
    }));
    let r = rig.apply(&[])?;
    rig.applier.set_test_hooks(None);
    assert_eq!(r.status, "failed");
    assert_eq!(r.error, "OSError: disk full; nothing was changed");
    assert_eq!(snapshot(&rig.home), before);
    assert_eq!(tmp_files(&rig.home), 0);
    assert_eq!(std::fs::read(rig.locks.dir.join("fake1.json"))?, lock_before);
    assert!(!rig.acts()?.values().any(|a| a == "conflict"));
    Ok(())
}

#[test]
fn failure_while_staging_touches_nothing_and_removes_new_dirs() -> R {
    let rig = Rig::new()?;
    let n2 = Arc::new(Mutex::new(0));
    rig.applier.set_test_hooks(Some(ApplyHooks {
        before_stage: Some(Box::new(move |_| {
            let mut c = n2.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            *c += 1;
            if *c == 4 {
                return Err(OpError::new("OSError", "boom"));
            }
            Ok(())
        })),
        ..ApplyHooks::default()
    }));
    let r = rig.apply(&[])?;
    assert_eq!(r.status, "failed");
    assert_eq!(std::fs::read_dir(&rig.home)?.count(), 0, "not even empty directories");
    assert!(rig.locks.load("fake1").is_none());
    Ok(())
}

// ---- TOCTOU (test_toctou.py on the engine) -----------------------------------------------------------------------

#[test]
fn symlink_swap_after_staging_cannot_redirect_the_write() -> R {
    let rig = Rig::new()?;
    let (home, outside) = (rig.home.clone(), rig.outside.clone());
    let swapped = Arc::new(Mutex::new(false));
    rig.applier.set_test_hooks(Some(ApplyHooks {
        before_commit: Some(Box::new(move |_| {
            let mut s = swapped.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if !*s {
                *s = true;
                let d = home.join("agents");
                if d.exists() {
                    let _ = std::fs::rename(&d, home.join("agents.moved"));
                }
                let _ = std::os::unix::fs::symlink(&outside, &d);
            }
            Ok(())
        })),
        ..ApplyHooks::default()
    }));
    rig.apply(&[])?;
    assert_eq!(
        std::fs::read_dir(&rig.outside)?.count(),
        0,
        "the held dirfd points at the ORIGINAL directory"
    );
    assert!(rig.home.join("agents.moved/reviewer.md").exists());
    Ok(())
}

#[test]
fn live_file_edited_between_plan_and_commit_aborts_the_whole_client() -> R {
    let mut rig = Rig::new()?;
    rig.apply(&[])?;
    let before = snapshot(&rig.home);
    rig.content.agent_body = Some("v2\n".into());
    rig.content.style = Some("v2\n".into());
    let home = rig.home.clone();
    let calls = Arc::new(Mutex::new(0));
    rig.applier.set_test_hooks(Some(ApplyHooks {
        before_commit: Some(Box::new(move |_| {
            let mut c = calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            *c += 1;
            if *c == 2 {
                let _ = std::fs::write(
                    home.join("agents/reviewer.md"),
                    "edited by a human at the worst moment\n",
                );
            }
            Ok(())
        })),
        ..ApplyHooks::default()
    }));
    let r = rig.apply(&[])?;
    assert_eq!(r.status, "failed");
    assert!(
        r.error.starts_with("conflict") && r.error.contains("nothing was changed"),
        "{}",
        r.error
    );
    let mut after = snapshot(&rig.home);
    assert_eq!(
        after.remove("agents/reviewer.md"),
        Some(b"edited by a human at the worst moment\n".to_vec())
    );
    let mut before = before;
    before.remove("agents/reviewer.md");
    assert_eq!(after, before);
    assert_eq!(tmp_files(&rig.home), 0);
    Ok(())
}

#[test]
fn removal_is_also_compare_and_swap() -> R {
    let mut rig = Rig::new()?;
    rig.apply(&[])?;
    rig.content.agent_body = None;
    let home = rig.home.clone();
    rig.applier.set_test_hooks(Some(ApplyHooks {
        before_commit: Some(Box::new(move |op| {
            if op.name == "reviewer.md" {
                let _ = std::fs::write(home.join("agents/reviewer.md"), "late edit");
            }
            Ok(())
        })),
        ..ApplyHooks::default()
    }));
    assert_eq!(rig.apply(&[])?.status, "failed");
    assert_eq!(
        std::fs::read_to_string(rig.home.join("agents/reviewer.md"))?,
        "late edit"
    );
    Ok(())
}

#[test]
fn stage_time_race_and_dir_swap_detection() -> R {
    let rig = Rig::new()?;
    let (home, outside) = (rig.home.clone(), rig.outside.clone());
    let n = Arc::new(Mutex::new(0));
    rig.applier.set_test_hooks(Some(ApplyHooks {
        before_stage: Some(Box::new(move |_| {
            let mut c = n.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            *c += 1;
            if *c == 1 {
                let _ = std::fs::create_dir_all(home.join("skills"));
                let _ = std::os::unix::fs::symlink(&outside, home.join("skills/alpha"));
            }
            Ok(())
        })),
        ..ApplyHooks::default()
    }));
    let r = rig.apply(&[])?;
    assert_eq!(r.status, "failed");
    assert!(
        r.error
            .starts_with("conflict: path component 'alpha' is not a plain directory"),
        "{}",
        r.error
    );
    assert_eq!(std::fs::read_dir(&rig.outside)?.count(), 0);
    Ok(())
}

#[test]
fn symlinked_dir_and_leaf_are_refused_at_plan_and_never_written_through() -> R {
    let rig = Rig::new()?;
    std::os::unix::fs::symlink(&rig.outside, rig.home.join("skills"))?;
    let victim = rig.outside.join("victim.txt");
    std::fs::write(&victim, "victim")?;
    std::fs::create_dir(rig.home.join("agents"))?;
    std::os::unix::fs::symlink(&victim, rig.home.join("agents/reviewer.md"))?;
    let ip = rig.plan(&[])?;
    assert!(ip.plan.blocked);
    let unsafe_paths: Vec<&Value> = ip
        .plan
        .diagnostics
        .iter()
        .filter(|d| d["code"] == "unsafe_path")
        .collect();
    assert_eq!(unsafe_paths.len(), 3, "{unsafe_paths:?}");
    assert_eq!(rig.apply(&[])?.status, "blocked");
    assert_eq!(std::fs::read_to_string(&victim)?, "victim");
    assert_eq!(std::fs::read_dir(&rig.outside)?.count(), 1);
    Ok(())
}

// ---- conflicts, adoption, merges -----------------------------------------------------------------------------------

#[test]
fn edited_live_file_is_a_conflict_until_adopted_and_adopt_can_be_scoped() -> R {
    let mut rig = Rig::new()?;
    rig.apply(&[])?;
    std::fs::write(rig.home.join("agents/reviewer.md"), "hand edited\n")?;
    rig.content.agent_body = Some("Review v2.\n".into());
    assert_eq!(rig.acts()?["home:agents/reviewer.md"], "conflict");
    let r = rig.apply(&[])?;
    assert_eq!(
        r.skipped_conflicts,
        vec![json!({"root": "home", "path": "agents/reviewer.md",
        "reason": "live content differs from what the hub last applied"})]
    );
    assert_eq!(
        std::fs::read_to_string(rig.home.join("agents/reviewer.md"))?,
        "hand edited\n"
    );
    let r = rig.apply(&["someone-else:home:agents/reviewer.md".into()])?;
    assert!(!r.skipped_conflicts.is_empty());
    let r = rig.apply(&["fake1:home:agents/reviewer.md".into()])?;
    assert!(r.skipped_conflicts.is_empty());
    assert_eq!(
        std::fs::read_to_string(rig.home.join("agents/reviewer.md"))?,
        "# reviewer\nReview v2.\n"
    );
    let bdir = PathBuf::from(r.backup_dir.ok_or("no backup")?);
    assert_eq!(
        std::fs::read_to_string(bdir.join("home/agents/reviewer.md"))?,
        "hand edited\n"
    );
    assert!(rig.acts()?.values().all(|a| a == "unchanged" || a == "advisory"));
    Ok(())
}

#[test]
fn preexisting_identical_file_is_adopted_silently_foreign_is_a_conflict() -> R {
    let rig = Rig::new()?;
    std::fs::create_dir_all(rig.home.join("agents"))?;
    std::fs::write(rig.home.join("agents/reviewer.md"), "mine\n")?;
    std::fs::create_dir_all(rig.home.join("skills/alpha"))?;
    std::fs::write(rig.home.join("skills/alpha/SKILL.md"), &rig.content.skill_md)?;
    let a = rig.acts()?;
    assert_eq!(
        (
            a["home:agents/reviewer.md"].as_str(),
            a["home:skills/alpha/SKILL.md"].as_str()
        ),
        ("conflict", "unchanged")
    );
    let r = rig.apply(&[])?;
    assert_eq!(r.status, "applied");
    assert_eq!(std::fs::read_to_string(rig.home.join("agents/reviewer.md"))?, "mine\n");
    let lock = rig.locks.load("fake1").ok_or("no lock")?;
    assert!(!lock.files.contains_key("home:agents/reviewer.md"));
    assert_eq!(
        lock.files.get("home:skills/alpha/SKILL.md").map(|e| e.created),
        Some(false)
    );
    Ok(())
}

#[test]
fn unreadable_live_target_cannot_be_adopted() -> R {
    let rig = Rig::new()?;
    std::fs::create_dir_all(rig.home.join("agents/reviewer.md"))?;
    let ip = rig.plan(&[])?;
    let f = ip
        .plan
        .files
        .iter()
        .find(|f| f.path == "agents/reviewer.md")
        .ok_or("no file")?;
    assert_eq!(
        (f.action.as_str(), f.reason.as_str()),
        ("conflict", "live file unreadable: not a regular file")
    );
    let r = rig.apply(&["home:agents/reviewer.md".into()])?;
    assert!(rig.home.join("agents/reviewer.md").is_dir() && !r.skipped_conflicts.is_empty());
    Ok(())
}

#[test]
fn json_and_block_merges_preserve_foreign_content_and_modes() -> R {
    let rig = Rig::new()?.full();
    std::fs::write(rig.home.join("CLAUDE.md"), "# My own notes\n\nkeep me\n")?;
    std::fs::write(
        rig.home.join("settings.json"),
        r#"{"theme": "dark", "permissions": {"defaultMode": "plan"}}"#,
    )?;
    std::fs::set_permissions(rig.home.join("settings.json"), std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(rig.home.join(".mcp.json"), r#"{"other": {"x": 1}, "mcpServers": {}}"#)?;
    let r = rig.apply(&[])?;
    assert_eq!(r.status, "applied", "{r:?}");
    let claude = std::fs::read_to_string(rig.home.join("CLAUDE.md"))?;
    assert!(claude.starts_with(&format!("# My own notes\n\nkeep me\n\n{BEGIN}")) && claude.contains("Be concise."));
    let st: Value = serde_json::from_str(&std::fs::read_to_string(rig.home.join("settings.json"))?)?;
    assert_eq!(
        (st["theme"].clone(), st["permissions"]["defaultMode"].clone()),
        (json!("dark"), json!("plan"))
    );
    assert_eq!(st["permissions"]["deny"], json!(["path_read:~/.aws/**"]));
    assert_eq!(mode_of(&rig.home.join("settings.json")), 0o600);
    let mcp: Value = serde_json::from_str(&std::fs::read_to_string(rig.home.join(".mcp.json"))?)?;
    assert_eq!(mcp["other"], json!({"x": 1}));
    assert_eq!(r.verify_ok, Some(true), "{:?}", r.checks);
    Ok(())
}

#[test]
fn editing_inside_the_block_is_a_conflict_outside_is_not() -> R {
    let mut rig = Rig::new()?;
    std::fs::write(rig.home.join("CLAUDE.md"), "top\n")?;
    rig.apply(&[])?;
    let text = std::fs::read_to_string(rig.home.join("CLAUDE.md"))?;
    std::fs::write(
        rig.home.join("CLAUDE.md"),
        format!("{text}\nappended by the user after the block\n"),
    )?;
    rig.content.style = Some("Be VERY concise.\n".into());
    rig.apply(&[])?;
    let out = std::fs::read_to_string(rig.home.join("CLAUDE.md"))?;
    assert!(
        out.starts_with("top\n") && out.ends_with("appended by the user after the block\n") && out.contains("VERY")
    );
    assert_eq!(out.matches(BEGIN).count(), 1);
    std::fs::write(
        rig.home.join("CLAUDE.md"),
        out.replace("Be VERY concise.", "Be chatty."),
    )?;
    rig.content.style = Some("v3\n".into());
    assert_eq!(rig.acts()?["home:CLAUDE.md"], "conflict");
    Ok(())
}

#[test]
fn unparseable_json_is_a_conflict_and_adopting_replaces_it() -> R {
    let rig = Rig::new()?.full();
    std::fs::write(rig.home.join(".mcp.json"), "{ // jsonc comment\n}")?;
    let ip = rig.plan(&[])?;
    let f = ip.plan.files.iter().find(|f| f.path == ".mcp.json").ok_or("no file")?;
    assert_eq!(
        (f.action.as_str(), f.reason.as_str()),
        (
            "conflict",
            "live file is not a mergeable json document (JSONDecodeError)"
        )
    );
    rig.apply(&["home:.mcp.json".into()])?;
    assert_eq!(
        std::fs::read_to_string(rig.home.join(".mcp.json"))?,
        "{\n  \"mcpServers\": {}\n}\n"
    );
    Ok(())
}

#[test]
fn disable_removes_only_hub_created_files_and_prunes_dirs() -> R {
    let mut rig = Rig::new()?;
    std::fs::create_dir(rig.home.join("skills"))?;
    std::fs::write(rig.home.join("skills/foreign.txt"), "not mine")?;
    std::fs::create_dir(rig.home.join("agents"))?;
    std::fs::write(rig.home.join("agents/mine.md"), "hand written agent")?;
    rig.apply(&[])?;
    rig.content.agent_body = None;
    // the skill goes too: a resolved context without it
    let mut res = rig.resolved();
    if let Some(ctx) = res.ctx.as_mut() {
        ctx.skills.clear();
    }
    let ip = rig.planner.plan_resolved(res.clone(), "fake1", &[])?;
    let acts: IndexMap<String, String> = ip
        .plan
        .files
        .iter()
        .map(|f| (f.path.clone(), f.action.clone()))
        .collect();
    assert_eq!(acts["skills/alpha/SKILL.md"], "remove");
    let plan = |a: &[String]| rig.planner.plan_resolved(res.clone(), "fake1", a);
    let r = rig
        .applier
        .apply_with(&plan, "t", "fake1", Some(&ip.plan.digest), &[], "p")?;
    assert!(!rig.home.join("skills/alpha").exists() && !rig.home.join("agents/reviewer.md").exists());
    assert_eq!(
        std::fs::read_to_string(rig.home.join("skills/foreign.txt"))?,
        "not mine"
    );
    assert!(rig.home.join("agents/mine.md").exists() && rig.home.join("skills").is_dir());
    let lock = rig.locks.load("fake1").ok_or("no lock")?;
    assert!(!lock.files.contains_key("home:skills/alpha/SKILL.md"));
    assert!(
        PathBuf::from(r.backup_dir.ok_or("no backup")?)
            .join("home/agents/reviewer.md")
            .exists()
    );
    Ok(())
}

#[test]
fn removal_of_an_edited_hub_file_is_a_conflict_until_adopted() -> R {
    let mut rig = Rig::new()?;
    rig.apply(&[])?;
    std::fs::write(rig.home.join("agents/reviewer.md"), "edited")?;
    rig.content.agent_body = None;
    assert_eq!(rig.acts()?["home:agents/reviewer.md"], "conflict");
    rig.apply(&[])?;
    assert!(rig.home.join("agents/reviewer.md").exists());
    rig.apply(&["home:agents/reviewer.md".into()])?;
    assert!(!rig.home.join("agents/reviewer.md").exists());
    Ok(())
}

#[test]
fn adopted_preexisting_file_is_released_not_deleted() -> R {
    let mut rig = Rig::new()?;
    std::fs::create_dir(rig.home.join("agents"))?;
    std::fs::write(rig.home.join("agents/reviewer.md"), "# reviewer\nReview carefully.\n")?;
    rig.apply(&[])?;
    rig.content.agent_body = None;
    assert_eq!(rig.acts()?["home:agents/reviewer.md"], "unchanged");
    rig.apply(&[])?;
    assert!(rig.home.join("agents/reviewer.md").exists());
    assert!(
        !rig.locks
            .load("fake1")
            .ok_or("no lock")?
            .files
            .contains_key("home:agents/reviewer.md")
    );
    Ok(())
}

#[test]
fn managed_slice_removal_preserves_the_rest_and_hub_created_merge_file_is_deleted_when_empty() -> R {
    let mut rig = Rig::new()?.full();
    std::fs::write(rig.home.join("CLAUDE.md"), "mine\n")?;
    std::fs::write(rig.home.join("settings.json"), r#"{"theme": "dark"}"#)?;
    rig.apply(&[])?;
    rig.content.style = None;
    rig.content.rules.clear();
    rig.apply(&[])?;
    assert_eq!(std::fs::read_to_string(rig.home.join("CLAUDE.md"))?, "mine\n");
    let st: Value = serde_json::from_str(&std::fs::read_to_string(rig.home.join("settings.json"))?)?;
    assert_eq!(st["theme"], json!("dark"));

    let mut rig = Rig::new()?.full();
    rig.apply(&[])?;
    assert!(rig.home.join("CLAUDE.md").exists());
    rig.content.style = None;
    rig.apply(&[])?;
    assert!(!rig.home.join("CLAUDE.md").exists());
    Ok(())
}

#[test]
fn going_advisory_never_deletes_and_releases_the_lock_entry() -> R {
    for hub_created in [true, false] {
        let mut rig = Rig::new()?.full();
        if !hub_created {
            std::fs::write(rig.home.join("CLAUDE.md"), "mine\n")?;
        }
        rig.apply(&[])?;
        assert!(
            rig.locks
                .load("fake1")
                .ok_or("no lock")?
                .files
                .contains_key("home:CLAUDE.md")
        );
        rig.manage.insert("instructions".into(), false);
        std::fs::write(
            rig.home.join("CLAUDE.md"),
            "edited by hand after the concern went advisory\n",
        )?;
        let ip = rig.plan(&[])?;
        let f = ip.plan.files.iter().find(|f| f.path == "CLAUDE.md").ok_or("no file")?;
        assert_eq!(f.action, "advisory");
        assert!(f.reason.contains("released from the lock"), "{}", f.reason);
        rig.apply(&[])?;
        assert!(
            !rig.locks
                .load("fake1")
                .ok_or("no lock")?
                .files
                .contains_key("home:CLAUDE.md")
        );
        assert_eq!(
            std::fs::read_to_string(rig.home.join("CLAUDE.md"))?,
            "edited by hand after the concern went advisory\n"
        );
    }
    Ok(())
}

#[test]
fn missing_root_blocks_the_plan_once_per_root() -> R {
    let mut rig = Rig::new()?;
    let gone = rig.home.join("not-created");
    rig.home = gone.clone();
    let ip = rig.plan(&[])?;
    assert!(ip.plan.blocked);
    assert_eq!(
        ip.plan
            .diagnostics
            .iter()
            .filter(|d| d["code"] == "root_missing")
            .count(),
        1
    );
    assert!(
        ip.plan
            .blocked_reasons
            .iter()
            .any(|r| r.contains("does not exist") && r.contains("never creates a client root"))
    );
    assert_eq!(rig.apply(&[])?.status, "blocked");
    assert!(!gone.exists());
    Ok(())
}

#[test]
fn drift_states() -> R {
    let mut rig = Rig::new()?;
    let d = rig.planner.drift(&rig.plan(&[])?)?;
    assert_eq!(d["status"], "never_applied");
    rig.apply(&[])?;
    let d = rig.planner.drift(&rig.plan(&[])?)?;
    assert_eq!(
        (d["status"].clone(), d["drift"].clone()),
        (json!("in_sync"), json!({"edited_outside": [], "pending_changes": 0}))
    );
    rig.content.style = Some("v2\n".into());
    let d = rig.planner.drift(&rig.plan(&[])?)?;
    assert_eq!(
        (d["status"].clone(), d["drift"]["pending_changes"].clone()),
        (json!("pending_changes"), json!(1))
    );
    rig.apply(&[])?;
    std::fs::write(rig.home.join("agents/reviewer.md"), "tampered")?;
    let d = rig.planner.drift(&rig.plan(&[])?)?;
    assert_eq!(d["status"], "edited_outside");
    assert_eq!(
        d["drift"]["edited_outside"],
        json!([{"root": "home", "path": "agents/reviewer.md", "reason": "changed"}])
    );
    let row = d["files"]
        .as_array()
        .ok_or("files")?
        .iter()
        .find(|f| f["path"] == "agents/reviewer.md")
        .ok_or("row")?;
    assert_eq!(row["expected"].as_str().map(str::len), Some(12));
    assert_ne!(row["expected"], row["actual"]);
    assert!(!d.to_string().contains("tampered"));
    std::fs::remove_file(rig.home.join("agents/reviewer.md"))?;
    let d = rig.planner.drift(&rig.plan(&[])?)?;
    assert_eq!(d["drift"]["edited_outside"][0]["reason"], "missing");
    Ok(())
}

// ---- rollback (test_rollback.py on the engine) ---------------------------------------------------------------------

#[test]
fn rollback_restores_bytes_removes_added_files_and_prunes() -> R {
    let rig = Rig::new()?;
    std::fs::create_dir(rig.home.join("agents"))?;
    std::fs::write(rig.home.join("agents/reviewer.md"), "original hand-written agent\n")?;
    std::fs::write(rig.home.join("CLAUDE.md"), "mine\n")?;
    let before = snapshot(&rig.home);
    let first = rig.apply(&["home:agents/reviewer.md".into()])?;
    assert_ne!(snapshot(&rig.home), before);
    let out = rig.rollback(None, false, &[])?;
    assert_eq!(out["status"], "rolled_back");
    assert_eq!(out["apply_id"], json!(first.apply_id));
    assert_eq!(out["conflicts"], json!([]));
    assert_eq!(snapshot(&rig.home), before);
    assert!(!rig.home.join("skills").exists());
    assert!(rig.log.list("fake1")?[0]["rolled_back_at"].is_f64());
    assert!(rig.locks.load("fake1").ok_or("no lock")?.files.is_empty());
    let again = rig.rollback(None, false, &[])?;
    assert_eq!(again["status"], "already_rolled_back");
    assert!(again["message"].as_str().is_some_and(|m| m.contains("nothing to do")));
    Ok(())
}

#[test]
fn later_hand_edit_is_protected_and_force_overrides() -> R {
    let rig = Rig::new()?;
    rig.apply(&[])?;
    std::fs::write(rig.home.join("agents/reviewer.md"), "edited after the apply\n")?;
    let dry = rig.rollback(None, true, &[])?;
    assert_eq!(dry["status"], "dry_run");
    assert_eq!(dry["conflicts"][0]["path"], "agents/reviewer.md");
    assert!(
        dry["changed"]
            .as_array()
            .ok_or("changed")?
            .iter()
            .all(|c| c.get("diff").is_some())
    );
    assert!(rig.home.join("skills/alpha/SKILL.md").exists());
    let out = rig.rollback(None, false, &[])?;
    assert_eq!(out["status"], "partially_rolled_back");
    assert_eq!(
        std::fs::read_to_string(rig.home.join("agents/reviewer.md"))?,
        "edited after the apply\n"
    );
    assert!(!rig.home.join("skills").exists());
    let again = rig.rollback(None, false, &[])?;
    assert_eq!(
        (again["status"].clone(), again["changed"].clone()),
        (json!("conflicts_only"), json!([]))
    );
    let fin = rig.rollback(None, false, &["home:agents/reviewer.md"])?;
    assert_eq!(fin["status"], "rolled_back");
    assert!(!rig.home.join("agents").exists());
    Ok(())
}

#[test]
fn older_apply_can_only_be_rolled_back_if_every_file_still_matches() -> R {
    let mut rig = Rig::new()?;
    let first = rig.apply(&[])?.apply_id.ok_or("id")?;
    std::thread::sleep(std::time::Duration::from_millis(1100)); // apply ids carry a second-resolution stamp
    rig.content.agent_body = Some("v2\n".into());
    let second = rig.apply(&[])?.apply_id.ok_or("id")?;
    assert_ne!(first, second);
    let e = rig.rollback(Some(&first), false, &[]).err().ok_or("rolled back")?;
    assert_eq!(e.code, "rollback_not_latest");
    assert!(e.extra.contains_key("conflicts"));
    rig.rollback(Some(&second), false, &[])?;
    assert!(std::fs::read_to_string(rig.home.join("agents/reviewer.md"))?.contains("Review carefully"));
    rig.rollback(Some(&first), false, &[])?;
    assert!(!rig.home.join("agents").exists() && !rig.home.join("skills").exists());
    assert_eq!(
        rig.rollback(Some("../../etc/passwd"), false, &[])
            .err()
            .map(|e| e.status),
        Some(400)
    );
    Ok(())
}

#[test]
fn rollback_restores_removed_files() -> R {
    let mut rig = Rig::new()?;
    rig.apply(&[])?;
    rig.content.agent_body = None;
    let r = rig.apply(&[])?;
    assert!(!rig.home.join("agents/reviewer.md").exists());
    let out = rig.rollback(r.apply_id.as_deref(), false, &[])?;
    assert_eq!(out["status"], "rolled_back");
    assert_eq!(
        std::fs::read_to_string(rig.home.join("agents/reviewer.md"))?,
        "# reviewer\nReview carefully.\n"
    );
    assert!(
        rig.locks
            .load("fake1")
            .ok_or("no lock")?
            .files
            .contains_key("home:agents/reviewer.md")
    );
    Ok(())
}

#[test]
fn rollback_aborts_atomically_when_a_file_changes_mid_way() -> R {
    let rig = Rig::new()?;
    rig.apply(&[])?;
    let after = snapshot(&rig.home);
    let home = rig.home.clone();
    let n = Arc::new(Mutex::new(0));
    rig.applier.set_test_hooks(Some(ApplyHooks {
        before_commit: Some(Box::new(move |_| {
            let mut c = n.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            *c += 1;
            if *c == 2 {
                let _ = std::fs::write(home.join("agents/reviewer.md"), "late edit\n");
            }
            Ok(())
        })),
        ..ApplyHooks::default()
    }));
    let e = rig.rollback(None, false, &[]).err().ok_or("rolled back")?;
    assert_eq!(e.code, "rollback_failed");
    let mut got = snapshot(&rig.home);
    assert_eq!(got.remove("agents/reviewer.md"), Some(b"late edit\n".to_vec()));
    let mut after = after;
    after.remove("agents/reviewer.md");
    assert_eq!(got, after);
    assert!(rig.log.list("fake1")?[0]["rolled_back_at"].is_null());
    Ok(())
}

#[test]
fn rollback_requires_a_recorded_apply() -> R {
    let rig = Rig::new()?;
    let e = rig.rollback(None, false, &[]).err().ok_or("rolled back")?;
    assert_eq!((e.status, e.code.as_str()), (404, "no_applies"));
    Ok(())
}

// ---- R-CONCURRENCY ---------------------------------------------------------------------------------------------------

/// Concurrent applies serialised by the hub's reentrant lock (Hub.apply holds `self.lock` for the whole apply): every
/// apply sees a consistent tree, nothing is torn, no temp file survives, and the lock is re-entered on the same thread.
#[test]
fn concurrent_applies_under_the_hub_lock_are_serialised() -> R {
    let rig = Arc::new(Rig::new()?);
    let hub_lock = Arc::new(crate::services::hub::HubLock::default());
    let inside = Arc::new(Mutex::new(0usize));
    let max_inside = Arc::new(Mutex::new(0usize));
    let mut threads = Vec::new();
    for _ in 0..8 {
        let (rig, hub_lock, inside, max_inside) = (rig.clone(), hub_lock.clone(), inside.clone(), max_inside.clone());
        threads.push(std::thread::spawn(move || -> Result<String, String> {
            let _g = hub_lock.lock();
            let _again = hub_lock.lock(); // RLock: the same thread may re-enter (rollback -> verify)
            {
                let mut n = inside.lock().map_err(|_| "poison")?;
                *n += 1;
                let mut m = max_inside.lock().map_err(|_| "poison")?;
                *m = (*m).max(*n);
            }
            let r = rig.apply(&[]).map_err(|p| p.detail)?;
            *inside.lock().map_err(|_| "poison")? -= 1;
            Ok(r.status)
        }));
    }
    let mut statuses = Vec::new();
    for t in threads {
        statuses.push(t.join().map_err(|_| "thread panicked")??);
    }
    statuses.sort();
    assert_eq!(statuses.iter().filter(|s| *s == "applied").count(), 1, "{statuses:?}");
    assert_eq!(statuses.iter().filter(|s| *s == "nothing").count(), 7, "{statuses:?}");
    assert_eq!(*max_inside.lock().map_err(|_| "poison")?, 1);
    assert_eq!(tmp_files(&rig.home), 0);
    assert_eq!(rig.log.list("fake1")?.len(), 1);
    Ok(())
}

/// Without the hub lock, two racing applies can never tear a file or leave a temp file: each op is compare-and-swap,
/// so the loser fails cleanly ("conflict: ... changed since it was planned") or finds nothing to do.
#[test]
fn racing_applies_without_the_lock_fail_cleanly() -> R {
    let rig = Arc::new(Rig::new()?);
    let barrier = Arc::new(std::sync::Barrier::new(4));
    let mut threads = Vec::new();
    for _ in 0..4 {
        let (rig, barrier) = (rig.clone(), barrier.clone());
        threads.push(std::thread::spawn(move || -> Result<ApplyResult, String> {
            barrier.wait();
            rig.apply(&[]).map_err(|p| format!("{} {}", p.code, p.detail))
        }));
    }
    for t in threads {
        match t.join().map_err(|_| "thread panicked")? {
            Ok(r) => assert!(
                matches!(r.status.as_str(), "applied" | "nothing")
                    || (r.status == "failed" && r.error.contains("nothing was changed")),
                "{r:?}"
            ),
            Err(e) => assert!(e.starts_with("plan_stale"), "{e}"),
        }
    }
    assert_eq!(tmp_files(&rig.home), 0);
    // nothing is torn: every file present has exactly the rendered bytes
    if let Ok(t) = std::fs::read_to_string(rig.home.join("agents/reviewer.md")) {
        assert_eq!(t, "# reviewer\nReview carefully.\n");
    }
    // and a calm apply afterwards converges
    assert!(matches!(rig.apply(&[])?.status.as_str(), "applied" | "nothing"));
    assert!(rig.acts()?.values().all(|a| a == "unchanged" || a == "advisory"));
    Ok(())
}
