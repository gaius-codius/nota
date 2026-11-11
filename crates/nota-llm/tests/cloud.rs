//! The cloud deciders against stand-ins: `claude -p` and `codex exec` as
//! `nota-fake-cli`, and an OpenAI-compatible server as a listener in the
//! test. Each gives a yes, a no, a malformed reply, a timeout and a
//! missing CLI or server, and no reason holds the question's words.

#![cfg(test)]
// Process ids are checked by signal, which is Linux here.
#![cfg(target_os = "linux")]

use std::error::Error;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use nota_core::decide::{Answer, Candidate, DecideError, Decider, Decisions, Place, Question};
use nota_core::{Clock, SystemClock};
use nota_llm::cli::{CLAUDE_SYSTEM, CODEX_TOOLS_OFF, Cli, CliConfig, CliDecider, CliError};
use nota_llm::endpoint::{Endpoint, EndpointConfig, EndpointDecider, EndpointError};
use nota_llm::prompt::English;
use nota_llm::reply::ReplyError;
use nota_llm::server::Launcher;
use rustix::process::{Pid, test_kill_process};

/// The stand-in CLI.
const FAKE: &str = env!("CARGO_BIN_EXE_nota-fake-cli");

/// The stand-in llama-server, whose `exec-tied` is nota's launch shim.
const SHIM: &str = env!("CARGO_BIN_EXE_nota-fake-llama-server");

/// How soon a timed-out or stopped decider comes back: far less than a
/// hang.
const PROMPTLY: Duration = Duration::from_secs(5); // check-bound

/// How long a hang test waits for an answer that never comes.
const HANG_LIMIT: Duration = Duration::from_secs(1);

/// The words of the question that must never be in a reason or the CLI's
/// arguments: both chunks, the heard words and the terms. Made up, and
/// unlike anything nota's own messages say.
const SECRETS: [&str; 6] = [
    "Quokkas were mentioned earlier.",
    "Start amylo diping, the quokka said.",
    "amylo diping",
    "amlodipine",
    "quokka said",
    "clonidine",
];

/// The question every test asks: two candidates, so the stand-ins answer
/// a yes and a no.
fn question() -> Question {
    let candidates = vec![
        Candidate::new("amylo diping".to_owned(), "amlodipine".to_owned(), None).unwrap(),
        Candidate::new("quokka said".to_owned(), "clonidine".to_owned(), None).unwrap(),
    ];
    Question::new(
        Some(SECRETS[0].to_owned()),
        SECRETS[1].to_owned(),
        candidates,
    )
    .unwrap()
}

/// The session clock.
fn clock() -> Arc<dyn Clock> {
    Arc::new(SystemClock::start().unwrap())
}

/// A `stop` that never says to.
fn go_on() -> bool {
    false
}

/// `decide`'s result, and how long it took on a clock started for it.
fn timed<T>(decide: impl FnOnce() -> T) -> (T, Duration) {
    let clock = clock();
    let result = decide();
    (result, clock.now().elapsed())
}

/// The answers in `decisions`, in candidate order.
fn answers(decisions: &Decisions) -> Vec<Answer> {
    decisions.iter().map(|(_, d)| d.answer).collect()
}

/// Everything `e` says: its text, its debug form, and its sources'.
fn everything_said(e: &DecideError) -> String {
    let mut said = format!("{e}\n{e:?}");
    let mut source = e.source();
    while let Some(cause) = source {
        let _ = write!(said, "\n{cause}\n{cause:?}");
        source = cause.source();
    }
    said
}

/// Panics if `said` holds any of the question's words.
fn assert_no_secrets(said: &str) {
    for secret in SECRETS {
        assert!(!said.contains(secret), "{secret:?} is in: {said}");
    }
}

/// The reason `e` wraps, as a `T`.
fn reason<T: Error + 'static>(e: &DecideError) -> &T {
    let (DecideError::Failed(e) | DecideError::Unavailable(e)) = e else {
        panic!("no reason: {e:?}")
    };
    e.downcast_ref::<T>()
        .unwrap_or_else(|| panic!("another reason: {e:?}"))
}

