use std::os::unix::fs::PermissionsExt;

use super::*;

/// A config with every path given, on `device`.
fn config(device: Device) -> ServerConfig {
    ServerConfig::new(
        "/opt/llama-server".into(),
        "/models/qwen.gguf".into(),
        device,
        "/run/nota".into(),
    )
}

/// The arguments `command` runs with, as text.
fn args(command: &Command) -> Vec<String> {
    command
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

/// A GPU is always named alone: never blank, a list, or the CPU's
/// `none`.
#[test]
fn a_gpu_is_one_named_device() {
    assert_eq!(GpuName::new(" Vulkan0 ").unwrap().as_str(), "Vulkan0");
    for name in ["", "  ", "Vulkan0,Vulkan1", "none", "NONE"] {
        assert_eq!(GpuName::new(name), None, "{name:?}");
    }
}

/// The server is told its model, socket, one device with every layer on
/// it, the context, and to stay offline with no web page.
#[test]
fn the_server_is_told_its_device_and_socket() {
    let gpu = config(Device::Gpu(GpuName::new("Vulkan0").unwrap()));
    let command = gpu.command(Path::new("/run/nota/llama-1-1.sock"));
    assert_eq!(command.get_program(), "/opt/llama-server");
    assert_eq!(
        args(&command),
        [
            "--model",
            "/models/qwen.gguf",
            "--host",
            "/run/nota/llama-1-1.sock",
            "--device",
            "Vulkan0",
            "--n-gpu-layers",
            "999",
            "--ctx-size",
            "49152",
            "--parallel",
            "1",
            "--no-webui",
            "--offline",
        ]
    );
    let cpu = args(&config(Device::Cpu).command(Path::new("/s.sock")));
    assert_eq!(cpu[4..8], ["--device", "none", "--n-gpu-layers", "0"]);
}

/// With a launcher, the launcher runs, given the server and its arguments
/// after its own.
#[test]
fn a_launcher_runs_the_server() {
    let mut launched = config(Device::Cpu);
    launched.launcher = Some(Launcher {
        program: "/bin/nota".into(),
        args: vec!["engine".into(), "llama-server".into()],
    });
    let command = launched.command(Path::new("/s.sock"));
    assert_eq!(command.get_program(), "/bin/nota");
    assert_eq!(
        args(&command)[..5],
        [
            "engine",
            "llama-server",
            "/opt/llama-server",
            "--model",
            "/models/qwen.gguf"
        ]
    );
}

/// A directory for one test in the temporary folder, missing to begin
/// with and removed when dropped.
struct Dir(PathBuf);

impl Dir {
    /// A new, missing directory named for `name`.
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding, not a recording"
    )]
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nota-llm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }

    /// Makes it, with `mode`.
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding, not a recording"
    )]
    fn make(&self, mode: u32) {
        std::fs::create_dir_all(&self.0).unwrap();
        std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Writes a file `name` in it.
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding, not a recording"
    )]
    fn file(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, b"x").unwrap();
        path
    }
}

impl Drop for Dir {
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding, not a recording"
    )]
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A missing socket directory is made, readable by this user alone; one
/// already there and private is taken as it is.
#[test]
fn a_missing_socket_directory_is_made_private() {
    let dir = Dir::new("made");
    let sockets = dir.0.join("sockets");
    private_dir(&sockets).unwrap();
    let mode = std::fs::metadata(&sockets).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
    private_dir(&sockets).unwrap();
}

/// A socket directory others can open is refused, by its path.
#[test]
fn an_open_socket_directory_is_refused() {
    let dir = Dir::new("open");
    dir.make(0o755);
    let err = private_dir(&dir.0).unwrap_err();
    assert!(
        matches!(&err, ServerError::SocketDirOpen(p) if *p == dir.0),
        "{err:?}"
    );
    dir.make(0o710);
    assert!(matches!(
        private_dir(&dir.0),
        Err(ServerError::SocketDirOpen(_))
    ));
}

/// A path that can't be a directory is reported with the path.
#[test]
fn an_unusable_socket_directory_is_reported() {
    let dir = Dir::new("file");
    dir.make(0o700);
    let below = dir.file("file").join("below");
    let err = private_dir(&below).unwrap_err();
    assert!(
        matches!(&err, ServerError::SocketDir(p, _) if *p == below),
        "{err:?}"
    );
}

