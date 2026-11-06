use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use tempfile::NamedTempFile;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    CreateNew,
    Replace,
}

pub fn validate_rel(rel: &str) -> Result<()> {
    if rel.is_empty() {
        bail!("empty relative path");
    }
    if rel.len() > 4096 {
        bail!("relative path too long");
    }
    if rel.chars().any(char::is_control) {
        bail!("control character in path: {rel:?}");
    }
    if rel.contains('\\') {
        bail!("backslash in path: {rel:?}");
    }
    if rel.starts_with('/') {
        bail!("absolute path: {rel:?}");
    }
    for (index, segment) in rel.split('/').enumerate() {
        if segment.is_empty() || segment == "." || segment == ".." {
            bail!("unsafe path segment in {rel:?}");
        }
        if index == 0 && has_drive_prefix(segment) {
            bail!("drive prefix in path: {rel:?}");
        }
        if cfg!(windows)
            && (segment.contains(':') || segment.ends_with('.') || segment.ends_with(' '))
        {
            bail!("path segment not representable on Windows: {rel:?}");
        }
    }
    Ok(())
}

fn has_drive_prefix(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

pub fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 255 {
        bail!("invalid identifier length");
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("invalid identifier: {id:?}");
    }
    if id.chars().all(|c| c == '.') {
        bail!("invalid identifier: {id:?}");
    }
    Ok(())
}

pub fn safe_join(base: &Path, rel: &str) -> Result<PathBuf> {
    validate_rel(rel)?;
    let mut current = base.to_path_buf();
    for segment in rel.split('/') {
        current.push(segment);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                bail!("refusing to traverse symlink: {}", current.display())
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| format!("inspecting {}", current.display()))
            }
        }
    }
    Ok(current)
}

fn create_private_dirs(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .with_context(|| format!("creating {}", path.display()))
}

pub fn set_private(file: &File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

#[cfg(windows)]
fn long_path(path: &Path) -> Result<PathBuf> {
    let absolute =
        std::path::absolute(path).with_context(|| format!("resolving {}", path.display()))?;
    let Some(text) = absolute.to_str() else {
        return Ok(absolute);
    };
    if text.starts_with(r"\\?\") || text.starts_with(r"\\.\") {
        return Ok(absolute);
    }
    Ok(match text.strip_prefix(r"\\") {
        Some(unc) => PathBuf::from(format!(r"\\?\UNC\{unc}")),
        None => PathBuf::from(format!(r"\\?\{text}")),
    })
}

#[cfg(not(windows))]
fn long_path(path: &Path) -> Result<PathBuf> {
    Ok(path.to_path_buf())
}

pub fn atomic_write_from<R: Read>(
    base: &Path,
    rel: &str,
    reader: &mut R,
    mode: WriteMode,
) -> Result<u64> {
    let dest = safe_join(base, rel)?;
    let target = long_path(&dest)?;
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("no parent for {}", dest.display()))?;
    create_private_dirs(parent)?;
    let mut temp = NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temporary file in {}", parent.display()))?;
    set_private(temp.as_file())?;
    let written = io::copy(reader, &mut temp)?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    match mode {
        WriteMode::CreateNew => {
            temp.persist_noclobber(&target)
                .map_err(|err| anyhow!("writing {}: {}", dest.display(), err.error))?;
        }
        WriteMode::Replace => {
            temp.persist(&target)
                .map_err(|err| anyhow!("writing {}: {}", dest.display(), err.error))?;
        }
    }
    Ok(written)
}

pub fn atomic_write(base: &Path, rel: &str, bytes: &[u8], mode: WriteMode) -> Result<()> {
    let mut reader = bytes;
    atomic_write_from(base, rel, &mut reader, mode).map(|_| ())
}

pub fn create_private_new(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("creating {}", path.display()))
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    #[test]
    fn long_path_uses_the_verbatim_prefix() {
        assert_eq!(
            super::long_path(std::path::Path::new(r"C:\a/b")).unwrap(),
            std::path::PathBuf::from(r"\\?\C:\a\b")
        );
        assert_eq!(
            super::long_path(std::path::Path::new(r"\\host\share\a")).unwrap(),
            std::path::PathBuf::from(r"\\?\UNC\host\share\a")
        );
    }

    use super::*;

    #[test]
    fn validate_rel_accepts_nested_names() {
        assert!(validate_rel("projects/a b/c.jsonl").is_ok());
        assert!(validate_rel("backups/x - 2026.maestribak").is_ok());
    }

    #[test]
    fn validate_rel_rejects_hostile_names() {
        for rel in [
            "",
            "/etc/passwd",
            "../x",
            "a/../../x",
            "a//b",
            "./a",
            "a/.",
            "C:/x",
            "c:x",
            "a\\b",
            "a\0b",
            "a\nb",
        ] {
            assert!(validate_rel(rel).is_err(), "{rel:?} must be rejected");
        }
    }

    #[test]
    fn validate_id_rejects_traversal() {
        assert!(validate_id("03682b1b-95be-4ed9-98ef-e20726b20860").is_ok());
        for id in ["", "..", "a/b", "a\\b", "../x", "a b"] {
            assert!(validate_id(id).is_err(), "{id:?} must be rejected");
        }
    }

    #[cfg(unix)]
    #[test]
    fn safe_join_refuses_symlink_ancestor_and_target() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("link")).unwrap();
        assert!(safe_join(tmp.path(), "link/file").is_err());
        assert!(safe_join(tmp.path(), "link").is_err());
        assert!(atomic_write(tmp.path(), "link/file", b"x", WriteMode::Replace).is_err());
        assert!(!outside.path().join("file").exists());
    }

    #[test]
    fn create_new_never_clobbers() {
        let tmp = tempfile::tempdir().unwrap();
        atomic_write(tmp.path(), "a/b", b"one", WriteMode::CreateNew).unwrap();
        assert!(atomic_write(tmp.path(), "a/b", b"two", WriteMode::CreateNew).is_err());
        assert_eq!(fs::read(tmp.path().join("a/b")).unwrap(), b"one");
        atomic_write(tmp.path(), "a/b", b"two", WriteMode::Replace).unwrap();
        assert_eq!(fs::read(tmp.path().join("a/b")).unwrap(), b"two");
    }

    #[cfg(unix)]
    #[test]
    fn written_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        atomic_write(tmp.path(), "f", b"x", WriteMode::CreateNew).unwrap();
        let mode = fs::metadata(tmp.path().join("f"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
