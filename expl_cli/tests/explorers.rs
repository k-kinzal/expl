//! A daemon hosting explorers that are not expl projects: whatever they
//! register is what runs. Their consumers here are shell scripts, so
//! nothing of the general-purpose explorer is involved.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

use expl_core::testing::scaled;

/// A daemon directory, the directory commands are typed in, and the
/// daemons serving it. Dropping it stops them; their workers stop with
/// them.
struct Host {
    dir: tempfile::TempDir,
    daemons: Vec<Child>,
}

impl Drop for Host {
    fn drop(&mut self) {
        for daemon in &mut self.daemons {
            drop(daemon.kill());
            drop(daemon.wait());
        }
    }
}

impl Host {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
            daemons: Vec::new(),
        }
    }

    fn cluster(&self) -> PathBuf {
        self.dir.path().join("cluster")
    }

    /// `expl`, typed in the host's directory (no project around it),
    /// against the daemon through `member`.
    fn command(&self, member: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_expl"));
        command
            .env("OTEL_SDK_DISABLED", "true")
            .current_dir(self.dir.path())
            .arg("--cluster")
            .arg(self.cluster())
            .args(["--member", member]);
        command
    }

    fn expl(&self, member: &str, args: &[&str]) -> Output {
        self.command(member).args(args).output().expect("expl")
    }

    fn ok(&self, member: &str, args: &[&str]) -> String {
        let out = self.expl(member, args);
        assert!(
            out.status.success(),
            "expl {args:?} failed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Start a daemon as `member`, and wait until it serves as `role`.
    fn daemon(&mut self, member: &str, role: &str) {
        let child = self
            .command(member)
            .arg("daemon")
            .env("EXPL_TEST_MEMBER", member)
            .spawn()
            .expect("daemon");
        self.daemons.push(child);
        self.wait_until(&format!("{member} to serve as {role}"), |host| {
            let out = host.expl(member, &["cluster", "status"]);
            out.status.success() && json(&String::from_utf8_lossy(&out.stdout))["role"] == role
        });
    }

    /// Write an explorer's file in a directory of its own; `@DIR@` in
    /// `body` is that directory. Returns the file, relative to where
    /// commands are typed.
    fn explorer(&self, name: &str, body: &str) -> String {
        let directory = self.dir.path().join(name);
        std::fs::create_dir_all(&directory).expect("explorer directory");
        let body = body.replace("@DIR@", &directory.display().to_string());
        std::fs::write(
            directory.join("explorer.toml"),
            format!("name = \"{name}\"\ndatabase = \"state/explorer.db\"\n{body}"),
        )
        .expect("explorer file");
        format!("{name}/explorer.toml")
    }

    /// The status and attempts of `consumer`'s delivery of `event`.
    fn delivery(&self, explorer: &str, consumer: &str, event: &str) -> Option<(String, i64)> {
        let database = self.dir.path().join(explorer).join("state/explorer.db");
        let conn = rusqlite::Connection::open(database).ok()?;
        conn.busy_timeout(scaled(Duration::from_secs(10))).ok()?;
        conn.query_row(
            "SELECT status, attempts FROM deliveries WHERE agent = ?1 AND event_id = ?2",
            rusqlite::params![consumer, event],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()
    }

    fn lines(&self, explorer: &str, file: &str) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join(explorer).join(file))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn wait_until(&self, what: &str, mut condition: impl FnMut(&Self) -> bool) {
        let timeout = scaled(Duration::from_secs(60));
        let start = Instant::now();
        while !condition(self) {
            assert!(
                start.elapsed() < timeout,
                "timed out waiting for {what}\n{}",
                self.logs()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Every worker log under the host's directory, for a failure message.
    fn logs(&self) -> String {
        let mut logs = String::new();
        for explorer in std::fs::read_dir(self.dir.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            let proc = explorer.path().join("state/proc");
            for log in std::fs::read_dir(proc).into_iter().flatten().flatten() {
                logs.push_str(&std::fs::read_to_string(log.path()).unwrap_or_default());
            }
        }
        logs
    }
}

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

fn down_time() -> String {
    scaled(Duration::from_secs(5))
        .as_secs_f64()
        .ceil()
        .to_string()
}

fn status(host: &Host, explorer: &str, consumer: &str, event: &str) -> Option<String> {
    host.delivery(explorer, consumer, event)
        .map(|(status, _)| status)
}

#[test]
fn a_daemon_runs_what_an_explorer_registered_for_each_event() {
    let mut host = Host::new();
    assert!(!Path::new(&host.dir.path().join(".git")).exists());
    host.daemon("local", "leader");

    let file = host.explorer(
        "counter",
        r#"
[consumer.reactor]
on = ["thing.added"]
run = ["sh", "-c", 'echo "$EXPL_CONSUMER $EXPL_EVENT $EXPL_SUBJECT $EXPL_REACTION ${EXPL_LEASE:+leased}" >> @DIR@/calls']
replicas = 1

[consumer.failer]
on = ["thing.broken"]
run = ["sh", "-c", 'echo "$EXPL_REACTION $EXPL_ATTEMPTS" >> @DIR@/failures; exit 1']
replicas = 1
"#,
    );
    // The file is named as typed here; the leader finds it all the same.
    assert_eq!(host.ok("local", &["explorer", "add", &file]), "counter");
    assert_eq!(host.ok("local", &["explorer", "ls", "-q"]), "counter");
    let status_now = json(&host.ok("local", &["cluster", "status"]));
    assert_eq!(status_now["explorers"], serde_json::json!(["counter"]));

    let mut workers: Vec<String> = host
        .ok("local", &["explorer", "up", "counter"])
        .lines()
        .map(str::to_owned)
        .collect();
    workers.sort();
    assert_eq!(workers, ["failer-1", "reactor-2"]);
    // Idempotent: nothing is missing the second time.
    assert_eq!(host.ok("local", &["explorer", "up", "counter"]), "");

    let event = host.ok(
        "local",
        &[
            "explorer",
            "publish",
            "counter",
            "thing.added",
            "--subject",
            "7",
            "--payload",
            r#"{"n": 1}"#,
        ],
    );
    host.wait_until("the reaction to be acknowledged", |host| {
        status(host, "counter", "reactor", &event).as_deref() == Some("done")
    });
    assert_eq!(
        host.lines("counter", "calls"),
        ["reactor thing.added 7 react leased"]
    );
    // The other consumer does not subscribe to it.
    assert_eq!(host.delivery("counter", "failer", &event), None);

    // A command that keeps failing is run for each attempt, asked to
    // clean up after the last, and its event is given up.
    let broken = host.ok("local", &["explorer", "publish", "counter", "thing.broken"]);
    host.wait_until("the failing event to be given up", |host| {
        status(host, "counter", "failer", &broken).as_deref() == Some("failed")
    });
    assert_eq!(
        host.lines("counter", "failures"),
        ["react 1", "react 2", "react 3", "abandon 3"]
    );
    assert_eq!(
        host.delivery("counter", "failer", &broken),
        Some(("failed".to_owned(), 3))
    );

    let listed = host.ok("local", &["explorer", "ps", "counter"]);
    assert!(
        listed.contains("reactor-2") && listed.contains("failer-1"),
        "{listed}"
    );
    // An explorer with workers stays registered.
    let refused = host.expl("local", &["explorer", "rm", "counter"]);
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("still has 2 worker(s) running"),
        "{refused:?}"
    );
    host.ok(
        "local",
        &["explorer", "down", "counter", "--time", &down_time()],
    );
    assert_eq!(host.ok("local", &["explorer", "ps", "counter", "-q"]), "");
    assert_eq!(host.ok("local", &["explorer", "rm", "counter"]), "counter");
    assert_eq!(host.ok("local", &["explorer", "ls", "-q"]), "");
    let unknown = host.expl("local", &["explorer", "up", "counter"]);
    assert_eq!(unknown.status.code(), Some(1), "{unknown:?}");
}

#[test]
fn members_run_commands_placed_anywhere_and_explorers_share_one_daemon() {
    let mut host = Host::new();
    host.daemon("alpha", "leader");
    host.daemon("beta", "member");

    // Each command waits for the other, so both run at once and the
    // second is given to the member without one.
    let spread = host.explorer(
        "spread",
        r#"
[consumer.worker]
on = ["job.added"]
placement = "any"
replicas = 2
run = ["sh", "-c", 'echo "$EXPL_TEST_MEMBER" >> @DIR@/members; while [ $(wc -l < @DIR@/members) -lt 2 ]; do sleep 0.05; done']
"#,
    );
    let other = host.explorer(
        "other",
        r#"
pool = "small"

[consumer.noter]
on = ["note.added"]
run = ["sh", "-c", 'echo "$EXPL_TEST_MEMBER" >> @DIR@/notes']
replicas = 1
"#,
    );
    // Any member takes the operator's commands; the leader carries them out.
    host.ok("beta", &["explorer", "add", &spread]);
    host.ok("alpha", &["explorer", "add", &other]);
    assert_eq!(host.ok("beta", &["explorer", "ls", "-q"]), "other\nspread");
    let listed = host.ok("beta", &["explorer", "ls"]);
    assert!(listed.contains("small"), "{listed}");

    host.ok("beta", &["explorer", "up", "spread"]);
    host.ok("beta", &["explorer", "up", "other"]);
    let first = host.ok(
        "beta",
        &[
            "explorer",
            "publish",
            "spread",
            "job.added",
            "--subject",
            "1",
        ],
    );
    let second = host.ok(
        "beta",
        &[
            "explorer",
            "publish",
            "spread",
            "job.added",
            "--subject",
            "2",
        ],
    );
    let note = host.ok("beta", &["explorer", "publish", "other", "note.added"]);
    host.wait_until("both explorers to settle their events", |host| {
        [
            ("spread", "worker", &first),
            ("spread", "worker", &second),
            ("other", "noter", &note),
        ]
        .iter()
        .all(|(explorer, consumer, event)| {
            status(host, explorer, consumer, event).as_deref() == Some("done")
        })
    });
    let mut members = host.lines("spread", "members");
    members.sort();
    assert_eq!(members, ["alpha", "beta"]);
    // A command placed on the leader runs beside the explorer's database.
    assert_eq!(host.lines("other", "notes"), ["alpha"]);

    for explorer in ["spread", "other"] {
        host.ok(
            "beta",
            &["explorer", "down", explorer, "--time", &down_time()],
        );
    }
}
