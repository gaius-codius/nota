//! The user's settings: where the local decider's llama-server and model
//! are, and which device it runs on, and how strongly the hotword pass
//! boosts the session's terms. Until nota downloads its own server and
//! model (M7), nothing else says where they are.
//!
//! They're in `settings.toml` in nota's config directory
//! (`~/.config/nota/settings.toml` on Linux). The user's guide to it is
//! `docs/settings.md`, which the tests hold to this module: its example
//! must make a ready decider, and it must name every key and message.
//!
//! ```toml
//! [local_decider]
//! program = "~/llama.cpp/build/bin/llama-server"  # or a bare name on PATH
//! model = "~/models/qwen.gguf"
//! device = "Vulkan0"   # one GPU, as `llama-server --list-devices` names
//!                      # it, or "cpu"; without it, the CPU
//! socket_dir = "/run/user/1000/nota"  # without it, nota's folder under
//!                                     # $XDG_RUNTIME_DIR
//!
//! [hotword_pass]
//! boost = 3.0   # above 0 and at most 10; without it, 3
//! ```
//!
//! [`local_decider`] reads them into what the clean-up job and the engines
//! screen need to know: [`LocalDecider::NotSetUp`] with no program or no
//! model, [`LocalDecider::Unavailable`] with the reason (a path that holds
//! nothing, a file that doesn't parse) or [`LocalDecider::Ready`] with the
//! server's [`ServerConfig`], started through `nota engine llama-server`
//! so it ends with nota. Only `Ready` has a config to start a server with.
//! It looks at the files each time it's called, so a model installed while
//! nota is open is seen at the next job. [`hotword_boost`] reads the
//! hotword pass's boost the same way, at each run.
//!
//! The file is the user's own text, so it's parsed at the edge: a key
//! nota doesn't know, a path that isn't absolute (or `~/…`), or a device
//! that isn't one is an error naming the file, never a silent "not set
//! up".

use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Read as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nota_llm::server::{Device, GpuName, Launcher, ServerConfig};

use crate::hotword_pass::Boost;

#[cfg(test)]
mod tests;

/// The settings file's name in nota's config directory.
const FILE: &str = "settings.toml";

/// The longest settings file read: it's a few lines.
const MAX_LEN: u64 = 64 * 1024;

/// The table that holds the local decider's settings.
const TABLE: &str = "local_decider";

/// The table that holds the hotword pass's settings.
const HOTWORD_TABLE: &str = "hotword_pass";

/// The keys the hotword pass's table may hold.
const HOTWORD_KEYS: [&str; 1] = ["boost"];

/// The keys the local decider's table may hold.
const KEYS: [&str; 4] = ["program", "model", "device", "socket_dir"];

/// What nota is given to find the local decider: the settings file and the
/// parts of its environment that the settings lean on. Passed in, so tests
/// can give their own.
#[derive(Debug)]
pub(crate) struct Environment {
    /// The settings file; `None` without a home directory to find it in.
    pub(crate) settings: Option<PathBuf>,
    /// The home directory, which `~/` in a path stands for.
    pub(crate) home: Option<PathBuf>,
    /// nota's folder in `$XDG_RUNTIME_DIR`, the socket's default folder.
    pub(crate) runtime_dir: Option<PathBuf>,
    /// `PATH`, where a bare program name is looked for.
    pub(crate) path: Option<OsString>,
    /// nota's own executable, which launches the server tied to nota.
    pub(crate) exe: Result<PathBuf, Arc<io::Error>>,
}

impl Environment {
    /// This process's environment: the user's settings file, home and
    /// runtime directory, `PATH`, and the running `nota`.
    pub(crate) fn current() -> Self {
        let dirs = directories::ProjectDirs::from("", "", "nota");
        Self {
            settings: dirs.as_ref().map(|d| d.config_dir().join(FILE)),
            home: directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()),
            runtime_dir: dirs
                .as_ref()
                .and_then(|d| d.runtime_dir().map(Path::to_path_buf)),
            path: std::env::var_os("PATH"),
            exe: std::env::current_exe().map_err(Arc::new),
        }
    }
}

