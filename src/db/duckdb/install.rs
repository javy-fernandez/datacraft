//! The DuckDB driver as a downloadable add-on: the official `libduckdb` of a pinned version,
//! fetched from DuckDB's GitHub release on request, checked against its SHA-256 and kept in the
//! app's data folder (`drivers/duckdb/<version>/`).

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context as _;

use crate::i18n::{tr, trf};

/// The DuckDB version this build uses (its C API is what `ffi.rs` declares).
pub const VERSION: &str = "2.0.0";

/// Release asset and its SHA-256 for this platform, None where DuckDB has no build.
fn asset() -> Option<(&'static str, &'static str)> {
    if cfg!(target_os = "macos") {
        Some(("duckdb-shared-libs-osx-universal.tar.gz", "14cd63180614cfd961f7959146ecd9087527ff20b67311efdc3fa1c29c18204d"))
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some(("duckdb-shared-libs-windows-amd64.tar.gz", "2b83169128239de5f45fe21de80f99bf436e53aa4bb24020dd951a1ba8d4b95f"))
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        Some(("duckdb-shared-libs-windows-arm64.tar.gz", "159ce60eb587112b8c5f290b8beea56af5f13be4d13679de127ad9bfaafb7447"))
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some(("duckdb-shared-libs-linux-amd64.tar.gz", "993ad89e7dfca66cd6da994c2cad090c2fc160ddf9f550498a098ab4bb13cfb1"))
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some(("duckdb-shared-libs-linux-arm64.tar.gz", "c60eecf91143ca6bbac55c4f4d40dc22520488b0ac257c86e0fe9a2b451cf2b7"))
    } else {
        None
    }
}

/// The library's file name inside the archive and on disk.
pub fn library_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "duckdb.dll"
    } else if cfg!(target_os = "macos") {
        "libduckdb.dylib"
    } else {
        "libduckdb.so"
    }
}

/// Approximate download size for the dialog.
pub fn download_size_mb() -> u32 {
    match asset().map(|(name, _)| name) {
        Some(n) if n.contains("osx") => 39,
        Some(n) if n.contains("linux") => 27,
        _ => 19,
    }
}

/// Where the downloaded library lives.
pub fn library_path() -> PathBuf {
    crate::config::data_dir().join("drivers").join("duckdb").join(VERSION).join(library_name())
}

/// The library to load: `DATACRAFT_DUCKDB_LIB` (a `libduckdb` of the same version installed
/// elsewhere, also used by the tests) or the downloaded one, when it exists.
pub fn installed_library() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("DATACRAFT_DUCKDB_LIB").map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(p);
    }
    let p = library_path();
    p.is_file().then_some(p)
}

pub fn is_installed() -> bool {
    installed_library().is_some()
}

/// Bytes received of the total (0 while unknown), for the progress bar.
#[derive(Default)]
pub struct Progress {
    pub received: AtomicU64,
    pub total: AtomicU64,
}

impl Progress {
    pub fn fraction(&self) -> Option<f32> {
        let total = self.total.load(Ordering::Relaxed);
        (total > 0).then(|| self.received.load(Ordering::Relaxed) as f32 / total as f32)
    }
}

/// Downloads, checks and installs the library; returns its path.
pub async fn download(progress: Arc<Progress>) -> anyhow::Result<PathBuf> {
    let Some((name, sha256)) = asset() else { anyhow::bail!(tr("DuckDB has no build for this platform")) };
    let url = format!("https://github.com/duckdb/duckdb/releases/download/v{VERSION}/{name}");
    let client = reqwest::Client::builder().user_agent(concat!("DataCraft/", env!("CARGO_PKG_VERSION"))).build()?;
    let mut response = client.get(&url).send().await?.error_for_status().with_context(|| trf("Could not download {url}", &[("url", &url)]))?;
    progress.total.store(response.content_length().unwrap_or(0), Ordering::Relaxed);
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        bytes.extend_from_slice(&chunk);
        progress.received.store(bytes.len() as u64, Ordering::Relaxed);
    }
    let target = library_path();
    tokio::task::spawn_blocking(move || install(&bytes, sha256, &target).map(|()| target)).await?
}

