//! End-to-end: a scripted `generic` agent reacts to graph events through
//! the real binary, processes, git worktrees, and SQLite database.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// One script plays every role. Exploration proposes a step only for a new
/// goal or a failed step, so the graph settles (a survey answers with
/// `@DIR@/survey.json`); execution fails while `@DIR@/fail` exists, else
/// writes a file named after its node
/// (and `shared.txt` too while `@DIR@/shared` exists, so joined steps
/// conflict) and sleeps while `@DIR@/slow` exists. Facing merge conflicts,
/// execution resolves them, or claims to while `@DIR@/unresolvable`
/// exists. Planning sleeps while `@DIR@/slow-planning` exists and answers
/// with `@DIR@/plan.json` when present; evaluation answers with
/// `@DIR@/evaluation.sh` when present. Every prompt is kept in
/// `@DIR@/prompts/` for inspection.
const AGENT: &str = r#"#!/bin/sh
mkdir -p "@DIR@/prompts"
cat > "@DIR@/prompts/$EXPL_ROLE-$$.txt"
case "$EXPL_ROLE" in
exploration)
  if grep -q '^## Map' "@DIR@/prompts/$EXPL_ROLE-$$.txt"; then
    cat "@DIR@/survey.json"
  elif [ "$EXPL_EVENT" = goal.added ] && grep -q 'subgoal of' "@DIR@/prompts/$EXPL_ROLE-$$.txt"; then
    echo '{"nodes":[{"title":"Aim at the top goal","objective":"x","before":[2]},{"title":"Aim at the subgoal","objective":"y","before":[]}]}'
  elif [ "$EXPL_EVENT" = goal.added ]; then
    echo '{"nodes":[{"title":"Write hello","objective":"Create hello.txt"}]}'
  elif grep -q '^## Assumed done' "@DIR@/prompts/$EXPL_ROLE-$$.txt"; then
    echo '{"nodes":[{"title":"After the open step","objective":"z","before":[]}]}'
  elif [ "$EXPL_EVENT" = node.failed ]; then
    echo '{"nodes":[{"title":"Write hello in smaller steps","objective":"Create an empty hello.txt"}]}'
  else
    echo '{"nodes":[]}'
  fi ;;
planning)
  while [ -f "@DIR@/slow-planning" ]; do sleep 0.2; done
  if [ -f "@DIR@/plan.json" ]; then
    cat "@DIR@/plan.json"
  else
    echo 'Plan follows: {"summary":"one path","paths":[{"goal":2,"nodes":[3],"rationale":"only option"}],"priorities":[{"node":3,"priority":5}],"prunes":[],"joins":[]}'
  fi ;;
execution)
  while [ -f "@DIR@/slow" ]; do sleep 0.2; done
  conflicted=$(git diff --name-only --diff-filter=U)
  if [ -n "$conflicted" ]; then
    if [ ! -f "@DIR@/unresolvable" ]; then
      for f in $conflicted; do echo resolved > "$f"; done
    fi
    echo '{"succeeded":true,"summary":"resolved conflicts"}'
    exit 0
  fi
  if [ -f "@DIR@/fail" ]; then
    echo '{"succeeded":false,"summary":"could not compile hello.rs"}'
    exit 0
  fi
  echo "node $EXPL_NODE" > "node-$EXPL_NODE.txt"
  if [ -f "@DIR@/shared" ]; then echo "node $EXPL_NODE" > shared.txt; fi
  echo '{"succeeded":true,"summary":"wrote a file"}' ;;
evaluation)
  if [ -f "@DIR@/evaluation.sh" ]; then
    sh "@DIR@/evaluation.sh"
  elif ls node-*.txt > /dev/null 2>&1; then
    echo '{"evaluations":[{"goal":2,"satisfied":true,"score":90,"reason":"file exists"}]}'
  else
    echo '{"evaluations":[{"goal":2,"satisfied":false,"score":0,"reason":"missing"}]}'
  fi ;;
esac
"#;

struct Sandbox {
    dir: tempfile::TempDir,
    repo: PathBuf,
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Never leave detached processes behind, even when a test fails.
        expl(&self.repo, &["down", "--time", "5"]);
    }
}

impl Sandbox {
    fn slow(&self, on: bool) {
        self.marker("slow", on);
    }

    fn marker(&self, name: &str, on: bool) {
        let marker = self.dir.path().join(name);
        if on {
            std::fs::write(marker, "").expect("marker");
        } else {
            std::fs::remove_file(marker).expect("marker");
        }
    }

    /// `parents` steps, each writing `shared.txt` its own way, and a step
    /// joining them all in order.
    fn conflicting_join(&self, parents: usize) -> (Vec<String>, String) {
        let repo = &self.repo;
        ok(repo, &["goal", "add", "Everything combined"]);
        self.marker("shared", true);
        let parents: Vec<String> = (0..parents)
            .map(|i| ok(repo, &["node", "add", &format!("P{i}"), "--priority", "1"]))
            .collect();
        let mut args = vec!["node", "add", "Join", "--priority", "1"];
        for parent in &parents {
            args.extend(["--after", parent.as_str()]);
        }
        let join = ok(repo, &args);
        (parents, join)
    }
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

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

fn json_lines(text: &str) -> Vec<serde_json::Value> {
    text.lines().map(json).collect()
}

fn node(repo: &Path, id: &str) -> serde_json::Value {
    json(&ok(repo, &["node", "inspect", id]))
}

/// Poll `condition` for up to 60 seconds.
fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(200));
    }
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
    let script = AGENT.replace("@DIR@", &dir.path().display().to_string());
    std::fs::write(&agent, script).expect("agent");
    let mut perms = std::fs::metadata(&agent).expect("meta").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&agent, perms).expect("chmod");

    assert_eq!(ok(&repo, &["init"]), "1");
    let declaration = expl_core::Declaration::TEMPLATE
        .replace("driver = \"claude_code\"", "driver = \"generic\"")
        .replace(
            "command = [\"claude\"]",
            &format!("command = [\"{}\"]", agent.display()),
        )
        .replace(
            "args = [\"--permission-mode\", \"bypassPermissions\"]",
            "args = []",
        );
    std::fs::write(repo.join("expl.toml"), declaration).expect("declaration");
    Sandbox { dir, repo }
}

