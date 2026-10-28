//! Port of services/targets.py: where the hub may deliver files. A client's roots and every artifact target are checked
//! against the floor's write denies, the hub's own state, the app tree (except out/), `.git` internals and the home dir
//! itself. Security control S-TARGETS. Owner: WS-F.
//!
//! Every path is compared after `os.path.realpath` (the exact `py::os::realpath` port), so a symlink or `..` detour
//! cannot reach a protected directory; `Path` equality and `d in p.parents` become string equality and a `d + "/"`
//! prefix test on those canonical strings.

use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use super::floor::{FloorPolicy, norm_path};

/// Home-relative paths nothing is ever delivered into: credentials, the hub's own state, and files a shell, git or a
/// session manager runs or trusts (since 0.3.0 also the login-shell, fish, environment.d, git, Docker, Kubernetes and
/// package-registry files: audit F12).
pub const SECRET_HOME_DIRS: [&str; 27] = [
    ".ssh",
    ".aws",
    ".config/gcloud",
    ".gnupg",
    ".config/agent-hub",
    ".config/agent-knowledge",
    ".local/share/agent-hub",
    ".local/share/agent-knowledge",
    ".bashrc",
    ".profile",
    ".zshrc",
    ".config/autostart",
    ".config/systemd",
    ".local/bin",
    ".bash_profile",
    ".bash_login",
    ".zprofile",
    ".zshenv",
    ".config/fish",
    ".config/environment.d",
    ".config/git",
    ".gitconfig",
    ".git-credentials",
    ".docker",
    ".kube",
    ".npmrc",
    ".pypirc",
];

/// `Path(os.path.realpath(p))`.
fn real(p: &Path) -> PathBuf {
    hub_core::py::os::realpath(p)
}

/// `p == d or d in p.parents` for canonical (realpath'ed) paths.
pub fn within(p: &Path, d: &Path) -> bool {
    let (ps, ds) = (p.to_string_lossy(), d.to_string_lossy());
    if ps == ds {
        return true;
    }
    if ds == "/" {
        return ps.starts_with('/');
    }
    ps.strip_prefix(ds.as_ref()).is_some_and(|rest| rest.starts_with('/'))
}

/// `_stem_dir(glob)`: the directory a write-deny glob protects (its literal stem), when it is absolute and not `/`.
fn stem_dir(glob: &str) -> Option<PathBuf> {
    let mut g = norm_path(glob);
    if let Some(i) = g.find(['*', '?', '[']) {
        g.truncate(i);
    }
    let g = g.trim_end_matches('/');
    (g.starts_with('/') && g.chars().count() > 1).then(|| real(Path::new(g)))
}

/// `name in p.parts` for a canonical absolute path.
fn has_part(p: &Path, part: &str) -> bool {
    p.to_string_lossy().split('/').any(|c| c == part)
}

pub struct TargetGuard {
    pub home: PathBuf,
    /// nothing may be delivered INTO these, and no root may CONTAIN them
    pub secret: Vec<PathBuf>,
    /// nothing may be delivered into these (a broad project root may contain them)
    pub inside_only: Vec<PathBuf>,
    /// exceptions inside `inside_only` (the app's out/ staging area)
    pub allowed_under: Vec<PathBuf>,
}

impl TargetGuard {
    /// `TargetGuard(policy, data_dir, content_dir, app_root, home=None)`.
    pub fn new(
        policy: &FloorPolicy,
        data_dir: &Path,
        content_dir: &Path,
        app_root: &Path,
        home: Option<&Path>,
    ) -> Self {
        let home = real(&home.map_or_else(crate::config::home, Path::to_path_buf));
        let mut secret: Vec<PathBuf> = SECRET_HOME_DIRS.iter().map(|d| home.join(d)).collect();
        secret.push(real(data_dir));
        secret.extend(policy.deny_path_write.iter().filter_map(|g| stem_dir(g)));
        Self {
            secret: secret.iter().map(|s| real(s)).collect(),
            inside_only: vec![real(content_dir), real(app_root)],
            allowed_under: vec![real(&app_root.join("out"))],
            home,
        }
    }

