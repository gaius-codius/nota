use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;

use nota_core::{FakeClock, SessionTime};

use super::*;
use crate::pack::{GlossaryAbout, glossary};

/// A fresh directory under the system temp dir, removed when dropped.
struct TestDir(PathBuf);

impl TestDir {
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding outside the recorder's write path"
    )]
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nota-sources-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// Writes `text` to `name` in the folder, returning its path.
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding outside the recorder's write path"
    )]
    fn file(&self, name: &str, text: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    /// An environment reading `settings.toml` in the folder.
    fn env(&self) -> Environment {
        Environment {
            settings: Some(self.0.join("settings.toml")),
            home: Some(self.0.clone()),
            runtime_dir: Some(self.0.join("run")),
            path: None,
            exe: Ok(PathBuf::from("/usr/bin/nota")),
        }
    }
}

impl Drop for TestDir {
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding outside the recorder's write path"
    )]
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A clock for deciders that are never asked.
fn clock() -> Arc<dyn Clock> {
    Arc::new(FakeClock::new(SessionTime::ZERO))
}

/// What a setup is, without its decider.
fn shown(setup: Setup) -> Result<DecidedBy, Option<String>> {
    match setup {
        Setup::NotSetUp => Err(None),
        Setup::Unavailable(why) => Err(Some(why)),
        Setup::Ready(_, by) => Ok(by),
    }
}

/// A model is recorded by its file's name, without the folder.
#[test]
fn a_model_is_named_by_its_file() {
    assert_eq!(
        model_name(Path::new("/models/qwen-4b.gguf")),
        "qwen-4b.gguf"
    );
    assert_eq!(model_name(Path::new("/")), "/");
}

/// No settings file: no decider is set up. One that doesn't parse: the
/// decider can't be used, and says why. A program and a model: a local
/// decider, recorded by the model's file name.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "test scaffolding outside the recorder's write path"
)]
fn the_settings_give_the_decider() {
    let dir = TestDir::new("decider");
    assert_eq!(shown(local_decider(&dir.env(), &clock())), Err(None));
    dir.file("settings.toml", "[local_decider\n");
    assert!(matches!(
        shown(local_decider(&dir.env(), &clock())),
        Err(Some(why)) if why.starts_with("the decider can't be used: ")
    ));
    let program = dir.file("llama-server", "#!/bin/sh\n");
    std::fs::set_permissions(&program, Permissions::from_mode(0o755)).unwrap();
    let model = dir.file("qwen.gguf", "");
    dir.file(
        "settings.toml",
        &format!(
            "[local_decider]\nprogram = '{}'\nmodel = '{}'\n",
            program.display(),
            model.display()
        ),
    );
    assert_eq!(
        shown(local_decider(&dir.env(), &clock())),
        Ok(DecidedBy {
            name: "llama-server".to_owned(),
            model: "qwen.gguf".to_owned(),
            place: Place::Local,
        })
    );
}

/// A glossary pack named `name`.
fn pack(name: &str) -> Pack {
    glossary(
        "amlodipine",
        &GlossaryAbout {
            name: name.to_owned(),
            version: "1".to_owned(),
            language: "en".to_owned(),
            author: "a tester".to_owned(),
        },
    )
    .unwrap()
}

/// The installed packs are the folder's valid `.pack` files, in file
/// name order: other files, and a `.pack` that isn't one, are passed
/// over. No folder means none installed; a folder that can't be listed is
/// an error.
#[test]
fn the_installed_packs_are_the_folders_valid_pack_files() {
    let dir = TestDir::new("packs");
    let packs = dir.0.join(PACKS);
    assert_eq!(installed(&packs).unwrap(), []);
    dir.file(PACKS, "not a folder");
    assert!(installed(&packs).is_err());
    let packs = dir.0.join("installed");
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding outside the recorder's write path"
    )]
    std::fs::create_dir(&packs).unwrap();
    pack("second").write(&packs.join("b.pack")).unwrap();
    pack("first").write(&packs.join("a.pack")).unwrap();
    pack("other").write(&packs.join("c.txt")).unwrap();
    dir.file("installed/broken.pack", "not a pack");
    let names: Vec<String> = installed(&packs)
        .unwrap()
        .iter()
        .map(|p| p.pack_ref().name().to_owned())
        .collect();
    assert_eq!(names, ["first", "second"]);
}