#[test]
fn events_flow_through_every_role() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    let goal = ok(
        repo,
        &["goal", "add", "Say hello", "--objective", "A file greets"],
    );
    assert_eq!(goal, "2");

    // Each agent reacts to what the previous one caused, then goes idle.
    for agent in [
        "explorer",
        "planner",
        "executor",
        "evaluator",
        "explorer",
        "planner",
    ] {
        ok(repo, &["run", agent, "--exit-when-idle"]);
    }

    let step = node(repo, "3");
    assert_eq!(step["node"]["status"], "completed");
    assert_eq!(step["node"]["priority"], 5);
    assert_eq!(step["predecessors"], serde_json::json!([1]));
    assert_eq!(step["successors"], serde_json::json!([2]));
    // explorer (from the evaluated step) + evaluator + executor.
    let runs = step["agent_runs"].as_array().expect("runs");
    let agents: Vec<&str> = runs.iter().filter_map(|r| r["agent"].as_str()).collect();
    assert_eq!(agents, ["explorer", "evaluator", "executor"], "{runs:?}");
    assert_eq!(git(repo, &["show", "expl/node-3:node-3.txt"]), "node 3");

    assert_eq!(ok(repo, &["candidate", "ls", "-q"]), "3");
    assert_eq!(ok(repo, &["path", "ls", "-q"]), "3");
    assert!(ok(repo, &["graph", "show", "--format", "mermaid"]).contains("n1 --> n3"));

    let kinds: Vec<String> = json_lines(&ok(repo, &["event", "ls", "--format", "json"]))
        .iter()
        .map(|e| e["kind"].as_str().unwrap_or_default().to_owned())
        .collect();
    for expected in [
        "goal.added",
        "node.proposed",
        "node.planned",
        "node.ready",
        "plan.recorded",
        "node.started",
        "node.completed",
        "node.evaluated",
    ] {
        assert!(
            kinds.iter().any(|k| k == expected),
            "{expected} in {kinds:?}"
        );
    }
    // Every agent has reacted to everything addressed to it.
    for agent in json_lines(&ok(repo, &["agent", "ls", "--format", "json"])) {
        assert_eq!(agent["backlog"], 0, "{agent}");
    }
    // The processes went idle and exited; none is left running.
    assert_eq!(ok(repo, &["ps", "-q"]), "");
    let exited = json_lines(&ok(repo, &["ps", "-a", "--format", "json"]));
    assert_eq!(exited.len(), 6);
    assert!(exited.iter().all(|p| p["state"] == "exited"), "{exited:?}");

    // Read-only roles' scratch worktrees are gone; the project stays clean.
    let worktrees: Vec<String> = std::fs::read_dir(repo.join(".expl/worktrees"))
        .expect("worktrees")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(worktrees, ["node-3"]);
    assert_eq!(
        git(repo, &["status", "--porcelain", "--untracked-files=all"]),
        "?? expl.toml"
    );

    // Agents are told their task, never that they run under expl: no role
    // prompt names expl, worktrees, branches, or the events behind it.
    let prompts = sandbox.dir.path().join("prompts");
    let mut roles = std::collections::BTreeSet::new();
    for entry in std::fs::read_dir(&prompts).expect("prompts") {
        let path = entry.expect("entry").path();
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        roles.insert(name.split('-').next().unwrap_or_default().to_owned());
        let text = std::fs::read_to_string(&path)
            .expect("prompt")
            .to_lowercase();
        for word in [
            "expl",
            "worktree",
            "branch",
            "goal.added",
            "node.",
            "agents",
        ] {
            let leaked = text.match_indices(word).any(|(at, _)| {
                !text[at + word.len()..].starts_with(|c: char| c.is_ascii_alphabetic())
            });
            assert!(!leaked, "{name} mentions `{word}`:\n{text}");
        }
    }
    assert_eq!(
        roles.into_iter().collect::<Vec<_>>(),
        ["evaluation", "execution", "exploration", "planning"]
    );
    // Commits carry the step's own words, not expl's.
    assert_eq!(
        git(repo, &["log", "-1", "--format=%s", "expl/node-3"]),
        "Write hello"
    );
}

#[test]
fn completed_predecessors_make_joined_steps_ready() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    ok(repo, &["goal", "add", "Both files"]);
    let a = ok(repo, &["node", "add", "A", "--priority", "1"]);
    let b = ok(repo, &["node", "add", "B", "--priority", "1"]);
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
    // C becomes ready only when both A and B complete; one executor
    // process handles the whole cascade.
    ok(repo, &["run", "executor", "--exit-when-idle"]);
    assert_eq!(node(repo, &c)["node"]["status"], "completed");

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
fn conflicting_joins_merge_every_predecessor() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    let (parents, join) = sandbox.conflicting_join(3);
    ok(repo, &["run", "executor", "--exit-when-idle"]);

    // The first merge conflicts, and so does the next; each is resolved
    // and merging resumes before the step itself is worked on.
    let step = node(repo, &join);
    assert_eq!(step["node"]["status"], "completed", "{step}");
    let tip = step["node"]["head_commit"].as_str().expect("tip");
    let branch = format!("expl/node-{join}");
    assert_eq!(git(repo, &["rev-parse", &branch]), tip);
    for parent in &parents {
        let commit = node(repo, parent)["node"]["head_commit"]
            .as_str()
            .expect("commit")
            .to_owned();
        git(repo, &["merge-base", "--is-ancestor", &commit, tip]);
    }
    let files = git(repo, &["ls-tree", "--name-only", &branch]);
    for id in parents.iter().chain([&join]) {
        let expected = format!("node-{id}.txt");
        assert!(
            files.lines().any(|f| f == expected),
            "{expected} missing from {files}"
        );
    }
    assert_eq!(
        git(repo, &["show", &format!("{branch}:shared.txt")]),
        format!("node {join}")
    );
    // Two resolutions, then the step's own work.
    let runs = step["agent_runs"].as_array().expect("runs");
    assert_eq!(runs.len(), 3, "{runs:?}");
    assert!(runs.iter().all(|r| r["status"] == "succeeded"), "{runs:?}");
    let resolutions = std::fs::read_dir(sandbox.dir.path().join("prompts"))
        .expect("prompts")
        .map(|e| std::fs::read_to_string(e.expect("entry").path()).expect("prompt"))
        .filter(|text| text.contains("conflicts in: shared.txt"))
        .count();
    assert_eq!(resolutions, 2);
}