/// Only the end of stderr is kept.
#[test]
fn only_the_end_of_stderr_is_kept() {
    let long: Vec<u8> = (0..10_000_u32)
        .map(|i| b'a' + u8::try_from(i % 26).unwrap())
        .collect();
    let tail = read_tail(&long[..], 100);
    assert_eq!(tail.as_bytes(), &long[long.len() - 100..]);
    assert_eq!(read_tail(&b"short"[..], 100), "short");
}

/// Errors say what went wrong, with the end of stderr where there is
/// one.
#[test]
fn errors_say_what_went_wrong() {
    assert_eq!(
        ServerError::NoAnswer(Duration::from_secs(120)).to_string(),
        "llama-server didn't answer within 120 s"
    );
    assert_eq!(
        ServerError::Exited(None, "  error: model not found\n".to_owned()).to_string(),
        "llama-server stopped: error: model not found"
    );
    assert_eq!(
        ServerError::Exited(None, String::new()).to_string(),
        "llama-server stopped"
    );
    let gave_up = ServerError::GaveUp(4, Box::new(ServerError::Status(500, String::new())));
    assert_eq!(
        gave_up.to_string(),
        "llama-server failed 4 times in a row: llama-server answered with error 500"
    );
    assert_eq!(
        ServerError::Status(500, " {\"error\":\"busy\"}\n".to_owned()).to_string(),
        "llama-server answered with error 500: {\"error\":\"busy\"}"
    );
    assert!(gave_up.source().is_some());
}

/// A deadline past the end of time never passes; one of no wait has.
#[test]
fn a_deadline_past_the_end_of_time_never_passes() {
    let clock = nota_core::FakeClock::new(SessionTime::from_nanos(u64::MAX - 1));
    assert!(!Deadline::after(&clock, Duration::from_secs(5)).passed(&clock));
    assert!(Deadline::after(&clock, Duration::ZERO).passed(&clock));
}

/// A pause that `stop` ends comes back stopped, before its time.
#[test]
fn a_pause_ends_when_told_to_stop() {
    let clock = nota_core::FakeClock::new(SessionTime::ZERO);
    let stopped = pause(&clock, Duration::from_secs(3_600), &|| true);
    assert!(matches!(stopped, Err(Interrupted::Stopped)));
    // A pause of nothing still looks once: what follows it starts a server.
    assert!(matches!(
        pause(&clock, Duration::ZERO, &|| true),
        Err(Interrupted::Stopped)
    ));
    assert!(pause(&clock, Duration::ZERO, &|| false).is_ok());
}

/// A stop that comes during a pause's last wait is seen before it ends.
#[test]
fn a_stop_during_the_last_wait_is_seen() {
    let clock = nota_core::FakeClock::new(SessionTime::ZERO);
    let asked = std::cell::Cell::new(0_u32);
    // The first look says go on; the clock then passes the pause's end
    // while it waits, and the next look says stop.
    let stop = || {
        asked.set(asked.get() + 1);
        if asked.get() == 1 {
            clock.advance(Duration::from_secs(1));
            false
        } else {
            true
        }
    };
    let stopped = pause(&clock, Duration::from_millis(500), &stop);
    assert!(matches!(stopped, Err(Interrupted::Stopped)), "{stopped:?}");
}

/// A socket path is taken up to the longest a UNIX socket allows, and
/// refused one byte past it.
#[test]
fn a_socket_path_is_taken_up_to_its_limit() {
    // "/" + padding + "/llama-1-2.sock" (15 bytes) makes the whole.
    let dir_of = |whole: usize| PathBuf::from(format!("/{}", "d".repeat(whole - 16)));
    let at = socket_path(&dir_of(MAX_SOCKET_PATH), 1, 2).unwrap();
    assert_eq!(at.as_os_str().len(), MAX_SOCKET_PATH);
    let over = socket_path(&dir_of(MAX_SOCKET_PATH + 1), 1, 2).unwrap_err();
    assert!(
        matches!(over, ServerError::SocketPathTooLong(_)),
        "{over:?}"
    );
}

/// A file where the socket directory should be is reported as unusable.
#[test]
fn a_file_for_a_socket_directory_is_reported() {
    let dir = Dir::new("isfile");
    dir.make(0o700);
    let file = dir.file("sockets");
    let err = private_dir(&file).unwrap_err();
    assert!(
        matches!(&err, ServerError::SocketDir(p, _) if *p == file),
        "{err:?}"
    );
}

