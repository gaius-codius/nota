//! A scratch directory for tests on the real filesystem.

use std::path::PathBuf;

/// A fresh directory under the system temp dir, removed when dropped.
#[derive(Debug)]
pub(crate) struct TestDir(pub(crate) PathBuf);

impl TestDir {
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding outside the recorder's write path"
    )]
    pub(crate) fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nota-recorder-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
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
