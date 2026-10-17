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

/// The part of `target` below `root` when `target` is `root` or inside it, comparing as Node's
/// win32 `path.relative` does (case-insensitively); `None` when it escapes. Both are resolved first.
pub fn relative_inside(root: &Path, target: &Path) -> io::Result<Option<PathBuf>> {
    let (root, target) = (resolve(root)?, resolve(target)?);
    let key = |component: Component<'_>| component.as_os_str().to_string_lossy().to_lowercase();
    let mut remaining = target.components();
    for component in root.components() {
        match remaining.next() {
            Some(next) if key(next) == key(component) => {}
            _ => return Ok(None),
        }
    }
    Ok(Some(remaining.collect()))
}

/// Node's win32 `path.extname`: from the last `.` of the last segment, except for dotfiles
/// (`.bashrc` → ``), `..`, and names without a dot; `file.` → `.`.
pub fn extname(path: &str) -> &str {
    let chars: Vec<(usize, char)> = path.char_indices().collect();
    let separator = |c: char| c == '/' || c == '\\';
    let mut start = 0;
    if chars.len() >= 2 && chars[1].1 == ':' && chars[0].1.is_ascii_alphabetic() {
        start = 2;
    }
    let (mut start_dot, mut start_part, mut end, mut matched_slash, mut pre_dot_state) =
        (None, start, None, true, 0);
    for i in (start..chars.len()).rev() {
        let c = chars[i].1;
        if separator(c) {
            if !matched_slash {
                start_part = i + 1;
                break;
            }
            continue;
        }
        if end.is_none() {
            matched_slash = false;
            end = Some(i + 1);
        }
        if c == '.' {
            if start_dot.is_none() {
                start_dot = Some(i);
            } else if pre_dot_state != 1 {
                pre_dot_state = 1;
            }
        } else if start_dot.is_some() {
            pre_dot_state = -1;
        }
    }
    let (Some(dot), Some(end)) = (start_dot, end) else {
        return "";
    };
    if pre_dot_state == 0 || (pre_dot_state == 1 && dot == end - 1 && dot == start_part + 1) {
        return "";
    }
    let byte = |index: usize| chars.get(index).map_or(path.len(), |(offset, _)| *offset);
    &path[byte(dot)..byte(end)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extname_matches_node() {
        for (path, expected) in [
            ("C:\\a\\b.txt", ".txt"),
            ("C:\\a\\archive.tar.gz", ".gz"),
            ("C:\\a\\.bashrc", ""),
            ("C:\\a\\file.", "."),
            ("C:\\a\\..foo", ".foo"),
            ("C:\\a\\..", ""),
            ("C:\\a\\noext", ""),
            ("C:\\dir.d\\noext", ""),
            ("C:\\a\\é.ünï", ".ünï"),
            ("C:\\a\\b.txt\\", ".txt"),
        ] {
            assert_eq!(extname(path), expected, "{path}");
        }
    }

    #[test]
    fn relative_inside_ignores_case_like_node() {
        let inside = relative_inside(
            Path::new(r"C:\Root\Store"),
            Path::new(r"c:\root\store\ab\file"),
        )
        .unwrap();
        assert_eq!(inside, Some(PathBuf::from(r"ab\file")));
        assert_eq!(
            relative_inside(Path::new(r"C:\Root"), Path::new(r"C:\Root")).unwrap(),
            Some(PathBuf::new())
        );
        assert_eq!(
            relative_inside(Path::new(r"C:\Root"), Path::new(r"C:\Root\..\Other")).unwrap(),
            None
        );
        assert_eq!(
            relative_inside(Path::new(r"C:\Root"), Path::new(r"C:\RootX")).unwrap(),
            None
        );
        assert_eq!(
            relative_inside(Path::new(r"C:\Root"), Path::new(r"D:\Root")).unwrap(),
            None
        );
    }

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