/// Only a file that can be run passes as a program.
#[test]
fn only_a_runnable_file_is_a_program() {
    assert!(runnable(Path::new("/bin/sh")).is_ok());
    let dir = Dir::new("runnable");
    dir.make(0o700);
    let plain = dir.file("plain");
    for (path, what) in [
        (&plain, "a file that can't be run"),
        (&dir.0, "a directory"),
    ] {
        let err = runnable(path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{what}");
    }
    assert_eq!(
        runnable(&dir.0.join("missing")).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
}

/// A reader that gives what it holds, one result at a time.
struct Script(Vec<io::Result<Vec<u8>>>);

impl Read for Script {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.0.is_empty() {
            return Ok(0);
        }
        let bytes = self.0.remove(0)?;
        buf[..bytes.len()].copy_from_slice(&bytes);
        Ok(bytes.len())
    }
}

/// A writer that fails, or would block, as it's told, then takes all.
struct Sink {
    /// The errors to give first, in order.
    errors: Vec<io::ErrorKind>,
    /// Whether flushing fails.
    flush_fails: bool,
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.errors.is_empty() {
            Ok(buf.len())
        } else {
            Err(self.errors.remove(0).into())
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.flush_fails {
            Err(io::ErrorKind::BrokenPipe.into())
        } else {
            Ok(())
        }
    }
}

/// A check that lets `n` waits go by, then says to give up.
fn give_up_after(n: u32) -> impl FnMut() -> io::Result<()> {
    let mut left = n;
    move || {
        if left == 0 {
            return Err(io::Error::other("gave up"));
        }
        left -= 1;
        Ok(())
    }
}

/// A connection's own failure comes back as it is, at once; a read that
/// times out or is interrupted is tried again.
#[test]
fn a_patient_reader_passes_on_failures_and_waits_out_timeouts() {
    let mut check = give_up_after(10);
    let mut stream = Script(vec![
        Err(io::ErrorKind::WouldBlock.into()),
        Err(io::ErrorKind::Interrupted.into()),
        Ok(b"ok".to_vec()),
        Err(io::ErrorKind::ConnectionReset.into()),
    ]);
    let mut patient = Patient {
        stream: &mut stream,
        check: &mut check,
    };
    let mut buf = [0_u8; 8];
    assert_eq!(patient.read(&mut buf).unwrap(), 2);
    assert_eq!(&buf[..2], b"ok");
    let err = patient.read(&mut buf).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::ConnectionReset, "{err}");
    assert_eq!(format!("{patient:?}"), "Patient");
}

/// A write that would block is tried again; one that fails comes back
/// as it is; a failed flush is passed on.
#[test]
fn a_patient_writer_passes_on_failures_and_waits_out_timeouts() {
    let mut check = give_up_after(10);
    let mut sink = Sink {
        errors: vec![io::ErrorKind::WouldBlock, io::ErrorKind::TimedOut],
        flush_fails: false,
    };
    let mut patient = Patient {
        stream: &mut sink,
        check: &mut check,
    };
    assert_eq!(patient.write(b"abc").unwrap(), 3);
    assert!(patient.flush().is_ok());

    let mut sink = Sink {
        errors: vec![io::ErrorKind::BrokenPipe],
        flush_fails: true,
    };
    let mut patient = Patient {
        stream: &mut sink,
        check: &mut check,
    };
    assert_eq!(
        patient.write(b"abc").unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
    assert_eq!(
        patient.flush().unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
}

/// An interrupted read of stderr is tried again, and a failed one ends
/// it with what came before.
#[test]
fn a_tail_reads_on_past_an_interruption() {
    let interrupted = Script(vec![
        Ok(b"loading ".to_vec()),
        Err(io::ErrorKind::Interrupted.into()),
        Ok(b"failed".to_vec()),
    ]);
    assert_eq!(read_tail(interrupted, 100), "loading failed");
    let broken = Script(vec![
        Ok(b"loading".to_vec()),
        Err(io::ErrorKind::BrokenPipe.into()),
        Ok(b" never seen".to_vec()),
    ]);
    assert_eq!(read_tail(broken, 100), "loading");
    // Exactly the kept length stays whole.
    assert_eq!(read_tail(&b"abcde"[..], 5), "abcde");
    assert_eq!(read_tail(&b"abcdef"[..], 5), "bcdef");
}

/// The errors that wrap another keep it as their source.
#[test]
fn wrapping_errors_keep_their_source() {
    let io = || io::Error::from(io::ErrorKind::NotFound);
    for err in [
        ServerError::Spawn(io()),
        ServerError::SocketDir(PathBuf::from("/s"), io()),
        ServerError::Connection(HttpError::Io(io())),
        ServerError::Unreadable(HttpError::TooLong("body")),
    ] {
        assert!(err.source().is_some(), "{err:?}");
    }
    assert!(ServerError::NotReady(Duration::ZERO).source().is_none());
}
