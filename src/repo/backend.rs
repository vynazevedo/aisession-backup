use anyhow::{bail, Context, Result};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub trait Backend: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    fn put(&self, key: &str, bytes: &[u8]) -> Result<()>;
    fn exists(&self, key: &str) -> Result<bool>;
    fn list(&self, prefix: &str) -> Result<Vec<String>>;
    fn delete(&self, key: &str) -> Result<()>;
    fn put_if_absent(&self, _key: &str, _bytes: &[u8]) -> Result<bool> {
        bail!("backend does not support atomic create-if-absent; repository locking unavailable")
    }
}

pub fn validate_key(key: &str) -> Result<()> {
    if key.is_empty()
        || key.len() > 4096
        || key.starts_with('/')
        || key.contains('\\')
        || key.contains("..")
        || key.chars().any(char::is_control)
        || key
            .split('/')
            .any(|part| part.is_empty() || part == "." || part.contains(':'))
    {
        bail!("unsafe repository path: {key:?}")
    }
    Ok(())
}

pub(crate) fn joined(root: &Path, key: &str) -> Result<PathBuf> {
    validate_key(key)?;
    let mut path = root.to_path_buf();
    for part in key.split('/') {
        path.push(part);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                bail!("symlink below repository or staging root: {key:?}")
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path)
}

pub(crate) fn private_dirs(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct LocalBackend {
    root: PathBuf,
}

impl LocalBackend {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        private_dirs(&root)?;
        Ok(Self {
            root: fs::canonicalize(root)?,
        })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    fn write(&self, key: &str, bytes: &[u8], exclusive: bool) -> Result<bool> {
        let dest = joined(&self.root, key)?;
        let parent = dest.parent().context("object has no parent")?;
        private_dirs(parent)?;
        let mut temp = tempfile::Builder::new()
            .prefix(".repo-tmp-")
            .tempfile_in(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        temp.write_all(bytes)?;
        temp.flush()?;
        temp.as_file().sync_all()?;
        joined(&self.root, key)?;
        if exclusive {
            match temp.persist_noclobber(&dest) {
                Ok(_) => {}
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Ok(false)
                }
                Err(error) => return Err(error.error.into()),
            }
        } else {
            temp.persist(&dest).map_err(|error| error.error)?;
        }
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        Ok(true)
    }
}

impl Backend for LocalBackend {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let path = joined(&self.root, key)?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) if !metadata.is_file() => {
                bail!("repository object is not a regular file: {key:?}")
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        match fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
    fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        self.write(key, bytes, false).map(|_| ())
    }
    fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<bool> {
        self.write(key, bytes, true)
    }
    fn exists(&self, key: &str) -> Result<bool> {
        match fs::symlink_metadata(joined(&self.root, key)?) {
            Ok(meta) if meta.is_file() => Ok(true),
            Ok(_) => bail!("repository object is not a regular file: {key:?}"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        if !prefix.is_empty() {
            let key = prefix.strip_suffix('/').unwrap_or(prefix);
            joined(&self.root, key)?;
        }
        let mut keys = Vec::new();
        for entry in walkdir::WalkDir::new(&self.root).follow_root_links(false) {
            let entry = entry?;
            if entry.depth() == 0 {
                continue;
            }
            if entry.file_type().is_symlink() {
                bail!("symlink below repository root")
            }
            if !entry.file_type().is_file() {
                continue;
            }
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(".repo-tmp-")
            {
                continue;
            }
            let key = entry
                .path()
                .strip_prefix(&self.root)?
                .components()
                .map(|part| part.as_os_str().to_str().context("non-UTF8 repository key"))
                .collect::<Result<Vec<_>>>()?
                .join("/");
            validate_key(&key)?;
            if key.starts_with(prefix) {
                keys.push(key);
            }
        }
        keys.sort();
        Ok(keys)
    }
    fn delete(&self, key: &str) -> Result<()> {
        let path = joined(&self.root, key)?;
        match fs::remove_file(&path) {
            Ok(()) => {
                #[cfg(unix)]
                fs::File::open(path.parent().context("object has no parent")?)?.sync_all()?;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}
