//! End-to-end: a scripted `generic` agent reacts to graph events through
//! the real binary, processes, git worktrees, and SQLite database.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use expl_core::testing::scaled;

/// One script supplies responses for every handler. Exploration proposes a step only for a new
/// goal or a failed step, so the graph settles (a survey answers with
/// `@DIR@/survey.json`); verification fails while `@DIR@/fail` exists, else
/// writes a file named after its node
/// (and `shared.txt` too while `@DIR@/shared` exists, so joined steps
/// conflict) and sleeps while `@DIR@/slow` exists. Facing merge conflicts,
/// verification resolves them, or claims to while `@DIR@/unresolvable`
/// exists, leaving a stray `litter-*.txt` behind while `@DIR@/litter` does. Planning sleeps while `@DIR@/slow-planning` exists and answers
/// with `@DIR@/plan.json` when present; evaluation answers with
/// `@DIR@/evaluation.sh` when present. Evaluation, which must not touch
/// its worktree, writes `stray.txt` there while `@DIR@/eval-writes` exists
/// and commits `sneaky.txt` while `@DIR@/eval-commits` exists. An
/// investigation answers `no` (whether `hello.txt` exists), and its findings
/// are judged to settle the goal; while `@DIR@/investigation-writes` exists
/// it first creates `hello.txt` itself, and it answers with
/// `@DIR@/finding.json` when present. Prompts are kept in `@DIR@/prompts/`
/// for inspection. While `@DIR@/fast-revisits` exists, the fake explorer
/// records the first revisit prompt and immediately returns no proposals.
/// While `@DIR@/switch` exists, verification first uses git in its clone as it
/// likes: points the branch named in it at its HEAD and switches to it (or
/// detaches HEAD when it says `detached`), and commits an edit of
/// `README.md` there; with `@DIR@/switch-fails` too, it then exits 1
/// instead of reporting.
const AGENT: &str = r#"#!/bin/sh
case "$EXPL_EVENT_ID" in ""|*[!0-9]*) exit 3 ;; esac
[ -n "$EXPL_CONSUMER" ] || exit 3
if [ "$EXPL_HANDLER" = exploration ] && [ "$EXPL_EVENT" = node.revisited ] && [ -f "@DIR@/fast-revisits" ]; then
  prompt="@DIR@/prompts/exploration-revisit.txt"
  if [ ! -f "$prompt" ]; then cat > "$prompt"; fi
  echo '{"nodes":[]}'
  exit 0
fi
mkdir -p "@DIR@/prompts"
cat > "@DIR@/prompts/$EXPL_HANDLER-$$.txt"
map=$(sed -n 's/^`\(.*map\.json\)`$/\1/p' "@DIR@/prompts/$EXPL_HANDLER-$$.txt" | head -n 1)
if [ -n "$map" ]; then cp "$map" "@DIR@/map.json"; fi
case "$EXPL_HANDLER" in
exploration)
  if grep -q '^## Map' "@DIR@/prompts/$EXPL_HANDLER-$$.txt"; then
    cat "@DIR@/survey.json"
  elif [ "$EXPL_EVENT" = goal.added ] && grep -q 'subgoal of' "@DIR@/prompts/$EXPL_HANDLER-$$.txt"; then
    echo '{"nodes":[{"title":"Aim at the top goal","objective":"x","before":[2]},{"title":"Aim at the subgoal","objective":"y","before":[]}]}'
  elif [ "$EXPL_EVENT" = goal.added ]; then
    echo '{"nodes":[{"title":"Write hello","objective":"Create hello.txt"}]}'
  elif grep -q '^## Assumed done' "@DIR@/prompts/$EXPL_HANDLER-$$.txt"; then
    echo '{"nodes":[{"title":"After the open step","objective":"z","before":[]}]}'
  elif grep -q '^## Failed step' "@DIR@/prompts/$EXPL_HANDLER-$$.txt"; then
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
verification)
  while [ -f "@DIR@/slow" ]; do sleep 0.2; done
  if grep -q '^## Question' "@DIR@/prompts/$EXPL_HANDLER-$$.txt"; then
    if [ -f "@DIR@/investigation-writes" ]; then echo made > hello.txt; fi
    if [ -f hello.txt ]; then answer=yes; else answer=no; fi
    if [ -f "@DIR@/finding.json" ]; then cat "@DIR@/finding.json"; exit 0; fi
    echo "{\"investigated\":true,\"answer\":\"$answer\",\"evidence\":\"looked for hello.txt\"}"
    exit 0
  fi
  conflicted=$(git diff --name-only --diff-filter=U)
  if [ -n "$conflicted" ]; then
    if [ ! -f "@DIR@/unresolvable" ]; then
      for f in $conflicted; do echo resolved > "$f"; done
    fi
    if [ -f "@DIR@/litter" ]; then echo junk > "litter-$$.txt"; fi
    echo '{"succeeded":true,"summary":"resolved conflicts"}'
    exit 0
  fi
  if [ -f "@DIR@/fail-partial" ]; then
    echo "half of node $EXPL_NODE" > "partial-$EXPL_NODE.txt"
  fi
  if [ -f "@DIR@/fail" ] || [ -f "@DIR@/fail-partial" ]; then
    echo '{"succeeded":false,"summary":"could not compile hello.rs"}'
    exit 0
  fi
  if [ -f "@DIR@/switch" ]; then
    target=$(cat "@DIR@/switch")
    if [ "$target" = detached ]; then
      git switch -q --detach
    else
      git branch -f "$target" HEAD && git switch -q "$target"
    fi
    echo "edited by $EXPL_NODE" > README.md
    git -c user.name=agent -c user.email=agent@example.com commit -q -am "agent's own"
    if [ -f "@DIR@/switch-fails" ]; then exit 1; fi
  fi
  echo "node $EXPL_NODE" > "node-$EXPL_NODE.txt"
  if [ -f "@DIR@/shared" ]; then echo "node $EXPL_NODE" > shared.txt; fi
  echo '{"succeeded":true,"summary":"wrote a file"}' ;;
evaluation)
  if [ -f "@DIR@/eval-writes" ]; then echo stray > stray.txt; fi
  if [ -f "@DIR@/eval-commits" ]; then
    echo sneaky > sneaky.txt && git add sneaky.txt && git commit -q -m sneaky
  fi
  if [ -f "@DIR@/evaluation.sh" ]; then
    sh "@DIR@/evaluation.sh"
  elif [ "$EXPL_EVENT" = node.failed ]; then
    echo '{"evaluations":[]}'
  elif grep -q '^## Findings' "@DIR@/prompts/$EXPL_HANDLER-$$.txt"; then
    echo '{"evaluations":[{"goal":2,"satisfied":true,"score":100,"reason":"answered"}]}'
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
        expl(&self.repo, &["down", "--time", &down_time()]);
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
    expl_command(repo).args(args).output().expect("expl")
}

