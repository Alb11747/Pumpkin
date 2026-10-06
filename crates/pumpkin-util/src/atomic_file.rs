use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::Path,
};

/// Write a complete sibling temporary file, sync it, and atomically replace the
/// destination. Failures before replacement leave the destination intact.
///
/// On Unix, the parent directory is also synced after replacement; a failure
/// there is reported even though the new file has already been committed.
pub fn atomic_write<E: From<io::Error> + std::fmt::Display>(
    path: &Path,
    write: impl FnOnce(&mut File) -> Result<(), E>,
) -> Result<(), E> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("file has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".{}.tmp", ::uuid::Uuid::new_v4()));
    // Cleanup is only attempted after create_new establishes ownership.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| {
        write(&mut file)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path).map_err(E::from)
    })();
    if let Err(original) = result {
        if let Err(cleanup) = fs::remove_file(&temporary)
            && cleanup.kind() != io::ErrorKind::NotFound
        {
            return Err(E::from(io::Error::other(format!(
                "{original}; failed to remove temporary file {}: {cleanup}",
                temporary.display()
            ))));
        }
        return Err(original);
    }
    // The rename has committed: a directory-sync failure cannot restore the old file.
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}