/// The local decider, as the settings leave it.
#[derive(Debug)]
pub(crate) enum LocalDecider {
    /// The settings name no program or no model: the engines screen shows
    /// it as not set up, and no server is started.
    NotSetUp,
    /// It's set up but can't be used now, and why.
    Unavailable(Unavailable),
    /// It can be started with this config, through nota's launcher.
    Ready(Box<ServerConfig>),
}

/// Why the local decider can't be used. Nothing was started.
#[derive(Debug)]
pub(crate) enum Unavailable {
    /// There's no home directory, so no settings file to read.
    NoSettingsFile,
    /// The settings file is there but couldn't be read: its path.
    Unreadable(PathBuf, io::Error),
    /// The settings file is bigger than any settings file: its path.
    TooLarge(PathBuf),
    /// The settings file doesn't say what nota reads: its path, and what's
    /// wrong.
    Invalid(PathBuf, String),
    /// The program named by path isn't there, or isn't a program: the path
    /// looked at.
    ProgramMissing(PathBuf),
    /// The program named bare isn't in any `PATH` folder: the name.
    ProgramNotOnPath(OsString),
    /// The model file isn't there: the path looked at.
    ModelMissing(PathBuf),
    /// No socket folder is set and there's no `$XDG_RUNTIME_DIR`.
    NoSocketDir,
    /// nota's own executable couldn't be found, to launch the server with.
    NoLauncher(Arc<io::Error>),
}

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSettingsFile => write!(f, "no home directory to find the settings in"),
            Self::Unreadable(path, e) => {
                write!(f, "the settings {} can't be read: {e}", path.display())
            }
            Self::TooLarge(path) => write!(
                f,
                "the settings {} are too big: over {MAX_LEN} bytes",
                path.display()
            ),
            Self::Invalid(path, why) => {
                write!(f, "the settings {} aren't usable: {why}", path.display())
            }
            Self::ProgramMissing(path) => {
                write!(f, "there's no llama-server to run at {}", path.display())
            }
            Self::ProgramNotOnPath(name) => {
                write!(f, "{} isn't in any PATH folder", name.display())
            }
            Self::ModelMissing(path) => write!(f, "the model isn't at {}", path.display()),
            Self::NoSocketDir => write!(
                f,
                "no folder for the server's socket: set socket_dir, or XDG_RUNTIME_DIR"
            ),
            Self::NoLauncher(e) => write!(f, "nota's own program can't be found: {e}"),
        }
    }
}

impl Error for Unavailable {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Unreadable(_, e) => Some(e),
            Self::NoLauncher(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}

/// The local decider's settings as written, before anything is looked at.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Written {
    /// `program`: a path, or a bare name to look for on `PATH`.
    program: Option<PathBuf>,
    /// `model`: the model file.
    model: Option<PathBuf>,
    /// `device`; the CPU if it isn't set.
    device: Device,
    /// `socket_dir`, if set.
    socket_dir: Option<PathBuf>,
}

/// The local decider as the settings in `env` leave it, looking at the
/// files they name now.
pub(crate) fn local_decider(env: &Environment) -> LocalDecider {
    let Some(file) = &env.settings else {
        return LocalDecider::Unavailable(Unavailable::NoSettingsFile);
    };
    let written = match read(file) {
        Ok(Some(text)) => parse(&text, env.home.as_deref()),
        // No file: nothing is set up.
        Ok(None) => return LocalDecider::NotSetUp,
        Err(why) => return LocalDecider::Unavailable(why),
    };
    match written {
        Ok(written) => ready(written, env),
        Err(why) => LocalDecider::Unavailable(Unavailable::Invalid(file.clone(), why)),
    }
}

