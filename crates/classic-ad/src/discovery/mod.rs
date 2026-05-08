//! Hardware-discovery probes plus the `Sysroot` abstraction they read
//! through. Tests inject a tempdir-backed sysroot fixture; production code
//! uses `RealSysroot` against `/`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub mod cpu;
pub mod gpu;
pub mod load;
pub mod mem;
pub mod numa;
pub mod pci;

/// File-system view used by every probe. Paths passed in are relative to
/// the sysroot root (e.g. `proc/cpuinfo`, `sys/bus/pci/devices`); the impl
/// is free to anchor them anywhere on disk. `read_dir` returns directory
/// entry names (basenames), not full paths, so callers can recompose them
/// against the relative parent.
pub trait Sysroot: Send + Sync {
    fn read(&self, rel: &Path) -> io::Result<Vec<u8>>;
    fn read_link(&self, rel: &Path) -> io::Result<PathBuf>;
    fn read_dir(&self, rel: &Path) -> io::Result<Vec<String>>;
}

/// Sysroot anchored at `/`. The default everywhere except tests.
pub struct RealSysroot;

impl RealSysroot {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RealSysroot {
    fn default() -> Self {
        Self::new()
    }
}

impl Sysroot for RealSysroot {
    fn read(&self, rel: &Path) -> io::Result<Vec<u8>> {
        fs::read(Path::new("/").join(rel))
    }
    fn read_link(&self, rel: &Path) -> io::Result<PathBuf> {
        fs::read_link(Path::new("/").join(rel))
    }
    fn read_dir(&self, rel: &Path) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(Path::new("/").join(rel))? {
            let entry = entry?;
            if let Some(s) = entry.file_name().to_str() {
                out.push(s.to_string());
            }
        }
        Ok(out)
    }
}

/// Tempdir-backed `Sysroot` for tests. Public so probe tests across module
/// boundaries can share it.
#[cfg(test)]
pub(crate) struct TempdirSysroot {
    pub root: tempfile::TempDir,
}

#[cfg(test)]
impl TempdirSysroot {
    pub fn new() -> Self {
        Self { root: tempfile::tempdir().unwrap() }
    }
    pub fn path(&self) -> &Path {
        self.root.path()
    }
    /// Convenience: write `contents` to `<root>/<rel>`, creating parent dirs.
    pub fn write(&self, rel: impl AsRef<Path>, contents: impl AsRef<[u8]>) {
        let abs = self.path().join(rel.as_ref());
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(abs, contents).unwrap();
    }
    pub fn symlink(&self, rel: impl AsRef<Path>, target: impl AsRef<Path>) {
        let abs = self.path().join(rel.as_ref());
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        std::os::unix::fs::symlink(target.as_ref(), abs).unwrap();
    }
}

#[cfg(test)]
impl Sysroot for TempdirSysroot {
    fn read(&self, rel: &Path) -> io::Result<Vec<u8>> {
        fs::read(self.path().join(rel))
    }
    fn read_link(&self, rel: &Path) -> io::Result<PathBuf> {
        fs::read_link(self.path().join(rel))
    }
    fn read_dir(&self, rel: &Path) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.path().join(rel))? {
            let entry = entry?;
            if let Some(s) = entry.file_name().to_str() {
                out.push(s.to_string());
            }
        }
        Ok(out)
    }
}

/// Read `rel` and decode as UTF-8, returning a trimmed `String`. Many
/// `/proc` and `/sys` files are single-line; this is the common case.
pub(crate) fn read_string<S: Sysroot + ?Sized>(sr: &S, rel: &Path) -> io::Result<String> {
    let bytes = sr.read(rel)?;
    let s = String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(s.trim().to_string())
}
