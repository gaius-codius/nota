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
    let gave_up = ServerError::GaveUp(4, Box::new(ServerError::Status(500)));
    assert_eq!(
        gave_up.to_string(),
        "llama-server failed 4 times in a row: llama-server answered with error 500"
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
    // A pause of nothing has nothing to wait for.
    assert!(pause(&clock, Duration::ZERO, &|| true).is_ok());
}