#[test]
fn unresolved_conflicts_fail_the_step() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    let (parents, join) = sandbox.conflicting_join(2);
    sandbox.marker("unresolvable", true);
    ok(repo, &["run", "executor", "--exit-when-idle"]);

    for parent in &parents {
        assert_eq!(node(repo, parent)["node"]["status"], "completed");
    }
    // The agent claims success, but the conflict markers are still there:
    // the step fails before its own work starts.
    let step = node(repo, &join);
    assert_eq!(step["node"]["status"], "failed", "{step}");
    let outcome = step["node"]["outcome"].as_str().expect("outcome");
    assert!(outcome.contains("shared.txt"), "{outcome}");
    assert_eq!(step["agent_runs"].as_array().expect("runs").len(), 1);
    let events = json_lines(&ok(repo, &["event", "ls", "--format", "json"]));
    let about_join: Vec<&str> = events
        .iter()
        .filter(|e| e["node"].as_i64() == join.parse().ok())
        .filter_map(|e| e["kind"].as_str())
        .collect();
    assert!(about_join.contains(&"node.failed"), "{about_join:?}");
    assert!(!about_join.contains(&"node.completed"), "{about_join:?}");
}

#[test]
fn planning_joins_paths() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    let goal = ok(repo, &["goal", "add", "Both files"]);
    let a = ok(repo, &["node", "add", "A", "--before", &goal]);
    let b = ok(repo, &["node", "add", "B", "--before", &goal]);
    let c = ok(
        repo,
        &["node", "add", "C", "--after", &a, "--before", &goal],
    );
    std::fs::write(
        sandbox.dir.path().join("plan.json"),
        format!(
            r#"{{"summary":"combine","paths":[{{"goal":{goal},"nodes":[{a},{b},{c}],"rationale":"C needs both"}}],"priorities":[{{"node":{a},"priority":3}},{{"node":{b},"priority":2}},{{"node":{c},"priority":1}}],"prunes":[],"joins":[{{"node":{c},"after":[{b}]}}]}}"#
        ),
    )
    .expect("plan");

    // Planning combines A's and B's paths into C; C then starts from both.
    ok(repo, &["run", "planner", "--exit-when-idle"]);
    let joined = node(repo, &c);
    let mut predecessors: Vec<i64> = joined["predecessors"]
        .as_array()
        .expect("predecessors")
        .iter()
        .filter_map(serde_json::Value::as_i64)
        .collect();
    predecessors.sort_unstable();
    assert_eq!(
        predecessors,
        [a.parse::<i64>().expect("a"), b.parse().expect("b")]
    );

    ok(repo, &["run", "executor", "--exit-when-idle"]);
    assert_eq!(node(repo, &c)["node"]["status"], "completed");
    let files = git(repo, &["ls-tree", "--name-only", &format!("expl/node-{c}")]);
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

