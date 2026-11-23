//! Start-at-login support via the XDG autostart specification.
//!
//! A desktop session runs every `*.desktop` file in `$XDG_CONFIG_HOME/autostart`
//! at login. Writing one file there is the portable way to make an app start
//! automatically on KDE, GNOME, XFCE and the rest; it needs no daemon, no root,
//! and it is trivially reversible, which matters because the user must be able
//! to undo anything we do to their session.
//!
//! The file records the *absolute* path of the running executable. That is
//! deliberate: a stale entry pointing at a deleted binary is worse than no
//! entry, so [`sync`] rewrites the file whenever the path changes and
//! [`status`] reports honestly when the entry exists but is broken.

use std::path::{Path, PathBuf};

/// File name we own inside the autostart directory.
pub const ENTRY_NAME: &str = "sotto.desktop";

/// `$XDG_CONFIG_HOME/autostart`, falling back to `~/.config/autostart`.
pub fn autostart_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".config")
        })
        .join("autostart")
}

pub fn entry_path() -> PathBuf {
    autostart_dir().join(ENTRY_NAME)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// An autostart entry exists on disk.
    pub entry_present: bool,
    /// The entry points at a file that still exists.
    pub target_exists: bool,
    /// The executable the entry launches, as written in the file.
    pub target: Option<PathBuf>,
    /// The entry exists but would launch nothing.
    pub broken: bool,
}

/// Escape a path for the `Exec=` key of a desktop entry.
///
/// The spec requires backslash-escaping of spaces and of the characters that
/// would otherwise start a field code. Paths with quotes are wrapped, because a
/// quoted argument is the only way to keep an embedded space intact.
fn exec_quote(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.chars().any(|c| c.is_whitespace() || "\"'\\$`".contains(c)) {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.into_owned()
    }
}

/// The desktop entry we write. Kept as a pure function so tests can check the
/// exact bytes without touching the user's session.
pub fn entry_contents(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Sotto\n\
         Comment=Quiet, local-first dictation\n\
         Exec={}\n\
         Icon=sotto\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n\
         X-KDE-autostart-after=panel\n\
         StartupNotify=false\n\
         Categories=Utility;AudioVideo;\n",
        exec_quote(exe)
    )
}

/// Read the `Exec=` value back out of an existing entry.
fn parse_exec(contents: &str) -> Option<PathBuf> {
    for line in contents.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("Exec=") else {
            continue;
        };
        let rest = rest.trim();
        if rest.is_empty() {
            return None;
        }
        // A quoted argument keeps embedded spaces; we only ever write a bare
        // path, so the first argument is the whole path in that case.
        if let Some(stripped) = rest.strip_prefix('"') {
            let mut out = String::new();
            let mut chars = stripped.chars();
            while let Some(c) = chars.next() {
                match c {
                    '"' => return Some(PathBuf::from(out)),
                    '\\' => {
                        // Desktop-entry quoting: only these escapes are defined.
                        match chars.next() {
                            Some('"') => out.push('"'),
                            Some('\\') => out.push('\\'),
                            Some('s') => out.push(' '),
                            Some('n') => out.push('\n'),
                            Some('t') => out.push('\t'),
                            Some('r') => out.push('\r'),
                            Some(other) => out.push(other),
                            None => break,
                        }
                    }
                    other => out.push(other),
                }
            }
            // Unterminated quote: treat what we have as the path rather than
            // silently returning nothing, so the UI can still report it.
            return Some(PathBuf::from(out));
        }
        // Unquoted: the path ends at the first space (any following tokens would
        // be arguments).
        let first = rest.split_whitespace().next()?;
        return Some(PathBuf::from(first));
    }
    None
}

/// What is currently installed, without changing anything.
pub fn status() -> Status {
    let path = entry_path();
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return Status {
            entry_present: false,
            target_exists: false,
            target: None,
            broken: false,
        };
    };
    let target = parse_exec(&contents);
    let target_exists = target.as_ref().map(|p| p.is_file()).unwrap_or(false);
    Status {
        entry_present: true,
        target_exists,
        broken: !target_exists,
        target,
    }
}

/// Create, update or remove the autostart entry so it matches `enabled`.
///
/// Returns the status *after* the change, so the caller can show the user what
/// actually happened rather than what was requested.
pub fn sync(enabled: bool, exe: &Path) -> std::io::Result<Status> {
    let path = entry_path();
    if enabled {
        let dir = autostart_dir();
        std::fs::create_dir_all(&dir)?;
        let wanted = entry_contents(exe);
        // Only touch the file when the content differs, so we do not churn the
        // file's mtime on every settings save.
        let same = std::fs::read_to_string(&path)
            .map(|cur| cur == wanted)
            .unwrap_or(false);
        if !same {
            // Write via a temporary file in the same directory, then rename, so
            // a crash can never leave a half-written entry that would fail to
            // launch at login.
            let tmp = dir.join(format!(".{ENTRY_NAME}.tmp"));
            std::fs::write(&tmp, wanted)?;
            std::fs::rename(&tmp, &path)?;
        }
    } else if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(status())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_has_required_keys() {
        let text = entry_contents(Path::new("/usr/bin/sotto"));
        assert!(text.starts_with("[Desktop Entry]\n"));
        assert!(text.contains("Type=Application\n"));
        assert!(text.contains("Exec=/usr/bin/sotto\n"));
        assert!(text.contains("X-GNOME-Autostart-enabled=true\n"));
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn plain_path_is_not_quoted() {
        assert_eq!(exec_quote(Path::new("/usr/bin/sotto")), "/usr/bin/sotto");
    }

    #[test]
    fn path_with_space_is_quoted() {
        let q = exec_quote(Path::new("/home/a b/sotto"));
        assert_eq!(q, "\"/home/a b/sotto\"");
    }

    #[test]
    fn parse_round_trips_plain_path() {
        let text = entry_contents(Path::new("/usr/bin/sotto"));
        assert_eq!(parse_exec(&text), Some(PathBuf::from("/usr/bin/sotto")));
    }

    #[test]
    fn parse_round_trips_quoted_path() {
        let text = entry_contents(Path::new("/home/a b/sotto"));
        assert_eq!(parse_exec(&text), Some(PathBuf::from("/home/a b/sotto")));
    }

    #[test]
    fn parse_handles_args_after_path() {
        assert_eq!(
            parse_exec("Exec=/usr/bin/sotto --quiet\n"),
            Some(PathBuf::from("/usr/bin/sotto"))
        );
    }

    #[test]
    fn missing_exec_yields_none() {
        assert_eq!(parse_exec("[Desktop Entry]\nType=Application\n"), None);
    }
}
