//! Running git on the box, in a project a Code sandbox can write.
//!
//! The sandbox gets the project read-write, and git takes a lot of its
//! behaviour from files inside the project: `.git/config` (`core.fsmonitor`,
//! `core.hooksPath`, filter and diff drivers, `core.worktree`), hooks, and
//! `.gitattributes`. A model that wrote those could have the agent run a
//! program of its choosing, outside the sandbox, the next time the agent
//! looks at what changed. Two layers keep that from happening:
//!
//! - the sandbox gets the project's `.git` (and its projects' `.git`, in a
//!   folder of projects) read-only, so it can read history but not change
//!   how git behaves (`workspace::launch`). Git inside the sandbox can still
//!   show status, diffs and history; committing is done from Changes, here.
//! - every git the agent runs here goes through [`git`]: the settings that run
//!   a program are overridden on the command line, which beats any config
//!   file (`core.fsmonitor`, `core.hooksPath`, pagers, editors, signing,
//!   auto-gc, transports), diffs never use external diff or textconv, and
//!   submodules are not looked into. What can't be overridden by name — a
//!   filter driver (`filter.<any>.clean`), a config include — is refused
//!   instead, see [`Reach`].
//!
//! Git has no switch to ignore a repository's own config, so for a sandboxed
//! session the rule is: run git only in a repository whose git directory the
//! sandbox can't write (outside the project, or mounted read-only), and whose
//! own config, if it lives in the project, names no driver or include.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Settings that would run a program, pinned. Command-line `-c` beats every
/// config file, so these hold whatever `.git/config` says.
const PINNED: &[&str] = &[
    "core.fsmonitor=false",
    "core.hooksPath=/dev/null",
    "core.pager=cat",
    "core.editor=true",
    "core.askPass=true",
    "core.sshCommand=false",
    "sequence.editor=true",
    "credential.helper=",
    "diff.external=",
    "diff.ignoreSubmodules=all",
    "status.submoduleSummary=false",
    "submodule.recurse=false",
    "commit.gpgSign=false",
    "tag.gpgSign=false",
    "gc.auto=0",
    "maintenance.auto=false",
    "protocol.allow=never",
];

/// Flags for every `git diff`: no external diff program, no textconv, no submodules.
pub(crate) const DIFF: &[&str] = &["--no-ext-diff", "--no-textconv", "--ignore-submodules=all"];

/// A sandboxed session's reach: its project folder, and the git directories
/// inside it that were mounted read-only into the sandbox.
#[derive(Debug, Clone)]
pub(crate) struct Reach {
    pub folder: PathBuf,
    pub read_only: Vec<PathBuf>,
}

/// `git -C dir` with every program-running setting pinned and nothing from
/// the environment steering it elsewhere.
pub(crate) fn command(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir);
    for p in PINNED {
        c.arg("-c").arg(p);
    }
    for v in ["GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR", "GIT_INDEX_FILE", "GIT_EXTERNAL_DIFF", "GIT_CONFIG", "GIT_CONFIG_PARAMETERS", "GIT_CONFIG_COUNT", "GIT_SSH", "GIT_SSH_COMMAND", "GIT_ASKPASS"] {
        c.env_remove(v);
    }
    c.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_PAGER", "cat")
        // Looking doesn't write the index (no index-refresh side effects).
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null());
    c
}

fn run(dir: &Path, args: &[&str]) -> Result<String, String> {
    let o = command(dir).args(args).output().map_err(|e| e.to_string())?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&o.stderr).trim().lines().last().unwrap_or("git failed").to_string())
    }
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Why git mustn't run in `dir` for a session with this reach, if it mustn't.
pub(crate) fn refuse(dir: &Path, reach: &Reach) -> Option<String> {
    let folder = canon(&reach.folder);
    let ro: Vec<PathBuf> = reach.read_only.iter().map(|p| canon(p)).collect();
    let Ok(out) = run(dir, &["rev-parse", "--path-format=absolute", "--git-dir", "--git-common-dir", "--show-toplevel"]) else {
        return Some("not a git repository".into());
    };
    let mut lines = out.lines().map(|l| canon(Path::new(l)));
    let (Some(git_dir), Some(common), Some(top)) = (lines.next(), lines.next(), lines.next()) else {
        return Some("not a git repository".into());
    };
    // `core.worktree` (or a `.git` file) pointing git at some other folder.
    if !canon(dir).starts_with(&top) {
        return Some("its work tree is somewhere else".into());
    }
    let mut in_project = false;
    for d in [&git_dir, &common] {
        if d.starts_with(&folder) {
            if !ro.iter().any(|r| d == r || d.starts_with(r)) {
                return Some("the sandbox can change this repository's settings".into());
            }
            in_project = true;
        }
    }
    if in_project {
        // Read-only now, but it may have been written by an earlier sandbox
        // before it was: no driver or include it could have planted.
        let Ok(cfg) = run(dir, &["config", "--list", "--show-scope", "-z"]) else {
            return Some("its settings can't be read".into());
        };
        let mut it = cfg.split('\0');
        while let (Some(scope), Some(entry)) = (it.next(), it.next()) {
            let key = entry.split('\n').next().unwrap_or("").to_ascii_lowercase();
            if (scope == "local" || scope == "worktree") && runs_something(&key) {
                return Some(format!("its settings name a program ({key})"));
            }
        }
    }
    None
}

