//! The external-command seam.
//!
//! Every shell-out (`docker`, `kind`, `kubectl`, `helm`, `curl`, `bash`) goes
//! through [`CommandRunner`]. Production uses [`RealRunner`], which spawns the
//! process; tests use [`MockRunner`], which matches argument patterns, returns
//! scripted output, and records each invocation for later assertions.
//!
//! This is the single abstraction that makes the provisioning layer
//! (preflight / kind / clusters / clients / handoff) unit-testable without
//! touching Docker or a real cluster. Adapted from the migration-assistant
//! CLI's runner so the two crates share one proven I/O discipline.

use std::sync::Mutex;

/// The captured result of running one external command.
#[derive(Debug, Clone)]
pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    /// Whether the command exited 0.
    pub fn success(&self) -> bool {
        self.status == 0
    }

    /// `stdout` with the trailing newline trimmed — the common case when a
    /// command prints a single value (a cluster name, an image id, a version).
    pub fn trimmed_stdout(&self) -> &str {
        self.stdout.trim_end_matches('\n')
    }
}

/// Abstraction over running an external program with arguments.
///
/// Implementors must be `Send + Sync` so a single runner can be shared across
/// the provisioner and any background watchers.
pub trait CommandRunner: Send + Sync {
    /// Run `program` with `args`, capturing stdout/stderr and the exit status.
    ///
    /// Implementations do NOT treat a non-zero exit as an error of this method
    /// — they return [`Output`] with the real status, and callers decide.
    fn run(&self, program: &str, args: &[&str]) -> Output;

    /// Run `program` while streaming its stdout/stderr straight to the terminal
    /// (inherited), rather than capturing it. For long-running, user-facing
    /// commands whose output we don't parse — e.g. `terraform apply`, which can
    /// take minutes — so the operator sees live progress instead of a frozen
    /// line. The returned [`Output`] carries the real exit status but EMPTY
    /// stdout/stderr (it went to the terminal). The default impl falls back to
    /// the capturing [`run`](Self::run), so mocks/tests need no extra wiring.
    fn run_streaming(&self, program: &str, args: &[&str]) -> Output {
        self.run(program, args)
    }

    /// Whether `program` resolves on PATH — `command -v` / `optional_cmd`.
    fn has_command(&self, program: &str) -> bool;

    /// Convenience: run and return `true` iff the command exited 0.
    fn run_ok(&self, program: &str, args: &[&str]) -> bool {
        self.run(program, args).success()
    }
}

/// Production runner: spawns real processes.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealRunner;

impl CommandRunner for RealRunner {
    fn run(&self, program: &str, args: &[&str]) -> Output {
        match std::process::Command::new(program).args(args).output() {
            Ok(out) => Output {
                status: out.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            },
            Err(e) => Output {
                // 127 mirrors the shell "command not found" exit code.
                status: 127,
                stdout: String::new(),
                stderr: format!("failed to spawn {program}: {e}"),
            },
        }
    }

    fn run_streaming(&self, program: &str, args: &[&str]) -> Output {
        // Inherit the parent's stdio so the child's output streams live to the
        // terminal. We don't capture it — only the exit status matters here.
        match std::process::Command::new(program).args(args).status() {
            Ok(status) => Output {
                status: status.code().unwrap_or(-1),
                stdout: String::new(),
                stderr: String::new(),
            },
            Err(e) => Output {
                status: 127,
                stdout: String::new(),
                stderr: format!("failed to spawn {program}: {e}"),
            },
        }
    }

    fn has_command(&self, program: &str) -> bool {
        // `which`-free PATH scan so we don't depend on an external tool.
        let Ok(path) = std::env::var("PATH") else {
            return false;
        };
        std::env::split_paths(&path).any(|dir| {
            let candidate = dir.join(program);
            candidate.is_file() && is_executable(&candidate)
        })
    }
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &std::path::Path) -> bool {
    true
}