/// The settings file's text, `None` if there's no file.
fn read(path: &Path) -> Result<Option<String>, Unavailable> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Unavailable::Unreadable(path.to_path_buf(), e)),
    };
    // Bytes first: a cut through a character mustn't read as broken text.
    let mut bytes = Vec::new();
    file.take(MAX_LEN + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Unavailable::Unreadable(path.to_path_buf(), e))?;
    if u64::try_from(bytes.len()).map_or(true, |len| len > MAX_LEN) {
        return Err(Unavailable::TooLarge(path.to_path_buf()));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| Unavailable::Invalid(path.to_path_buf(), "not UTF-8 text".to_owned()))?;
    Ok(Some(text))
}

/// A settings file's `text` as TOML, if it holds no table nota doesn't
/// know.
fn tables(text: &str) -> Result<toml::Table, String> {
    let file: toml::Table = text.parse().map_err(|e| format!("not TOML: {e}"))?;
    if let Some(key) = file.keys().find(|k| *k != TABLE && *k != HOTWORD_TABLE) {
        return Err(format!("nota has no setting {key}"));
    }
    Ok(file)
}

/// The table `name` in `file` (empty if it isn't there), if it holds only
/// `keys`.
fn table<'f>(
    file: &'f toml::Table,
    name: &str,
    keys: &[&str],
) -> Result<Option<&'f toml::Table>, String> {
    let table = match file.get(name) {
        None => return Ok(None),
        Some(toml::Value::Table(t)) => t,
        Some(_) => return Err(format!("{name} isn't a table")),
    };
    if let Some(key) = table.keys().find(|k| !keys.contains(&k.as_str())) {
        return Err(format!("nota has no setting {name}.{key}"));
    }
    Ok(Some(table))
}

/// The local decider's settings in a settings file's `text`, with `~/`
/// read as `home`.
fn parse(text: &str, home: Option<&Path>) -> Result<Written, String> {
    let file = tables(text)?;
    let empty = toml::Table::new();
    let table = table(&file, TABLE, &KEYS)?.unwrap_or(&empty);
    Ok(Written {
        program: text_of(table, "program")?
            .map(|p| program(p, home))
            .transpose()?,
        model: text_of(table, "model")?
            .map(|p| absolute("model", p, home))
            .transpose()?,
        device: text_of(table, "device")?.map_or(Ok(Device::Cpu), device)?,
        socket_dir: text_of(table, "socket_dir")?
            .map(|p| absolute("socket_dir", p, home))
            .transpose()?,
    })
}

/// The text of `key` in `table`, if it's there.
fn text_of<'t>(table: &'t toml::Table, key: &str) -> Result<Option<&'t str>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(format!("{TABLE}.{key} isn't text")),
    }
}

/// `program` as written: a bare name stays one, to look for on `PATH`;
/// anything with a `/` is a path, which must be absolute or `~/…`.
fn program(text: &str, home: Option<&Path>) -> Result<PathBuf, String> {
    if text.is_empty() {
        return Err(format!("{TABLE}.program is empty"));
    }
    if text.contains('/') {
        absolute("program", text, home)
    } else {
        Ok(PathBuf::from(text))
    }
}

/// The path `text` given for `key`: absolute, or `~/…` under `home`.
fn absolute(key: &str, text: &str, home: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(rest) = text.strip_prefix("~/") {
        return home
            .map(|home| home.join(rest))
            .ok_or_else(|| format!("{TABLE}.{key} starts with ~/ but there's no home directory"));
    }
    let path = PathBuf::from(text);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(format!(
            "{TABLE}.{key} must be an absolute path or start with ~/: {text}"
        ))
    }
}

/// The device named `text`: `cpu`, or one GPU's name.
fn device(text: &str) -> Result<Device, String> {
    if text.trim().eq_ignore_ascii_case("cpu") {
        return Ok(Device::Cpu);
    }
    GpuName::new(text).map(Device::Gpu).ok_or_else(|| {
        format!("{TABLE}.device must be cpu or one GPU's name, as llama-server lists it: {text}")
    })
}