fn expl_command(repo: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_expl"));
    command.env("OTEL_SDK_DISABLED", "true").arg("-C").arg(repo);
    command
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

/// Poll `condition` for up to 120 seconds, [`scaled`] by
/// `EXPL_TEST_TIMEOUT_SCALE`.
fn wait_until(repo: &Path, what: &str, mut condition: impl FnMut() -> bool) {
    let timeout = scaled(Duration::from_secs(120));
    let start = Instant::now();
    while !condition() {
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            let events = expl(repo, &["event", "ls"]);
            panic!(
                "timed out after {elapsed:.2?} waiting for {what}\nRecent events:\n{}{}",
                String::from_utf8_lossy(&events.stdout),
                String::from_utf8_lossy(&events.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(200).min(timeout.saturating_sub(elapsed)));
    }
}

/// How long SQLite waits for a lock the running processes hold, [`scaled`].
fn busy_timeout() -> Duration {
    scaled(Duration::from_secs(30))
}

/// The `--time` for `expl down`: 5 seconds for processes to stop before
/// they are killed, [`scaled`] and rounded up to whole seconds.
fn down_time() -> String {
    scaled(Duration::from_secs(5))
        .as_secs_f64()
        .ceil()
        .to_string()
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
fn events_flow_through_every_handler() {
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
        "verifier",
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
    // explorer (from the evaluated step) + evaluator + verifier.
    let runs = step["agent_runs"].as_array().expect("runs");
    let agents: Vec<&str> = runs.iter().filter_map(|r| r["agent"].as_str()).collect();
    assert_eq!(agents, ["explorer", "evaluator", "verifier"], "{runs:?}");
    assert_eq!(git(repo, &["show", "expl/node-3:node-3.txt"]), "node 3");

    assert_eq!(ok(repo, &["hypothesis", "ls", "-q"]), "3");
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
    for agent in json_lines(&ok(repo, &["consumer", "ls", "--format", "json"])) {
        assert_eq!(agent["backlog"], 0, "{agent}");
    }
    // The processes went idle and exited; none is left running.
    assert_eq!(ok(repo, &["ps", "-q"]), "");
    let exited = json_lines(&ok(repo, &["ps", "-a", "--format", "json"]));
    assert_eq!(exited.len(), 6);
    assert!(exited.iter().all(|p| p["state"] == "exited"), "{exited:?}");

    // Read-only roles' scratch worktrees are gone, and so is the settled
    // step's (its branch holds the work); the project stays clean.
    let worktrees: Vec<String> = std::fs::read_dir(repo.join(".expl/worktrees"))
        .expect("worktrees")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(worktrees, Vec::<String>::new());
    assert!(git(repo, &["branch", "--list", "expl/node-3"]).contains("expl/node-3"));
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
        ["evaluation", "exploration", "planning", "verification"]
    );
    // Commits carry the step's own words, not expl's.
    assert_eq!(
        git(repo, &["log", "-1", "--format=%s", "expl/node-3"]),
        "Write hello"
    );
}

#[test]
fn an_investigation_that_changes_the_code_fails_instead_of_answering() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    ok(repo, &["goal", "add", "Is hello written yet?"]);
    let question = ok(
        repo,
        &[
            "node",
            "add",
            "Look for hello.txt",
            "--objective",
            "Does hello.txt exist?",
            "--investigation",
            "--before",
            "2",
            "--priority",
            "1",
        ],
    );
    // The agent writes hello.txt and then reports finding it.
    sandbox.marker("investigation-writes", true);
    ok(repo, &["run", "verifier", "--exit-when-idle"]);
    let step = node(repo, &question)["node"].clone();
    assert_eq!(step["status"], "failed", "{step}");
    assert!(step["answer"].is_null(), "{step}");
    let outcome = step["outcome"].as_str().unwrap_or_default();
    assert!(
        outcome.contains("changed the code") && outcome.contains("hello.txt"),
        "{outcome}"
    );
}

#[test]
fn an_investigation_answers_without_a_commit_and_its_finding_is_judged() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    ok(repo, &["goal", "add", "Is hello written yet?"]);
    let change = ok(repo, &["node", "add", "Write a file", "--priority", "1"]);
    let question = ok(
        repo,
        &[
            "node",
            "add",
            "Look for hello.txt",
            "--objective",
            "Does hello.txt exist?",
            "--investigation",
            "--after",
            &change,
            "--before",
            "2",
            "--priority",
            "1",
        ],
    );
    let then = ok(
        repo,
        &[
            "node",
            "add",
            "Write after asking",
            "--after",
            &question,
            "--priority",
            "1",
        ],
    );
    // Long, multiline evidence whose decisive lines come past the digest's
    // cutoff.
    let padding = "Searched the tree for hello.txt, directory by directory. ".repeat(15);
    let decisive = [
        "Decisive:",
        "$ ls hello.txt",
        "ls: hello.txt: No such file or directory (exit status 1)",
        "src/main.rs:7: never writes hello.txt",
    ];
    let evidence = format!("{padding}\n\n{}\n", decisive.join("\n"));
    assert!(evidence.find("Decisive").expect("decisive") > 600);
    let finding = serde_json::json!({"investigated": true, "answer": "no", "evidence": evidence});
    std::fs::write(sandbox.dir.path().join("finding.json"), finding.to_string()).expect("finding");
    ok(repo, &["run", "verifier", "--exit-when-idle"]);

    // Answered, at the code it was about, with no branch or commit of its own.
    let code = git(repo, &["rev-parse", &format!("expl/node-{change}")]);
    let finding = node(repo, &question)["node"].clone();
    assert_eq!(finding["status"], "completed", "{finding}");
    assert_eq!(finding["investigation"], true);
    assert_eq!(finding["answer"], "no");
    assert_eq!(finding["branch"], serde_json::Value::Null);
    assert_eq!(finding["head_commit"].as_str(), Some(code.trim()));
    let branches = git(repo, &["branch", "--list", "expl/*"]);
    assert!(
        !branches.contains(&format!("expl/node-{question}")),
        "{branches}"
    );
    // The change after it starts from that same code.
    assert_eq!(node(repo, &then)["node"]["status"], "completed");
    let parent = git(repo, &["rev-parse", &format!("expl/node-{then}^")]);
    assert_eq!(parent.trim(), code.trim());

    // Its finding, not a working directory, is what evaluation judges.
    ok(repo, &["run", "evaluator", "--exit-when-idle"]);
    let prompts = sandbox.dir.path().join("prompts");
    let judged = std::fs::read_dir(&prompts)
        .expect("prompts")
        .map(|e| std::fs::read_to_string(e.expect("entry").path()).expect("prompt"))
        .find(|text| text.contains("## Findings") && text.contains("Look for hello.txt: no"))
        .expect("no evaluation was shown the finding");
    // Complete: every decisive line past the cutoff, in its original form.
    for line in decisive {
        assert!(
            judged.lines().any(|l| l == format!("    {line}")),
            "{line:?} did not reach the evaluator:\n{judged}"
        );
    }
    assert!(judged.contains(padding.trim_end()), "{judged}");
    let satisfying = ok(repo, &["hypothesis", "ls"]);
    assert!(satisfying.contains("answer: no"), "{satisfying}");

    // Adopting the change it read takes in the change, never the
    // investigation — not even after the change after it is adopted too.
    for step in [&change, &then] {
        let branch = format!("expl/node-{step}");
        git(repo, &["merge", "-q", "--no-ff", "-m", "adopt", &branch]);
        ok(repo, &["run", "planner", "--exit-when-idle"]);
        let lineage = json_lines(&ok(repo, &["origin", "ls", "--format", "json"]));
        let adopted = &lineage.last().expect("origin")["adopted"];
        let id: i64 = step.parse().expect("id");
        assert_eq!(adopted, &serde_json::json!([id]), "adopting {step}");
    }
    let satisfying = json_lines(&ok(repo, &["hypothesis", "ls", "--format", "json"]));
    let answer = satisfying
        .iter()
        .find(|c| c["node"].as_i64() == question.parse().ok())
        .expect("the finding still satisfies the goal");
    assert_eq!(answer["adopted_into"], serde_json::Value::Null, "{answer}");
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
    // C becomes ready only when both A and B complete; one verifier
    // process handles the whole cascade.
    ok(repo, &["run", "verifier", "--exit-when-idle"]);
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
    sandbox.marker("litter", true);
    ok(repo, &["run", "verifier", "--exit-when-idle"]);

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
    // What the resolving agent left besides the conflicts is not kept.
    assert!(!files.contains("litter-"), "{files}");
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
fn falling_over_starts_the_next_cli_clean_and_keeps_a_merge_in_progress() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    let dir = sandbox.dir.path();
    // A second verifier CLI that dirties its directory, then fails.
    let failing = dir.join("failing-verifier.sh");
    std::fs::write(
        &failing,
        "#!/bin/sh\ncat > /dev/null\necho dirt > \"dirt-$$.txt\"\nexit 1\n",
    )
    .expect("failing verifier");
    let mut perms = std::fs::metadata(&failing).expect("meta").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&failing, perms).expect("chmod");
    let declaration = std::fs::read_to_string(repo.join("expl.toml")).expect("read");
    std::fs::write(
        repo.join("expl.toml"),
        format!(
            "{declaration}\n[[consumer.verifier.alternative]]\ndriver = \"generic\"\ncommand = [\"{}\"]\n",
            failing.display()
        ),
    )
    .expect("declaration");

    // One process takes turns between the CLIs: some steps, and the first
    // conflict resolution, start at the failing one and fall over.
    let (parents, join) = sandbox.conflicting_join(3);
    ok(repo, &["run", "verifier", "--exit-when-idle"]);
    let logs = std::fs::read_dir(repo.join(".expl/logs"))
        .expect("logs")
        .map(|e| std::fs::read_to_string(e.expect("entry").path()).expect("log"))
        .collect::<Vec<_>>();
    assert!(
        logs.iter()
            .any(|l| l.contains("resolved conflicts") && l.contains("falling over")),
        "no resolution fell over"
    );
    for id in parents.iter().chain([&join]) {
        let step = node(repo, id);
        assert_eq!(step["node"]["status"], "completed", "{step}");
        let files = git(
            repo,
            &["ls-tree", "--name-only", &format!("expl/node-{id}")],
        );
        assert!(!files.contains("dirt-"), "#{id}: {files}");
    }
}