/// A test-only planner policy, the counterexample to parallel search: of
/// the steps that build on exactly the same nodes (siblings), keep only the
/// highest-priority one and prune the rest. Joins onto a pruned sibling and
/// its place on paths go with it.
fn keep_highest_priority_sibling(
    plan: &serde_json::Value,
    predecessors: impl Fn(i64) -> Vec<i64>,
) -> serde_json::Value {
    let as_i64 = |v: &serde_json::Value| v.as_i64().expect("an integer");
    let priorities: Vec<(i64, i64)> = plan["priorities"]
        .as_array()
        .expect("priorities")
        .iter()
        .map(|p| (as_i64(&p["node"]), as_i64(&p["priority"])))
        .collect();
    let mut top: std::collections::BTreeMap<Vec<i64>, (i64, i64)> =
        std::collections::BTreeMap::new();
    for &(node, priority) in &priorities {
        let mut after = predecessors(node);
        after.sort_unstable();
        let best = top.entry(after).or_insert((node, priority));
        if priority > best.1 {
            *best = (node, priority);
        }
    }
    let kept: std::collections::BTreeSet<i64> = top.values().map(|&(node, _)| node).collect();
    let pruned: Vec<i64> = priorities
        .iter()
        .map(|&(node, _)| node)
        .filter(|node| !kept.contains(node))
        .collect();

    let mut policy = plan.clone();
    policy["summary"] = serde_json::json!("keep only the highest-priority sibling");
    policy["priorities"] = plan["priorities"]
        .as_array()
        .expect("priorities")
        .iter()
        .filter(|p| kept.contains(&as_i64(&p["node"])))
        .cloned()
        .collect();
    let mut prune_list = plan["prunes"].as_array().cloned().unwrap_or_default();
    prune_list.extend(pruned.iter().map(
        |node| serde_json::json!({"node": node, "reason": "a higher-priority sibling is kept"}),
    ));
    policy["prunes"] = prune_list.into();
    policy["joins"] = plan["joins"]
        .as_array()
        .expect("joins")
        .iter()
        .filter(|j| {
            !pruned.contains(&as_i64(&j["node"]))
                && !j["after"]
                    .as_array()
                    .expect("after")
                    .iter()
                    .any(|a| pruned.contains(&as_i64(a)))
        })
        .cloned()
        .collect();
    for path in policy["paths"].as_array_mut().expect("paths") {
        let nodes: Vec<serde_json::Value> = path["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .filter(|n| !pruned.contains(&as_i64(n)))
            .cloned()
            .collect();
        path["nodes"] = nodes.into();
    }
    policy
}

/// What one planner made of the join fixture.
struct JoinOutcome {
    /// The top sibling, the lower-ranked sibling, and the step joining them.
    top: i64,
    other: i64,
    joined: i64,
    /// The goal the top sibling satisfies alone, and the one that needs both.
    first: i64,
    both: i64,
    /// Steps carried out, in id order.
    executed: Vec<i64>,
    /// What the joining step builds on.
    joined_after: Vec<i64>,
    /// `(goal, step)` for every candidate.
    candidates: std::collections::BTreeSet<(i64, i64)>,
}

/// Two sibling steps build on the origin: the top-ranked one writes what
/// the first goal needs, the lower-ranked one the second half of what the
/// second goal needs. A third step builds on the top sibling, and the
/// existing planner joins it with the other sibling, so it satisfies both.
/// `policy` may rewrite the existing planner's plan before planning acts.
fn plan_join_fixture(
    policy: impl FnOnce(&serde_json::Value, &dyn Fn(i64) -> Vec<i64>) -> serde_json::Value,
) -> JoinOutcome {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    let id = |text: String| text.parse::<i64>().expect("an id");
    let first = id(ok(
        repo,
        &["goal", "add", "Greet", "--objective", "Say hello"],
    ));
    let both = id(ok(
        repo,
        &[
            "goal",
            "add",
            "Greet and part",
            "--objective",
            "Say hello and goodbye",
        ],
    ));
    let top = id(ok(repo, &["node", "add", "Write hello"]));
    let other = id(ok(repo, &["node", "add", "Write goodbye"]));
    let joined = id(ok(
        repo,
        &["node", "add", "Combine", "--after", &top.to_string()],
    ));
    std::fs::write(
        sandbox.dir.path().join("evaluation.sh"),
        format!(
            r#"verdict() {{ if $2; then echo "{{\"goal\":$1,\"satisfied\":true,\"score\":90,\"reason\":\"present\"}}"; else echo "{{\"goal\":$1,\"satisfied\":false,\"score\":10,\"reason\":\"missing\"}}"; fi; }}
hello=false; goodbye=false
[ -f node-{top}.txt ] && hello=true
[ -f node-{other}.txt ] && goodbye=true
both=false; $hello && $goodbye && both=true
echo "{{\"evaluations\":[$(verdict {first} $hello),$(verdict {both} $both)]}}"
"#
        ),
    )
    .expect("evaluation");

    let existing = serde_json::json!({
        "summary": "both siblings, joined",
        "paths": [
            {"goal": first, "nodes": [top], "rationale": "hello alone greets"},
            {"goal": both, "nodes": [top, other, joined], "rationale": "the join has both"},
        ],
        "priorities": [
            {"node": top, "priority": 3},
            {"node": other, "priority": 2},
            {"node": joined, "priority": 1},
        ],
        "prunes": [],
        "joins": [{"node": joined, "after": [other]}],
    });
    let predecessors =
        |step: i64| -> Vec<i64> { node_ids(&node(repo, &step.to_string())["predecessors"]) };
    let plan = policy(&existing, &predecessors);
    std::fs::write(sandbox.dir.path().join("plan.json"), plan.to_string()).expect("plan");

    for agent in ["planner", "executor", "evaluator"] {
        ok(repo, &["run", agent, "--exit-when-idle"]);
    }
    let executed = ok(
        repo,
        &[
            "node",
            "ls",
            "--kind",
            "step",
            "--status",
            "completed",
            "-q",
        ],
    )
    .lines()
    .map(|line| id(line.to_owned()))
    .collect();
    let candidates = json_lines(&ok(repo, &["candidate", "ls", "--format", "json"]))
        .iter()
        .map(|c| {
            (
                c["goal"].as_i64().expect("goal"),
                c["node"].as_i64().expect("node"),
            )
        })
        .collect();
    JoinOutcome {
        top,
        other,
        joined,
        first,
        both,
        executed,
        joined_after: predecessors(joined),
        candidates,
    }
}

/// Sorted node ids from a JSON array.
fn node_ids(value: &serde_json::Value) -> Vec<i64> {
    let mut ids: Vec<i64> = value
        .as_array()
        .expect("ids")
        .iter()
        .filter_map(serde_json::Value::as_i64)
        .collect();
    ids.sort_unstable();
    ids
}

#[test]
fn keeping_only_the_top_sibling_loses_the_joined_candidate() {
    let existing = plan_join_fixture(|plan, _| plan.clone());
    let greedy =
        plan_join_fixture(|plan, predecessors| keep_highest_priority_sibling(plan, predecessors));
    let JoinOutcome {
        top,
        other,
        joined,
        first,
        both,
        ..
    } = existing;
    assert_eq!(
        (greedy.top, greedy.other, greedy.joined),
        (top, other, joined)
    );

    // The existing planner runs both siblings and the step joining them,
    // which satisfies both goals.
    assert_eq!(existing.executed, [top, other, joined]);
    assert_eq!(existing.joined_after, [top, other]);
    for expected in [(first, top), (first, joined), (both, joined)] {
        assert!(
            existing.candidates.contains(&expected),
            "{expected:?} in {:?}",
            existing.candidates
        );
    }

    // Keeping only the top sibling executes fewer steps: the other sibling
    // is pruned, so the join never forms. The top sibling still satisfies
    // the first goal, but no candidate satisfies the second.
    assert!(greedy.executed.len() < existing.executed.len());
    assert_eq!(greedy.executed, [top, joined]);
    assert_eq!(greedy.joined_after, [top]);
    assert!(greedy.candidates.contains(&(first, top)));
    assert!(
        greedy.candidates.iter().all(|&(goal, _)| goal != both),
        "{:?}",
        greedy.candidates
    );
    assert!(!greedy.candidates.contains(&(both, joined)));
}

#[test]
fn planning_settles_only_the_proposals_it_was_shown() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    let goal = ok(repo, &["goal", "add", "Say hello"]);
    let a = ok(repo, &["node", "add", "A", "--before", &goal]);
    let prompts = sandbox.dir.path().join("prompts");
    let planning_prompts = || -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&prompts) else {
            return Vec::new();
        };
        let mut paths: Vec<PathBuf> = entries
            .map(|e| e.expect("entry").path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("planning-"))
            })
            .collect();
        paths.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
        paths
            .iter()
            .map(|p| std::fs::read_to_string(p).expect("prompt"))
            .collect()
    };

    // A step proposed while the planner's agent is working.
    sandbox.marker("slow-planning", true);
    let mut planner = Command::new(env!("CARGO_BIN_EXE_expl"))
        .arg("-C")
        .arg(repo)
        .args(["run", "planner", "--once"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("planner");
    wait_until("the planner's agent to start", || {
        planning_prompts()
            .first()
            .is_some_and(|p| p.contains("## Response"))
    });
    let later = ok(repo, &["node", "add", "Later", "--before", &goal]);
    // Even a priority for the unseen step is not taken.
    std::fs::write(
        sandbox.dir.path().join("plan.json"),
        format!(
            r#"{{"summary":"s","paths":[],"priorities":[{{"node":{a},"priority":3}},{{"node":{later},"priority":4}}],"prunes":[],"joins":[]}}"#
        ),
    )
    .expect("plan");
    sandbox.marker("slow-planning", false);
    assert!(planner.wait().expect("planner").success());

    let first = planning_prompts();
    assert_eq!(first.len(), 1);
    assert!(first[0].contains(" A\n"), "{}", first[0]);
    assert!(!first[0].contains("Later"), "{}", first[0]);
    assert_eq!(node(repo, &a)["node"]["status"], "planned");
    assert_eq!(node(repo, &a)["node"]["priority"], 3);
    assert_eq!(node(repo, &later)["node"]["status"], "proposed");
    let unsettled = |repo: &Path| -> Vec<i64> {
        json_lines(&ok(
            repo,
            &[
                "event",
                "ls",
                "--agent",
                "planner",
                "--kind",
                "node.proposed",
                "--format",
                "json",
            ],
        ))
        .iter()
        .filter(|e| e["delivery"] != "done")
        .filter_map(|e| e["node"].as_i64())
        .collect()
    };
    assert_eq!(unsettled(repo), [later.parse::<i64>().expect("id")]);

    // The next planning is shown the step, and settles it.
    ok(repo, &["run", "planner", "--exit-when-idle"]);
    let second = planning_prompts();
    assert_eq!(second.len(), 2);
    assert!(second[1].contains("Later"), "{}", second[1]);
    assert_eq!(node(repo, &later)["node"]["status"], "planned");
    assert_eq!(node(repo, &later)["node"]["priority"], 4);
    assert!(unsettled(repo).is_empty());
}

#[test]
fn up_runs_every_agent_until_down() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    let started = ok(repo, &["up"]);
    // explorer, planner, 2 executors, evaluator.
    assert_eq!(started.lines().count(), 5, "{started}");
    assert_eq!(ok(repo, &["ps", "-q"]).lines().count(), 5);

    // Adding a goal is all it takes: the running processes react.
    ok(repo, &["goal", "add", "Say hello"]);
    wait_until("a candidate", || {
        ok(repo, &["candidate", "ls", "-q"]) == "3"
    });

    // `up` is idempotent.
    assert_eq!(ok(repo, &["up"]), "");
    assert_eq!(ok(repo, &["ps", "-q"]).lines().count(), 5);
    let executor = ok(repo, &["ps", "--format", "json"])
        .lines()
        .map(json)
        .find(|p| p["agent"] == "executor")
        .expect("an executor");
    let name = executor["name"].as_str().expect("name");
    assert!(ok(repo, &["logs", name]).contains("started process"));

    assert_eq!(ok(repo, &["down"]).lines().count(), 5);
    assert_eq!(ok(repo, &["ps", "-q"]), "");
    let all = json_lines(&ok(repo, &["ps", "-a", "--format", "json"]));
    assert!(all.iter().all(|p| p["exit_reason"] == "stopped"), "{all:?}");
    ok(repo, &["rm", name]);
    assert_eq!(ok(repo, &["ps", "-a", "-q"]).lines().count(), 4);
}