/// A config key that runs a program or pulls in another config file, and
/// that the pinned settings can't override by name.
fn runs_something(key: &str) -> bool {
    let last = key.rsplit('.').next().unwrap_or("");
    key.starts_with("include.")
        || key.starts_with("includeif.")
        || (key.starts_with("filter.") && ["clean", "smudge", "process"].contains(&last))
        || (key.starts_with("diff.") && ["command", "textconv"].contains(&last))
        || (key.starts_with("merge.") && last == "driver")
}

/// Runs git in `dir`. With a sandbox's `reach`, first makes sure the
/// repository can't have been rigged by it.
pub(crate) fn git(dir: &Path, reach: Option<&Reach>, args: &[&str]) -> Result<String, String> {
    if let Some(why) = reach.and_then(|r| refuse(dir, r)) {
        return Err(format!("git not run here: {why}"));
    }
    run(dir, args)
}

/// `git status --porcelain` lines in the folder (empty when it isn't a repository,
/// or is one git mustn't run in).
pub(crate) fn changed_files(dir: &Path, reach: Option<&Reach>) -> Vec<String> {
    git(dir, reach, &["status", "--porcelain", "--ignore-submodules=all"])
        .map(|o| o.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn sh(dir: &Path, cmd: &str) {
        let ok = Command::new("sh").arg("-c").arg(cmd).current_dir(dir).env("GIT_CONFIG_NOSYSTEM", "1").status().unwrap().success();
        assert!(ok, "{cmd}");
    }

    /// A repository rigged the way a sandboxed model could rig one: every
    /// hook, fsmonitor, pager, textconv, external diff and signing program
    /// touches a file in `marks`. The project is its `app` folder.
    fn rigged(root: &Path, marks: &Path, diff_drivers: bool, filter: bool) -> PathBuf {
        let repo = root.join("project");
        let app = repo.join("app");
        std::fs::create_dir_all(&app).unwrap();
        sh(&repo, "git init -q && echo one > app/a.txt && git add app/a.txt && git -c user.name=t -c user.email=t@t commit -q -m a");
        let m = marks.display();
        let script = repo.join("evil.sh");
        std::fs::write(&script, format!("#!/bin/sh\ntouch {m}/script-$$\ncat\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let s = script.display();
        sh(&repo, &format!("git config core.fsmonitor 'touch {m}/fsmonitor; false' && git config core.pager 'touch {m}/pager; cat' && git config diff.external '{s}' && git config core.hooksPath app/hooks && git config commit.gpgSign true && git config gpg.program '{s}'"));
        if diff_drivers {
            sh(&repo, &format!("git config diff.evil.textconv '{s}' && git config diff.evil.command '{s}' && echo '* diff=evil' > app/.gitattributes"));
        }
        for hook in ["pre-commit", "commit-msg", "post-commit", "prepare-commit-msg", "post-checkout", "post-index-change", "reference-transaction", "pre-auto-gc"] {
            for d in [app.join("hooks"), repo.join(".git/hooks")] {
                std::fs::create_dir_all(&d).unwrap();
                std::fs::write(d.join(hook), format!("#!/bin/sh\ntouch {m}/hook-{hook}\n")).unwrap();
                std::fs::set_permissions(d.join(hook), std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        if filter {
            sh(&repo, &format!("git config filter.evil.clean '{s}' && git config filter.evil.smudge '{s}' && echo '* filter=evil diff=evil' > app/.gitattributes"));
        }
        std::fs::write(app.join("a.txt"), "two\n").unwrap();
        repo
    }

    fn marks(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect()
    }

    /// Everything the Changes view and its buttons run.
    fn look_commit_discard(dir: &Path, r: Option<&Reach>) {
        assert!(!changed_files(dir, r).is_empty());
        let mut diff = vec!["diff"];
        diff.extend(DIFF);
        diff.push("HEAD");
        assert!(git(dir, r, &diff).unwrap().contains("+two"));
        git(dir, r, &["log", "--oneline", "-8"]).unwrap();
        git(dir, r, &["add", "-A", "--", "."]).unwrap();
        git(dir, r, &["-c", "user.name=TT Station", "-c", "user.email=tt-station@localhost", "commit", "-q", "-m", "x"]).unwrap();
        std::fs::write(dir.join("a.txt"), "three\n").unwrap();
        git(dir, r, &["checkout", "--", "."]).unwrap();
        git(dir, r, &["ls-files", "--others", "--exclude-standard", "--", "."]).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "two\n");
        std::fs::write(dir.join("a.txt"), "two\nmore\n").unwrap();
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
    }

    #[test]
    fn hooks_fsmonitor_pager_signing_and_diff_drivers_never_run() {
        let t = tempfile::tempdir().unwrap();
        let mk = t.path().join("marks");
        std::fs::create_dir_all(&mk).unwrap();
        let repo = rigged(t.path(), &mk, true, false);
        let app = repo.join("app");
        // The project is a folder inside the repository: its git directory is
        // out of the sandbox's reach, so even the diff drivers are allowed --
        // and still never run.
        let reach = Reach { folder: app.clone(), read_only: vec![] };
        assert!(refuse(&app, &reach).is_none(), "{:?}", refuse(&app, &reach));
        look_commit_discard(&app, Some(&reach));
        // An unsandboxed session (or Agents) gets the same pinned settings.
        std::fs::write(app.join("a.txt"), "two\nthree\n").unwrap();
        assert!(!changed_files(&app, None).is_empty());
        let mut diff = vec!["diff"];
        diff.extend(DIFF);
        assert!(git(&app, None, &diff).unwrap().contains("+three"));
        assert!(marks(&mk).is_empty(), "ran: {:?}", marks(&mk));
        // Plain git, for contrast, does run them.
        std::fs::write(app.join("a.txt"), "changed\n").unwrap();
        sh(&repo, "git status --porcelain >/dev/null </dev/null; git diff HEAD >/dev/null </dev/null; true");
        assert!(!marks(&mk).is_empty());
    }

    #[test]
    fn a_read_only_git_directory_in_the_project_is_used_and_runs_nothing() {
        let t = tempfile::tempdir().unwrap();
        let mk = t.path().join("marks");
        std::fs::create_dir_all(&mk).unwrap();
        let repo = rigged(t.path(), &mk, false, false);
        let reach = Reach { folder: repo.clone(), read_only: vec![repo.join(".git")] };
        assert!(refuse(&repo, &reach).is_none(), "{:?}", refuse(&repo, &reach));
        look_commit_discard(&repo.join("app"), Some(&reach));
        assert!(marks(&mk).is_empty(), "ran: {:?}", marks(&mk));
    }

    #[test]
    fn a_driver_planted_in_the_project_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let mk = t.path().join("marks");
        std::fs::create_dir_all(&mk).unwrap();
        let repo = rigged(t.path(), &mk, false, true);
        let reach = Reach { folder: repo.clone(), read_only: vec![repo.join(".git")] };
        assert!(refuse(&repo, &reach).unwrap().contains("filter.evil"));
        assert!(changed_files(&repo, Some(&reach)).is_empty());
        assert!(git(&repo, Some(&reach), &["add", "-A"]).is_err());
        assert!(marks(&mk).is_empty(), "ran: {:?}", marks(&mk));
    }

    #[test]
    fn a_git_directory_the_sandbox_can_write_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let mk = t.path().join("marks");
        std::fs::create_dir_all(&mk).unwrap();
        // A plain folder in which the model ran `git init` and rigged it.
        let repo = rigged(t.path(), &mk, false, false);
        let reach = Reach { folder: repo.clone(), read_only: vec![] };
        assert!(refuse(&repo, &reach).unwrap().contains("sandbox can change"));
        assert!(changed_files(&repo, Some(&reach)).is_empty());
        // ...or one inside a folder of projects.
        let reach = Reach { folder: t.path().to_path_buf(), read_only: vec![] };
        assert!(refuse(&repo.join("app"), &reach).is_some());
        assert!(marks(&mk).is_empty(), "ran: {:?}", marks(&mk));
    }

    #[test]
    fn a_work_tree_pointed_elsewhere_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let repo = t.path().join("p");
        std::fs::create_dir_all(&repo).unwrap();
        sh(&repo, &format!("git init -q && git config core.worktree {}", t.path().join("elsewhere").display()));
        std::fs::create_dir_all(t.path().join("elsewhere")).unwrap();
        let reach = Reach { folder: repo.clone(), read_only: vec![repo.join(".git")] };
        assert!(refuse(&repo, &reach).is_some());
    }

    #[test]
    fn keys_that_run_something() {
        for k in ["filter.lfs.clean", "filter.x.process", "diff.x.textconv", "diff.x.command", "merge.x.driver", "include.path", "includeif.gitdir:/x.path"] {
            assert!(runs_something(k), "{k}");
        }
        for k in ["core.bare", "remote.origin.url", "filter.x.required", "diff.x.binary", "core.hookspath", "user.name"] {
            assert!(!runs_something(k), "{k}");
        }
    }
}