#[test]
fn unresolved_conflicts_fail_the_step() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    let (parents, join) = sandbox.conflicting_join(2);
    sandbox.marker("unresolvable", true);
    ok(repo, &["run", "verifier", "--exit-when-idle"]);

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

    ok(repo, &["run", "verifier", "--exit-when-idle"]);
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
    let mut planner = expl_command(repo)
        .args(["run", "planner", "--once"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("planner");
    wait_until(repo, "the planner's agent to start", || {
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
                "--consumer",
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
fn an_invalid_plan_falls_over_to_the_alternative_cli() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    let dir = sandbox.dir.path();
    // A second planning CLI that answers with a well-formed JSON object
    // missing `summary` and with `paths` of the wrong type.
    let invalid = dir.join("invalid-planner.sh");
    std::fs::write(
        &invalid,
        format!(
            "#!/bin/sh\ncat > /dev/null\necho run >> \"{}\"\necho '{{\"paths\":\"none\",\"priorities\":[]}}'\n",
            dir.join("invalid-runs").display()
        ),
    )
    .expect("invalid planner");
    let mut perms = std::fs::metadata(&invalid).expect("meta").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&invalid, perms).expect("chmod");
    let declaration = std::fs::read_to_string(repo.join("expl.toml")).expect("read");
    std::fs::write(
        repo.join("expl.toml"),
        format!(
            "{declaration}\n[[consumer.planner.alternative]]\ndriver = \"generic\"\ncommand = [\"{}\"]\n",
            invalid.display()
        ),
    )
    .expect("declaration");

    let goal = ok(repo, &["goal", "add", "Say hello"]);
    let a = ok(repo, &["node", "add", "A", "--before", &goal]);
    let prompts = dir.join("prompts");
    let valid_started = || {
        std::fs::read_dir(&prompts).is_ok_and(|entries| {
            entries.flatten().any(|e| {
                e.file_name().to_string_lossy().starts_with("planning-")
                    && std::fs::read_to_string(e.path()).is_ok_and(|p| p.contains("## Response"))
            })
        })
    };

    // Two plannings in one process: its routes start at different CLIs, so
    // one of them starts at the invalid one.
    sandbox.marker("slow-planning", true);
    let mut planner = expl_command(repo)
        .args(["run", "planner", "--exit-when-idle"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("planner");
    wait_until(repo, "the valid planner to start", valid_started);
    let later = ok(repo, &["node", "add", "Later", "--before", &goal]);
    std::fs::write(
        dir.join("plan.json"),
        format!(
            r#"{{"summary":"the valid plan","paths":[{{"goal":{goal},"nodes":[{a}],"rationale":"from the valid CLI"}}],"priorities":[{{"node":{a},"priority":3}},{{"node":{later},"priority":4}}],"prunes":[],"joins":[]}}"#
        ),
    )
    .expect("plan");
    sandbox.marker("slow-planning", false);
    assert!(planner.wait().expect("planner").success());

    let invalid_runs = std::fs::read_to_string(dir.join("invalid-runs")).expect("invalid runs");
    assert_eq!(invalid_runs.lines().count(), 1, "{invalid_runs}");

    // The log of the planning that started at the invalid CLI records its
    // response and why it was rejected, then the valid CLI's answer.
    let logs: Vec<String> = std::fs::read_dir(repo.join(".expl/logs"))
        .expect("logs")
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with("-planner.log"))
        .map(|e| std::fs::read_to_string(e.path()).expect("log"))
        .collect();
    assert_eq!(logs.len(), 2, "{logs:?}");
    let fell_over: Vec<&String> = logs
        .iter()
        .filter(|log| log.contains(r#"{"paths":"none","priorities":[]}"#))
        .collect();
    assert_eq!(fell_over.len(), 1, "{logs:?}");
    let log = fell_over[0];
    let rejected = log
        .find("=== generic failed: agent response does not match its schema")
        .unwrap_or_else(|| panic!("no rejection in {log}"));
    let answered = log
        .rfind("the valid plan")
        .unwrap_or_else(|| panic!("no valid plan in {log}"));
    assert!(rejected < answered, "{log}");
    assert!(log.contains("falling over to the next CLI"), "{log}");

    // Both plannings were recorded from the valid CLI's plan.
    let plans = json_lines(&ok(
        repo,
        &["event", "ls", "--kind", "plan.recorded", "--format", "json"],
    ));
    assert_eq!(plans.len(), 2, "{plans:?}");
    let listed = expl(repo, &["path", "ls"]);
    assert!(
        String::from_utf8_lossy(&listed.stderr).contains("the valid plan"),
        "{listed:?}"
    );
    let paths = json_lines(&ok(repo, &["path", "ls", "--format", "json"]));
    assert_eq!(paths.len(), 1, "{paths:?}");
    assert_eq!(paths[0]["rationale"], "from the valid CLI");
    assert_eq!(node(repo, &a)["node"]["status"], "planned");
    assert_eq!(node(repo, &a)["node"]["priority"], 3);
    assert_eq!(node(repo, &later)["node"]["status"], "planned");
    assert_eq!(node(repo, &later)["node"]["priority"], 4);
}

#[test]
fn up_runs_every_agent_until_down() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    let started = ok(repo, &["up"]);
    // explorer, planner, 2 verifiers, evaluator.
    assert_eq!(started.lines().count(), 5, "{started}");
    assert_eq!(ok(repo, &["ps", "-q"]).lines().count(), 5);

    // Adding a goal is all it takes: the running processes react.
    ok(repo, &["goal", "add", "Say hello"]);
    wait_until(repo, "a hypothesis that satisfies the goal", || {
        ok(repo, &["hypothesis", "ls", "-q"]) == "3"
    });

    // `up` is idempotent.
    assert_eq!(ok(repo, &["up"]), "");
    assert_eq!(ok(repo, &["ps", "-q"]).lines().count(), 5);
    let verifier = ok(repo, &["ps", "--format", "json"])
        .lines()
        .map(json)
        .find(|p| p["consumer"] == "verifier")
        .expect("a verifier");
    let name = verifier["name"].as_str().expect("name");
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
    let orphan = table.create("verifier", None, true).expect("create");
    let id = orphan.id.to_string();

    // While its startup deadline holds, it occupies one of the two slots.
    assert_eq!(ok(repo, &["up", "verifier"]).lines().count(), 1);
    let record = json(&ok(repo, &["process", "inspect", &id]));
    assert_eq!(record["state"], "created", "{record}");

    // Past its deadline it is dead, and `up` abandons it and starts a
    // replacement in its place.
    let db = rusqlite::Connection::open(project.database_path()).expect("db");
    db.busy_timeout(busy_timeout()).expect("timeout");
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
    let up = expl(repo, &["up", "verifier"]);
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
    let verifiers = || {
        json_lines(&ok(repo, &["ps", "--format", "json"]))
            .into_iter()
            .filter(|p| p["consumer"] == "verifier")
            .collect::<Vec<_>>()
    };
    assert_eq!(verifiers().len(), 2);
    assert!(verifiers().iter().all(|p| p["id"] != orphan.id.0));

    // The child arriving late cannot adopt the expired record.
    let late = expl(repo, &["run", "verifier", "--adopt", &id]);
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
    wait_until(repo, "both replicas to run", || {
        verifiers().iter().all(|p| p["state"] == "running")
    });
    let running = verifiers()[0]["id"].to_string();
    let twin = expl(repo, &["run", "verifier", "--adopt", &running]);
    assert_eq!(twin.status.code(), Some(1), "{twin:?}");

    assert_eq!(ok(repo, &["up", "verifier"]), "");
    assert_eq!(verifiers().len(), 2);
}

#[test]
fn stopping_mid_verification_gives_the_event_back() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    ok(repo, &["goal", "add", "g"]);
    let a = ok(repo, &["node", "add", "A", "--priority", "1"]);
    sandbox.slow(true);
    let name = ok(repo, &["run", "verifier", "-d"]);
    wait_until(repo, "the step to start", || {
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
            "--consumer",
            "verifier",
            "--format",
            "json",
        ],
    );
    assert_eq!(json(&ready)["delivery"], "pending");

    // The released event is delivered to the next verifier process.
    sandbox.slow(false);
    ok(repo, &["run", "verifier", "--exit-when-idle"]);
    assert_eq!(node(repo, &a)["node"]["status"], "completed");
}

#[test]
fn a_timed_out_verification_fails_the_step() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    let path = repo.join("expl.toml");
    let declaration = std::fs::read_to_string(&path).expect("declaration");
    let declaration = declaration.replacen("replicas = 2", "replicas = 2\ntimeout = \"1s\"", 1);
    std::fs::write(&path, declaration).expect("declaration");
    ok(repo, &["goal", "add", "g"]);
    let a = ok(repo, &["node", "add", "A", "--priority", "1"]);
    sandbox.slow(true);

    ok(repo, &["run", "verifier", "--exit-when-idle"]);
    let step = node(repo, &a);
    assert_eq!(step["node"]["status"], "failed", "{step}");
    let outcome = step["node"]["outcome"].as_str().unwrap_or_default();
    assert!(outcome.contains("agent timed out after 1s"), "{step}");
    let run = &step["agent_runs"][0];
    assert_eq!(run["status"], "failed", "{step}");
    assert_eq!(run["error"], "agent timed out after 1s", "{step}");
}

#[test]
fn losing_the_lease_interrupts_the_reaction() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    ok(repo, &["goal", "add", "g"]);
    let a = ok(repo, &["node", "add", "A", "--priority", "1"]);
    sandbox.slow(true);
    let name = ok(repo, &["run", "verifier", "-d"]);
    wait_until(repo, "the agent run to start", || {
        node(repo, &a)["agent_runs"][0]["status"] == "running"
    });

    // Its lease runs out and another verifier takes the event over, in one
    // step so the process cannot reclaim the event itself first.
    let db = rusqlite::Connection::open(repo.join(".expl/expl.db")).expect("db");
    db.busy_timeout(busy_timeout()).expect("timeout");
    let taken = db
        .execute(
            "UPDATE deliveries SET lease_token = 'replacement', process_id = 999, \
             attempts = attempts + 1, \
             leased_until = CAST(strftime('%s','now') AS INTEGER) + 600 \
             WHERE agent = 'verifier' AND status = 'leased'",
            [],
        )
        .expect("take over");
    assert_eq!(taken, 1);

    wait_until(repo, "the loss to be reported", || {
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
             WHERE agent = 'verifier' AND event_id = (SELECT MAX(id) FROM events \
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
    assert!(String::from_utf8_lossy(&out.stderr).contains("no consumer named `nobody`"));
    let out = expl(&sandbox.repo, &["stop", "nothing"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn discovery_stops_at_node_worktrees() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    ok(repo, &["goal", "add", "g"]);
    let a = ok(repo, &["node", "add", "A", "--priority", "1"]);
    ok(repo, &["run", "verifier", "--exit-when-idle"]);
    // A settled step's worktree is gone; check out its branch where one
    // would be while it runs.
    let worktree = repo.join(".expl/worktrees").join(format!("node-{a}-a1"));
    let path = worktree.display().to_string();
    git(
        repo,
        &["worktree", "add", "-q", &path, &format!("expl/node-{a}")],
    );
    let out = expl(&worktree, &["node", "ls"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no expl project"));

    let nested = worktree.join("nested");
    std::fs::create_dir(&nested).expect("nested directory");
    std::fs::remove_file(worktree.join(".git")).expect("remove worktree git file");
    for dir in [&worktree, &nested] {
        let out = expl(dir, &["node", "ls"]);
        assert_eq!(out.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&out.stderr).contains("not inside a git repository"));
    }
}

#[test]
fn the_origin_follows_the_trunk() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    ok(repo, &["goal", "add", "Say hello"]);
    let a = ok(repo, &["node", "add", "A", "--priority", "1"]);
    let b = ok(repo, &["node", "add", "B", "--priority", "1"]);
    ok(repo, &["run", "verifier", "--exit-when-idle"]);
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
    // Every step keeps its base when the trunk moves: B stays on
    // the old origin, and so does the step that had not started — until
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
    let satisfying = json_lines(&ok(repo, &["hypothesis", "ls", "--format", "json"]));
    let entry = |id: &str| {
        satisfying
            .iter()
            .find(|c| c["node"].as_i64() == id.parse().ok())
            .unwrap_or_else(|| panic!("{id} in {satisfying:?}"))
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
        .filter(|p| p["consumer"] == "planner")
        .max_by_key(|p| p["id"].as_i64())
        .expect("a planner process");
    let log = ok(repo, &["logs", planner["name"].as_str().expect("name")]);
    assert!(log.contains("trunk main moved to"), "{log}");
}

#[test]
fn a_step_takes_a_fresh_branch_and_leaves_the_trunk_and_other_branches_alone() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    let tip = |branch: &str| git(repo, &["rev-parse", &format!("refs/heads/{branch}")]);
    ok(repo, &["goal", "add", "Say hello"]);
    let a = ok(repo, &["node", "add", "A", "--priority", "1"]);
    let b = ok(repo, &["node", "add", "B", "--priority", "1"]);

    // The trunk is a branch that is not checked out and is named like A's
    // first branch name.
    let trunk = format!("expl/node-{a}");
    git(repo, &["checkout", "-q", "-b", &trunk]);
    std::fs::write(repo.join("trunk.txt"), "trunk\n").expect("write");
    git(repo, &["add", "trunk.txt"]);
    git(repo, &["commit", "-q", "-m", "Trunk work"]);
    // An unrelated branch, never the trunk, takes B's first name.
    let unrelated = format!("expl/node-{b}");
    git(repo, &["checkout", "-q", "-b", &unrelated, "main"]);
    std::fs::write(repo.join("unrelated.txt"), "unrelated\n").expect("write");
    git(repo, &["add", "unrelated.txt"]);
    git(repo, &["commit", "-q", "-m", "Unrelated work"]);
    git(repo, &["checkout", "-q", "main"]);
    let trunk_tip = tip(&trunk);
    let unrelated_tip = tip(&unrelated);
    let path = repo.join("expl.toml");
    let declaration = std::fs::read_to_string(&path).expect("declaration");
    assert!(declaration.contains("trunk = \"main\""), "{declaration}");
    let declaration = declaration.replacen("trunk = \"main\"", &format!("trunk = \"{trunk}\""), 1);
    std::fs::write(&path, declaration).expect("declaration");

    // A first attempt that fails stays on a branch of its own.
    sandbox.marker("fail", true);
    ok(repo, &["run", "verifier", "--exit-when-idle"]);
    let mut failed = Vec::new();
    for id in [&a, &b] {
        let step = node(repo, id);
        assert_eq!(step["node"]["status"], "failed", "{step}");
        let branch = format!("expl/node-{id}-2");
        assert_eq!(step["node"]["branch"], branch.as_str(), "{step}");
        failed.push((branch.clone(), tip(&branch)));
    }
    assert_eq!(tip(&trunk), trunk_tip, "the trunk is never written to");
    assert_eq!(
        tip(&unrelated),
        unrelated_tip,
        "an unrelated branch is kept"
    );

    // A retry takes yet another fresh name; nothing earlier is replaced.
    sandbox.marker("fail", false);
    ok(repo, &["node", "retry", &a]);
    ok(repo, &["node", "retry", &b]);
    ok(repo, &["run", "verifier", "--exit-when-idle"]);
    for id in [&a, &b] {
        let step = node(repo, id);
        assert_eq!(step["node"]["status"], "completed", "{step}");
        let branch = format!("expl/node-{id}-3");
        assert_eq!(step["node"]["branch"], branch.as_str(), "{step}");
        assert_eq!(step["node"]["head_commit"], tip(&branch).as_str(), "{step}");
        assert_eq!(
            git(repo, &["show", &format!("{branch}:node-{id}.txt")]),
            format!("node {id}")
        );
    }
    assert_eq!(tip(&trunk), trunk_tip, "the trunk is never written to");
    assert_eq!(git(repo, &["show", &format!("{trunk}:trunk.txt")]), "trunk");
    assert_eq!(
        tip(&unrelated),
        unrelated_tip,
        "an unrelated branch is kept"
    );
    assert_eq!(
        git(repo, &["show", &format!("{unrelated}:unrelated.txt")]),
        "unrelated"
    );
    for (branch, commit) in &failed {
        assert_eq!(&tip(branch), commit, "an earlier attempt's branch is kept");
    }
}

#[test]
fn a_symbolic_trunk_and_its_target_are_left_alone_when_a_step_takes_that_name() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    let tip = |branch: &str| git(repo, &["rev-parse", &format!("refs/heads/{branch}")]);
    ok(repo, &["goal", "add", "Say hello"]);
    let a = ok(repo, &["node", "add", "A", "--priority", "1"]);

    // The declared trunk is a symbolic ref to a branch named like A's
    // first branch name; neither name is the other, so only the ref
    // itself shows that the step's name is the trunk's.
    let target = format!("expl/node-{a}");
    let target_ref = format!("refs/heads/{target}");
    git(repo, &["checkout", "-q", "-b", &target]);
    std::fs::write(repo.join("trunk.txt"), "trunk\n").expect("write");
    git(repo, &["add", "trunk.txt"]);
    git(repo, &["commit", "-q", "-m", "Trunk work"]);
    git(repo, &["checkout", "-q", "main"]);
    git(repo, &["symbolic-ref", "refs/heads/trunk", &target_ref]);
    let trunk_tip = tip("trunk");
    assert_eq!(tip(&target), trunk_tip);
    let path = repo.join("expl.toml");
    let declaration = std::fs::read_to_string(&path).expect("declaration");
    assert!(declaration.contains("trunk = \"main\""), "{declaration}");
    let declaration = declaration.replacen("trunk = \"main\"", "trunk = \"trunk\"", 1);
    std::fs::write(&path, declaration).expect("declaration");

    ok(repo, &["run", "verifier", "--exit-when-idle"]);
    let step = node(repo, &a);
    assert_eq!(step["node"]["status"], "completed", "{step}");
    let branch = format!("expl/node-{a}-2");
    assert_eq!(step["node"]["branch"], branch.as_str(), "{step}");
    assert_eq!(step["node"]["head_commit"], tip(&branch).as_str(), "{step}");
    assert_eq!(
        git(repo, &["show", &format!("{branch}:node-{a}.txt")]),
        format!("node {a}")
    );

    // The trunk still points at its target, which still holds its commit.
    assert_eq!(
        git(repo, &["symbolic-ref", "refs/heads/trunk"]),
        target_ref,
        "the trunk keeps its symref target"
    );
    assert_eq!(tip("trunk"), trunk_tip, "the trunk is never written to");
    assert_eq!(tip(&target), trunk_tip, "the trunk's target is kept");
    assert_eq!(git(repo, &["show", "trunk:trunk.txt"]), "trunk");
}

#[test]
fn a_reinitialised_graph_reusing_an_id_keeps_the_unadopted_branch() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    let tip = |branch: &str| git(repo, &["rev-parse", &format!("refs/heads/{branch}")]);
    let is_ancestor = |ancestor: &str, of: &str| {
        Command::new("git")
            .args(["merge-base", "--is-ancestor", ancestor, of])
            .current_dir(repo)
            .status()
            .expect("git")
            .success()
    };

    // A step of the first graph completes on `expl/node-2` and is never
    // adopted.
    let old = ok(repo, &["node", "add", "Old work", "--priority", "1"]);
    assert_eq!(old, "2");
    ok(repo, &["run", "verifier", "--exit-when-idle"]);
    let step = node(repo, &old);
    assert_eq!(step["node"]["status"], "completed", "{step}");
    assert_eq!(step["node"]["branch"], "expl/node-2", "{step}");
    let old_commit = tip("expl/node-2");
    assert_eq!(step["node"]["head_commit"], old_commit.as_str(), "{step}");
    assert!(!is_ancestor(&old_commit, "main"), "not adopted");

    // The graph is thrown away and started again: ids count from the
    // start, so the new graph's first step is node 2 again.
    std::fs::remove_dir_all(repo.join(".expl")).expect("remove .expl");
    assert_eq!(ok(repo, &["init"]), "1");
    let new = ok(repo, &["node", "add", "New work", "--priority", "1"]);
    assert_eq!(new, "2");
    ok(repo, &["run", "verifier", "--exit-when-idle"]);
    let step = node(repo, &new);
    assert_eq!(step["node"]["status"], "completed", "{step}");
    assert_eq!(step["node"]["branch"], "expl/node-2-2", "{step}");
    let new_commit = tip("expl/node-2-2");
    assert_eq!(step["node"]["head_commit"], new_commit.as_str(), "{step}");
    assert_ne!(new_commit, old_commit);

    // The old work is still on its branch.
    assert_eq!(tip("expl/node-2"), old_commit, "the old branch is kept");
    assert!(is_ancestor(&old_commit, "expl/node-2"));
    assert!(!is_ancestor(&old_commit, "expl/node-2-2"));
    assert_eq!(
        git(repo, &["log", "-1", "--format=%s", "expl/node-2"]),
        "Old work"
    );
}

/// Every ref in `repo`, with what it points at.
fn refs(repo: &Path) -> String {
    git(repo, &["for-each-ref", "--format=%(refname) %(objectname)"])
}

/// A step whose agent uses git in its clone as it likes — points the
/// branch `target` at its own work and switches there (or detaches HEAD),
/// and commits — moves no ref of the project's but the step's own branch,
/// and leaves the main checkout alone. Without `fallback` the step
/// completes with what the agent's files hold; with it, the agent fails
/// there, the clone is made again for a second CLI, which fails too, and
/// the step fails.
fn a_step_that_moves_refs_in_its_clone(target: &str, fallback: bool) {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    let dir = sandbox.dir.path();
    git(repo, &["switch", "-q", "-c", "other"]);
    std::fs::write(repo.join("other.txt"), "other\n").expect("write");
    git(repo, &["add", "other.txt"]);
    git(repo, &["commit", "-q", "-m", "Other work"]);
    git(repo, &["switch", "-q", "main"]);
    if fallback {
        let declaration = std::fs::read_to_string(repo.join("expl.toml")).expect("read");
        std::fs::write(
            repo.join("expl.toml"),
            format!(
                "{declaration}\n[[consumer.verifier.alternative]]\ndriver = \"generic\"\ncommand = [\"{}\"]\n",
                dir.join("agent.sh").display()
            ),
        )
        .expect("declaration");
        sandbox.marker("switch-fails", true);
    }
    ok(repo, &["goal", "add", "Say hello"]);
    let step = ok(repo, &["node", "add", "A", "--priority", "1"]);
    std::fs::write(dir.join("switch"), target).expect("switch marker");

    let trunk = git(repo, &["rev-parse", "refs/heads/main"]);
    let refs_before = refs(repo);
    let status_before = git(repo, &["status", "--porcelain", "--untracked-files=all"]);
    ok(repo, &["run", "verifier", "--exit-when-idle"]);

    let inspected = node(repo, &step);
    let branch = format!("refs/heads/expl/node-{step}");
    if fallback {
        assert_eq!(inspected["node"]["status"], "failed", "{inspected}");
        let log = std::fs::read_dir(repo.join(".expl/logs"))
            .expect("logs")
            .map(|e| std::fs::read_to_string(e.expect("entry").path()).expect("log"))
            .collect::<String>();
        assert!(log.contains("restored the working directory"), "{log}");
        assert!(log.contains("falling over"), "{log}");
    } else {
        assert_eq!(inspected["node"]["status"], "completed", "{inspected}");
        assert_eq!(
            git(repo, &["show", &format!("{branch}:README.md")]),
            format!("edited by {step}")
        );
        assert_eq!(
            git(repo, &["show", &format!("{branch}:node-{step}.txt")]),
            format!("node {step}")
        );
        // One commit of expl's on the trunk; the agent's own stayed in
        // its clone.
        assert_eq!(
            git(repo, &["log", "--format=%s", &format!("{trunk}..{branch}")]),
            "A"
        );
    }
    // Every ref but the step's own branch is where it was.
    let refs_after = refs(repo);
    let refs_after: Vec<&str> = refs_after
        .lines()
        .filter(|line| !line.starts_with(&format!("{branch} ")))
        .collect();
    assert_eq!(refs_after.join("\n"), refs_before, "{target}");
    assert_eq!(git(repo, &["rev-parse", "refs/heads/main"]), trunk);
    // The main checkout is untouched.
    assert_eq!(git(repo, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(
        git(repo, &["status", "--porcelain", "--untracked-files=all"]),
        status_before
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("README.md")).expect("readme"),
        "hello\n"
    );
}

#[test]
fn a_step_that_moves_the_trunks_name_in_its_clone_moves_no_ref_of_the_project() {
    a_step_that_moves_refs_in_its_clone("main", false);
}

#[test]
fn a_step_that_moves_another_branchs_name_in_its_clone_moves_no_ref_of_the_project() {
    a_step_that_moves_refs_in_its_clone("other", false);
}

#[test]
fn a_step_that_detaches_its_clone_moves_no_ref_of_the_project() {
    a_step_that_moves_refs_in_its_clone("detached", false);
}

#[test]
fn the_restore_before_a_fallback_cli_makes_the_clone_again() {
    for target in ["main", "other", "detached"] {
        a_step_that_moves_refs_in_its_clone(target, true);
    }
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
    // again, and tells the agent what already exists there. The fake agent
    // answers revisits immediately so the wait measures event production.
    sandbox.marker("fast-revisits", true);
    ok(repo, &["run", "explorer", "-d"]);
    wait_until(repo, "four node.revisited events", || {
        revisits(repo).len() >= 4
    });
    ok(repo, &["down", "--time", &down_time()]);
    let nodes: std::collections::BTreeSet<i64> = revisits(repo)
        .iter()
        .filter_map(|e| e["node"].as_i64())
        .collect();
    assert!(
        !nodes.is_empty() && nodes.iter().all(|n| [1, 2].contains(n)),
        "revisited {nodes:?}"
    );
    let prompt =
        std::fs::read_to_string(sandbox.dir.path().join("prompts/exploration-revisit.txt"))
            .expect("revisit prompt");
    assert!(
        prompt.contains("## Already proposed") && prompt.contains("- Write hello"),
        "the revisit prompt did not list the existing steps"
    );
}

#[test]
fn failure_can_be_explored_when_evaluation_subscribes_only_to_completion() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    let path = repo.join("expl.toml");
    let config = std::fs::read_to_string(&path)
        .expect("declaration")
        .replace(
            "on = [\"node.completed\", \"node.failed\", \"origin.advanced\"]",
            "on = [\"node.completed\"]",
        );
    std::fs::write(path, config).expect("declaration");

    ok(repo, &["goal", "add", "G"]);
    let step = ok(repo, &["node", "add", "Try a step", "--priority", "1"]);
    sandbox.marker("fail", true);
    ok(repo, &["run", "verifier", "--once"]);
    ok(repo, &["run", "evaluator", "--exit-when-idle"]);

    let consumers = json_lines(&ok(repo, &["consumer", "ls", "--format", "json"]));
    let evaluator = consumers
        .iter()
        .find(|c| c["name"] == "evaluator")
        .expect("evaluator");
    assert_eq!(evaluator["on"], serde_json::json!(["node.completed"]));
    assert_eq!(evaluator["backlog"], 0);
    assert_eq!(
        json_lines(&ok(
            repo,
            &[
                "event",
                "ls",
                "--consumer",
                "evaluator",
                "--kind",
                "node.failed",
                "--format",
                "json",
            ]
        ))[0]["delivery"],
        "undelivered",
    );

    ok(repo, &["run", "explorer", "--exit-when-idle"]);
    let failed = node(repo, &step);
    let runs = failed["agent_runs"].as_array().expect("runs");
    assert!(runs.iter().any(|r| r["role"] == "exploration"), "{failed}");
}

#[test]
fn nested_routers_assign_one_worker_and_share_capacity_across_groups() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    let path = repo.join("expl.toml");
    let base = std::fs::read_to_string(&path)
        .expect("declaration")
        .replace(
            "[consumer.explorer]",
            "[consumer.explorer]\ngroup = \"directions\"",
        )
        .replace(
            "[consumer.verifier]",
            "[consumer.verifier]\ngroup = \"checks\"",
        );
    let fringe = base
        .split("\n[consumer.")
        .find(|section| section.starts_with("explorer]"))
        .expect("explorer")
        .replacen("explorer]", "fringe]", 1);
    std::fs::write(
        &path,
        format!(
            "{base}\n[consumer.{fringe}\n{}",
            r#"
[routing]
directions = "all"
checks = "all"

[router.all]
targets = ["explorers", "verifier"]
policy = "first"
concurrency = 1

[router.explorers]
targets = ["explorer", "fringe"]
policy = "first"
concurrency = 1
"#
        ),
    )
    .expect("declaration");

    ok(repo, &["goal", "add", "G"]);
    let step = ok(repo, &["node", "add", "Verify first", "--priority", "1"]);
    sandbox.slow(true);
    ok(repo, &["run", "verifier", "-d", "--once"]);
    wait_until(repo, "verification in progress", || {
        node(repo, &step)["agent_runs"][0]["status"] == "running"
    });
    // Another worker records the recipient but cannot lease its event.
    ok(repo, &["run", "fringe", "--exit-when-idle"]);
    let rerouted = std::fs::read_to_string(&path)
        .expect("declaration")
        .replace(
            "targets = [\"explorer\", \"fringe\"]",
            "targets = [\"fringe\", \"explorer\"]",
        );
    std::fs::write(&path, rerouted).expect("reordered policy");
    let explorer = ok(repo, &["run", "explorer", "-d", "--exit-when-idle"]);
    wait_until(repo, "explorer to wait for the shared slot", || {
        json(&ok(repo, &["process", "inspect", &explorer]))["activity"] == "waiting for capacity"
    });

    let db = rusqlite::Connection::open(repo.join(".expl/expl.db")).expect("db");
    let assigned: (String, i64) = db
        .query_row(
            "SELECT a.agent, (SELECT COUNT(*) FROM deliveries d WHERE d.event_id = e.id)
         FROM assignments a JOIN events e ON e.id = a.event_id WHERE e.kind = 'goal.added'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("assignment");
    assert_eq!(assigned, ("explorer".to_owned(), 0));

    sandbox.slow(false);
    wait_until(repo, "explorer to finish after the slot is freed", || {
        json(&ok(repo, &["process", "inspect", &explorer]))["exit_reason"] == "idle"
    });
    assert_eq!(node(repo, &step)["node"]["status"], "completed");
    let runs = node(repo, "1");
    let runs = runs["agent_runs"].as_array().expect("runs");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["agent"], "explorer");
}

#[test]
fn weighted_agents_of_one_role_split_its_events() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    let declaration = std::fs::read_to_string(repo.join("expl.toml")).expect("read");
    let declaration = legacy_declaration(&declaration);
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

fn legacy_declaration(text: &str) -> String {
    text.lines()
        .filter(|line| !line.starts_with("on = "))
        .collect::<Vec<_>>()
        .join("\n")
        .replace("[consumer.", "[agent.")
        .replace("handler = ", "role = ")
}

/// `base` with a `contrarian` exploration beside the `explorer`, the two
/// sharing the role by weight, nearly every event routed to `favored`.
fn weighted_declaration(base: &str, favored: &str) -> String {
    let weight = |agent: &str| if agent == favored { 1_000_000 } else { 1 };
    let explorer = base
        .split("\n[agent.")
        .find(|section| section.starts_with("explorer]"))
        .expect("explorer section");
    let contrarian = explorer.replacen("explorer]", "contrarian]", 1);
    format!(
        "{}\n[agent.{}\nweight = {}\n",
        base.replacen(
            "breadth = 3\n",
            &format!("breadth = 3\nweight = {}\n", weight("explorer")),
            1
        ),
        contrarian.trim_end(),
        weight("contrarian")
    )
}

#[test]
fn weighted_assignments_hold_across_declarations_and_restarts() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    let base = std::fs::read_to_string(repo.join("expl.toml")).expect("read");
    let base = legacy_declaration(&base);
    let favor = |agent: &str| {
        std::fs::write(repo.join("expl.toml"), weighted_declaration(&base, agent))
            .expect("declaration");
    };
    let add_goals = |prefix: &str| {
        for i in 0..4 {
            ok(repo, &["goal", "add", &format!("{prefix} {i}")]);
        }
    };
    let settled = |agent: &str, goals: usize| {
        let deliveries = json_lines(&ok(
            repo,
            &[
                "event",
                "ls",
                "--consumer",
                agent,
                "--kind",
                "goal.added",
                "--format",
                "json",
            ],
        ));
        deliveries.len() == goals && deliveries.iter().all(|e| e["delivery"] == "done")
    };

    // Overlapping processes, each of which loaded weights that route
    // nearly every event to itself, race for the same goals.
    favor("explorer");
    let explorer = ok(repo, &["run", "explorer", "-d"]);
    let running = |name: &str| json(&ok(repo, &["process", "inspect", name]))["state"] == "running";
    wait_until(repo, "the explorer to start", || running(&explorer));
    favor("contrarian");
    let contrarian = ok(repo, &["run", "contrarian", "-d"]);
    wait_until(repo, "the contrarian to start", || running(&contrarian));
    add_goals("Overlap");
    wait_until(repo, "both agents to settle every goal", || {
        settled("explorer", 4) && settled("contrarian", 4)
    });
    ok(repo, &["down", "--time", &down_time()]);

    // After a restart, each agent loads weights that route nearly every
    // event to itself again, but the first to route an event decides.
    add_goals("Explorer first");
    favor("explorer");
    ok(repo, &["run", "explorer", "--exit-when-idle"]);
    favor("contrarian");
    ok(repo, &["run", "contrarian", "--exit-when-idle"]);
    // …and with weights that route every event away from itself, an agent
    // still reacts to what was assigned to it.
    add_goals("Explorer routes away");
    ok(repo, &["run", "explorer", "--exit-when-idle"]);
    favor("explorer");
    ok(repo, &["run", "contrarian", "--exit-when-idle"]);
    assert!(settled("explorer", 12) && settled("contrarian", 12));

    // Every goal was explored exactly once, by the agent it is assigned to.
    let db = rusqlite::Connection::open(repo.join(".expl/expl.db")).expect("db");
    let mut statement = db
        .prepare(
            "SELECT e.id, s.agent, \
             (SELECT group_concat(r.agent) FROM agent_runs r WHERE r.event_id = e.id) \
             FROM events e LEFT JOIN assignments s \
             ON s.role = 'exploration' AND s.event_id = e.id \
             WHERE e.kind = 'goal.added' ORDER BY e.id",
        )
        .expect("prepare");
    let rows: Vec<(i64, Option<String>, Option<String>)> = statement
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(rows.len(), 12, "{rows:?}");
    for (event, assigned, ran) in &rows {
        assert!(assigned.is_some(), "e{event} was never assigned: {rows:?}");
        assert_eq!(assigned, ran, "e{event}: {rows:?}");
    }
    let assigned = |range: std::ops::Range<usize>| -> Vec<&str> {
        rows[range]
            .iter()
            .filter_map(|(_, a, _)| a.as_deref())
            .collect()
    };
    assert_eq!(assigned(4..8), vec!["explorer"; 4], "{rows:?}");
    assert_eq!(assigned(8..12), vec!["contrarian"; 4], "{rows:?}");
}

#[test]
fn a_failed_step_grows_new_hypotheses_in_its_place() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    ok(repo, &["goal", "add", "Say hello"]);
    ok(repo, &["run", "explorer", "--exit-when-idle"]);
    ok(repo, &["run", "planner", "--exit-when-idle"]);
    sandbox.marker("fail", true);
    ok(repo, &["run", "verifier", "--exit-when-idle"]);
    assert_eq!(node(repo, "3")["node"]["status"], "failed");

    // Exploration reacts to the failure once evaluation concluded about
    // it: a new hypothesis that builds on what the failed step built on
    // and heads for its goal.
    ok(repo, &["run", "explorer", "--exit-when-idle"]);
    let steps = json_lines(&ok(repo, &["node", "ls", "--format", "json"]));
    assert!(
        !steps
            .iter()
            .any(|n| n["title"] == "Write hello in smaller steps"),
        "explored past a failure evaluation has not concluded about: {steps:?}"
    );
    ok(repo, &["run", "evaluator", "--exit-when-idle"]);
    ok(repo, &["run", "explorer", "--exit-when-idle"]);
    let steps = json_lines(&ok(repo, &["node", "ls", "--format", "json"]));
    let derived = steps
        .iter()
        .find(|n| n["title"] == "Write hello in smaller steps")
        .unwrap_or_else(|| panic!("no hypothesis past the failure: {steps:?}"));
    let derived_id = derived["id"].clone();
    let derived = node(repo, &derived_id.to_string());
    assert_eq!(derived["predecessors"], serde_json::json!([1]));
    assert_eq!(derived["successors"], serde_json::json!([2]));
    // It records the failure it was grown from — not as an edge, since the
    // failed step is not built on — and the failed step lists what grew.
    assert_eq!(derived["node"]["derived_from"], 3, "{derived}");
    assert_eq!(derived["node"]["proposed_by"], "explorer", "{derived}");
    let failed = node(repo, "3");
    assert_eq!(
        failed["grown_from_failure"],
        serde_json::json!([derived_id]),
        "{failed}"
    );
    // The exploration is journaled on the failed step, and was told what
    // went wrong.
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

    // A human can grow one too, from the CLI.
    let by_hand = ok(
        repo,
        &["node", "add", "Write hello by hand", "--derived-from", "3"],
    );
    let by_hand = node(repo, by_hand.trim());
    assert_eq!(by_hand["node"]["derived_from"], 3);
    assert!(by_hand["node"]["proposed_by"].is_null(), "{by_hand}");
    let from_goal = expl(
        repo,
        &["node", "add", "Not from a step", "--derived-from", "2"],
    );
    assert!(!from_goal.status.success(), "grown from a goal");

    // Failing again, the next exploration is told what already grew from
    // this failure, so it looks for other causes.
    ok(repo, &["node", "retry", "3"]);
    ok(repo, &["run", "verifier", "--exit-when-idle"]);
    assert_eq!(node(repo, "3")["node"]["status"], "failed");
    std::fs::remove_dir_all(&prompts).expect("clear prompts");
    ok(repo, &["run", "evaluator", "--exit-when-idle"]);
    ok(repo, &["run", "explorer", "--exit-when-idle"]);
    let told = std::fs::read_dir(&prompts)
        .expect("prompts")
        .map(|e| std::fs::read_to_string(e.expect("entry").path()).expect("prompt"))
        .find(|text| text.contains("## Failed step"))
        .expect("an exploration past the second failure");
    let grown = told
        .split("## Hypotheses already grown from this failure")
        .nth(1)
        .unwrap_or_else(|| panic!("no hypotheses listed: {told}"));
    assert!(
        grown.contains("Write hello in smaller steps") && grown.contains("Write hello by hand"),
        "{grown}"
    );
}

#[test]
fn evaluation_carries_on_a_step_that_stopped_short_from_where_it_stopped() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    ok(repo, &["goal", "add", "Say hello"]);
    ok(repo, &["run", "explorer", "--exit-when-idle"]);
    ok(repo, &["run", "planner", "--exit-when-idle"]);
    sandbox.marker("fail-partial", true);
    ok(repo, &["run", "verifier", "--exit-when-idle"]);
    let first = node(repo, "3");
    assert_eq!(first["node"]["status"], "failed", "{first}");
    let left = first["node"]["head_commit"]
        .as_str()
        .expect("the attempt's commit")
        .to_owned();
    sandbox.marker("fail-partial", false);

    // Evaluation finds it stopped short of a reachable objective: the step
    // goes back to run, with what evaluation found.
    let script = sandbox.dir.path().join("evaluation.sh");
    std::fs::write(
        &script,
        r#"echo '{"conclusion":{"decision":"continue","hypothesis":"hello can be written","finding":"half written when the compiler stopped","consequence":"finish the other half"},"evaluations":[]}'"#,
    )
    .expect("script");
    ok(repo, &["run", "evaluator", "--exit-when-idle"]);
    let step = node(repo, "3");
    assert_eq!(step["node"]["status"], "planned", "{step}");
    let conclusion = &step["conclusions"][0];
    assert_eq!(conclusion["decision"], "continue", "{step}");
    assert_eq!(conclusion["reopened"], true, "{step}");
    assert_eq!(conclusion["resume_commit"], left.as_str(), "{step}");
    assert_eq!(step["evaluations"], serde_json::json!([]), "{step}");

    // The next attempt starts from what the first left, and is told.
    let prompts = sandbox.dir.path().join("prompts");
    std::fs::remove_dir_all(&prompts).expect("clear prompts");
    ok(repo, &["run", "verifier", "--exit-when-idle"]);
    let step = node(repo, "3");
    assert_eq!(step["node"]["status"], "completed", "{step}");
    let branch = step["node"]["branch"].as_str().expect("branch").to_owned();
    assert_ne!(branch, "expl/node-3", "a new attempt gets a new branch");
    assert_eq!(
        git(repo, &["show", &format!("{branch}:partial-3.txt")]).trim(),
        "half of node 3"
    );
    git(repo, &["merge-base", "--is-ancestor", &left, &branch]);
    let told = std::fs::read_dir(&prompts)
        .expect("prompts")
        .map(|e| std::fs::read_to_string(e.expect("entry").path()).expect("prompt"))
        .any(|text| {
            text.contains("## Earlier attempts")
                && text.contains("half written when the compiler stopped")
                && text.contains("finish the other half")
        });
    assert!(
        told,
        "the continued attempt was not told how far the last got"
    );

    // Done this time: judged against the goal in the same record, and
    // exploration past it is told what it established.
    std::fs::write(
        &script,
        r#"echo '{"conclusion":{"decision":"done","hypothesis":"hello can be written","finding":"hello is written","consequence":"greeting can build on it"},"evaluations":[{"goal":2,"satisfied":true,"score":90,"reason":"file exists"}]}'"#,
    )
    .expect("script");
    ok(repo, &["run", "evaluator", "--exit-when-idle"]);
    let step = node(repo, "3");
    assert_eq!(step["conclusions"][1]["decision"], "done", "{step}");
    assert_eq!(step["evaluations"][0]["score"], 90, "{step}");
    std::fs::remove_dir_all(&prompts).expect("clear prompts");
    ok(repo, &["run", "explorer", "--exit-when-idle"]);
    let told = std::fs::read_dir(&prompts)
        .expect("prompts")
        .map(|e| std::fs::read_to_string(e.expect("entry").path()).expect("prompt"))
        .any(|text| text.contains("Concluded done.") && text.contains("greeting can build on it"));
    assert!(
        told,
        "exploration past the step was not told what it changes"
    );
    // And it was handed the whole map, conclusions included, to analyse.
    let map: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(sandbox.dir.path().join("map.json")).expect("map"),
    )
    .expect("json");
    let concluded: Vec<&str> = map["conclusions"]
        .as_array()
        .expect("conclusions")
        .iter()
        .filter_map(|c| c["decision"].as_str())
        .collect();
    assert_eq!(concluded, ["continue", "done"], "{map}");
    assert!(
        map["nodes"].as_array().is_some_and(|n| n.len() >= 3),
        "{map}"
    );
}

