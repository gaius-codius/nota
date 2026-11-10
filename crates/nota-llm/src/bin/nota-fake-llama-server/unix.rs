//! The Unix program; see the binary's own docs.

use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Value, json};

/// What one start of the server does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// Answers properly.
    Answer,
    /// Never answers a question.
    Hang,
    /// Exits when a question arrives.
    Exit,
    /// Answers with a body that isn't JSON.
    Garbage,
    /// Refuses the question with 400.
    Refuse,
    /// Fails the question with 500.
    Busy,
    /// Never says it's ready.
    NeverReady,
    /// Exits at once.
    Crash,
    /// Answers a byte at a time, slowly.
    Trickle,
    /// Says its answer is longer than nota reads.
    Huge,
}

impl Plan {
    /// The plan named `name`.
    fn named(name: &str) -> Option<Self> {
        Some(match name.trim() {
            "answer" => Self::Answer,
            "hang" => Self::Hang,
            "exit" => Self::Exit,
            "garbage" => Self::Garbage,
            "refuse" => Self::Refuse,
            "busy" => Self::Busy,
            "never-ready" => Self::NeverReady,
            "crash" => Self::Crash,
            "trickle" => Self::Trickle,
            "huge" => Self::Huge,
            _ => return None,
        })
    }
}

/// Appends `line` to the log in `dir`.
#[expect(
    clippy::disallowed_methods,
    reason = "a test stand-in's log of what it did, not a recording"
)]
fn log(dir: &Path, line: &str) {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("log"));
    if let Ok(mut file) = file {
        let _ = writeln!(file, "{line}");
    }
}

/// This start's plan, from the plan in `dir`: the one at the position of
/// this start in the log.
fn plan(dir: &Path) -> Plan {
    let starts = std::fs::read_to_string(dir.join("log")).map_or(0, |log| {
        log.lines().filter(|l| l.starts_with("start ")).count()
    });
    let plans = std::fs::read_to_string(dir.join("plan")).unwrap_or_default();
    let plans: Vec<Plan> = plans.split(',').filter_map(Plan::named).collect();
    plans
        .get(starts.saturating_sub(1))
        .or(plans.last())
        .copied()
        .unwrap_or(Plan::Answer)
}

/// One request: its path and body.
struct Request {
    /// The path asked for.
    path: String,
    /// The body, empty for a GET.
    body: Vec<u8>,
}

/// Reads one request from `stream`.
fn read_request(stream: &UnixStream) -> io::Result<Request> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let path = line.split(' ').nth(1).unwrap_or_default().to_owned();
    let mut length = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(Request { path, body })
}

/// Writes a response with `status` and `body`.
fn respond(mut stream: &UnixStream, status: u16, body: &[u8]) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

/// Writes a 200 response with `body`, one byte every 20 ms.
fn trickle(mut stream: &UnixStream, body: &[u8]) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    // Nothing is ever sent: the channel only gives a wait.
    let (_keep, ticks) = std::sync::mpsc::channel::<()>();
    for byte in head.as_bytes().iter().chain(body) {
        stream.write_all(&[*byte])?;
        stream.flush()?;
        let _ = ticks.recv_timeout(std::time::Duration::from_millis(20));
    }
    Ok(())
}

/// The reply to a chat request `body`: yes to odd candidates, no to even,
/// with token probabilities that lean the same way.
fn answer(body: &[u8]) -> Vec<u8> {
    let request: Value = serde_json::from_slice(body).unwrap_or_default();
    let ids = request["response_format"]["json_schema"]["schema"]["properties"]["decisions"]
        ["items"]["properties"]["id"]["enum"]
        .as_array()
        .map_or(0, Vec::len);
    let decisions: Vec<Value> = (1..=ids)
        .map(|id| json!({"id": id, "apply": id % 2 == 1}))
        .collect();
    let tokens: Vec<Value> = (1..=ids)
        .map(|id| {
            let (chosen, other) = if id % 2 == 1 {
                ("true", "false")
            } else {
                ("false", "true")
            };
            json!({"token": chosen, "logprob": -0.1, "top_logprobs": [
                {"token": chosen, "logprob": -0.1}, {"token": other, "logprob": -2.4}
            ]})
        })
        .collect();
    let content = json!({ "decisions": decisions }).to_string();
    json!({"choices": [{
        "finish_reason": "stop",
        "message": {"role": "assistant", "content": content},
        "logprobs": {"content": tokens}
    }]})
    .to_string()
    .into_bytes()
}