#[test]
fn an_abandoned_start_gives_its_replica_slot_back() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    let project = expl_core::Project::discover(repo).expect("project");
    // A launch interrupted between creating the record and its child
    // adopting it.
    let mut table = expl_core::Processes::open(&project.database_path(), &project.processes_dir())
        .expect("processes");
    let orphan = table.create("executor", None, true).expect("create");
    let id = orphan.id.to_string();

    // While its startup deadline holds, it occupies one of the two slots.
    assert_eq!(ok(repo, &["up", "executor"]).lines().count(), 1);
    let record = json(&ok(repo, &["process", "inspect", &id]));
    assert_eq!(record["state"], "created", "{record}");

    // Past its deadline it is dead, and `up` abandons it and starts a
    // replacement in its place.
    let db = rusqlite::Connection::open(project.database_path()).expect("db");
    db.busy_timeout(Duration::from_secs(30)).expect("timeout");
    let aged = db
        .execute(
            "UPDATE processes SET start_by = CAST(strftime('%s','now') AS INTEGER) - 1 \
             WHERE id = ?1",
            [orphan.id.0],
        )
        .expect("age");
    assert_eq!(aged, 1);
    assert_eq!(
        json(&ok(repo, &["process", "inspect", &id]))["state"],
        "dead"
    );
    let up = expl(repo, &["up", "executor"]);
    assert!(up.status.success(), "{up:?}");
    assert_eq!(String::from_utf8_lossy(&up.stdout).lines().count(), 1);
    assert!(
        String::from_utf8_lossy(&up.stderr).contains("abandoned"),
        "{up:?}"
    );
    let record = json(&ok(repo, &["process", "inspect", &id]));
    assert_eq!(record["state"], "exited", "{record}");
    let reason = record["exit_reason"].as_str().expect("reason").to_owned();
    assert!(reason.starts_with("abandoned"), "{record}");
    let executors = || {
        json_lines(&ok(repo, &["ps", "--format", "json"]))
            .into_iter()
            .filter(|p| p["agent"] == "executor")
            .collect::<Vec<_>>()
    };
    assert_eq!(executors().len(), 2);
    assert!(executors().iter().all(|p| p["id"] != orphan.id.0));

    // The child arriving late cannot adopt the expired record.
    let late = expl(repo, &["run", "executor", "--adopt", &id]);
    assert_eq!(late.status.code(), Some(1), "{late:?}");
    assert!(
        String::from_utf8_lossy(&late.stderr).contains("cannot be adopted"),
        "{late:?}"
    );
    let record = json(&ok(repo, &["process", "inspect", &id]));
    assert_eq!(record["state"], "exited", "{record}");
    assert_eq!(record["pid"], serde_json::Value::Null, "{record}");
    assert_eq!(record["exit_reason"], reason.as_str(), "{record}");

    // Nor can a second child adopt a record that is already running.
    wait_until("both replicas to run", || {
        executors().iter().all(|p| p["state"] == "running")
    });
    let running = executors()[0]["id"].to_string();
    let twin = expl(repo, &["run", "executor", "--adopt", &running]);
    assert_eq!(twin.status.code(), Some(1), "{twin:?}");

    assert_eq!(ok(repo, &["up", "executor"]), "");
    assert_eq!(executors().len(), 2);
}

#[test]
fn stopping_mid_execution_gives_the_event_back() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    ok(repo, &["goal", "add", "g"]);
    let a = ok(repo, &["node", "add", "A", "--priority", "1"]);
    sandbox.slow(true);
    let name = ok(repo, &["run", "executor", "-d"]);
    wait_until("the step to start", || {
        node(repo, &a)["node"]["status"] == "running"
    });

    ok(repo, &["stop", &name]);
    let record = json(&ok(repo, &["process", "inspect", &name]));
    assert_eq!(record["state"], "exited");
    assert_eq!(record["exit_reason"], "stopped");
    let step = node(repo, &a);
    assert_eq!(step["node"]["status"], "planned", "{step}");
    assert_eq!(step["agent_runs"][0]["status"], "interrupted");
    let ready = ok(
        repo,
        &[
            "event",
            "ls",
            "--kind",
            "node.ready",
            "--agent",
            "executor",
            "--format",
            "json",
        ],
    );
    assert_eq!(json(&ready)["delivery"], "pending");

    // The released event is delivered to the next executor process.
    sandbox.slow(false);
    ok(repo, &["run", "executor", "--exit-when-idle"]);
    assert_eq!(node(repo, &a)["node"]["status"], "completed");
}

