//! End-to-end: a scripted `generic` agent drives every role through the
//! real binary, git worktrees, and SQLite graph.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const AGENT: &str = r#"#!/bin/sh
cat > /dev/null
case "$EXPL_ROLE" in
explorer)
  echo '{"nodes":[{"title":"Write hello","objective":"Create hello.txt","after":[],"before":[2]}]}' ;;
planner)
  echo 'Plan follows: {"summary":"one path","paths":[{"goal":2,"nodes":[3],"rationale":"only option"}],"priorities":[{"node":3,"priority":5}],"prunes":[]}' ;;
executor)
  echo "node $EXPL_NODE" > "node-$EXPL_NODE.txt"
  echo '{"succeeded":true,"summary":"wrote a file"}' ;;
evaluator)
  if ls node-*.txt > /dev/null 2>&1; then
    echo '{"evaluations":[{"goal":2,"satisfied":true,"score":90,"reason":"file exists"}]}'
  else
    echo '{"evaluations":[{"goal":2,"satisfied":false,"score":0,"reason":"missing"}]}'
  fi ;;
esac
"#;

struct Sandbox {
    _dir: tempfile::TempDir,
    repo: PathBuf,
}

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn expl(repo: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_expl"))
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("expl")
}

fn ok(repo: &Path, args: &[&str]) -> String {
    let out = expl(repo, args);
    assert!(
        out.status.success(),
        "expl {args:?} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn sandbox() -> Sandbox {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "test"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("readme");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);

    let agent = dir.path().join("agent.sh");
    std::fs::write(&agent, AGENT).expect("agent");
    let mut perms = std::fs::metadata(&agent).expect("meta").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&agent, perms).expect("chmod");

    assert_eq!(ok(&repo, &["init"]), "1");
    let declaration = expl_core::Declaration::TEMPLATE
        .replace("kind = \"claude_code\"", "kind = \"generic\"")
        .replace(
            "command = [\"claude\"]",
            &format!("command = [\"{}\"]", agent.display()),
        )
        .replace(
            "args = [\"--permission-mode\", \"bypassPermissions\"]",
            "args = []",
        );
    std::fs::write(repo.join("expl.toml"), declaration).expect("declaration");
    Sandbox { _dir: dir, repo }
}

#[test]
fn one_cycle_explores_plans_executes_and_evaluates() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    assert_eq!(
        ok(
            repo,
            &["goal", "add", "Say hello", "--objective", "A file greets"]
        ),
        "2"
    );

    ok(repo, &["run", "--cycles", "1"]);

    let step = ok(repo, &["node", "ls", "--kind", "step", "--format", "json"]);
    let step: serde_json::Value = serde_json::from_str(&step).expect("json");
    assert_eq!(step["id"], 3);
    assert_eq!(step["status"], "completed");
    assert_eq!(step["priority"], 5);
    assert_eq!(git(repo, &["show", "expl/node-3:node-3.txt"]), "node 3");

    assert_eq!(ok(repo, &["candidate", "ls", "-q"]), "3");
    assert_eq!(ok(repo, &["path", "ls", "-q"]), "3");
    let inspect: serde_json::Value =
        serde_json::from_str(&ok(repo, &["inspect", "3"])).expect("json");
    assert_eq!(inspect["predecessors"], serde_json::json!([1]));
    assert_eq!(inspect["successors"], serde_json::json!([2]));
    // executor + evaluator (the explorer run is journaled against the origin).
    assert_eq!(inspect["agent_runs"].as_array().map(Vec::len), Some(2));
    assert!(ok(repo, &["graph", "show", "--format", "mermaid"]).contains("n1 --> n3"));

    // The project's own working tree stays clean: state is excluded.
    assert_eq!(
        git(repo, &["status", "--porcelain", "--untracked-files=all"]),
        "?? expl.toml"
    );
}

#[test]
fn joined_paths_merge_predecessor_branches() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    ok(repo, &["goal", "add", "Both files"]);
    let a = ok(repo, &["node", "add", "A", "--priority", "1"]);
    let b = ok(repo, &["node", "add", "B", "--priority", "1"]);
    ok(repo, &["execute", &a]);
    ok(repo, &["execute", &b]);
    let c = ok(
        repo,
        &[
            "node",
            "add",
            "C",
            "--after",
            &a,
            "--after",
            &b,
            "--priority",
            "1",
        ],
    );
    let report: serde_json::Value =
        serde_json::from_str(&ok(repo, &["execute", &c, "--format", "json"])).expect("json");
    assert_eq!(report["status"], "completed");

    let branch = format!("expl/node-{c}");
    let files = git(repo, &["ls-tree", "--name-only", &branch]);
    for expected in [
        format!("node-{a}.txt"),
        format!("node-{b}.txt"),
        format!("node-{c}.txt"),
    ] {
        assert!(
            files.lines().any(|f| f == expected),
            "{expected} missing from {files}"
        );
    }
}

#[test]
fn usage_errors_exit_with_code_one() {
    let sandbox = sandbox();
    let out = expl(&sandbox.repo, &["inspect", "99"]);
    assert_eq!(out.status.code(), Some(1));
    let out = expl(&sandbox.repo, &["run", "--cycles", "1"]);
    assert_eq!(out.status.code(), Some(1), "no goals");
    assert!(String::from_utf8_lossy(&out.stderr).contains("no active goals"));
}

#[test]
fn discovery_stops_at_node_worktrees() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    ok(repo, &["goal", "add", "g"]);
    let a = ok(repo, &["node", "add", "A", "--priority", "1"]);
    ok(repo, &["execute", &a]);
    let worktree = repo.join(".expl/worktrees").join(format!("node-{a}"));
    let out = expl(&worktree, &["node", "ls"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no expl project"));
}
