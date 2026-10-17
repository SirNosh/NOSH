//! Paths the way Node computes them, because both implementations store and compare them.

use std::io;
use std::path::{Component, Path, PathBuf};

/// Node's `path.resolve`: absolute, `.`/`..` collapsed, separators normalised, no trailing
/// separator except at a root. Casing and 8.3 short names are kept as given.
pub fn resolve(path: &Path) -> io::Result<PathBuf> {
    Ok(std::path::absolute(path)?.components().collect())
}

/// Node's JavaScript `fs.realpathSync`: [`resolve`], then symbolic links and junctions followed
/// component by component. Casing and 8.3 short names are kept.
pub fn realpath(path: &Path) -> io::Result<PathBuf> {
    follow_links(&resolve(path)?, 0)
}

fn follow_links(path: &Path, depth: usize) -> io::Result<PathBuf> {
    if depth > 40 {
        return Err(io::Error::other(format!(
            "ELOOP: too many symbolic links, realpath '{}'",
            path.display()
        )));
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        if !matches!(component, Component::Normal(_)) {
            continue;
        }
        if std::fs::symlink_metadata(&current)?.is_symlink() {
            let target = strip_verbatim(std::fs::read_link(&current)?);
            let base = current.parent().map(Path::to_path_buf).unwrap_or_default();
            current = follow_links(&resolve(&base.join(target))?, depth + 1)?;
        }
    }
    Ok(current)
}

/// Node's `fs.realpathSync.native` (libuv `GetFinalPathNameByHandleW`): the final on-disk path,
/// with real casing and 8.3 short names expanded, without the `\\?\` prefix.
pub fn realpath_native(path: &Path) -> io::Result<PathBuf> {
    Ok(strip_verbatim(std::fs::canonicalize(path)?))
}

/// `\\?\C:\x` becomes `C:\x` and `\\?\UNC\server\share` becomes `\\server\share`, as libuv does.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path
}

/// A path's text with `\` replaced by `/`, as Git configuration values and comparisons use it.
pub fn forward_slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_matches_node() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            resolve(Path::new("a/./b/../c/")).unwrap(),
            cwd.join("a").join("c")
        );
        assert_eq!(
            resolve(Path::new(r"C:\x\..\y\")).unwrap(),
            PathBuf::from(r"C:\y")
        );
        assert_eq!(resolve(Path::new(r"C:\")).unwrap(), PathBuf::from(r"C:\"));
        assert_eq!(
            resolve(Path::new("c:/Mixed/Case")).unwrap(),
            PathBuf::from(r"c:\Mixed\Case")
        );
    }

    /// Windows 8.3 names (e.g. GitHub's C:\Users\RUNNER~1) must expand natively, never in the JS realpath.
    #[test]
    fn native_realpath_expands_short_names() {
        let dir = tempfile::tempdir().unwrap();
        let long = dir.path().join("Long Folder Name Here");
        std::fs::create_dir(&long).unwrap();
        let query = format!(
            "(New-Object -ComObject Scripting.FileSystemObject).GetFolder('{}').ShortPath",
            long.display()
        );
        let output = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", &query])
            .output()
            .unwrap();
        let short = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        if short == long {
            return; // 8.3 names are disabled on this volume.
        }
        assert_eq!(
            realpath_native(&short).unwrap(),
            realpath_native(&long).unwrap()
        );
        assert!(
            !realpath_native(&short)
                .unwrap()
                .to_string_lossy()
                .starts_with(r"\\?\")
        );
        assert_eq!(
            realpath(&short).unwrap(),
            short,
            "the JS realpath keeps short names"
        );
    }

    #[test]
    fn strips_verbatim_prefixes() {
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\C:\x")),
            PathBuf::from(r"C:\x")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\UNC\srv\share\x")),
            PathBuf::from(r"\\srv\share\x")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from(r"C:\x")),
            PathBuf::from(r"C:\x")
        );
    }
}