#[test]
fn losing_the_lease_interrupts_the_reaction() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    ok(repo, &["goal", "add", "g"]);
    let a = ok(repo, &["node", "add", "A", "--priority", "1"]);
    sandbox.slow(true);
    let name = ok(repo, &["run", "executor", "-d"]);
    wait_until("the step to start", || {
        node(repo, &a)["node"]["status"] == "running"
    });

    // Its lease runs out and another executor takes the event over, in one
    // step so the process cannot reclaim the event itself first.
    let db = rusqlite::Connection::open(repo.join(".expl/expl.db")).expect("db");
    db.busy_timeout(Duration::from_secs(30)).expect("timeout");
    let taken = db
        .execute(
            "UPDATE deliveries SET lease_token = 'replacement', process_id = 999, \
             attempts = attempts + 1, \
             leased_until = CAST(strftime('%s','now') AS INTEGER) + 600 \
             WHERE agent = 'executor' AND status = 'leased'",
            [],
        )
        .expect("take over");
    assert_eq!(taken, 1);

    wait_until("the loss to be reported", || {
        ok(repo, &["logs", &name]).contains("lost the lease")
    });
    let step = node(repo, &a);
    assert_eq!(step["agent_runs"][0]["status"], "interrupted", "{step}");
    // The step is left to the new holder, not given back to the plan.
    assert_eq!(step["node"]["status"], "running", "{step}");
    let record = json(&ok(repo, &["process", "inspect", &name]));
    assert_eq!(record["state"], "running", "{record}");

    ok(repo, &["stop", &name]);
    let lease: (String, i64, String) = db
        .query_row(
            "SELECT status, process_id, lease_token FROM deliveries \
             WHERE agent = 'executor' AND event_id = (SELECT MAX(id) FROM events \
             WHERE kind = 'node.ready' AND node_id = ?1)",
            [a.parse::<i64>().expect("id")],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("lease");
    assert_eq!(lease, ("leased".into(), 999, "replacement".into()));
}