/// What happens after a request has been handled.
enum Then {
    /// Take the next connection.
    Serve,
    /// Exit, with this code.
    Exit(u8),
}

/// Handles one connection under `plan`, logging to `dir`; `healthy`
/// counts health checks.
fn handle(stream: &UnixStream, dir: &Path, plan: Plan, healthy: &mut u32) -> io::Result<Then> {
    let request = read_request(stream)?;
    if request.path == "/health" {
        *healthy += 1;
        let loading = plan == Plan::NeverReady || *healthy == 1;
        let (status, body) = if loading {
            (503, &br#"{"error":{"message":"Loading model"}}"#[..])
        } else {
            (200, &br#"{"status":"ok"}"#[..])
        };
        respond(stream, status, body)?;
        return Ok(Then::Serve);
    }
    log(dir, &format!("request {}", std::process::id()));
    match plan {
        Plan::Answer | Plan::NeverReady | Plan::Crash => {
            respond(stream, 200, &answer(&request.body))?;
        }
        Plan::Hang => loop {
            // Holds the question, unanswered, until killed.
            std::thread::park();
        },
        Plan::Exit => {
            let _ = writeln!(io::stderr(), "fake: exiting on a question");
            return Ok(Then::Exit(3));
        }
        Plan::Garbage => respond(stream, 200, b"<html>not json</html>")?,
        Plan::Refuse => respond(
            stream,
            400,
            br#"{"error":{"message":"the request exceeds the available context size"}}"#,
        )?,
        Plan::Busy => respond(stream, 500, br#"{"error":{"message":"busy"}}"#)?,
        Plan::Trickle => trickle(stream, &answer(&request.body))?,
        Plan::Huge => {
            let mut stream = stream;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: 2000000\r\nConnection: close\r\n\r\n"
            )?;
            stream.flush()?;
        }
    }
    Ok(Then::Serve)
}

/// The value after `flag` in `args`.
fn value(args: &[OsString], flag: &str) -> Option<PathBuf> {
    let at = args.iter().position(|a| a == flag)?;
    args.get(at + 1).map(PathBuf::from)
}

/// The program, on Unix.
pub(crate) fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|a| a == "exec-tied") {
        let err = nota_llm::lifeline::exec_tied(args.get(1..).unwrap_or_default());
        let _ = writeln!(io::stderr(), "fake: {err}");
        return ExitCode::from(2);
    }
    let (Some(dir), Some(socket)) = (value(&args, "--model"), value(&args, "--host")) else {
        let _ = writeln!(io::stderr(), "fake: no --model or --host");
        return ExitCode::from(2);
    };
    let shown: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    log(
        &dir,
        &format!("start {} {}", std::process::id(), shown.join(" ")),
    );
    let plan = plan(&dir);
    if plan == Plan::Crash {
        let _ = writeln!(io::stderr(), "fake: error: failed to load model");
        return ExitCode::from(1);
    }
    let listener = match UnixListener::bind(&socket) {
        Ok(listener) => listener,
        Err(e) => {
            let _ = writeln!(
                io::stderr(),
                "fake: couldn't bind {}: {e}",
                socket.display()
            );
            return ExitCode::from(1);
        }
    };
    let mut healthy = 0;
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if let Ok(Then::Exit(code)) = handle(&stream, &dir, plan, &mut healthy) {
            return ExitCode::from(code);
        }
    }
    ExitCode::SUCCESS
}