/// One scripted response in a [`MockRunner`]: when the joined `program + args`
/// string contains every substring in `match_all`, reply with this output.
/// `remaining` bounds how many times the stub may fire (`None` = unlimited);
/// once exhausted it is skipped, so later stubs / the default reply take over —
/// this models recovery paths (fail once, then succeed).
#[derive(Debug, Clone)]
struct Stub {
    program: String,
    match_all: Vec<String>,
    output: Output,
    remaining: Option<u32>,
}

/// A recorded invocation: the program and its joined arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub program: String,
    pub args: Vec<String>,
}

impl Call {
    /// The argument vector joined by spaces — convenient for `contains`
    /// assertions on a recorded invocation.
    pub fn joined(&self) -> String {
        self.args.join(" ")
    }
}

/// Test runner: matches argument patterns, returns scripted output, and records
/// every call. The default reply (no matching stub) is exit 0 with empty
/// output, so an un-stubbed command is a benign no-op.
#[derive(Default)]
pub struct MockRunner {
    stubs: Mutex<Vec<Stub>>,
    commands: Mutex<Vec<String>>,
    calls: Mutex<Vec<Call>>,
    default_status: Mutex<i32>,
}

impl MockRunner {
    /// A fresh mock with no stubs and no known commands.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `program` as present on PATH for [`CommandRunner::has_command`].
    pub fn with_command(self, program: &str) -> Self {
        self.commands.lock().unwrap().push(program.to_string());
        self
    }

    /// Add a stub: when an invocation of `program` has arguments whose joined
    /// form contains all of `match_all`, return `(status, stdout)`.
    ///
    /// Stubs are matched in registration order; the first match wins. Register
    /// the most specific patterns first.
    pub fn stub(self, program: &str, match_all: &[&str], status: i32, stdout: &str) -> Self {
        self.stubs.lock().unwrap().push(Stub {
            program: program.to_string(),
            match_all: match_all.iter().map(|s| s.to_string()).collect(),
            output: Output {
                status,
                stdout: stdout.to_string(),
                stderr: String::new(),
            },
            remaining: None,
        });
        self
    }

    /// Add a stub that returns on stderr instead of stdout — used to exercise
    /// error-classification paths.
    pub fn stub_stderr(self, program: &str, match_all: &[&str], status: i32, stderr: &str) -> Self {
        self.stubs.lock().unwrap().push(Stub {
            program: program.to_string(),
            match_all: match_all.iter().map(|s| s.to_string()).collect(),
            output: Output {
                status,
                stdout: String::new(),
                stderr: stderr.to_string(),
            },
            remaining: None,
        });
        self
    }

    /// Like [`stub_stderr`](Self::stub_stderr) but fires at most `times` before
    /// being skipped — so a subsequent matching stub or the default reply takes
    /// over. Models a recovery path (e.g. apply fails once, then succeeds).
    pub fn stub_stderr_once(
        self,
        program: &str,
        match_all: &[&str],
        status: i32,
        stderr: &str,
        times: u32,
    ) -> Self {
        self.stubs.lock().unwrap().push(Stub {
            program: program.to_string(),
            match_all: match_all.iter().map(|s| s.to_string()).collect(),
            output: Output {
                status,
                stdout: String::new(),
                stderr: stderr.to_string(),
            },
            remaining: Some(times),
        });
        self
    }

    /// Set the status returned when no stub matches (default 0).
    pub fn default_status(self, status: i32) -> Self {
        *self.default_status.lock().unwrap() = status;
        self
    }

    /// Every recorded call, in order.
    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    /// Recorded calls to a specific program, in order.
    pub fn calls_to(&self, program: &str) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|c| c.program == program)
            .collect()
    }

    /// Whether any recorded call's joined `program args` contains `needle`.
    pub fn any_call_contains(&self, needle: &str) -> bool {
        self.calls()
            .iter()
            .any(|c| format!("{} {}", c.program, c.joined()).contains(needle))
    }
}

