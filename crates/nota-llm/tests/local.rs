//! The local decider against a stand-in llama-server
//! (`nota-fake-llama-server`): answers, restarts, giving up, and stopping,
//! with every server it starts accounted for.

#![cfg(test)]
// Signals by process id, and the kernel tie the shim test checks, are
// Linux here.
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use nota_core::decide::{Answer, Candidate, DecideError, Decider, Decisions, Question};
use nota_core::lifeline::RECORDER_PID_VAR;
use nota_core::{Clock, SystemClock};
use nota_llm::http::HttpError;
use nota_llm::local::{LocalDecider, Refused};
use nota_llm::prompt::English;
use nota_llm::reply::ReplyError;
use nota_llm::server::{Device, GpuName, Launcher, ServerConfig, ServerError};
use rustix::process::{Pid, Signal, kill_process, test_kill_process};

/// The stand-in server.
const FAKE: &str = env!("CARGO_BIN_EXE_nota-fake-llama-server");

/// How soon a decider that's given up, or been told to stop, comes back:
/// far less than a hang.
const PROMPTLY: Duration = Duration::from_secs(5); // check-bound

/// How soon after `stop` says to stop the server is gone.
const STOPPED_WITHIN: Duration = Duration::from_secs(1); // check-bound

/// The longest any one wait in these tests lasts before it fails with its
/// own message. Two in a row stay inside the ten seconds nextest gives each
/// of nota-llm's tests (`.config/nextest.toml`), so a wait that runs out
/// says what never happened rather than being cut off.
const WAIT: Duration = Duration::from_secs(4); // check-bound

/// A test's folder (the stand-in's `--model`, holding its plan and log) and
/// the socket folder under it, removed when dropped.
struct Fixture {
    /// The folder.
    dir: PathBuf,
}