/// A test's folder, holding the CLI's work folder, removed when dropped.
struct Fixture {
    /// The folder.
    dir: PathBuf,
}

impl Fixture {
    /// A fresh folder for the test `name`, whose CLI follows `plan`.
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding, not a recording"
    )]
    fn new(name: &str, plan: &str) -> Self {
        use std::os::unix::fs::DirBuilderExt;

        let dir = std::env::temp_dir().join(format!("nota-cloud-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let fixture = Self { dir };
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(fixture.work())
            .unwrap();
        std::fs::write(fixture.work().join("plan"), plan).unwrap();
        fixture
    }

    /// Lets every user open the work folder.
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding, not a recording"
    )]
    fn open_to_all(&self) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(self.work(), std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The CLI's work folder, where the stand-in reads its plan.
    fn work(&self) -> PathBuf {
        self.dir.join("work")
    }

    /// A config for the stand-in as `cli`, with a time limit no answer
    /// comes near on a busy machine.
    fn config(&self, cli: Cli) -> CliConfig {
        let mut config = CliConfig::new(cli, self.work());
        config.program = FAKE.into();
        config.model = Some("a-model".to_owned());
        config.timeout = Duration::from_secs(30);
        config
    }

    /// A decider on the stand-in as `cli`.
    fn decider(&self, cli: Cli) -> CliDecider {
        CliDecider::new(self.config(cli), Box::new(English::new(None)), clock())
    }

    /// A decider on the stand-in as `cli` that gives up after a second.
    fn impatient(&self, cli: Cli) -> CliDecider {
        let mut config = self.config(cli);
        config.timeout = HANG_LIMIT;
        CliDecider::new(config, Box::new(English::new(None)), clock())
    }

    /// What the stand-in wrote to `name` in its work folder.
    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.work().join(name)).unwrap_or_default()
    }

    /// Whether the stand-in's process has gone.
    fn gone(&self) -> bool {
        let pid: i32 = self.read("pid").parse().unwrap();
        test_kill_process(Pid::from_raw(pid).unwrap()).is_err()
    }
}

impl Drop for Fixture {
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding, not a recording"
    )]
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Both CLIs.
const CLIS: [Cli; 2] = [Cli::Claude, Cli::Codex];

/// Each CLI's answer is read as a yes for the first candidate and a no for
/// the second.
#[test]
fn a_cli_gives_a_yes_and_a_no() {
    for cli in CLIS {
        let fixture = Fixture::new(&format!("answer-{cli}"), "answer");
        let decisions = fixture.decider(cli).decide(&question(), &go_on).unwrap();
        assert_eq!(answers(&decisions), [Answer::Yes, Answer::No], "{cli}");
    }
}

/// The question goes in on stdin, in nota's words, and none of its words
/// is in the arguments, which anyone on the machine can read.
#[test]
fn a_cli_is_given_the_question_on_stdin_only() {
    let english = English::new(None);
    for cli in CLIS {
        let fixture = Fixture::new(&format!("stdin-{cli}"), "answer");
        fixture.decider(cli).decide(&question(), &go_on).unwrap();
        assert_eq!(
            fixture.read("stdin"),
            nota_llm::request::one_message(&english, &question()),
            "{cli}"
        );
        assert_no_secrets(&fixture.read("args"));
    }
}