/// Checks the archive's SHA-256 and writes its library to `target` (through a temporary file,
/// so an interrupted install never leaves a half-written library behind).
fn install(archive: &[u8], sha256: &str, target: &Path) -> anyhow::Result<()> {
    use sha2::Digest as _;
    let digest: String = sha2::Sha256::digest(archive).iter().map(|b| format!("{b:02x}")).collect();
    if digest != sha256 {
        anyhow::bail!(tr("The downloaded DuckDB driver failed its checksum; nothing was installed"));
    }
    let dir = target.parent().context("driver folder")?;
    std::fs::create_dir_all(dir)?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in tar.entries()? {
        let mut entry = entry?;
        if entry.path()?.file_name().is_some_and(|n| n == library_name()) {
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            let tmp = dir.join(format!(".{}.part", library_name()));
            std::fs::write(&tmp, &data)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
            }
            std::fs::rename(&tmp, target)?;
            return Ok(());
        }
    }
    anyhow::bail!(trf("The DuckDB archive has no {name}", &[("name", &library_name())]))
}

/// Where the driver stands, for the connection dialog.
#[derive(Clone, Debug, PartialEq)]
pub enum State {
    NotInstalled,
    /// Downloading: the fraction received, when the size is known.
    Downloading(Option<f32>),
    Installed,
    /// The last download failed (the message), and the driver is still missing.
    Failed(String),
}

/// The download in progress or its outcome, shared by every dialog.
#[derive(Default)]
struct Download {
    progress: Option<Arc<Progress>>,
    error: Option<String>,
}

static DOWNLOAD: std::sync::Mutex<Download> = std::sync::Mutex::new(Download { progress: None, error: None });

pub fn state() -> State {
    let d = DOWNLOAD.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = &d.progress {
        return State::Downloading(p.fraction());
    }
    if is_installed() {
        return State::Installed;
    }
    match &d.error {
        Some(e) => State::Failed(e.clone()),
        None => State::NotInstalled,
    }
}

/// Starts the download in the background (once); `ctx` repaints while it runs and when it ends.
pub fn start_download(ctx: egui::Context) {
    let progress = {
        let mut d = DOWNLOAD.lock().unwrap_or_else(|e| e.into_inner());
        if d.progress.is_some() {
            return;
        }
        let p = Arc::new(Progress::default());
        d.progress = Some(Arc::clone(&p));
        d.error = None;
        p
    };
    std::thread::spawn(move || {
        let ticker = ctx.clone();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ticking = Arc::clone(&done);
        // Repaints the progress bar while the download runs.
        std::thread::spawn(move || {
            while !ticking.load(Ordering::Relaxed) {
                ticker.request_repaint();
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
        });
        let result = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(anyhow::Error::from).and_then(|rt| rt.block_on(download(Arc::clone(&progress))));
        done.store(true, Ordering::Relaxed);
        let mut d = DOWNLOAD.lock().unwrap_or_else(|e| e.into_inner());
        d.progress = None;
        d.error = result.err().map(|e| format!("{e:#}"));
        ctx.request_repaint();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_platform_has_an_asset() {
        assert!(asset().is_some());
        assert!(library_path().ends_with(Path::new("drivers/duckdb").join(VERSION).join(library_name())));
    }

    /// The real download from GitHub (network): `cargo test downloads_the_real_driver -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn downloads_the_real_driver() {
        let progress = Arc::new(Progress::default());
        let path = download(Arc::clone(&progress)).await.unwrap();
        assert_eq!(path, library_path());
        assert_eq!(progress.fraction(), Some(1.0));
        super::super::ffi::Api::load(&path).unwrap();
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_wrong_checksum_installs_nothing() {
        let target = std::env::temp_dir().join(format!("datacraft-duckdb-{}", uuid::Uuid::new_v4())).join(library_name());
        assert!(install(b"not an archive", "00", &target).is_err());
        assert!(!target.exists());
    }

    #[test]
    fn installs_the_library_from_a_verified_archive() {
        use sha2::Digest as _;
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
        for (name, data) in [("duckdb.h", &b"header"[..]), (library_name(), &b"library"[..])] {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, data).unwrap();
        }
        let archive = builder.into_inner().unwrap().finish().unwrap();
        let sha: String = sha2::Sha256::digest(&archive).iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir().join(format!("datacraft-duckdb-{}", uuid::Uuid::new_v4()));
        let target = dir.join(library_name());
        install(&archive, &sha, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"library");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
