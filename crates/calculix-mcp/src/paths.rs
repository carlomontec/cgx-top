//! Path confinement: every path a tool touches must resolve inside an allowed root.

use crate::error::ToolError;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Roots(Vec<PathBuf>);

impl Roots {
    /// Canonicalise the allowed roots. At least one is required.
    pub fn new(roots: Vec<PathBuf>) -> std::io::Result<Self> {
        let roots = roots.iter().map(|r| r.canonicalize()).collect::<std::io::Result<Vec<_>>>()?;
        if roots.is_empty() {
            return Err(std::io::Error::other("no root directory"));
        }
        Ok(Self(roots))
    }

    pub fn first(&self) -> &Path {
        &self.0[0]
    }

    /// Path shown to the model: relative to the root that contains it.
    pub fn display(&self, path: &Path) -> String {
        self.0
            .iter()
            .find_map(|r| path.strip_prefix(r).ok())
            .unwrap_or(path)
            .display()
            .to_string()
    }

    fn absolute(&self, raw: &str) -> Result<PathBuf, ToolError> {
        if raw.is_empty() || raw.contains('\0') || raw.len() > 4096 {
            return Err(ToolError::invalid("path is empty, too long or contains NUL"));
        }
        let p = Path::new(raw);
        Ok(if p.is_absolute() { p.to_path_buf() } else { self.first().join(p) })
    }

    fn confine(&self, canonical: PathBuf, exts: &[&str]) -> Result<PathBuf, ToolError> {
        if !self.0.iter().any(|r| canonical.starts_with(r)) {
            return Err(ToolError::new("PATH_OUTSIDE_ROOTS", format!("{} is outside the allowed roots", canonical.display()))
                .valid(self.0.iter().map(|r| r.display().to_string()).collect()));
        }
        if !exts.is_empty() && !has_ext(&canonical, exts) {
            return Err(ToolError::new("BAD_EXTENSION", format!("expected a file ending in {}", exts.join(" or ")))
                .valid(siblings(&canonical, exts)));
        }
        Ok(canonical)
    }

    /// Resolve an existing file. `exts` are lower-case extensions without the dot; empty = any.
    pub fn existing(&self, raw: &str, exts: &[&str]) -> Result<PathBuf, ToolError> {
        let abs = self.absolute(raw)?;
        match abs.canonicalize() {
            Ok(c) if c.is_file() => self.confine(c, exts),
            _ => {
                // List alternatives only when the nearest existing parent is itself allowed.
                let parent = abs.ancestors().skip(1).find_map(|a| a.canonicalize().ok());
                let valid = match parent {
                    Some(dir) if self.0.iter().any(|r| dir.starts_with(r)) => siblings(&dir.join("x"), exts),
                    _ => Vec::new(),
                };
                Err(ToolError::new("FILE_NOT_FOUND", format!("no such file: {}", raw)).valid(valid))
            }
        }
    }

    /// Resolve a file to be created: the parent must exist inside a root.
    pub fn creatable(&self, raw: &str, exts: &[&str]) -> Result<PathBuf, ToolError> {
        let abs = self.absolute(raw)?;
        let name = abs.file_name().ok_or_else(|| ToolError::invalid("path has no file name"))?.to_owned();
        let parent = abs
            .parent()
            .and_then(|p| p.canonicalize().ok())
            .ok_or_else(|| ToolError::new("FILE_NOT_FOUND", format!("parent directory of {} does not exist", raw)))?;
        self.confine(parent.join(name), exts)
    }
}

fn has_ext(path: &Path, exts: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| exts.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// Files with an accepted extension next to `path`, sorted.
fn siblings(path: &Path, exts: &[&str]) -> Vec<String> {
    let Some(dir) = path.parent() else { return Vec::new() };
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && (exts.is_empty() || has_ext(p, exts)))
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("calculix-mcp-paths-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("root/sub")).unwrap();
        std::fs::write(dir.join("root/a.frd"), "x").unwrap();
        std::fs::write(dir.join("root/b.inp"), "x").unwrap();
        std::fs::write(dir.join("outside.frd"), "x").unwrap();
        dir
    }

    #[test]
    fn confines_and_lists_alternatives() {
        let dir = sandbox("confine");
        let roots = Roots::new(vec![dir.join("root")]).unwrap();

        assert!(roots.existing("a.frd", &["frd"]).is_ok());
        assert_eq!(roots.existing("../outside.frd", &["frd"]).unwrap_err().code, "PATH_OUTSIDE_ROOTS");
        assert_eq!(roots.existing(dir.join("outside.frd").to_str().unwrap(), &["frd"]).unwrap_err().code, "PATH_OUTSIDE_ROOTS");
        assert_eq!(roots.existing("b.inp", &["frd"]).unwrap_err().code, "BAD_EXTENSION");

        let missing = roots.existing("nope.frd", &["frd"]).unwrap_err();
        assert_eq!((missing.code, missing.valid.as_slice()), ("FILE_NOT_FOUND", &["a.frd".to_string()][..]));
        // A missing file outside the roots must not leak a directory listing.
        assert!(roots.existing("../nope.frd", &["frd"]).unwrap_err().valid.is_empty());

        assert!(roots.creatable("sub/new.inp", &["inp"]).is_ok());
        assert_eq!(roots.creatable("../new.inp", &["inp"]).unwrap_err().code, "PATH_OUTSIDE_ROOTS");
        assert_eq!(roots.creatable("missing_dir/new.inp", &["inp"]).unwrap_err().code, "FILE_NOT_FOUND");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escaping_the_root_is_rejected() {
        let dir = sandbox("symlink");
        std::os::unix::fs::symlink(dir.join("outside.frd"), dir.join("root/link.frd")).unwrap();
        let roots = Roots::new(vec![dir.join("root")]).unwrap();
        assert_eq!(roots.existing("link.frd", &["frd"]).unwrap_err().code, "PATH_OUTSIDE_ROOTS");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