    /// None when the root is acceptable, else why not.
    pub fn root_problem(&self, root: &Path) -> Option<String> {
        let rp = real(root);
        let shown = rp.display();
        // `rp == Path("/") or rp == self.home or rp in self.home.parents`
        if rp == Path::new("/") || within(&self.home, &rp) {
            return Some(format!(
                "root {shown} is the filesystem root or the home directory (or an ancestor of it)"
            ));
        }
        if has_part(&rp, ".git") {
            return Some(format!("root {shown} is inside a .git directory"));
        }
        for s in &self.secret {
            if within(&rp, s) || within(s, &rp) {
                return Some(format!(
                    "root {shown} overlaps the protected path {} (floor write deny / hub state)",
                    s.display()
                ));
            }
        }
        for d in &self.inside_only {
            if within(&rp, d) && !self.allowed_under.iter().any(|x| within(&rp, x)) {
                return Some(format!("root {shown} is inside the protected path {}", d.display()));
            }
        }
        None
    }

    /// None when the artifact may be written at `target` (relative path `rel` under its root), else why not.
    pub fn target_problem(&self, target: &Path, rel: &str, generic: bool) -> Option<String> {
        let tp = real(target);
        let segs: Vec<&str> = rel.split('/').collect();
        if segs.contains(&".git") || (generic && segs.contains(&"hooks")) {
            return Some(format!(
                "artifact path {} has a .git{} segment",
                hub_core::py::repr(rel),
                if generic { " or hooks" } else { "" }
            ));
        }
        if self.allowed_under.iter().any(|x| within(&tp, x)) {
            return None;
        }
        for d in self.secret.iter().chain(&self.inside_only) {
            if within(&tp, d) {
                return Some(format!(
                    "artifact target {} is inside the protected path {}",
                    tp.display(),
                    d.display()
                ));
            }
        }
        None
    }

    /// Files that configure the agent's own permissions or repo: no adapter may write them unless it explicitly owns
    /// them (claude-code: .claude/settings.json only with manage.permissions; opencode/generic: AGENTS.md as
    /// instructions).
    pub fn sensitive_target(
        adapter_id: &str,
        manage: &IndexMap<String, bool>,
        kind: &str,
        rel: &str,
    ) -> Option<String> {
        let segs: Vec<&str> = rel.split('/').collect();
        let r = hub_core::py::repr(rel);
        let on = |k: &str| manage.get(k).copied().unwrap_or(false);
        if segs.first() == Some(&"security") {
            return Some(format!("artifact path {r} is under security/"));
        }
        if segs.contains(&".git") {
            return Some(format!("artifact path {r} is inside .git"));
        }
        if rel.ends_with(".claude/settings.local.json") {
            return Some(format!("artifact path {r} is the user's local settings file"));
        }
        if rel.ends_with(".claude/settings.json")
            && !(adapter_id == "claude-code" && on("permissions") && kind == "rule")
        {
            return Some(format!(
                "artifact path {r} may only be written by claude-code with manage.permissions enabled"
            ));
        }
        if segs.last() == Some(&"AGENTS.md")
            && !(matches!(adapter_id, "opencode" | "generic") && kind == "instruction" && on("instructions"))
        {
            return Some(format!(
                "artifact path {r} (AGENTS.md) may only be written as managed instructions by an adapter that declares it"
            ));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn within_matches_path_parents_semantics() {
        let p = |s: &str| PathBuf::from(s);
        assert!(within(&p("/a/b"), &p("/a")));
        assert!(within(&p("/a"), &p("/a")));
        assert!(within(&p("/a"), &p("/")));
        assert!(within(&p("/"), &p("/")));
        assert!(!within(&p("/ab"), &p("/a")));
        assert!(!within(&p("/a"), &p("/a/b")));
    }

    #[test]
    fn stem_dir_takes_the_literal_absolute_prefix() {
        assert_eq!(stem_dir("/srv/x/**"), Some(real(Path::new("/srv/x"))));
        assert_eq!(stem_dir("**/admin.key"), None);
        assert_eq!(stem_dir("/*"), None);
        assert_eq!(stem_dir("relative/**"), None);
    }
}
