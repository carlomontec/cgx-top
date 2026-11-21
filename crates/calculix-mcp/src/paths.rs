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

    /// The canonical folders; the first one hosts run directories.
    pub fn all(&self) -> &[PathBuf] {
        &self.0
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

    /// Whether an existing path resolves inside an allowed root.
    pub fn contains(&self, path: &Path) -> bool {
        path.canonicalize().is_ok_and(|c| self.0.iter().any(|r| c.starts_with(r)))
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

/// The user's home folder, canonicalised (`HOME`, or `USERPROFILE` on Windows).
pub fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).filter(|v| !v.is_empty()).and_then(|v| PathBuf::from(v).canonicalize().ok())
}

/// Folders that are never a models folder themselves (folders inside them may be).
#[cfg(not(windows))]
const SYSTEM_DIRS: &[&str] = &[
    "/Users", "/home", "/root", "/tmp", "/var", "/var/tmp", "/private", "/private/var", "/private/tmp", "/Volumes",
    "/mnt", "/media", "/opt", "/srv", "/run", "/snap", "/Applications", "/Library", "/Network", "/cores",
];
/// Folders that are refused together with everything inside them.
#[cfg(not(windows))]
const SYSTEM_TREES: &[&str] = &[
    "/System", "/bin", "/sbin", "/usr", "/etc", "/private/etc", "/dev", "/proc", "/sys", "/boot", "/lib", "/lib32",
    "/lib64", "/libx32",
];
/// Windows: compared case-insensitively with the path stripped of its drive (`C:`).
#[cfg(windows)]
const SYSTEM_DIRS: &[&str] = &[r"\Users", r"\Users\Public", r"\Users\Default"];
#[cfg(windows)]
const SYSTEM_TREES: &[&str] =
    &[r"\Windows", r"\Program Files", r"\Program Files (x86)", r"\ProgramData", r"\$Recycle.Bin", r"\System Volume Information"];

/// A comparable spelling of a path: no `\\?\` prefix, no trailing separator; on Windows without
/// the drive letter, with `\` separators and in lower case. Lower case on macOS too, whose
/// default file system ignores case.
fn comparable(path: &Path) -> String {
    let mut s = path.to_string_lossy().into_owned();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        s = rest.to_string();
    }
    if cfg!(windows) {
        s = s.replace('/', r"\");
        if s.len() >= 2 && s.as_bytes()[1] == b':' {
            s = s[2..].to_string();
        }
    }
    while s.len() > 1 && (s.ends_with('/') || s.ends_with('\\')) {
        s.pop();
    }
    if cfg!(any(windows, target_os = "macos")) {
        s = s.to_lowercase();
    }
    s
}

/// Why the canonical `path` can never be a models folder, or None when it can be: the root of
/// a file system (`/`, `C:\`), the user's home folder itself, or a system folder.
pub fn refusal(path: &Path, home: Option<&Path>) -> Option<&'static str> {
    if path.parent().is_none() {
        return Some("it is the root of the file system");
    }
    let c = comparable(path);
    if home.is_some_and(|h| comparable(h) == c) {
        return Some("it is the home folder itself; use a project folder inside it");
    }
    let norm = |d: &str| comparable(Path::new(d));
    let in_tree = |d: &str| {
        let d = norm(d);
        c == d || c.strip_prefix(d.as_str()).is_some_and(|rest| rest.starts_with(['/', '\\']))
    };
    if SYSTEM_DIRS.iter().any(|d| c == norm(d)) || SYSTEM_TREES.iter().any(|d| in_tree(d)) {
        return Some("it is a system folder");
    }
    None
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