/// Claude is told to use no tools, keep no session and load none of the
/// user's settings, with nota's own system prompt and the model asked for.
#[test]
fn claude_runs_without_tools_or_settings() {
    let fixture = Fixture::new("claude-args", "answer");
    fixture
        .decider(Cli::Claude)
        .decide(&question(), &go_on)
        .unwrap();
    let args = fixture.read("args");
    let args: Vec<&str> = args.split('\n').collect();
    let schema = args.get(4).copied().unwrap_or_default();
    assert!(schema.contains(r#""maxItems":2"#), "{schema}");
    let mut without_schema = args.clone();
    without_schema.remove(4);
    assert_eq!(
        without_schema,
        [
            "-p",
            "--output-format",
            "json",
            "--json-schema",
            "--tools",
            "",
            "--no-session-persistence",
            "--setting-sources",
            "",
            "--strict-mcp-config",
            "--disable-slash-commands",
            "--system-prompt",
            CLAUDE_SYSTEM,
            "--model",
            "a-model",
        ]
    );
}

/// Codex ignores the user's config, keeps no session, may only read, has
/// no tools, and is held to a schema file that's removed once it has
/// answered.
#[test]
fn codex_runs_read_only_without_tools_or_the_users_config() {
    let fixture = Fixture::new("codex-args", "answer");
    fixture
        .decider(Cli::Codex)
        .decide(&question(), &go_on)
        .unwrap();
    let args = fixture.read("args");
    let mut args: Vec<&str> = args.split('\n').collect();
    let at = args.iter().position(|a| *a == "--output-schema").unwrap() + 1;
    let schema = args.remove(at);
    assert!(
        Path::new(schema).starts_with(fixture.work()),
        "{schema} {args:?}"
    );
    assert!(!Path::new(schema).exists(), "{schema} was left");
    let mut expected = vec![
        "exec",
        "--ignore-user-config",
        "--ephemeral",
        "--skip-git-repo-check",
        "--sandbox",
        "read-only",
        "--color",
        "never",
        "-c",
        "model_reasoning_effort=\"low\"",
        "-c",
        "web_search=\"disabled\"",
    ];
    for tool in CODEX_TOOLS_OFF {
        expected.extend(["--disable", tool]);
    }
    expected.extend(["--output-schema", "--model", "a-model", "-"]);
    assert_eq!(args, expected);
}

/// A malformed answer is a failed decision, with no word of the question
/// in its reason, though the answer quoted it.
#[test]
fn a_malformed_cli_answer_is_a_failed_decision() {
    for cli in CLIS {
        let fixture = Fixture::new(&format!("malformed-{cli}"), "malformed");
        let e = fixture
            .decider(cli)
            .decide(&question(), &go_on)
            .unwrap_err();
        assert!(matches!(e, DecideError::Failed(_)), "{cli}: {e:?}");
        assert!(
            matches!(reason(&e), CliError::Unreadable(c, ReplyError::NotJson(_)) if *c == cli),
            "{e:?}"
        );
        assert_no_secrets(&everything_said(&e));
    }
}

/// An answer longer than nota reads is a failed decision.
#[test]
fn a_huge_cli_answer_is_a_failed_decision() {
    let fixture = Fixture::new("huge", "huge");
    let e = fixture
        .decider(Cli::Codex)
        .decide(&question(), &go_on)
        .unwrap_err();
    assert!(matches!(reason(&e), CliError::TooLong(Cli::Codex)), "{e:?}");
    assert!(matches!(e, DecideError::Failed(_)), "{e:?}");
}

/// A CLI that hasn't answered in time is killed, and the decider is
/// unavailable, without the question it echoed.
#[test]
fn a_cli_that_hangs_is_killed_at_its_time_limit() {
    for cli in CLIS {
        let fixture = Fixture::new(&format!("hang-{cli}"), "hang");
        let mut decider = fixture.impatient(cli);
        let (result, took) = timed(|| decider.decide(&question(), &go_on));
        let e = result.unwrap_err();
        assert!(took < PROMPTLY, "took {took:?}");
        assert!(
            matches!(reason(&e), CliError::NoAnswer(c, limit) if *c == cli && *limit == HANG_LIMIT),
            "{e:?}"
        );
        assert!(matches!(e, DecideError::Unavailable(_)), "{e:?}");
        assert!(fixture.gone(), "{cli} still runs");
        assert_no_secrets(&everything_said(&e));
    }
}

/// A stop while the CLI works kills it, and the question comes back
/// stopped.
#[test]
fn a_stop_kills_the_cli() {
    let fixture = Fixture::new("stop", "hang");
    let mut decider = fixture.decider(Cli::Claude);
    let stop = Arc::new(AtomicBool::new(false));
    let setter = Arc::clone(&stop);
    // Long after the stand-in has started, well before its time limit.
    let pid = fixture.work().join("pid");
    let waiter = thread::spawn(move || {
        while !pid.exists() {
            thread::yield_now();
        }
        setter.store(true, Ordering::SeqCst);
    });
    let e = decider
        .decide(&question(), &|| stop.load(Ordering::SeqCst))
        .unwrap_err();
    waiter.join().unwrap();
    assert!(matches!(e, DecideError::Stopped), "{e:?}");
    assert!(fixture.gone(), "the CLI still runs");
}

/// A CLI that isn't installed makes the decider unavailable, naming the
/// CLI and where it looked.
#[test]
fn a_missing_cli_is_named() {
    for cli in CLIS {
        let fixture = Fixture::new(&format!("missing-{cli}"), "answer");
        let mut config = CliConfig::new(cli, fixture.work());
        config.program = "nota-no-such-cli".into();
        let mut decider = CliDecider::new(config, Box::new(English::new(None)), clock());
        let e = decider.decide(&question(), &go_on).unwrap_err();
        assert!(matches!(e, DecideError::Unavailable(_)), "{e:?}");
        assert_eq!(
            reason::<CliError>(&e).to_string(),
            format!("{cli} isn't installed: there's no program nota-no-such-cli")
        );
    }
}

/// Claude's error result (a usage limit) makes the decider unavailable,
/// with its status and none of the question it quoted.
#[test]
fn a_claude_error_gives_its_status_alone() {
    let fixture = Fixture::new("claude-error", "error");
    let e = fixture
        .decider(Cli::Claude)
        .decide(&question(), &go_on)
        .unwrap_err();
    assert!(matches!(e, DecideError::Unavailable(_)), "{e:?}");
    assert_eq!(
        reason::<CliError>(&e).to_string(),
        "claude answered with an error (status 429)"
    );
    assert_no_secrets(&everything_said(&e));
}

/// A CLI that fails gives its exit status alone, though it printed the
/// question on stdout and stderr.
#[test]
fn a_failed_cli_gives_its_exit_status_alone() {
    for (plan, code) in [("fail", 3), ("error", 1)] {
        let fixture = Fixture::new(&format!("fail-{plan}"), plan);
        let e = fixture
            .decider(Cli::Codex)
            .decide(&question(), &go_on)
            .unwrap_err();
        assert!(matches!(e, DecideError::Unavailable(_)), "{e:?}");
        assert_eq!(
            reason::<CliError>(&e).to_string(),
            format!("codex failed (exit status: {code})")
        );
        assert_no_secrets(&everything_said(&e));
    }
    let fixture = Fixture::new("fail-claude", "fail");
    let e = fixture
        .decider(Cli::Claude)
        .decide(&question(), &go_on)
        .unwrap_err();
    assert_eq!(
        reason::<CliError>(&e).to_string(),
        "claude failed (exit status: 3)"
    );
    assert_no_secrets(&everything_said(&e));
}

/// Every CLI decider says it runs in the cloud, and its debug form holds
/// nothing of a question it asked.
#[test]
fn a_cli_decider_runs_in_the_cloud() {
    for cli in CLIS {
        let fixture = Fixture::new(&format!("place-{cli}"), "answer");
        let mut decider = fixture.decider(cli);
        decider.decide(&question(), &go_on).unwrap();
        assert_eq!(decider.place(), Place::Cloud, "{cli}");
        assert_no_secrets(&format!("{decider:?}"));
    }
}

/// What a stand-in server does with the one request it takes.
enum Serve {
    /// Answers with this whole response.
    With(String),
    /// Reads the request and never answers.
    Hang,
}

/// A stand-in server on this machine: the endpoint for it, and the
/// request it took, once it has.
fn serve(serve: Serve) -> (Endpoint, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_request(&mut stream);
        let _ = tx.send(request);
        match serve {
            Serve::With(response) => {
                let _ = stream.write_all(response.as_bytes());
            }
            // Held open until the test's process ends.
            Serve::Hang => HELD.lock().unwrap().push(stream),
        }
    });
    let endpoint = Endpoint::parse(&format!("http://127.0.0.1:{port}/v1")).unwrap();
    (endpoint, rx)
}

