//! Resolving module specifiers to files, the way `quickjs-libc`'s loader does.
//!
//! Two responsibilities that look like one: turning a specifier into a path (which needs
//! to know the importer's location) and reporting the canonical *name* the cache and
//! `import.meta.url` use. The name is the canonicalised path, because `./a.js` from two
//! different directories is two modules while `./a.js` and `a.js` from one directory are
//! one — and getting that wrong makes a cyclic import look like two separate cycles.

use std::path::{Path, PathBuf};

use quickjs::module::{LoadedModule, ModuleLoader};

pub struct FileModuleLoader {
    /// Where the entry module lives, for resolving specifiers it makes.
    entry_dir: PathBuf,
}

impl FileModuleLoader {
    pub fn new() -> FileModuleLoader {
        FileModuleLoader {
            entry_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }

    /// `file://` URL for `path`, which is what `import.meta.url` reports.
    fn url_for(path: &Path) -> String {
        let mut text = String::from("file://");
        for component in path.components() {
            let part = component.as_os_str().to_string_lossy();
            match component {
                std::path::Component::RootDir => {}
                _ => {
                    text.push('/');
                    // Percent-encode the characters that would break a URL. Enough for
                    // ordinary paths, which is what the reference's loader does too.
                    for ch in part.chars() {
                        match ch {
                            ' ' => text.push_str("%20"),
                            '#' => text.push_str("%23"),
                            '?' => text.push_str("%3F"),
                            other => text.push(other),
                        }
                    }
                }
            }
        }
        text
    }

    /// The module name for a path, normalised the way the reference normalises one.
    ///
    /// Textual only — `.` and `..` are folded, nothing is resolved against the real
    /// filesystem. That is deliberate: the reference's names are textual, and they are
    /// visible in diagnostics (`Could not find export 'x' in module 'lib.js'`), so
    /// resolving to an absolute path here would make every message disagree.
    fn normalize(base: &str, specifier: &str) -> String {
        if !specifier.starts_with('.') {
            return specifier.to_string();
        }
        let dir = match base.rfind('/') {
            Some(index) => &base[..index],
            None => "",
        };
        // Only join when there *is* a directory: `format!("{dir}/{specifier}")` with an
        // empty `dir` yields "/lib.js", which then looks absolute.
        let joined = if dir.is_empty() {
            specifier.to_string()
        } else {
            format!("{dir}/{specifier}")
        };
        let absolute = joined.starts_with('/');
        let mut parts: Vec<&str> = Vec::new();
        for part in joined.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                other => parts.push(other),
            }
        }
        // An absolute path keeps its root; dropping it turns `/a/b.js` into `a/b.js`,
        // which then resolves relative to the working directory instead.
        let joined = parts.join("/");
        if absolute {
            format!("/{joined}")
        } else {
            joined
        }
    }
}

impl ModuleLoader for FileModuleLoader {
    fn load(
        &self,
        specifier: &str,
        _attributes: &[(String, String)],
        referrer: &str,
    ) -> Result<LoadedModule, String> {
        let _ = &self.entry_dir;
        let name = Self::normalize(referrer, specifier);
        let path = PathBuf::from(&name);
        if referrer.is_empty() {
            // The entry module is named by the command line: a path as given, not
            // something to resolve against an importer.
        } else if !(specifier.starts_with("./")
            || specifier.starts_with("../")
            || specifier.starts_with('/'))
        {
            // A bare specifier is not a file: the reference looks for a shared object or
            // fails, and this engine has no C modules to load.
            return Err(format!("could not load module filename '{specifier}'"));
        };
        let source = std::fs::read(&path).map_err(|_| {
            // The reference registers a `.so` handler for native modules, so a missing one
            // says so explicitly ("as shared library"). Every other extension gets the
            // plain message. Matching the wording matters here because a `.so` import is
            // the one case where the failure is not about a missing file at all — it is
            // about a C module this engine cannot load.
            if name.ends_with(".so") {
                format!("could not load module filename '{name}' as shared library")
            } else {
                format!("could not load module filename '{name}'")
            }
        })?;
        // The *url* is resolved because it identifies the file; the *name* stays textual
        // because it is what diagnostics print. `qjs` does the same, which is why
        // `import.meta.url` shows `/private/tmp` on macOS while an error message shows the
        // path as written.
        let resolved = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        Ok(LoadedModule {
            url: Self::url_for(&resolved),
            name,
            source,
        })
    }
}