#[test]
fn usage_errors_exit_with_code_one() {
    let sandbox = sandbox();
    let out = expl(&sandbox.repo, &["node", "inspect", "99"]);
    assert_eq!(out.status.code(), Some(1));
    let out = expl(&sandbox.repo, &["run", "nobody"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no agent named `nobody`"));
    let out = expl(&sandbox.repo, &["stop", "nothing"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn discovery_stops_at_node_worktrees() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    ok(repo, &["goal", "add", "g"]);
    let a = ok(repo, &["node", "add", "A", "--priority", "1"]);
    ok(repo, &["run", "executor", "--exit-when-idle"]);
    let worktree = repo.join(".expl/worktrees").join(format!("node-{a}"));
    let out = expl(&worktree, &["node", "ls"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no expl project"));
}

#[test]
fn the_origin_follows_the_trunk() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    ok(repo, &["goal", "add", "Say hello"]);
    let a = ok(repo, &["node", "add", "A", "--priority", "1"]);
    let b = ok(repo, &["node", "add", "B", "--priority", "1"]);
    ok(repo, &["run", "executor", "--exit-when-idle"]);
    let open = ok(repo, &["node", "add", "Later"]);

    // The owner merges A with git; the next process to run follows the
    // trunk, and the origin moves.
    git(
        repo,
        &["merge", "-q", "--no-edit", &format!("expl/node-{a}")],
    );
    ok(repo, &["run", "evaluator", "--exit-when-idle"]);
    let lineage = json_lines(&ok(repo, &["origin", "ls", "--format", "json"]));
    assert_eq!(lineage.len(), 2, "{lineage:?}");
    assert_eq!(lineage[0]["origin"]["status"], "superseded");
    assert_eq!(lineage[1]["origin"]["status"], "active");
    assert_eq!(lineage[1]["adopted"], serde_json::json!([3]));
    let second = lineage[1]["origin"]["id"].to_string();
    // Every step keeps its base when the trunk moves: B stays a candidate
    // on the old origin, and so does the step that had not started — until
    // planning moves it onto the new one.
    assert_eq!(node(repo, &open)["predecessors"], serde_json::json!([1]));
    assert_eq!(node(repo, &b)["predecessors"], serde_json::json!([1]));
    let plan = sandbox.dir.path().join("plan.json");
    std::fs::write(
        &plan,
        format!(
            r#"{{"summary":"move it","paths":[],"priorities":[],"prunes":[],"joins":[],"rebases":[{open},{b}]}}"#
        ),
    )
    .expect("plan");
    ok(repo, &["run", "planner", "--exit-when-idle"]);
    std::fs::remove_file(&plan).expect("plan");
    assert_eq!(node(repo, &open)["predecessors"], serde_json::json!([6]));
    // B is completed: only steps that have not started move.
    assert_eq!(node(repo, &b)["predecessors"], serde_json::json!([1]));

    // The trunk is judged against the goals, and exploration starts again
    // from it.
    ok(repo, &["run", "evaluator", "--exit-when-idle"]);
    ok(repo, &["run", "explorer", "--exit-when-idle"]);
    let origin = node(repo, &second);
    assert_eq!(origin["evaluations"][0]["score"], 90, "{origin}");
    let agents: Vec<&str> = origin["agent_runs"]
        .as_array()
        .expect("runs")
        .iter()
        .filter_map(|r| r["agent"].as_str())
        .collect();
    assert!(
        agents.contains(&"explorer") && agents.contains(&"evaluator"),
        "{agents:?}"
    );
    let candidates = json_lines(&ok(repo, &["candidate", "ls", "--format", "json"]));
    let entry = |id: &str| {
        candidates
            .iter()
            .find(|c| c["node"].as_i64() == id.parse().ok())
            .unwrap_or_else(|| panic!("{id} in {candidates:?}"))
            .clone()
    };
    assert_eq!(entry(&a)["adopted_into"], 6);
    assert_eq!(entry(&b)["explored_from"], 1);
    assert_eq!(entry(&b)["behind"], 1);

    // A squash merge is recognised by its patch.
    git(
        repo,
        &["merge", "-q", "--squash", &format!("expl/node-{b}")],
    );
    git(repo, &["commit", "-q", "-m", "Squash B"]);
    ok(repo, &["run", "planner", "--exit-when-idle"]);
    let lineage = json_lines(&ok(repo, &["origin", "ls", "--format", "json"]));
    assert_eq!(lineage.len(), 3);
    assert_eq!(lineage[2]["origin"]["title"], "Squash B");
    assert_eq!(lineage[2]["adopted"], serde_json::json!([4]));
    // The process that noticed (the latest planner) says so in its log.
    let planner = json_lines(&ok(repo, &["ps", "-a", "--format", "json"]))
        .into_iter()
        .filter(|p| p["agent"] == "planner")
        .max_by_key(|p| p["id"].as_i64())
        .expect("a planner process");
    let log = ok(repo, &["logs", planner["name"].as_str().expect("name")]);
    assert!(log.contains("trunk main moved to"), "{log}");
}

#[test]
fn idle_exploration_revisits_random_starting_points() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    let declaration = std::fs::read_to_string(repo.join("expl.toml")).expect("read");
    assert!(
        declaration.contains("revisit_cooldown = 600"),
        "{declaration}"
    );
    std::fs::write(
        repo.join("expl.toml"),
        declaration.replacen("revisit_cooldown = 600", "revisit_cooldown = 0", 1),
    )
    .expect("declaration");
    ok(repo, &["goal", "add", "Say hello", "--counter"]);
    // A process that exits when idle never revisits.
    ok(repo, &["run", "explorer", "--exit-when-idle"]);
    let revisits = |repo: &Path| {
        json_lines(&ok(
            repo,
            &[
                "event",
                "ls",
                "--kind",
                "node.revisited",
                "--format",
                "json",
            ],
        ))
    };
    assert!(revisits(repo).is_empty());

    // A long-running one keeps exploring from random starting points (the
    // origin, or the counter-exploration goal), the same ones again and
    // again, and tells the agent what already exists there.
    ok(repo, &["run", "explorer", "-d"]);
    wait_until("several revisits", || revisits(repo).len() >= 4);
    ok(repo, &["down", "--time", "5"]);
    let nodes: std::collections::BTreeSet<i64> = revisits(repo)
        .iter()
        .filter_map(|e| e["node"].as_i64())
        .collect();
    assert!(
        !nodes.is_empty() && nodes.iter().all(|n| [1, 2].contains(n)),
        "revisited {nodes:?}"
    );
    let prompts = sandbox.dir.path().join("prompts");
    let told = std::fs::read_dir(&prompts)
        .expect("prompts")
        .map(|e| std::fs::read_to_string(e.expect("entry").path()).expect("prompt"))
        .any(|text| text.contains("## Already proposed") && text.contains("- Write hello"));
    assert!(told, "no exploration prompt listed the existing steps");
}

#[test]
fn weighted_agents_of_one_role_split_its_events() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    let declaration = std::fs::read_to_string(repo.join("expl.toml")).expect("read");
    let explorer = declaration
        .split("\n[agent.")
        .find(|section| section.starts_with("explorer]"))
        .expect("explorer section")
        .to_owned();
    let contrarian = explorer.replacen("explorer]", "contrarian]", 1);
    std::fs::write(
        repo.join("expl.toml"),
        format!(
            "{}\n[agent.{}weight = 1\n",
            declaration.replacen("breadth = 3\n", "breadth = 3\nweight = 1\n", 1),
            contrarian.trim_end().to_owned() + "\n"
        ),
    )
    .expect("declaration");
    for title in ["One", "Two", "Three", "Four", "Five", "Six"] {
        ok(repo, &["goal", "add", title]);
    }
    ok(repo, &["run", "explorer", "--exit-when-idle"]);
    ok(repo, &["run", "contrarian", "--exit-when-idle"]);

    // Each goal.added was explored by exactly one of the two agents (not
    // by both), and both agents are done with every one of them.
    for agent in ["explorer", "contrarian"] {
        let deliveries = json_lines(&ok(
            repo,
            &[
                "event",
                "ls",
                "--agent",
                agent,
                "--kind",
                "goal.added",
                "--format",
                "json",
            ],
        ));
        assert_eq!(deliveries.len(), 6);
        assert!(
            deliveries.iter().all(|e| e["delivery"] == "done"),
            "{deliveries:?}"
        );
    }
    // Forward explorations are journaled on the node they start from.
    let runs = |agent: &str| {
        node(repo, "1")["agent_runs"]
            .as_array()
            .expect("runs")
            .iter()
            .filter(|r| r["agent"] == agent)
            .count()
    };
    let (explorer, contrarian) = (runs("explorer"), runs("contrarian"));
    assert_eq!(explorer + contrarian, 6, "{explorer} + {contrarian}");
    assert!(explorer > 0 && contrarian > 0, "{explorer} + {contrarian}");
}

#[test]
fn a_failed_step_grows_new_hypotheses_in_its_place() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    ok(repo, &["goal", "add", "Say hello"]);
    ok(repo, &["run", "explorer", "--exit-when-idle"]);
    ok(repo, &["run", "planner", "--exit-when-idle"]);
    sandbox.marker("fail", true);
    ok(repo, &["run", "executor", "--exit-when-idle"]);
    assert_eq!(node(repo, "3")["node"]["status"], "failed");

    // Exploration reacts to the failure: a new hypothesis that builds on
    // what the failed step built on and heads for its goal.
    ok(repo, &["run", "explorer", "--exit-when-idle"]);
    let steps = json_lines(&ok(repo, &["node", "ls", "--format", "json"]));
    let derived = steps
        .iter()
        .find(|n| n["title"] == "Write hello in smaller steps")
        .unwrap_or_else(|| panic!("no hypothesis past the failure: {steps:?}"));
    let derived = node(repo, &derived["id"].to_string());
    assert_eq!(derived["predecessors"], serde_json::json!([1]));
    assert_eq!(derived["successors"], serde_json::json!([2]));
    // The exploration is journaled on the failed step, and was told what
    // went wrong.
    let failed = node(repo, "3");
    assert!(
        failed["agent_runs"]
            .as_array()
            .expect("runs")
            .iter()
            .any(|r| r["role"] == "exploration"),
        "{failed}"
    );
    let prompts = sandbox.dir.path().join("prompts");
    let told = std::fs::read_dir(&prompts)
        .expect("prompts")
        .map(|e| std::fs::read_to_string(e.expect("entry").path()).expect("prompt"))
        .any(|text| text.contains("## Failed step") && text.contains("could not compile hello.rs"));
    assert!(told, "no exploration prompt described the failure");
}

#[test]
fn a_graph_wide_exploration_surveys_and_attaches_anywhere() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    ok(repo, &["goal", "add", "Say hello"]);
    ok(repo, &["run", "explorer", "--exit-when-idle"]);
    ok(repo, &["run", "planner", "--exit-when-idle"]);
    ok(repo, &["run", "executor", "--exit-when-idle"]);
    assert_eq!(node(repo, "3")["node"]["status"], "completed");

    let declaration = std::fs::read_to_string(repo.join("expl.toml")).expect("read");
    let explorer = declaration
        .split("\n[agent.")
        .find(|section| section.starts_with("explorer]"))
        .expect("explorer section")
        .to_owned();
    let surveyor = explorer.replacen("explorer]", "surveyor]", 1);
    std::fs::write(
        repo.join("expl.toml"),
        format!(
            "{declaration}\n[agent.{}\nscope = \"graph\"\n",
            surveyor.trim_end()
        ),
    )
    .expect("declaration");
    // Attach one proposal after the completed step (an unknown id is
    // dropped), one after a goal (not buildable: the origin instead).
    std::fs::write(
        sandbox.dir.path().join("survey.json"),
        r#"{"nodes":[
            {"title":"Build on hello","objective":"x","after":[3,999],"before":[2,77]},
            {"title":"Start over","objective":"y","after":[2],"before":[]}
        ]}"#,
    )
    .expect("survey");

    // Its inbox holds the whole history (goal.added, node.evaluated…): the
    // first event surveys, and every older one is already covered.
    ok(repo, &["run", "surveyor", "--exit-when-idle"]);
    let prompts = sandbox.dir.path().join("prompts");
    let surveys: Vec<String> = std::fs::read_dir(&prompts)
        .expect("prompts")
        .map(|e| std::fs::read_to_string(e.expect("entry").path()).expect("prompt"))
        .filter(|text| text.contains("## Map"))
        .collect();
    assert_eq!(surveys.len(), 1, "surveys: {}", surveys.len());
    assert!(surveys[0].contains("Write hello"), "{}", surveys[0]);

    let steps = json_lines(&ok(repo, &["node", "ls", "--format", "json"]));
    let id = |title: &str| {
        steps
            .iter()
            .find(|n| n["title"] == title)
            .unwrap_or_else(|| panic!("no {title}: {steps:?}"))["id"]
            .to_string()
    };
    let built = node(repo, &id("Build on hello"));
    assert_eq!(built["predecessors"], serde_json::json!([3]));
    assert_eq!(built["successors"], serde_json::json!([2]));
    let over = node(repo, &id("Start over"));
    assert_eq!(over["predecessors"], serde_json::json!([1]));
    assert_eq!(over["successors"], serde_json::json!([2]));
}