/// Connections a hanging stand-in keeps open.
static HELD: Mutex<Vec<TcpStream>> = Mutex::new(Vec::new());

/// One request read from `stream`: its head, and the body its length
/// gives.
fn read_request(stream: &mut TcpStream) -> String {
    let mut read = Vec::new();
    let mut buf = [0_u8; 4_096];
    loop {
        let n = stream.read(&mut buf).unwrap();
        read.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&read).into_owned();
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let length: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length: "))
                .map_or(0, |l| l.parse().unwrap());
            if body.len() >= length || n == 0 {
                return text;
            }
        }
    }
}

/// An HTTP response with `status` and `body`.
fn response(status: u16, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// A chat completion whose message is `content`.
fn completion(content: &str) -> String {
    serde_json::json!({
        "choices": [{"finish_reason": "stop", "message": {"content": content}}]
    })
    .to_string()
}

/// A decider on `endpoint`, giving up after `limit`.
fn endpoint_within(endpoint: Endpoint, limit: Duration) -> EndpointDecider {
    let mut config = EndpointConfig::new(endpoint);
    config.model = Some("qwen3".to_owned());
    config.timeout = limit;
    EndpointDecider::new(config, Box::new(English::new(None)), clock())
}

/// A decider on `endpoint`, with a time limit no answer comes near on a
/// busy machine.
fn endpoint_decider(endpoint: Endpoint) -> EndpointDecider {
    endpoint_within(endpoint, Duration::from_secs(30))
}

/// A server's answer is read as a yes and a no, from a request posted to
/// the API's path with the host and the model named.
#[test]
fn a_server_gives_a_yes_and_a_no() {
    let answer = r#"{"decisions":[{"id":1,"apply":true},{"id":2,"apply":false}]}"#;
    let (endpoint, request) = serve(Serve::With(response(200, &completion(answer))));
    let port = endpoint.to_string();
    let decisions = endpoint_decider(endpoint)
        .decide(&question(), &go_on)
        .unwrap();
    assert_eq!(answers(&decisions), [Answer::Yes, Answer::No]);
    let request = request.recv().unwrap();
    let host = port.trim_start_matches("http://").trim_end_matches("/v1");
    assert!(
        request.starts_with(&format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: {host}\r\n"
        )),
        "{request}"
    );
    assert!(request.contains(r#""model":"qwen3""#), "{request}");
}

/// A malformed answer, or a refusal, is a failed decision with no word of
/// the question in its reason, though the server quoted it.
#[test]
fn a_malformed_server_answer_is_a_failed_decision() {
    let chunk = serde_json::to_string(SECRETS[1]).unwrap();
    let twice = format!(r#"{{"decisions":[],{chunk}:1,{chunk}:2}}"#);
    let cases = [
        response(200, &format!("{} not json", SECRETS[1])),
        response(200, &completion(&twice)),
        response(400, &format!("bad request: {}", SECRETS[1])),
    ];
    for case in cases {
        let (endpoint, _request) = serve(Serve::With(case.clone()));
        let e = endpoint_decider(endpoint)
            .decide(&question(), &go_on)
            .unwrap_err();
        assert!(matches!(e, DecideError::Failed(_)), "{case}: {e:?}");
        assert_no_secrets(&everything_said(&e));
    }
}

/// A server error makes the decider unavailable, with the status alone.
#[test]
fn a_server_error_gives_its_status_alone() {
    let (endpoint, _request) = serve(Serve::With(response(503, SECRETS[1])));
    let e = endpoint_decider(endpoint)
        .decide(&question(), &go_on)
        .unwrap_err();
    assert!(matches!(e, DecideError::Unavailable(_)), "{e:?}");
    assert_eq!(
        reason::<EndpointError>(&e).to_string(),
        "the server failed (status 503)"
    );
}

/// A server that doesn't answer in time makes the decider unavailable.
#[test]
fn a_server_that_hangs_times_out() {
    let (endpoint, _request) = serve(Serve::Hang);
    let mut decider = endpoint_within(endpoint, Duration::from_millis(500));
    let (result, took) = timed(|| decider.decide(&question(), &go_on));
    let e = result.unwrap_err();
    assert!(took < PROMPTLY, "took {took:?}");
    assert!(matches!(e, DecideError::Unavailable(_)), "{e:?}");
    assert_eq!(
        reason::<EndpointError>(&e).to_string(),
        "the server gave no answer within 500ms"
    );
}

/// A stop while the server works comes back stopped.
#[test]
fn a_stop_ends_a_question_to_a_server() {
    let (endpoint, request) = serve(Serve::Hang);
    let stop = Arc::new(AtomicBool::new(false));
    let setter = Arc::clone(&stop);
    // Once the server has the question, so the stop comes mid-answer.
    let waiter = thread::spawn(move || {
        let _ = request.recv();
        setter.store(true, Ordering::SeqCst);
    });
    let mut decider = endpoint_decider(endpoint);
    let e = decider
        .decide(&question(), &|| stop.load(Ordering::SeqCst))
        .unwrap_err();
    waiter.join().unwrap();
    assert!(matches!(e, DecideError::Stopped), "{e:?}");
}

/// A server that isn't there makes the decider unavailable, naming where
/// it looked.
#[test]
fn a_missing_server_is_named() {
    // A port that was free a moment ago, and has nothing listening now.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let endpoint = Endpoint::parse(&format!("http://127.0.0.1:{port}/v1")).unwrap();
    let e = endpoint_decider(endpoint)
        .decide(&question(), &go_on)
        .unwrap_err();
    assert!(matches!(e, DecideError::Unavailable(_)), "{e:?}");
    let EndpointError::Unreachable(at, cause) = reason::<EndpointError>(&e) else {
        panic!("{e:?}")
    };
    assert_eq!(at, &format!("http://127.0.0.1:{port}/v1"));
    assert_eq!(cause.kind(), std::io::ErrorKind::ConnectionRefused);
}

/// A server on another machine is the cloud; one on this machine isn't.
#[test]
fn a_server_elsewhere_runs_in_the_cloud() {
    for url in [
        "http://192.168.1.20:8080/v1",
        "http://gpu-box.local/v1",
        "http://example.com:11434/v1",
        "http://[2001:db8::1]:8080",
        "http://10.0.0.1",
    ] {
        let decider = endpoint_decider(Endpoint::parse(url).unwrap());
        assert_eq!(decider.place(), Place::Cloud, "{url}");
    }
    for url in [
        "http://localhost:11434/v1",
        "http://LOCALHOST/v1",
        "http://127.0.0.1:8080",
        "http://127.5.6.7/v1",
        "http://[::1]:8080/v1",
    ] {
        let decider = endpoint_decider(Endpoint::parse(url).unwrap());
        assert_eq!(decider.place(), Place::Local, "{url}");
    }
}

/// A server that sends its whole answer in chunks, as Go's servers do once
/// it's over a couple of KiB, is read like one that gives its length.
#[test]
fn a_server_answer_in_chunks_is_read() {
    let answer = r#"{"decisions":[{"id":1,"apply":true},{"id":2,"apply":false}]}"#;
    let body = completion(answer);
    let (first, second) = body.split_at(body.len() / 2);
    let chunked = format!(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
         {:x}\r\n{first}\r\n{:x}\r\n{second}\r\n0\r\n\r\n",
        first.len(),
        second.len()
    );
    let (endpoint, _request) = serve(Serve::With(chunked));
    let decisions = endpoint_decider(endpoint)
        .decide(&question(), &go_on)
        .unwrap();
    assert_eq!(answers(&decisions), [Answer::Yes, Answer::No]);
}

/// A refusal the server would give every question (it wants a key, or
/// has no such path or model) makes the decider unavailable, so no more
/// of the transcript goes to it.
#[test]
fn a_server_that_refuses_every_question_stops_the_job() {
    for status in [401, 403, 404, 405, 429] {
        let (endpoint, _request) = serve(Serve::With(response(status, SECRETS[1])));
        let e = endpoint_decider(endpoint)
            .decide(&question(), &go_on)
            .unwrap_err();
        assert!(matches!(e, DecideError::Unavailable(_)), "{status}: {e:?}");
        assert!(
            matches!(reason(&e), EndpointError::RefusesAll(s) if *s == status),
            "{e:?}"
        );
        assert_no_secrets(&everything_said(&e));
    }
}

/// An endpoint decider's debug form holds nothing of a question it
/// asked.
#[test]
fn an_endpoint_deciders_debug_form_holds_no_question() {
    let answer = r#"{"decisions":[{"id":1,"apply":true},{"id":2,"apply":false}]}"#;
    let (endpoint, _request) = serve(Serve::With(response(200, &completion(answer))));
    let mut decider = endpoint_decider(endpoint);
    decider.decide(&question(), &go_on).unwrap();
    assert_no_secrets(&format!("{decider:?}"));
}

/// Through a launcher, the CLI is started as its last argument but one
/// (the shim, then the CLI and its own arguments), and still answers.
#[test]
fn a_cli_can_be_started_through_a_launcher() {
    let fixture = Fixture::new("launcher", "answer");
    let mut config = fixture.config(Cli::Codex);
    config.launcher = Some(Launcher {
        program: SHIM.into(),
        args: vec!["exec-tied".into()],
    });
    let mut decider = CliDecider::new(config, Box::new(English::new(None)), clock());
    let decisions = decider.decide(&question(), &go_on).unwrap();
    assert_eq!(answers(&decisions), [Answer::Yes, Answer::No]);
    // The stand-in saw its own arguments, so the shim ran it by exec.
    assert!(
        fixture.read("args").starts_with("exec\n"),
        "{}",
        fixture.read("args")
    );
}

/// Through a launcher, a CLI named by a path that isn't there is named
/// as missing, rather than read as a CLI that fails.
#[test]
fn a_missing_cli_behind_a_launcher_is_named() {
    let fixture = Fixture::new("launcher-missing", "answer");
    let mut config = fixture.config(Cli::Claude);
    config.program = fixture.dir.join("no-such-claude");
    config.launcher = Some(Launcher {
        program: SHIM.into(),
        args: vec!["exec-tied".into()],
    });
    let mut decider = CliDecider::new(config, Box::new(English::new(None)), clock());
    let e = decider.decide(&question(), &go_on).unwrap_err();
    assert!(
        matches!(reason(&e), CliError::Missing(Cli::Claude, p) if p.ends_with("no-such-claude")),
        "{e:?}"
    );
}

/// A work folder other users can open is refused, naming it.
#[test]
fn an_open_work_folder_is_refused() {
    let fixture = Fixture::new("open-work", "answer");
    fixture.open_to_all();
    let e = fixture
        .decider(Cli::Claude)
        .decide(&question(), &go_on)
        .unwrap_err();
    assert!(matches!(e, DecideError::Unavailable(_)), "{e:?}");
    assert!(
        matches!(reason(&e), CliError::WorkDirOpen(dir) if *dir == fixture.work()),
        "{e:?}"
    );
}