impl Fixture {
    /// A fresh folder for the test `name`, whose servers follow `plan`.
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding, not a recording"
    )]
    fn new(name: &str, plan: &str) -> Self {
        // Short, under the temporary folder: a socket's path is limited
        // to 107 bytes.
        let dir = std::env::temp_dir().join(format!("nl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plan"), plan).unwrap();
        Self { dir }
    }

    /// The socket folder.
    fn sockets(&self) -> PathBuf {
        self.dir.join("s")
    }

    /// A config for the stand-in, with short waits.
    fn config(&self) -> ServerConfig {
        let gpu = Device::Gpu(GpuName::new("Vulkan0").unwrap());
        let mut config = ServerConfig::new(FAKE.into(), self.dir.clone(), gpu, self.sockets());
        config.start_timeout = WAIT;
        config.request_timeout = WAIT;
        config.initial_backoff = Duration::from_millis(10);
        config.max_backoff = Duration::from_millis(40);
        config.restarts = 2;
        config
    }

    /// A config that starts the stand-in through the launch shim, as nota
    /// starts the real server.
    fn tied_config(&self) -> ServerConfig {
        let mut config = self.config();
        config.launcher = Some(Launcher {
            program: FAKE.into(),
            args: vec!["exec-tied".into()],
        });
        config
    }

    /// A decider on `config`.
    fn decider(config: ServerConfig) -> LocalDecider {
        LocalDecider::new(config, Box::new(English::new(None)), clock())
    }

    /// Each server started so far: its process id and arguments.
    fn starts(&self) -> Vec<(u32, String)> {
        self.log("start ")
            .into_iter()
            .map(|line| {
                let (pid, args) = line.split_once(' ').unwrap_or((&line, ""));
                (pid.parse().unwrap(), args.to_owned())
            })
            .collect()
    }

    /// The process ids of the servers that took a question, one per
    /// question.
    fn requests(&self) -> Vec<u32> {
        self.log("request ")
            .iter()
            .map(|pid| pid.parse().unwrap())
            .collect()
    }

    /// The log's lines that start with `kind`, without it.
    fn log(&self, kind: &str) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.strip_prefix(kind).map(str::to_owned))
            .collect()
    }

    /// The socket files left in the socket folder.
    fn sockets_left(&self) -> Vec<PathBuf> {
        std::fs::read_dir(self.sockets())
            .map(|entries| entries.filter_map(|e| e.ok().map(|e| e.path())).collect())
            .unwrap_or_default()
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

/// The session clock.
fn clock() -> Arc<dyn Clock> {
    Arc::new(SystemClock::start().unwrap())
}

/// A question about `n` candidates.
fn question(n: usize) -> Question {
    let one = Candidate::new("amylo diping".to_owned(), "amlodipine".to_owned(), None).unwrap();
    Question::new(None, "Start amylo diping.".to_owned(), vec![one; n]).unwrap()
}

/// Never stop.
fn go_on() -> bool {
    false
}

/// Whether process `pid` still exists (a zombie counts).
fn exists(pid: u32) -> bool {
    let pid = Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
    test_kill_process(pid).is_ok()
}

/// Kills process `pid` with SIGKILL.
fn kill(pid: u32) {
    let pid = Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
    kill_process(pid, Signal::KILL).unwrap();
}

/// Waits up to [`WAIT`] for `done` to hold, looking every 10 ms.
fn until(what: &str, done: impl Fn() -> bool) {
    let clock = clock();
    let (_keep, ticks) = mpsc::channel::<()>();
    while !done() {
        assert!(clock.now().elapsed() < WAIT, "never: {what}");
        let _ = ticks.recv_timeout(Duration::from_millis(10));
    }
}

/// How long `work` takes, and what it gives.
fn timed<T>(work: impl FnOnce() -> T) -> (T, Duration) {
    let clock = clock();
    let result = work();
    (result, clock.now().elapsed())
}

/// The answers in `decisions`.
fn answers(decisions: &Decisions) -> Vec<Answer> {
    decisions.iter().map(|(_, d)| d.answer).collect()
}

/// The server's error that `err` gave up on, or reported.
fn server_error(err: &DecideError) -> &ServerError {
    let DecideError::Unavailable(e) = err else {
        panic!("not unavailable: {err:?}")
    };
    e.downcast_ref::<ServerError>().unwrap()
}

/// A running server answers each candidate, with how likely a yes was;
/// it's started once, on the device it was given, and is gone with its
/// socket once the decider is dropped.
#[test]
fn a_server_answers_and_goes_with_the_decider() {
    let fixture = Fixture::new("answers", "answer");
    let mut decider = Fixture::decider(fixture.config());
    let decisions = decider.decide(&question(3), &go_on).unwrap();
    assert_eq!(answers(&decisions), [Answer::Yes, Answer::No, Answer::Yes]);
    let yes: Vec<f64> = decisions
        .iter()
        .map(|(_, d)| d.yes.unwrap().get())
        .collect();
    let likely = (-0.1_f64).exp() / ((-0.1_f64).exp() + (-2.4_f64).exp());
    assert!(
        (yes[0] - likely).abs() < 1e-9 && (yes[1] - (1.0 - likely)).abs() < 1e-9,
        "{yes:?}"
    );

    let starts = fixture.starts();
    let [(pid, args)] = &starts[..] else {
        panic!("{starts:?}")
    };
    assert!(
        args.contains("--device Vulkan0 --n-gpu-layers 999"),
        "{args}"
    );
    assert_eq!(fixture.sockets_left().len(), 1);

    // Dropping the decider stops its server: the job is over.
    drop(decider);
    assert!(!exists(*pid), "server {pid} outlived its decider");
    assert_eq!(fixture.sockets_left(), Vec::<PathBuf>::new());
}

/// One server answers question after question: it isn't restarted.
#[test]
fn one_server_answers_every_question() {
    let fixture = Fixture::new("kept", "answer");
    let mut decider = Fixture::decider(fixture.config());
    for n in 1..=3 {
        assert_eq!(
            decider.decide(&question(n), &go_on).unwrap().iter().len(),
            n
        );
    }
    assert_eq!(fixture.starts().len(), 1);
    assert_eq!(fixture.requests().len(), 3);
}

/// A server killed while it holds a question is restarted, and the
/// question asked again of the new one, which answers it.
#[test]
fn a_server_killed_mid_question_is_restarted_and_asked_again() {
    let fixture = Fixture::new("killed", "hang,answer");
    let killer = {
        let dir = fixture.dir.clone();
        thread::spawn(move || {
            let seen = Fixture { dir };
            until("the first server takes the question", || {
                !seen.requests().is_empty()
            });
            let pid = seen.requests()[0];
            kill(pid);
            // Not the fixture's to remove.
            std::mem::forget(seen);
            pid
        })
    };
    let mut decider = Fixture::decider(fixture.config());
    let (decisions, took) = timed(|| decider.decide(&question(2), &go_on));
    let decisions = decisions.unwrap();
    let victim = killer.join().unwrap();
    // Seen as the connection broke, not at the 10 s request timeout.
    assert!(took < PROMPTLY, "took {took:?}");
    assert_eq!(answers(&decisions), [Answer::Yes, Answer::No]);
    let starts: Vec<u32> = fixture.starts().into_iter().map(|(pid, _)| pid).collect();
    assert_eq!(starts.len(), 2, "{starts:?}");
    assert_eq!(starts[0], victim);
    assert_eq!(fixture.requests(), [victim, starts[1]]);
    // Each start listens on a socket of its own.
    let hosts: Vec<String> = fixture
        .starts()
        .iter()
        .map(|(_, args)| {
            args.split(' ')
                .skip_while(|a| *a != "--host")
                .nth(1)
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    assert!(
        hosts[0].ends_with("-1.sock") && hosts[1].ends_with("-2.sock"),
        "{hosts:?}"
    );
    assert!(!exists(victim), "the killed server wasn't reaped");
}

/// A server that keeps dying on the question is restarted as many times
/// as allowed, then the decider gives up with the last reason, at once:
/// the job fails rather than waiting.
#[test]
fn a_server_that_keeps_dying_is_given_up_on() {
    let fixture = Fixture::new("dying", "exit");
    let mut decider = Fixture::decider(fixture.config());
    let (result, took) = timed(|| decider.decide(&question(1), &go_on));
    let err = result.unwrap_err();
    let ServerError::GaveUp(3, last) = server_error(&err) else {
        panic!("{err}")
    };
    let ServerError::Exited(Some(status), tail) = &**last else {
        panic!("{last}")
    };
    assert_eq!(status.code(), Some(3));
    assert!(tail.contains("fake: exiting on a question"), "{tail:?}");
    assert_eq!(fixture.starts().len(), 3);
    assert!(took < PROMPTLY, "took {took:?}");
    assert!(fixture.starts().iter().all(|&(pid, _)| !exists(pid)));
}

/// A server that holds a question past the request timeout is killed and
/// restarted, and the decider gives up once restarts run out, not hangs.
#[test]
fn a_server_that_hangs_is_given_up_on() {
    let fixture = Fixture::new("hangs", "hang");
    let mut config = fixture.config();
    config.request_timeout = Duration::from_millis(300);
    config.restarts = 1;
    let mut decider = Fixture::decider(config);
    let (result, took) = timed(|| decider.decide(&question(1), &go_on));
    let err = result.unwrap_err();
    let ServerError::GaveUp(2, last) = server_error(&err) else {
        panic!("{err}")
    };
    assert!(
        matches!(**last, ServerError::NoAnswer(limit) if limit == Duration::from_millis(300)),
        "{last}"
    );
    assert_eq!(fixture.requests().len(), 2);
    assert!(took < PROMPTLY, "took {took:?}");
    assert!(
        fixture.starts().iter().all(|&(pid, _)| !exists(pid)),
        "a hung server is still running"
    );
}

/// A server that never finishes loading is given up on after the start
/// timeout, each time.
#[test]
fn a_server_that_never_gets_ready_is_given_up_on() {
    let fixture = Fixture::new("loading", "never-ready");
    let mut config = fixture.config();
    config.start_timeout = Duration::from_millis(300);
    config.restarts = 1;
    let mut decider = Fixture::decider(config);
    let err = decider.decide(&question(1), &go_on).unwrap_err();
    let ServerError::GaveUp(2, last) = server_error(&err) else {
        panic!("{err}")
    };
    assert!(matches!(**last, ServerError::NotReady(_)), "{last}");
    assert_eq!(fixture.starts().len(), 2);
    assert_eq!(fixture.requests(), Vec::<u32>::new());
}

/// A server that exits as it starts is reported with the end of its
/// stderr: the reason the user sees.
#[test]
fn a_server_that_exits_at_start_says_why() {
    let fixture = Fixture::new("crash", "crash");
    let mut decider = Fixture::decider(fixture.config());
    let err = decider.decide(&question(1), &go_on).unwrap_err();
    let ServerError::GaveUp(3, last) = server_error(&err) else {
        panic!("{err}")
    };
    assert_eq!(
        last.to_string(),
        "llama-server stopped (exit status: 1): fake: error: failed to load model"
    );
}

/// A server that answers with a server error is restarted like one that
/// crashed.
#[test]
fn a_server_error_is_restarted() {
    let fixture = Fixture::new("busy", "busy,answer");
    let mut decider = Fixture::decider(fixture.config());
    let decisions = decider.decide(&question(1), &go_on).unwrap();
    assert_eq!(answers(&decisions), [Answer::Yes]);
    assert_eq!(fixture.starts().len(), 2);
}

/// A program that isn't there is unavailable at once, with no restarts.
#[test]
fn a_missing_program_is_unavailable_at_once() {
    let fixture = Fixture::new("missing", "answer");
    let mut config = fixture.config();
    config.program = fixture.dir.join("no-such-llama-server");
    config.initial_backoff = Duration::from_secs(60);
    let mut decider = Fixture::decider(config);
    let (result, took) = timed(|| decider.decide(&question(1), &go_on));
    let err = result.unwrap_err();
    assert!(
        matches!(server_error(&err), ServerError::Spawn(e) if e.kind() == std::io::ErrorKind::NotFound),
        "{err}"
    );
    assert!(took < PROMPTLY, "took {took:?}");
}

/// A reply that isn't JSON is a failed decision: not asked again, and the
/// server stays for the next question.
#[test]
fn a_malformed_reply_is_a_failed_decision() {
    let fixture = Fixture::new("garbage", "garbage");
    let mut decider = Fixture::decider(fixture.config());
    for _ in 0..2 {
        let err = decider.decide(&question(1), &go_on).unwrap_err();
        let DecideError::Failed(e) = &err else {
            panic!("{err:?}")
        };
        assert!(
            matches!(e.downcast_ref::<ReplyError>(), Some(ReplyError::NotJson(_))),
            "{e}"
        );
    }
    assert_eq!(fixture.starts().len(), 1);
    assert_eq!(fixture.requests().len(), 2);
}

/// A question the server refuses is a failed decision with what it said.
#[test]
fn a_refused_question_is_a_failed_decision() {
    let fixture = Fixture::new("refuse", "refuse");
    let mut decider = Fixture::decider(fixture.config());
    let err = decider.decide(&question(1), &go_on).unwrap_err();
    let DecideError::Failed(e) = &err else {
        panic!("{err:?}")
    };
    let refused = e.downcast_ref::<Refused>().unwrap();
    assert_eq!(refused.status, 400);
    assert!(
        refused.said.contains("exceeds the available context size"),
        "{}",
        refused.said
    );
    assert_eq!(fixture.starts().len(), 1);
}

/// While a recording is running (`stop` says so), no server is started.
#[test]
fn no_server_starts_while_told_to_stop() {
    let fixture = Fixture::new("held", "answer");
    let mut decider = Fixture::decider(fixture.config());
    let err = decider.decide(&question(1), &|| true).unwrap_err();
    assert!(matches!(err, DecideError::Stopped), "{err:?}");
    assert_eq!(fixture.starts(), Vec::<(u32, String)>::new());
}

/// A recording that starts while the server holds a question stops it:
/// the server is gone, reaped and its socket removed, by the time the
/// decider returns, within a second of being told.
#[test]
fn a_server_is_stopped_when_a_recording_starts_mid_question() {
    let fixture = Fixture::new("stopped", "hang");
    let recording = Arc::new(AtomicBool::new(false));
    let told = Arc::new(Mutex::new(None));
    let starter = {
        let (dir, recording, told) = (
            fixture.dir.clone(),
            Arc::clone(&recording),
            Arc::clone(&told),
        );
        thread::spawn(move || {
            let seen = Fixture { dir };
            until("the server takes the question", || {
                !seen.requests().is_empty()
            });
            std::mem::forget(seen);
            *told.lock().unwrap() = Some(clock());
            recording.store(true, Ordering::SeqCst);
        })
    };
    let mut decider = Fixture::decider(fixture.config());
    let err = decider
        .decide(&question(1), &|| recording.load(Ordering::SeqCst))
        .unwrap_err();
    let after = told.lock().unwrap().as_ref().map(|c| c.now().elapsed());
    starter.join().unwrap();
    assert!(matches!(err, DecideError::Stopped), "{err:?}");
    let [(pid, _)] = fixture.starts()[..] else {
        panic!("{:?}", fixture.starts())
    };
    assert!(!exists(pid), "server {pid} still runs during the recording");
    assert_eq!(fixture.sockets_left(), Vec::<PathBuf>::new());
    let after = after.unwrap();
    assert!(after < STOPPED_WITHIN, "stopped {after:?} after being told");
}

/// A server started through the launch shim dies with the thread that
/// started it, as the real one dies with nota, however nota dies.
#[test]
fn a_server_started_through_the_shim_dies_with_its_starter() {
    let fixture = Fixture::new("tied", "answer");
    let socket = fixture.dir.join("t.sock");
    let (tx, rx) = mpsc::channel::<Child>();
    let (end, ended) = mpsc::channel::<()>();
    let model = fixture.dir.clone();
    // The thread starts the server, then ends when told: to the kernel, as
    // if nota had died. It mustn't end before the shim has tied itself, or
    // there's no tie to the thread left to break.
    let starter = thread::spawn(move || {
        let child = Command::new(FAKE)
            .args(["exec-tied", FAKE, "--host"])
            .arg(&socket)
            .arg("--model")
            .arg(&model)
            .env(RECORDER_PID_VAR, std::process::id().to_string())
            .spawn()
            .unwrap();
        tx.send(child).unwrap();
        let _ = ended.recv();
    });
    let mut child = rx.recv().unwrap();
    // The server logs its start after the shim's exec, so after the tie.
    until("the server starts", || !fixture.starts().is_empty());
    end.send(()).unwrap();
    starter.join().unwrap();
    let status: ExitStatus = wait_within(&mut child, WAIT);
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(9)
    );
    let starts = fixture.starts();
    assert_eq!(
        starts.len(),
        1,
        "the shim didn't become the server: {starts:?}"
    );
    assert_eq!(
        starts[0].0,
        child.id(),
        "the server isn't the process started"
    );
}

/// Waits for `child` to exit, for up to `limit`.
fn wait_within(child: &mut Child, limit: Duration) -> ExitStatus {
    let clock = clock();
    let (_keep, ticks) = mpsc::channel::<()>();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(
            clock.now().elapsed() < limit,
            "the server outlived its starter"
        );
        let _ = ticks.recv_timeout(Duration::from_millis(10));
    }
}

/// A server that answers a byte at a time, slower than the request
/// timeout, is given up on as one that doesn't answer: every byte that
/// comes doesn't reset the wait.
#[test]
fn a_server_that_trickles_its_answer_is_given_up_on() {
    let fixture = Fixture::new("trickle", "trickle");
    let mut config = fixture.config();
    // Its answer takes a few seconds at a byte every 20 ms.
    config.request_timeout = Duration::from_millis(300);
    config.restarts = 0;
    let mut decider = Fixture::decider(config);
    let (result, took) = timed(|| decider.decide(&question(1), &go_on));
    let err = result.unwrap_err();
    let ServerError::GaveUp(1, last) = server_error(&err) else {
        panic!("{err}")
    };
    assert!(matches!(**last, ServerError::NoAnswer(_)), "{last}");
    assert!(took < PROMPTLY, "took {took:?}");
}

/// A response longer than nota reads is a failed decision, as a malformed
/// reply is: the server isn't restarted, and stays for the next question.
#[test]
fn a_response_over_the_limit_is_a_failed_decision() {
    let fixture = Fixture::new("huge", "huge");
    let mut decider = Fixture::decider(fixture.config());
    for _ in 0..2 {
        let err = decider.decide(&question(1), &go_on).unwrap_err();
        let DecideError::Failed(e) = &err else {
            panic!("{err:?}")
        };
        assert!(
            matches!(
                e.downcast_ref::<ServerError>(),
                Some(ServerError::Unreadable(HttpError::TooLong("body")))
            ),
            "{e}"
        );
    }
    assert_eq!(fixture.starts().len(), 1);
    assert_eq!(fixture.requests().len(), 2);
}

/// A recording that starts just as the answer comes keeps the answer, but
/// not the server: it's gone by the time the decider returns.
#[test]
fn a_stop_that_comes_with_the_answer_stops_the_server() {
    let fixture = Fixture::new("late", "answer");
    let mut decider = Fixture::decider(fixture.config());
    // Says to stop from the moment the server has the question.
    let recording = || !fixture.requests().is_empty();
    match decider.decide(&question(2), &recording) {
        Ok(decisions) => assert_eq!(answers(&decisions), [Answer::Yes, Answer::No]),
        Err(DecideError::Stopped) => {}
        Err(e) => panic!("{e:?}"),
    }
    let [(pid, _)] = fixture.starts()[..] else {
        panic!("{:?}", fixture.starts())
    };
    assert!(!exists(pid), "server {pid} still runs during the recording");
    assert_eq!(fixture.sockets_left(), Vec::<PathBuf>::new());
}

/// Started through the launch shim, as nota starts it, the server answers
/// as it does when started directly.
#[test]
fn a_server_started_through_the_launcher_answers() {
    let fixture = Fixture::new("launched", "answer");
    let mut decider = Fixture::decider(fixture.tied_config());
    let decisions = decider.decide(&question(2), &go_on).unwrap();
    assert_eq!(answers(&decisions), [Answer::Yes, Answer::No]);
    let starts = fixture.starts();
    let [(pid, args)] = &starts[..] else {
        panic!("{starts:?}")
    };
    // The shim became the server: what was started is what answered.
    assert!(args.starts_with("--model "), "{args}");
    drop(decider);
    assert!(!exists(*pid), "server {pid} outlived its decider");
}

/// Through the launcher too, a program that isn't there is unavailable at
/// once: the launcher isn't started only to fail.
#[test]
fn a_missing_program_behind_the_launcher_is_unavailable_at_once() {
    let fixture = Fixture::new("gone", "answer");
    let mut config = fixture.tied_config();
    config.program = fixture.dir.join("no-such-llama-server");
    config.initial_backoff = Duration::from_secs(60);
    let mut decider = Fixture::decider(config);
    let (result, took) = timed(|| decider.decide(&question(1), &go_on));
    let err = result.unwrap_err();
    assert!(
        matches!(server_error(&err), ServerError::Spawn(e) if e.kind() == std::io::ErrorKind::NotFound),
        "{err}"
    );
    assert!(took < PROMPTLY, "took {took:?}");
    assert_eq!(fixture.starts(), Vec::<(u32, String)>::new());
}

/// A bare program name behind the launcher is left to the launcher's exec,
/// which looks for it on `PATH`: it isn't refused before it's started.
#[test]
fn a_bare_program_name_behind_the_launcher_is_left_to_its_exec() {
    let fixture = Fixture::new("bare", "answer");
    let mut config = fixture.tied_config();
    config.program = "no-such-llama-server-on-path".into();
    config.restarts = 0;
    let mut decider = Fixture::decider(config);
    let err = decider.decide(&question(1), &go_on).unwrap_err();
    // The launcher ran and its exec failed: the server never started.
    let ServerError::GaveUp(1, last) = server_error(&err) else {
        panic!("{err}")
    };
    assert!(matches!(**last, ServerError::Exited(..)), "{last}");
    assert_eq!(fixture.starts(), Vec::<(u32, String)>::new());
}

/// A server whose health check answers with a server error is given up
/// on at once, with what it said, rather than waited on as loading.
#[test]
fn a_server_whose_health_check_fails_is_given_up_on_at_once() {
    let fixture = Fixture::new("unhealthy", "unhealthy");
    let mut config = fixture.config();
    config.restarts = 0;
    let mut decider = Fixture::decider(config);
    let (result, took) = timed(|| decider.decide(&question(1), &go_on));
    let err = result.unwrap_err();
    let ServerError::GaveUp(1, last) = server_error(&err) else {
        panic!("{err}")
    };
    assert!(
        matches!(&**last, ServerError::Status(500, said) if said.contains("no slots")),
        "{last}"
    );
    assert!(took < PROMPTLY, "took {took:?}");
}
