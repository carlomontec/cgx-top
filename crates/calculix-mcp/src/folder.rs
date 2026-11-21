//! The models folder: where the user's decks, meshes and results live, and the folder every tool
//! resolves relative paths against.
//!
//! Resolution order: `--root` flags (fixed for the session), else the client's MCP roots (fetched
//! after initialisation and on `notifications/roots/list_changed`), else the process's current
//! directory. The root of a file system, the home folder itself and system folders are never used.
//! The `models_folder` tool shows the active folder and switches it.

use crate::error::ToolError;
use crate::paths::{refusal, Roots};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::watch;

pub const NO_FOLDER_HINT: &str = "Ask the user which folder holds the models, then call models_folder with its absolute path.";

/// Where the active models folder came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// `--root` flags on the server command line
    Flag,
    /// The workspace folders the MCP client reported (`roots/list`)
    ClientRoots,
    /// The directory the client started the server in
    Cwd,
    /// Chosen with the models_folder tool
    Tool,
    /// No usable folder
    None,
}

#[derive(Debug, Clone)]
struct Active {
    roots: Option<Arc<Roots>>,
    source: Source,
    /// Why there is no folder (only when `roots` is None)
    why_none: String,
}

/// Shared, swappable models-folder state. Read at the start of each tool call.
pub struct ModelsFolder {
    active: RwLock<Active>,
    /// Canonical `--root` folders as given (also refused ones: narrowing inside them is allowed)
    fixed: Vec<PathBuf>,
    /// Canonical `--allow` folders: models_folder may only switch inside them (empty = anywhere)
    allow: Vec<PathBuf>,
    cwd: Option<PathBuf>,
    home: Option<PathBuf>,
    /// false while the client's first roots/list answer is awaited
    settled: watch::Sender<bool>,
}

/// What a change did, for logging and for activating the new folder's run directories.
#[derive(Debug, Clone)]
pub struct Change {
    pub roots: Option<Arc<Roots>>,
    pub source: Source,
}

impl Change {
    pub fn describe(&self) -> String {
        match &self.roots {
            Some(r) => format!("{} (source {})", join(r.all()), source_name(self.source)),
            None => "none".to_string(),
        }
    }
}

pub fn source_name(s: Source) -> &'static str {
    match s {
        Source::Flag => "flag",
        Source::ClientRoots => "client_roots",
        Source::Cwd => "cwd",
        Source::Tool => "tool",
        Source::None => "none",
    }
}

fn join(paths: &[PathBuf]) -> String {
    paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
}

fn inside(path: &Path, parents: &[PathBuf]) -> bool {
    parents.iter().any(|p| path.starts_with(p))
}

/// Decode a `file://` URI to a path; None for other schemes or malformed escapes.
pub fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://").or_else(|| uri.strip_prefix("FILE://"))?;
    // Host part: empty or localhost; anything else (a network share) is not supported.
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = rest.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let mut s = String::from_utf8(out).ok()?;
    // file:///C:/x -> C:/x on Windows.
    let b = s.as_bytes();
    if cfg!(windows) && b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        s.remove(0);
    }
    Some(PathBuf::from(s))
}

impl ModelsFolder {
    /// Resolve the startup folder from `--root` flags (`fixed`), `--allow` folders and the
    /// current directory. Missing `--root` or `--allow` folders are an error, as before.
    pub fn new(fixed: Vec<PathBuf>, allow: Vec<PathBuf>, cwd: Option<PathBuf>, home: Option<PathBuf>) -> anyhow::Result<Self> {
        let canon = |flag: &str, list: Vec<PathBuf>| -> anyhow::Result<Vec<PathBuf>> {
            list.iter()
                .map(|p| {
                    let c = p.canonicalize().map_err(|e| anyhow::anyhow!("{} {}: {}", flag, p.display(), e))?;
                    anyhow::ensure!(c.is_dir(), "{} {} is not a directory", flag, c.display());
                    Ok(c)
                })
                .collect()
        };
        let fixed = canon("--root", fixed)?;
        let allow = canon("--allow", allow)?;
        let cwd = cwd.and_then(|c| c.canonicalize().ok());
        let folder = Self {
            active: RwLock::new(Active { roots: None, source: Source::None, why_none: String::new() }),
            fixed,
            allow,
            cwd,
            home,
            settled: watch::channel(true).0,
        };
        let start = if folder.is_fixed() { folder.resolve_flags() } else { folder.resolve_cwd() };
        *folder.active.write().unwrap() = start;
        Ok(folder)
    }