#[test]
fn a_graph_wide_exploration_surveys_and_attaches_anywhere() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    ok(repo, &["goal", "add", "Say hello"]);
    ok(repo, &["run", "explorer", "--exit-when-idle"]);
    ok(repo, &["run", "planner", "--exit-when-idle"]);
    ok(repo, &["run", "verifier", "--exit-when-idle"]);
    assert_eq!(node(repo, "3")["node"]["status"], "completed");

    let declaration = std::fs::read_to_string(repo.join("expl.toml")).expect("read");
    let explorer = declaration
        .split("\n[consumer.")
        .find(|section| section.starts_with("explorer]"))
        .expect("explorer section")
        .to_owned();
    let surveyor = explorer.replacen("explorer]", "surveyor]", 1);
    std::fs::write(
        repo.join("expl.toml"),
        format!(
            "{declaration}\n[consumer.{}\nscope = \"graph\"\n",
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
    // Planned, never verified (no verifier runs here).
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
    wait_until(repo, "an exploration ahead of the open step", || {
        ahead().is_some()
    });
    ok(repo, &["down", "--time", &down_time()]);
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

/// A completed step for the evaluator to judge, then the evaluator's
/// reaction to it, evaluating with `marker` set when there is one.
/// Returns the step, inspected.
fn evaluate_a_completed_step(sandbox: &Sandbox, marker: Option<&str>) -> serde_json::Value {
    let repo = &sandbox.repo;
    ok(repo, &["goal", "add", "A file greets"]);
    let step = ok(repo, &["node", "add", "Write a file", "--priority", "1"]);
    ok(repo, &["run", "verifier", "--exit-when-idle"]);
    assert_eq!(node(repo, &step)["node"]["status"], "completed");
    if let Some(marker) = marker {
        sandbox.marker(marker, true);
    }
    ok(repo, &["run", "evaluator", "--exit-when-idle"]);
    node(repo, &step)
}

/// The evaluator's runs on `step`, each checked to be a failure reported as
/// a read-only violation naming `path`, in its journal and its log.
fn assert_reported_violations(step: &serde_json::Value, path: &str) {
    let runs: Vec<&serde_json::Value> = step["agent_runs"]
        .as_array()
        .expect("runs")
        .iter()
        .filter(|r| r["role"] == "evaluation")
        .collect();
    assert!(!runs.is_empty(), "{step}");
    for run in runs {
        assert_eq!(run["status"], "failed", "{run}");
        let error = run["error"].as_str().expect("error");
        assert!(
            error.contains("modified its read-only worktree") && error.contains(path),
            "{error}"
        );
        let log = std::fs::read_to_string(run["log_path"].as_str().expect("log")).expect("log");
        assert!(log.contains(error), "{log}");
    }
}

/// What the evaluator recorded: no verdicts on `step`, no `node.evaluated`,
/// and its scratch worktrees are gone.
fn assert_nothing_recorded(repo: &Path, step: &serde_json::Value) {
    assert_eq!(step["evaluations"], serde_json::json!([]), "{step}");
    let kinds: Vec<String> = json_lines(&ok(repo, &["event", "ls", "--format", "json"]))
        .iter()
        .map(|e| e["kind"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(!kinds.iter().any(|k| k == "node.evaluated"), "{kinds:?}");
    let worktrees: Vec<String> = std::fs::read_dir(repo.join(".expl/worktrees"))
        .expect("worktrees")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(worktrees, Vec::<String>::new());
}

#[test]
fn an_evaluator_writing_into_its_scratch_is_reported_and_records_no_verdicts() {
    let sandbox = sandbox();
    let step = evaluate_a_completed_step(&sandbox, Some("eval-writes"));
    assert_reported_violations(&step, "stray.txt");
    assert_nothing_recorded(&sandbox.repo, &step);
}

#[test]
fn an_evaluator_committing_in_its_scratch_is_reported() {
    let sandbox = sandbox();
    let step = evaluate_a_completed_step(&sandbox, Some("eval-commits"));
    // Reported by the file it added: the commit itself stayed in the
    // evaluator's clone.
    assert_reported_violations(&step, "sneaky.txt");
    assert_nothing_recorded(&sandbox.repo, &step);
    // The stray commit never reached a branch.
    let subjects = git(&sandbox.repo, &["log", "--branches", "--format=%s"]);
    assert!(!subjects.lines().any(|s| s == "sneaky"), "{subjects}");
}

#[test]
fn a_clean_evaluation_records_its_verdicts() {
    let sandbox = sandbox();
    let step = evaluate_a_completed_step(&sandbox, None);
    let runs = step["agent_runs"].as_array().expect("runs");
    let evaluation = runs
        .iter()
        .find(|r| r["role"] == "evaluation")
        .unwrap_or_else(|| panic!("no evaluation run: {runs:?}"));
    assert_eq!(evaluation["status"], "succeeded", "{evaluation}");
    assert_eq!(step["evaluations"][0]["score"], 90, "{step}");
    assert_eq!(step["evaluations"][0]["satisfied"], true, "{step}");
}

#[test]
fn a_partial_evaluation_records_nothing_until_a_complete_retry() {
    let sandbox = sandbox();
    let repo = &sandbox.repo;
    let first: i64 = ok(repo, &["goal", "add", "A file greets"])
        .parse()
        .expect("first goal ID");
    let second: i64 = ok(repo, &["goal", "add", "A file says goodbye"])
        .parse()
        .expect("second goal ID");
    let step_id = ok(repo, &["node", "add", "Write a file", "--priority", "1"]);
    ok(repo, &["run", "verifier", "--exit-when-idle"]);
    assert_eq!(node(repo, &step_id)["node"]["status"], "completed");

    let script = sandbox.dir.path().join("evaluation.sh");
    let write_reply = |evaluations: &[serde_json::Value]| {
        let reply = serde_json::json!({
            "conclusion": {
                "decision": "done",
                "hypothesis": "A greeting can be written",
                "finding": "The file contains a greeting",
                "consequence": "The greeting goal is satisfied",
            },
            "evaluations": evaluations,
        });
        std::fs::write(&script, format!("echo '{reply}'\n")).expect("evaluation script");
    };
    let greeting =
        serde_json::json!({"goal": first, "satisfied": true, "score": 100, "reason": "greeting"});
    let goodbye =
        serde_json::json!({"goal": second, "satisfied": false, "score": 0, "reason": "missing"});
    write_reply(std::slice::from_ref(&greeting));
    let attempt = expl(repo, &["run", "evaluator", "--once"]);
    let stderr = String::from_utf8_lossy(&attempt.stderr);
    assert!(attempt.status.success(), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "agent response is unusable: missing verdict for goal #{second}"
        )) && stderr.contains("will be delivered again"),
        "{stderr}"
    );
    let step = node(repo, &step_id);
    assert_nothing_recorded(repo, &step);
    assert_eq!(step["conclusions"], serde_json::json!([]), "{step}");
    assert_eq!(step["agent_runs"][0]["status"], "failed", "{step}");

    write_reply(&[goodbye, greeting]);
    ok(repo, &["run", "evaluator", "--exit-when-idle"]);
    let step = node(repo, &step_id);
    let verdicts: Vec<_> = step["evaluations"]
        .as_array()
        .expect("evaluations")
        .iter()
        .map(|verdict| (verdict["goal"].as_i64(), verdict["score"].as_i64()))
        .collect();
    assert_eq!(
        verdicts,
        [(Some(second), Some(0)), (Some(first), Some(100))]
    );
    assert_eq!(
        step["conclusions"].as_array().expect("conclusions").len(),
        1
    );
    let events = json_lines(&ok(
        repo,
        &[
            "event",
            "ls",
            "--kind",
            "node.evaluated",
            "--format",
            "json",
        ],
    ));
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["node"], step["node"]["id"]);
}
