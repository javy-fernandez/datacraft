//! Publish a completed export without replacing another writer's destination.

use std::{io, path::Path};

#[cfg(target_os = "windows")]
pub(super) fn no_replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "MoveFileExW"]
        fn move_file_ex(from: *const u16, to: *const u16, flags: u32) -> i32;
    }
    fn wide(path: &Path) -> io::Result<Vec<u16>> {
        let name = path.file_name().ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        // canonicalize the existing parent for absolute, extended-length and UNC paths;
        // the destination itself need not exist and is never followed.
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let mut path: Vec<u16> = std::fs::canonicalize(parent)?.join(name).as_os_str().encode_wide().collect();
        if path.contains(&0) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        path.push(0);
        Ok(path)
    }
    let (from, to) = (wide(from)?, wide(to)?);
    // SAFETY: both buffers are live, NUL-terminated UTF-16 without interior NULs.
    // No REPLACE_EXISTING or COPY_ALLOWED: this is a same-volume no-clobber rename.
    if unsafe { move_file_ex(from.as_ptr(), to.as_ptr(), 0) } != 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn unix_path(path: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| io::ErrorKind::InvalidInput.into())
}

#[cfg(target_os = "linux")]
pub(super) fn no_replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::ffi::{c_char, c_int, c_uint, c_void};
    #[link(name = "dl")]
    unsafe extern "C" {
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }
    type Rename = unsafe extern "C" fn(c_int, *const c_char, c_int, *const c_char, c_uint) -> c_int;
    let (from, to) = (unix_path(from)?, unix_path(to)?);
    // SAFETY: RTLD_DEFAULT is null on Linux; the symbol is a fixed NUL-terminated name.
    // Runtime lookup avoids requiring the glibc version that first exported renameat2.
    let symbol = unsafe { dlsym(std::ptr::null_mut(), c"renameat2".as_ptr()) };
    if symbol.is_null() {
        return Err(io::ErrorKind::Unsupported.into());
    }
    // SAFETY: renameat2 has exactly this libc ABI and libc remains loaded for the call.
    let rename = unsafe { std::mem::transmute::<*mut c_void, Rename>(symbol) };
    // SAFETY: both CStrings remain live; AT_FDCWD=-100 and RENAME_NOREPLACE=1 are Linux constants.
    if unsafe { rename(-100, from.as_ptr(), -100, to.as_ptr(), 1) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(target_os = "macos")]
pub(super) fn no_replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::ffi::{c_char, c_int, c_uint};
    unsafe extern "C" {
        fn renamex_np(from: *const c_char, to: *const c_char, flags: c_uint) -> c_int;
    }
    let (from, to) = (unix_path(from)?, unix_path(to)?);
    // SAFETY: both CStrings remain live and RENAME_EXCL=4 is the Darwin no-replace flag.
    if unsafe { renamex_np(from.as_ptr(), to.as_ptr(), 4) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
pub(super) fn no_replace(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::{Arc, Barrier};

    #[test]
    fn review_publication_rejects_nul_without_truncating_a_path() {
        let dir = std::env::temp_dir().join(format!("dc-publication-nul-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let from = dir.join("stage");
        let existing = dir.join("target");
        fs::write(&from, b"new").unwrap();
        fs::write(&existing, b"keep").unwrap();
        assert_eq!(no_replace(&from, &dir.join("target\0suffix")).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read(existing).unwrap(), b"keep");
        assert_eq!(fs::read(from).unwrap(), b"new");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn review_publication_supports_long_unicode_paths() {
        let dir = std::env::temp_dir().join(format!("dc-publication-long-{}", uuid::Uuid::new_v4()));
        let nested = dir.join("資料").join("a".repeat(120)).join("b".repeat(120));
        fs::create_dir_all(&nested).unwrap();
        let from = nested.join("stage.tmp");
        let to = nested.join("Données-報告.json");
        fs::write(&from, b"complete").unwrap();
        no_replace(&from, &to).unwrap();
        assert_eq!(fs::read(to).unwrap(), b"complete");
        assert!(!from.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn review_publication_moves_complete_file_without_hard_links() {
        let dir = std::env::temp_dir().join(format!("dc-publication-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let from = dir.join("stage");
        let to = dir.join("export.json");
        fs::write(&from, b"complete export").unwrap();
        no_replace(&from, &to).unwrap();
        assert_eq!(fs::read(&to).unwrap(), b"complete export");
        assert!(!from.exists(), "publication must use a no-replace rename, not a hard link");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn review_publication_race_has_exactly_one_winner_and_never_clobbers() {
        let dir = std::env::temp_dir().join(format!("dc-publication-race-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let target = dir.join("export.json");
        let barrier = Arc::new(Barrier::new(12));
        let threads = (0u8..12)
            .map(|id| {
                let dir = dir.clone();
                let target = target.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let from = dir.join(format!("stage-{id}"));
                    let content = vec![id; 65536];
                    fs::write(&from, &content).unwrap();
                    barrier.wait();
                    (id, no_replace(&from, &target).is_ok())
                })
            })
            .collect::<Vec<_>>();
        let winners = threads.into_iter().map(|t| t.join().unwrap()).filter(|(_, ok)| *ok).collect::<Vec<_>>();
        assert_eq!(winners.len(), 1);
        assert_eq!(fs::read(&target).unwrap(), vec![winners[0].0; 65536]);
        let again = dir.join("again");
        fs::write(&again, b"must not replace").unwrap();
        assert!(no_replace(&again, &target).is_err());
        assert!(again.exists());
        assert_eq!(fs::read(&target).unwrap(), vec![winners[0].0; 65536]);
        fs::remove_dir_all(dir).unwrap();
    }
}