#[test]
fn subgoals_nest_and_each_proposal_picks_what_it_heads_for() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    ok(repo, &["goal", "add", "Say hello"]);
    ok(repo, &["goal", "add", "A greeting file", "--under", "2"]);
    ok(repo, &["goal", "add", "An empty one first", "--under", "3"]);
    let bad = expl(repo, &["goal", "add", "Nowhere", "--under", "99"]);
    assert!(!bad.status.success(), "a subgoal of a missing goal");

    let goals = json_lines(&ok(repo, &["goal", "ls", "--format", "json"]));
    let listed: Vec<(i64, serde_json::Value)> = goals
        .iter()
        .map(|g| (g["id"].as_i64().expect("id"), g["under"].clone()))
        .collect();
    assert_eq!(
        listed,
        [
            (2, serde_json::json!([])),
            (3, serde_json::json!([2])),
            (4, serde_json::json!([3]))
        ]
    );

    // Exploring toward a new subgoal shows the goals it serves, and each
    // proposal heads where it says: the top goal, or (saying nothing) the
    // subgoal itself.
    ok(repo, &["run", "explorer", "--exit-when-idle"]);
    let steps = json_lines(&ok(repo, &["node", "ls", "--format", "json"]));
    let heads = |title: &str| -> std::collections::BTreeSet<String> {
        steps
            .iter()
            .filter(|n| n["title"] == title)
            .map(|n| node(repo, &n["id"].to_string())["successors"].to_string())
            .collect()
    };
    assert_eq!(heads("Write hello"), ["[2]".to_owned()].into());
    assert_eq!(heads("Aim at the top goal"), ["[2]".to_owned()].into());
    assert_eq!(
        heads("Aim at the subgoal"),
        ["[3]".to_owned(), "[4]".to_owned()].into()
    );
    let prompts = sandbox.dir.path().join("prompts");
    let told = std::fs::read_dir(&prompts)
        .expect("prompts")
        .map(|e| std::fs::read_to_string(e.expect("entry").path()).expect("prompt"))
        .any(|text| {
            text.contains("[4] An empty one first (subgoal of [3])")
                && text.contains("[2] Say hello")
                && text.contains("alternative routes")
        });
    assert!(told, "no exploration prompt showed the subgoal in its tree");
}

#[test]
fn speculative_exploration_grows_past_steps_not_carried_out() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    let declaration = std::fs::read_to_string(repo.join("expl.toml")).expect("read");
    std::fs::write(
        repo.join("expl.toml"),
        declaration.replacen(
            "revisit_cooldown = 600",
            "revisit_cooldown = 0\nspeculate = true",
            1,
        ),
    )
    .expect("declaration");
    ok(repo, &["goal", "add", "Say hello"]);
    // Planned, never executed (no executor runs here).
    let open = ok(
        repo,
        &[
            "node",
            "add",
            "Open step",
            "--before",
            "2",
            "--priority",
            "1",
        ],
    );
    let open_id: i64 = open.trim().parse().expect("node id");
    ok(repo, &["run", "explorer", "-d"]);
    // Revisits pick at random, so steps proposed meanwhile can be explored
    // ahead of too: wait for the one that starts from the open step.
    let ahead = || {
        json_lines(&ok(repo, &["node", "ls", "--format", "json"]))
            .into_iter()
            .filter(|n| n["title"] == "After the open step")
            .map(|n| node(repo, &n["id"].to_string()))
            .find(|n| n["predecessors"] == serde_json::json!([open_id]))
    };
    wait_until("an exploration ahead of the open step", || {
        ahead().is_some()
    });
    ok(repo, &["down", "--time", "5"]);
    // It heads for the goal (and may have been explored ahead of in turn).
    let after = ahead().expect("ahead");
    assert!(
        after["successors"]
            .as_array()
            .expect("successors")
            .contains(&serde_json::json!(2)),
        "{after}"
    );
    assert_eq!(node(repo, &open)["node"]["status"], "planned");
    let prompts = sandbox.dir.path().join("prompts");
    let told = std::fs::read_dir(&prompts)
        .expect("prompts")
        .map(|e| std::fs::read_to_string(e.expect("entry").path()).expect("prompt"))
        .any(|text| text.contains("## Assumed done") && text.contains("- Open step"));
    assert!(told, "no prompt assumed the open step done");
}