impl CommandRunner for MockRunner {
    fn run(&self, program: &str, args: &[&str]) -> Output {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        self.calls.lock().unwrap().push(Call {
            program: program.to_string(),
            args: owned,
        });

        let joined = format!("{} {}", program, args.join(" "));
        let mut stubs = self.stubs.lock().unwrap();
        for stub in stubs.iter_mut() {
            if stub.program == program && stub.match_all.iter().all(|m| joined.contains(m.as_str()))
            {
                match stub.remaining {
                    // Exhausted bounded stub — skip it, let later stubs/default win.
                    Some(0) => continue,
                    Some(n) => stub.remaining = Some(n - 1),
                    None => {}
                }
                return stub.output.clone();
            }
        }
        Output {
            status: *self.default_status.lock().unwrap(),
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    fn has_command(&self, program: &str) -> bool {
        self.commands.lock().unwrap().iter().any(|c| c == program)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_returns_matching_stub() {
        let r = MockRunner::new().stub("docker", &["info"], 0, "Server Version: 29.4.2");
        let out = r.run("docker", &["info", "--format", "{{.ServerVersion}}"]);
        assert!(out.success());
        assert!(out.stdout.contains("29.4.2"));
    }

    #[test]
    fn mock_first_match_wins() {
        let r = MockRunner::new()
            .stub("kind", &["get", "clusters"], 0, "ma-demo-source")
            .stub("kind", &["get"], 0, "other");
        assert_eq!(
            r.run("kind", &["get", "clusters"]).trimmed_stdout(),
            "ma-demo-source"
        );
    }

    #[test]
    fn mock_records_calls_in_order() {
        let r = MockRunner::new();
        r.run("kind", &["create", "cluster"]);
        r.run("kubectl", &["apply", "-f", "src.yaml"]);
        let calls = r.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].program, "kind");
        assert_eq!(calls[1].joined(), "apply -f src.yaml");
        assert!(r.any_call_contains("kubectl apply -f src.yaml"));
    }

    #[test]
    fn mock_default_status_when_no_stub() {
        let r = MockRunner::new().default_status(1);
        assert_eq!(r.run("docker", &["whatever"]).status, 1);
    }

    #[test]
    fn run_streaming_default_delegates_to_run_and_records() {
        // The default run_streaming impl falls back to run, so mocks honor
        // stubs AND record the call (terraform init/apply assertions rely on it).
        let r = MockRunner::new().stub("terraform", &["apply"], 0, "Apply complete!");
        let out = r.run_streaming("terraform", &["-chdir=tf", "apply", "-auto-approve"]);
        assert!(out.success());
        assert!(r.any_call_contains("terraform -chdir=tf apply"));
    }

    #[test]
    fn mock_has_command() {
        let r = MockRunner::new().with_command("kind");
        assert!(r.has_command("kind"));
        assert!(!r.has_command("helm"));
    }

    #[test]
    fn stub_once_fires_then_falls_through() {
        // Fails once, then the default-0 reply takes over — the recovery model.
        let r =
            MockRunner::new().stub_stderr_once("kubectl", &["apply"], 1, "field is immutable", 1);
        let first = r.run("kubectl", &["apply", "-f", "x.yaml"]);
        assert_eq!(first.status, 1);
        assert!(first.stderr.contains("immutable"));
        let second = r.run("kubectl", &["apply", "-f", "x.yaml"]);
        assert_eq!(
            second.status, 0,
            "second apply should fall through to default"
        );
        assert!(second.stderr.is_empty());
    }

    #[test]
    fn calls_to_filters_by_program() {
        let r = MockRunner::new();
        r.run("docker", &["ps"]);
        r.run("kind", &["get", "clusters"]);
        r.run("docker", &["info"]);
        assert_eq!(r.calls_to("docker").len(), 2);
        assert_eq!(r.calls_to("kind").len(), 1);
    }
}