    pub fn is_fixed(&self) -> bool {
        !self.fixed.is_empty()
    }

    fn refused(&self, p: &Path) -> Option<&'static str> {
        refusal(p, self.home.as_deref())
    }

    fn none(why: String) -> Active {
        Active { roots: None, source: Source::None, why_none: why }
    }

    fn some(folders: Vec<PathBuf>, source: Source) -> Active {
        // The folders are canonical existing directories already.
        match Roots::new(folders) {
            Ok(r) => Active { roots: Some(Arc::new(r)), source, why_none: String::new() },
            Err(e) => Self::none(e.to_string()),
        }
    }

    fn resolve_flags(&self) -> Active {
        let usable: Vec<PathBuf> = self.fixed.iter().filter(|p| self.refused(p).is_none()).cloned().collect();
        if usable.is_empty() {
            let why = self.fixed.iter().map(|p| format!("{}: {}", p.display(), self.refused(p).unwrap_or("unusable"))).collect::<Vec<_>>();
            return Self::none(format!("no --root folder is usable ({}); a folder inside one of them can be chosen", why.join("; ")));
        }
        Self::some(usable, Source::Flag)
    }

    fn resolve_cwd(&self) -> Active {
        match &self.cwd {
            None => Self::none("the server's current directory is unknown".into()),
            Some(c) => match self.refused(c) {
                Some(why) => Self::none(format!("the server was started in {}, which is never used because {}", c.display(), why)),
                None => Self::some(vec![c.clone()], Source::Cwd),
            },
        }
    }

    /// The active folders, or NO_MODELS_FOLDER.
    pub fn current(&self) -> Result<Arc<Roots>, ToolError> {
        let a = self.active.read().unwrap();
        a.roots.clone().ok_or_else(|| {
            ToolError::new("NO_MODELS_FOLDER", format!("no models folder is set: {}", a.why_none)).hint(NO_FOLDER_HINT)
        })
    }

    /// Like `current`, but first waits (bounded) for the client's first roots answer.
    pub async fn current_settled(&self) -> Result<Arc<Roots>, ToolError> {
        let mut rx = self.settled.subscribe();
        let _ = tokio::time::timeout(Duration::from_secs(5), rx.wait_for(|s| *s)).await;
        self.current()
    }

    /// The client declared the roots capability: tool calls wait for its first answer.
    pub fn expect_client_roots(&self) {
        if !self.is_fixed() {
            self.settled.send_replace(false);
        }
    }

    pub fn settle(&self) {
        self.settled.send_replace(true);
    }

    pub fn view(&self) -> ModelsFolderOut {
        let a = self.active.read().unwrap();
        let folders: Vec<String> = a.roots.as_ref().map(|r| r.all().iter().map(|p| p.display().to_string()).collect()).unwrap_or_default();
        let hint = match (&a.roots, self.is_fixed()) {
            (None, _) => format!("{} ({})", NO_FOLDER_HINT, a.why_none),
            (Some(r), true) => format!(
                "Paths in other tools are relative to {}. The server was started with --root, so only folders inside {} can be chosen.",
                r.first().display(),
                join(&self.fixed)
            ),
            (Some(r), false) => format!(
                "Paths in other tools are relative to {}. When the user names a different project folder, call models_folder with its absolute path.",
                r.first().display()
            ),
        };
        ModelsFolderOut { folders, source: a.source, fixed: self.is_fixed(), hint }
    }

    fn replace(&self, next: Active) -> Option<Change> {
        let mut a = self.active.write().unwrap();
        let same = a.source == next.source && a.roots.as_ref().map(|r| r.all().to_vec()) == next.roots.as_ref().map(|r| r.all().to_vec());
        *a = next;
        (!same).then(|| Change { roots: a.roots.clone(), source: a.source })
    }

    /// Switch to `raw` (the models_folder tool). Returns the change, if any.
    pub fn set_from_tool(&self, raw: &str) -> Result<Option<Change>, ToolError> {
        let p = Path::new(raw);
        if raw.is_empty() || raw.contains('\0') || !p.is_absolute() {
            return Err(ToolError::invalid(format!("'{}' is not an absolute path", raw)).hint("Pass the folder's absolute path, e.g. /Users/me/project or C:\\Users\\me\\project."));
        }
        let c = p.canonicalize().map_err(|e| ToolError::new("FILE_NOT_FOUND", format!("{}: {}", raw, e)))?;
        if !c.is_dir() {
            return Err(ToolError::invalid(format!("{} is not a folder", c.display())));
        }
        if let Some(why) = self.refused(&c) {
            return Err(ToolError::new("FOLDER_REFUSED", format!("{} cannot be the models folder: {}", c.display(), why))
                .hint("Choose the project folder that holds the decks."));
        }
        if self.is_fixed() && !inside(&c, &self.fixed) {
            return Err(ToolError::new(
                "ROOT_FIXED",
                format!("the server was started with --root {}, so the models folder can only move to a folder inside it", join(&self.fixed)),
            )
            .valid(self.fixed.iter().map(|p| p.display().to_string()).collect())
            .hint("Ask the user to change --root in the server configuration, or choose a folder inside it."));
        }
        if !self.is_fixed() && !self.allow.is_empty() && !inside(&c, &self.allow) {
            return Err(ToolError::new("PATH_OUTSIDE_ROOTS", format!("{} is outside the folders allowed by --allow", c.display()))
                .valid(self.allow.iter().map(|p| p.display().to_string()).collect()));
        }
        Ok(self.replace(Self::some(vec![c], Source::Tool)))
    }

    /// Apply the client's roots (`file://` URIs). `notified`: this answers a list_changed
    /// notification (a deliberate change by the user), which overrides a folder chosen with the
    /// tool; the first answer after initialisation does not. Ignored when `--root` is given.
    pub fn set_from_client(&self, uris: &[String], notified: bool) -> Option<Change> {
        if self.is_fixed() {
            return None;
        }
        let source = self.active.read().unwrap().source;
        if source == Source::Tool && !notified {
            return None;
        }
        let mut usable: Vec<PathBuf> = Vec::new();
        for uri in uris {
            let Some(c) = file_uri_to_path(uri).and_then(|p| p.canonicalize().ok()).filter(|c| c.is_dir()) else {
                eprintln!("models folder: ignoring client root {} (not an existing local folder)", uri);
                continue;
            };
            if let Some(why) = self.refused(&c) {
                eprintln!("models folder: ignoring client root {} because {}", c.display(), why);
            } else if !self.allow.is_empty() && !inside(&c, &self.allow) {
                eprintln!("models folder: ignoring client root {} (outside --allow)", c.display());
            } else if !usable.contains(&c) {
                usable.push(c);
            }
        }
        if !usable.is_empty() {
            return self.replace(Self::some(usable, Source::ClientRoots));
        }
        // No usable client root: fall back to the current directory, unless the user chose one.
        if source == Source::Tool {
            return None;
        }
        self.replace(self.resolve_cwd())
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ModelsFolderIn {
    /// Absolute path of the folder to switch to. Omit to only show the active folder
    pub path: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ModelsFolderOut {
    /// Active models folders (absolute); relative paths in other tools resolve against the first
    pub folders: Vec<String>,
    /// flag (--root), client_roots, cwd (where the client started the server), tool (chosen with models_folder) or none
    pub source: Source,
    /// The server was started with --root: only folders inside those can be chosen
    pub fixed: bool,
    pub hint: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("calculix-mcp-folder-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["a", "b", "a/inner", "allowed/x", "home"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        dir.canonicalize().unwrap()
    }

    fn uri(p: &Path) -> String {
        let s = p.display().to_string();
        let s = s.strip_prefix(r"\\?\").unwrap_or(&s).replace('\\', "/").replace(' ', "%20");
        if s.starts_with('/') { format!("file://{}", s) } else { format!("file:///{}", s) }
    }

    #[test]
    fn refused_folders() {
        let dir = sandbox("refused");
        let home = dir.join("home");
        assert!(refusal(Path::new(if cfg!(windows) { r"C:\" } else { "/" }), Some(&home)).is_some());
        assert!(refusal(&home, Some(&home)).is_some(), "the home folder itself");
        assert!(refusal(&dir.join("a"), Some(&home)).is_none());
        #[cfg(not(windows))]
        {
            for p in ["/Users", "/home", "/usr", "/usr/local/share", "/etc", "/System/Library", "/private/tmp", "/tmp"] {
                assert!(refusal(Path::new(p), None).is_some(), "{}", p);
            }
            for p in ["/Users/me/project", "/home/me/models", "/tmp/case", "/Library2", "/libfoo"] {
                assert!(refusal(Path::new(p), None).is_none(), "{}", p);
            }
        }
        #[cfg(windows)]
        {
            for p in [r"C:\", r"D:\", r"C:\Users", r"C:\Windows", r"C:\Windows\System32", r"\\?\C:\Program Files\x"] {
                assert!(refusal(Path::new(p), None).is_some(), "{}", p);
            }
            assert!(refusal(Path::new(r"C:\Users\me\project"), None).is_none());
        }
    }

    #[test]
    fn startup_resolution_order() {
        let dir = sandbox("order");
        let home = Some(dir.join("home"));
        // --root wins over the current directory.
        let f = ModelsFolder::new(vec![dir.join("a")], vec![], Some(dir.join("b")), home.clone()).unwrap();
        let v = f.view();
        assert_eq!((v.folders, v.source, v.fixed), (vec![dir.join("a").display().to_string()], Source::Flag, true));
        // Without --root: the current directory.
        let f = ModelsFolder::new(vec![], vec![], Some(dir.join("b")), home.clone()).unwrap();
        assert_eq!((f.view().source, f.current().unwrap().first().to_path_buf()), (Source::Cwd, dir.join("b")));
        // A refused current directory (the home folder, or /) leaves no folder.
        for bad in [dir.join("home"), PathBuf::from(if cfg!(windows) { r"C:\" } else { "/" })] {
            let f = ModelsFolder::new(vec![], vec![], Some(bad), home.clone()).unwrap();
            assert_eq!(f.view().source, Source::None);
            let e = f.current().unwrap_err();
            assert_eq!((e.code, e.hint.as_deref()), ("NO_MODELS_FOLDER", Some(NO_FOLDER_HINT)));
        }
        // A refused --root is never used, but narrowing inside it is allowed.
        let f = ModelsFolder::new(vec![dir.join("home")], vec![], None, home.clone()).unwrap();
        assert_eq!(f.view().source, Source::None);
        std::fs::create_dir_all(dir.join("home/proj")).unwrap();
        f.set_from_tool(dir.join("home/proj").to_str().unwrap()).unwrap();
        assert_eq!(f.view().source, Source::Tool);
        // Missing --root folders are still a startup error.
        assert!(ModelsFolder::new(vec![dir.join("missing")], vec![], None, home).is_err());
    }

    #[test]
    fn client_roots_win_over_cwd_and_follow_changes() {
        let dir = sandbox("client");
        let f = ModelsFolder::new(vec![], vec![], Some(dir.join("b")), Some(dir.join("home"))).unwrap();
        f.set_from_client(&[uri(&dir.join("a")), "https://example.com/x".into(), uri(&dir.join("missing"))], false).unwrap();
        let v = f.view();
        assert_eq!((v.folders, v.source), (vec![dir.join("a").display().to_string()], Source::ClientRoots));
        // Only refused roots: back to the current directory.
        f.set_from_client(&[uri(&dir.join("home"))], true).unwrap();
        assert_eq!(f.view().source, Source::Cwd);
        // A folder chosen with the tool survives the first answer but not a change notification.
        f.set_from_tool(dir.join("a/inner").to_str().unwrap()).unwrap();
        assert!(f.set_from_client(&[uri(&dir.join("b"))], false).is_none());
        assert_eq!(f.view().source, Source::Tool);
        f.set_from_client(&[uri(&dir.join("b"))], true).unwrap();
        assert_eq!(f.view().source, Source::ClientRoots);
        // --root ignores client roots.
        let fixed = ModelsFolder::new(vec![dir.join("a")], vec![], None, None).unwrap();
        assert!(fixed.set_from_client(&[uri(&dir.join("b"))], true).is_none());
        assert_eq!(fixed.view().source, Source::Flag);
    }

    #[test]
    fn tool_switch_refusals_and_allow() {
        let dir = sandbox("tool");
        let home = Some(dir.join("home"));
        let path = |p: &str| dir.join(p).display().to_string();
        let f = ModelsFolder::new(vec![], vec![], Some(dir.join("a")), home.clone()).unwrap();
        assert_eq!(f.set_from_tool("relative/dir").unwrap_err().code, "INVALID_ARGUMENT");
        assert_eq!(f.set_from_tool(&path("missing")).unwrap_err().code, "FILE_NOT_FOUND");
        assert_eq!(f.set_from_tool(&path("home")).unwrap_err().code, "FOLDER_REFUSED");
        assert_eq!(f.set_from_tool(if cfg!(windows) { r"C:\" } else { "/" }).unwrap_err().code, "FOLDER_REFUSED");
        assert!(f.set_from_tool(&path("b")).unwrap().is_some());
        assert!(f.set_from_tool(&path("b")).unwrap().is_none(), "no change, nothing to log");
        let v = f.view();
        assert_eq!((v.folders, v.source, v.fixed), (vec![path("b")], Source::Tool, false));

        // --root: only narrowing inside it.
        let fixed = ModelsFolder::new(vec![dir.join("a")], vec![], None, home.clone()).unwrap();
        let e = fixed.set_from_tool(&path("b")).unwrap_err();
        assert_eq!((e.code, e.valid.clone()), ("ROOT_FIXED", vec![path("a")]));
        fixed.set_from_tool(&path("a/inner")).unwrap();
        let v = fixed.view();
        assert_eq!((v.folders, v.source, v.fixed), (vec![path("a/inner")], Source::Tool, true));

        // --allow: only inside the allowed parents.
        let allow = ModelsFolder::new(vec![], vec![dir.join("allowed")], Some(dir.join("a")), home).unwrap();
        let e = allow.set_from_tool(&path("b")).unwrap_err();
        assert_eq!((e.code, e.valid.clone()), ("PATH_OUTSIDE_ROOTS", vec![path("allowed")]));
        allow.set_from_tool(&path("allowed/x")).unwrap();
        assert_eq!(allow.view().folders, vec![path("allowed/x")]);
    }

    #[test]
    fn file_uris_are_decoded() {
        assert_eq!(file_uri_to_path("file:///a/My%20Proj"), Some(PathBuf::from("/a/My Proj")));
        assert_eq!(file_uri_to_path("file://localhost/a/b"), Some(PathBuf::from("/a/b")));
        assert_eq!(file_uri_to_path("file://server/share"), None);
        assert_eq!(file_uri_to_path("https://x/y"), None);
        assert_eq!(file_uri_to_path("file:///a/%zz"), None);
    }
}
