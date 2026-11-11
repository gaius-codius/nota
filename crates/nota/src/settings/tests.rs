use std::fs::Permissions;

use super::*;

/// A fresh folder under the system temp dir, removed when dropped.
struct TestDir(PathBuf);

impl TestDir {
    /// A new empty folder named for `name`.
    #[expect(clippy::disallowed_methods, reason = "test scaffolding")]
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nota-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// Writes `text` to `name` in the folder, returning its path.
    #[expect(clippy::disallowed_methods, reason = "test scaffolding")]
    fn file(&self, name: &str, text: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    /// A file named `name` that may be run, as llama-server would be.
    #[expect(clippy::disallowed_methods, reason = "test scaffolding")]
    fn program(&self, name: &str) -> PathBuf {
        let path = self.file(name, "#!/bin/sh\n");
        std::fs::set_permissions(&path, Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A new empty folder named `name` in the folder, returning its path.
    #[expect(clippy::disallowed_methods, reason = "test scaffolding")]
    fn folder(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::create_dir(&path).unwrap();
        path
    }

    /// The settings file, `settings.toml`, holding `text`.
    fn settings(&self, text: &str) -> PathBuf {
        self.file(FILE, text)
    }

    /// An environment reading this folder's settings, with `home` as the
    /// home directory, `run` as the runtime folder, no `PATH` and a
    /// `nota` that's found.
    fn env(&self) -> Environment {
        Environment {
            settings: Some(self.0.join(FILE)),
            home: Some(self.0.clone()),
            runtime_dir: Some(self.0.join("run")),
            path: None,
            exe: Ok(PathBuf::from("/usr/bin/nota")),
        }
    }
}

impl Drop for TestDir {
    #[expect(clippy::disallowed_methods, reason = "test scaffolding")]
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `path` as a TOML literal string, which takes any text but `'` and line
/// breaks as written (Rust's `{:?}` escapes aren't all TOML's).
fn toml_path(path: &Path) -> String {
    let text = path.display().to_string();
    assert!(!text.contains(['\'', '\n', '\r']), "{text}");
    format!("'{text}'")
}

/// The config of a decider that's ready, or a panic naming what it was.
fn ready_config(decider: LocalDecider) -> ServerConfig {
    match decider {
        LocalDecider::Ready(config) => *config,
        other => panic!("not ready: {other:?}"),
    }
}

/// Why a decider is unavailable, or a panic naming what it was.
fn unavailable(decider: LocalDecider) -> Unavailable {
    match decider {
        LocalDecider::Unavailable(why) => why,
        other => panic!("not unavailable: {other:?}"),
    }
}

/// A program, a model and a device in the settings make a config with
/// those three, the socket in the runtime folder, and nota's launcher.
#[test]
fn named_program_model_and_device_make_a_config_with_nota_s_launcher() {
    let dir = TestDir::new("ready");
    let program = dir.program("llama-server");
    let model = dir.file("model.gguf", "");
    dir.settings(&format!(
        "[local_decider]\nprogram = {}\nmodel = {}\ndevice = \"Vulkan0\"\n",
        toml_path(&program),
        toml_path(&model)
    ));

    let config = ready_config(local_decider(&dir.env()));

    assert_eq!(config.program, program);
    assert_eq!(config.model, model);
    assert_eq!(config.device, Device::Gpu(GpuName::new("Vulkan0").unwrap()));
    assert_eq!(config.socket_dir, dir.0.join("run"));
    // Started through nota, so the server ends when nota does.
    assert_eq!(
        config.launcher,
        Some(Launcher {
            program: PathBuf::from("/usr/bin/nota"),
            args: vec!["engine".into(), "llama-server".into()],
        })
    );
}

/// A bare program name is looked for in each `PATH` folder in turn, and
/// the config runs the first one found.
#[test]
fn a_bare_program_name_runs_the_first_one_on_path() {
    let dir = TestDir::new("on-path");
    let model = dir.file("model.gguf", "");
    dir.settings(&format!(
        "[local_decider]\nprogram = \"llama-server\"\nmodel = {}\n",
        toml_path(&model)
    ));
    let mut env = dir.env();
    // The first folder has none; the second and third both have one, and
    // the second's is the one that runs.
    let empty = dir.folder("empty");
    let second = dir.folder("second");
    let third = dir.folder("third");
    dir.program("second/llama-server");
    dir.program("third/llama-server");
    env.path = Some(std::env::join_paths([&empty, &second, &third]).unwrap());

    let config = ready_config(local_decider(&env));

    assert_eq!(config.program, second.join("llama-server"));
}

/// Paths written `~/…` are under the home directory.
#[test]
fn paths_starting_with_a_tilde_are_under_home() {
    let dir = TestDir::new("tilde");
    dir.program("llama-server");
    dir.file("model.gguf", "");
    dir.settings(
        "[local_decider]\nprogram = \"~/llama-server\"\nmodel = \"~/model.gguf\"\n\
         socket_dir = \"~/sockets\"\n",
    );

    let config = ready_config(local_decider(&dir.env()));

    assert_eq!(
        (config.program, config.model, config.socket_dir),
        (
            dir.0.join("llama-server"),
            dir.0.join("model.gguf"),
            dir.0.join("sockets")
        )
    );
}

/// Without a device, the model runs on the CPU.
#[test]
fn no_device_is_the_cpu() {
    let dir = TestDir::new("no-device");
    dir.program("llama-server");
    dir.file("model.gguf", "");
    dir.settings("[local_decider]\nprogram = \"~/llama-server\"\nmodel = \"~/model.gguf\"\n");

    assert_eq!(ready_config(local_decider(&dir.env())).device, Device::Cpu);
}

/// `cpu`, in any case, is the CPU.
#[test]
fn cpu_names_the_cpu() {
    assert_eq!(device(" CPU "), Ok(Device::Cpu));
}

/// A list of devices isn't one device, and is refused with the text.
#[test]
fn a_list_of_devices_is_refused() {
    assert_eq!(
        device("Vulkan0,Vulkan1"),
        Err(
            "local_decider.device must be cpu or one GPU's name, as llama-server lists it: \
             Vulkan0,Vulkan1"
                .to_owned()
        )
    );
}

/// No settings file at all is a decider that isn't set up.
#[test]
fn no_settings_file_is_not_set_up() {
    let dir = TestDir::new("no-file");

    assert!(matches!(local_decider(&dir.env()), LocalDecider::NotSetUp));
}

/// A settings file with no program is a decider that isn't set up, even
/// with a model.
#[test]
fn no_program_is_not_set_up() {
    let dir = TestDir::new("no-program");
    dir.file("model.gguf", "");
    dir.settings("[local_decider]\nmodel = \"~/model.gguf\"\n");

    assert!(matches!(local_decider(&dir.env()), LocalDecider::NotSetUp));
}

/// A settings file with no model is a decider that isn't set up, even
/// with a program.
#[test]
fn no_model_is_not_set_up() {
    let dir = TestDir::new("no-model");
    dir.program("llama-server");
    dir.settings("[local_decider]\nprogram = \"~/llama-server\"\n");

    assert!(matches!(local_decider(&dir.env()), LocalDecider::NotSetUp));
}

/// A program path that holds nothing makes the decider unavailable, naming
/// the path looked at.
#[test]
fn a_missing_program_is_reported_with_its_path() {
    let dir = TestDir::new("missing-program");
    dir.file("model.gguf", "");
    dir.settings("[local_decider]\nprogram = \"~/gone\"\nmodel = \"~/model.gguf\"\n");

    let why = unavailable(local_decider(&dir.env()));

    let path = dir.0.join("gone");
    assert!(
        matches!(&why, Unavailable::ProgramMissing(p) if *p == path),
        "{why:?}"
    );
    assert_eq!(
        why.to_string(),
        format!("there's no llama-server to run at {}", path.display())
    );
}

/// A program path that holds a file nobody may run is missing too.
#[test]
fn a_file_that_cant_be_run_isnt_the_program() {
    let dir = TestDir::new("not-a-program");
    dir.file("llama-server", "");
    dir.file("model.gguf", "");
    dir.settings("[local_decider]\nprogram = \"~/llama-server\"\nmodel = \"~/model.gguf\"\n");

    let why = unavailable(local_decider(&dir.env()));

    assert!(
        matches!(&why, Unavailable::ProgramMissing(p) if *p == dir.0.join("llama-server")),
        "{why:?}"
    );
}

/// A bare name that's in no `PATH` folder makes the decider unavailable,
/// naming the program looked for.
#[test]
fn a_bare_name_not_on_path_is_reported_with_its_name() {
    let dir = TestDir::new("not-on-path");
    dir.file("model.gguf", "");
    dir.settings("[local_decider]\nprogram = \"llama-server\"\nmodel = \"~/model.gguf\"\n");
    let mut env = dir.env();
    // The program is in the folder, but the folder isn't on PATH.
    dir.program("llama-server");
    env.path = Some(dir.0.join("empty").into_os_string());

    let why = unavailable(local_decider(&env));

    assert_eq!(why.to_string(), "llama-server isn't in any PATH folder");
}

/// A relative `PATH` entry (an empty one too) depends on the working
/// folder, so it isn't searched.
#[test]
fn a_relative_path_entry_isnt_searched() {
    // Tests run in the package's folder, where this holds a program that
    // a search of relative entries would find.
    let relative = Path::new("../../scripts");
    assert!(is_program(&relative.join("check-files.sh")));
    let path = std::env::join_paths([Path::new(""), relative]).unwrap();

    let found = find_program(Path::new("check-files.sh"), Some(&path));

    assert!(
        matches!(found, Err(Unavailable::ProgramNotOnPath(ref n)) if n == "check-files.sh"),
        "{found:?}"
    );
}

/// A model path that holds nothing makes the decider unavailable, naming
/// the path looked at.
#[test]
fn a_missing_model_is_reported_with_its_path() {
    let dir = TestDir::new("missing-model");
    dir.program("llama-server");
    dir.settings("[local_decider]\nprogram = \"~/llama-server\"\nmodel = \"~/gone.gguf\"\n");

    let why = unavailable(local_decider(&dir.env()));

    let path = dir.0.join("gone.gguf");
    assert!(
        matches!(&why, Unavailable::ModelMissing(p) if *p == path),
        "{why:?}"
    );
    assert_eq!(
        why.to_string(),
        format!("the model isn't at {}", path.display())
    );
}

/// A model path that names a folder isn't a model.
#[test]
fn a_folder_isnt_a_model() {
    let dir = TestDir::new("folder-model");
    dir.program("llama-server");
    dir.settings("[local_decider]\nprogram = \"~/llama-server\"\nmodel = \"~/\"\n");

    let why = unavailable(local_decider(&dir.env()));

    assert!(matches!(why, Unavailable::ModelMissing(_)), "{why:?}");
}

/// With no socket folder set and no runtime folder, there's nowhere for
/// the socket, and nothing starts.
#[test]
fn no_socket_folder_is_unavailable() {
    let dir = TestDir::new("no-socket");
    dir.program("llama-server");
    dir.file("model.gguf", "");
    dir.settings("[local_decider]\nprogram = \"~/llama-server\"\nmodel = \"~/model.gguf\"\n");
    let mut env = dir.env();
    env.runtime_dir = None;

    let why = unavailable(local_decider(&env));

    assert!(matches!(why, Unavailable::NoSocketDir), "{why:?}");
}

/// Without nota's own program there's no launcher to tie the server to
/// nota, so it isn't started untied.
#[test]
fn no_launcher_is_unavailable() {
    let dir = TestDir::new("no-exe");
    dir.program("llama-server");
    dir.file("model.gguf", "");
    dir.settings("[local_decider]\nprogram = \"~/llama-server\"\nmodel = \"~/model.gguf\"\n");
    let mut env = dir.env();
    env.exe = Err(Arc::new(io::Error::from(io::ErrorKind::NotFound)));

    let why = unavailable(local_decider(&env));

    assert!(
        matches!(&why, Unavailable::NoLauncher(e) if e.kind() == io::ErrorKind::NotFound),
        "{why:?}"
    );
}

/// A key nota doesn't know is an error naming it, not a silent "not set
/// up": a misspelt key would otherwise hide what was meant.
#[test]
fn an_unknown_key_is_refused() {
    let dir = TestDir::new("unknown-key");
    let file = dir.settings("[local_decider]\nport = \"8080\"\n");

    let why = unavailable(local_decider(&dir.env()));

    assert!(
        matches!(&why, Unavailable::Invalid(p, w) if *p == file && w == "nota has no setting local_decider.port"),
        "{why:?}"
    );
}

/// A table nota doesn't know is refused too.
#[test]
fn an_unknown_table_is_refused() {
    assert_eq!(
        parse("[engines]\n", None),
        Err("nota has no setting engines".to_owned())
    );
}

/// A setting that isn't text is refused.
#[test]
fn a_setting_that_isnt_text_is_refused() {
    assert_eq!(
        parse("[local_decider]\nmodel = 3\n", None),
        Err("local_decider.model isn't text".to_owned())
    );
}

/// `local_decider` that isn't a table is refused.
#[test]
fn a_local_decider_that_isnt_a_table_is_refused() {
    assert_eq!(
        parse("local_decider = 1\n", None),
        Err("local_decider isn't a table".to_owned())
    );
}

/// Text that isn't TOML is refused.
#[test]
fn text_that_isnt_toml_is_refused() {
    let parsed = parse("[local_decider\n", None);

    assert!(
        parsed.as_ref().is_err_and(|e| e.starts_with("not TOML: ")),
        "{parsed:?}"
    );
}

/// A relative path would depend on the folder nota was started in, so
/// it's refused.
#[test]
fn a_relative_path_is_refused() {
    assert_eq!(
        parse("[local_decider]\nmodel = \"models/qwen.gguf\"\n", None),
        Err(
            "local_decider.model must be an absolute path or start with ~/: models/qwen.gguf"
                .to_owned()
        )
    );
}

/// A relative program path (with a `/`) is refused, unlike a bare name.
#[test]
fn a_relative_program_path_is_refused() {
    assert_eq!(
        parse("[local_decider]\nprogram = \"bin/llama-server\"\n", None),
        Err(
            "local_decider.program must be an absolute path or start with ~/: \
             bin/llama-server"
                .to_owned()
        )
    );
}

/// An empty program is refused, not looked for.
#[test]
fn an_empty_program_is_refused() {
    assert_eq!(
        parse("[local_decider]\nprogram = \"\"\n", None),
        Err("local_decider.program is empty".to_owned())
    );
}

/// `~/` without a home directory is refused.
#[test]
fn a_tilde_without_home_is_refused() {
    assert_eq!(
        parse("[local_decider]\nmodel = \"~/m.gguf\"\n", None),
        Err("local_decider.model starts with ~/ but there's no home directory".to_owned())
    );
}

/// An empty file sets nothing up.
#[test]
fn an_empty_file_sets_nothing() {
    assert_eq!(
        parse("", None),
        Ok(Written {
            program: None,
            model: None,
            device: Device::Cpu,
            socket_dir: None,
        })
    );
}

/// A settings file over the size limit isn't read.
#[test]
fn a_huge_settings_file_is_refused() {
    let dir = TestDir::new("huge");
    let pad = usize::try_from(MAX_LEN).unwrap() + 1;
    let file = dir.settings(&"#".repeat(pad));

    let why = unavailable(local_decider(&dir.env()));

    assert!(
        matches!(&why, Unavailable::TooLarge(p) if *p == file),
        "{why:?}"
    );
}

/// A settings file over the limit is too big even when the limit cuts
/// through a character.
#[test]
fn a_huge_file_cut_through_a_character_is_too_big() {
    let dir = TestDir::new("huge-utf8");
    let file = dir.settings(&"é".repeat(usize::try_from(MAX_LEN).unwrap()));

    let why = unavailable(local_decider(&dir.env()));

    assert!(
        matches!(&why, Unavailable::TooLarge(p) if *p == file),
        "{why:?}"
    );
}

/// A settings file that isn't UTF-8 text is refused, naming the file.
#[test]
fn a_file_that_isnt_text_is_refused() {
    let dir = TestDir::new("not-utf8");
    let file = dir.0.join(FILE);
    write_bytes(&file, &[0xff, 0xfe]);

    let why = unavailable(local_decider(&dir.env()));

    assert!(
        matches!(&why, Unavailable::Invalid(p, w) if *p == file && w == "not UTF-8 text"),
        "{why:?}"
    );
}

/// Writes `bytes` to `path`.
#[expect(clippy::disallowed_methods, reason = "test scaffolding")]
fn write_bytes(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
}

/// A settings file exactly at the limit is read.
#[test]
fn a_settings_file_at_the_limit_is_read() {
    let dir = TestDir::new("at-limit");
    let pad = usize::try_from(MAX_LEN).unwrap();
    dir.settings(&"#".repeat(pad));

    assert!(matches!(local_decider(&dir.env()), LocalDecider::NotSetUp));
}

/// A settings path that can't be read as a file is reported with its
/// cause.
#[test]
fn an_unreadable_settings_file_is_reported() {
    let dir = TestDir::new("unreadable");
    dir.folder(FILE);

    let why = unavailable(local_decider(&dir.env()));

    assert!(
        matches!(&why, Unavailable::Unreadable(p, _) if *p == dir.0.join(FILE)),
        "{why:?}"
    );
    assert!(why.source().is_some());
}

/// With no home directory there's no settings file to read.
#[test]
fn no_settings_path_is_unavailable() {
    let dir = TestDir::new("no-path");
    let mut env = dir.env();
    env.settings = None;

    assert!(matches!(
        unavailable(local_decider(&env)),
        Unavailable::NoSettingsFile
    ));
}

/// This process's environment finds its settings in nota's config folder.
#[test]
fn the_current_environment_reads_nota_s_settings_file() {
    let env = Environment::current();

    if let Some(settings) = env.settings {
        assert!(
            settings.ends_with("nota/settings.toml"),
            "{}",
            settings.display()
        );
    }
    assert!(env.exe.is_ok());
    // The socket's default folder is nota's, under XDG_RUNTIME_DIR, which
    // only Linux reads, and only when it's absolute.
    let run = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    if let Some(run) = run.filter(|run| cfg!(target_os = "linux") && run.is_absolute()) {
        assert_eq!(env.runtime_dir, Some(run.join("nota")));
    }
}