/// The decider for `written`, once the files it names are found.
fn ready(written: Written, env: &Environment) -> LocalDecider {
    let (Some(program), Some(model)) = (written.program, written.model) else {
        return LocalDecider::NotSetUp;
    };
    match config(&program, model, written.device, written.socket_dir, env) {
        Ok(config) => LocalDecider::Ready(Box::new(config)),
        Err(why) => LocalDecider::Unavailable(why),
    }
}

/// The server's config, with nota's launcher, once the program and model
/// are found and there's a socket folder.
fn config(
    program: &Path,
    model: PathBuf,
    device: Device,
    socket_dir: Option<PathBuf>,
    env: &Environment,
) -> Result<ServerConfig, Unavailable> {
    let program = find_program(program, env.path.as_deref())?;
    if !model.is_file() {
        return Err(Unavailable::ModelMissing(model));
    }
    let socket_dir = socket_dir
        .or_else(|| env.runtime_dir.clone())
        .ok_or(Unavailable::NoSocketDir)?;
    let exe = env.exe.clone().map_err(Unavailable::NoLauncher)?;
    let mut config = ServerConfig::new(program, model, device, socket_dir);
    config.launcher = Some(Launcher {
        program: exe,
        args: vec!["engine".into(), "llama-server".into()],
    });
    Ok(config)
}

/// The program `program` names: itself if it's a path, else the first
/// `PATH` folder's program of that name.
fn find_program(program: &Path, path: Option<&OsStr>) -> Result<PathBuf, Unavailable> {
    if program.is_absolute() {
        return if is_program(program) {
            Ok(program.to_path_buf())
        } else {
            Err(Unavailable::ProgramMissing(program.to_path_buf()))
        };
    }
    std::env::split_paths(path.unwrap_or_default())
        // An empty or relative PATH entry would be the working folder.
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(program))
        .find(|candidate| is_program(candidate))
        .ok_or_else(|| Unavailable::ProgramNotOnPath(program.as_os_str().to_owned()))
}

/// Whether `path` is a file someone may run.
fn is_program(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The hotword pass's boost as the settings in `env` leave it: the
/// setting `[hotword_pass] boost`, or [`Boost::DEFAULT`] without one (or
/// without a settings file to hold one).
///
/// # Errors
///
/// [`Unavailable::Unreadable`] or [`Unavailable::TooLarge`] if the file
/// can't be read, and [`Unavailable::Invalid`] if it doesn't parse or the
/// boost isn't a number above 0 and at most [`Boost::MAX`].
pub(crate) fn hotword_boost(env: &Environment) -> Result<Boost, Unavailable> {
    let Some(file) = &env.settings else {
        return Ok(Boost::DEFAULT);
    };
    match read(file)? {
        Some(text) => parse_boost(&text).map_err(|why| Unavailable::Invalid(file.clone(), why)),
        None => Ok(Boost::DEFAULT),
    }
}

/// The hotword pass's boost in a settings file's `text`.
fn parse_boost(text: &str) -> Result<Boost, String> {
    let file = tables(text)?;
    let Some(table) = table(&file, HOTWORD_TABLE, &HOTWORD_KEYS)? else {
        return Ok(Boost::DEFAULT);
    };
    let (score, written) = match table.get("boost") {
        None => return Ok(Boost::DEFAULT),
        Some(toml::Value::Float(score)) => (*score, score.to_string()),
        // A whole number is as good: `boost = 3` means 3. One too big for
        // an `i32` is far over the maximum, and refused as that.
        Some(toml::Value::Integer(score)) => (
            i32::try_from(*score).map_or(f64::INFINITY, f64::from),
            score.to_string(),
        ),
        Some(_) => return Err(format!("{HOTWORD_TABLE}.boost isn't a number")),
    };
    Boost::new(score).ok_or_else(|| {
        format!(
            "{HOTWORD_TABLE}.boost must be a number above 0 and at most {}: {written}",
            Boost::MAX
        )
    })
}
