//! Download + unzip LanguageTool's English n-gram dataset (~8.4 GB) on a
//! background thread, for the Providers panel's "Download n-grams" button.
//!
//! The dataset is what lets LanguageTool catch real-word confusions
//! (`wear`/`where`); the `erikvl87` image doesn't fetch it, so the app
//! does. egui-free: the UI polls a [`DownloadHandle`] for progress and,
//! on completion, points the container's `langtool_languageModel` at the
//! extracted folder (via `docker::enable_ngrams`).

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// LanguageTool's English n-gram archive — a stable dated release.
const NGRAM_URL: &str = "https://languagetool.org/download/ngram-data/ngrams-en-20150817.zip";
/// Bytes that must be free in the target before we start: the archive is
/// ~8.4 GB and unzips to ~16 GB, so require headroom for both at once.
const MIN_FREE_BYTES: u64 = 20 * 1024 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Where a download is in its lifecycle. Cloned out via
/// [`DownloadHandle::phase`] each UI frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadPhase {
    /// Streaming the archive; `total` is 0 until the `Content-Length` is
    /// known.
    Downloading { done: u64, total: u64 },
    /// Unzipping the archive into the target folder.
    Extracting,
    /// Finished — the path is the directory containing `en/` (what
    /// `langtool_languageModel` must point at).
    Done(PathBuf),
    /// Aborted via [`DownloadHandle::cancel`].
    Cancelled,
    /// Gave up — message is user-facing.
    Failed(String),
}

/// Handle to a background n-gram download. Poll [`phase`](Self::phase)
/// each frame; [`cancel`](Self::cancel) requests a clean stop.
pub struct DownloadHandle {
    phase: Arc<Mutex<DownloadPhase>>,
    cancel: Arc<AtomicBool>,
}

impl DownloadHandle {
    pub fn phase(&self) -> DownloadPhase {
        self.phase
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| DownloadPhase::Failed("internal lock error".into()))
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Spawn the download into `dest` (created if needed). Returns at once.
pub fn spawn_ngram_download(dest: PathBuf) -> DownloadHandle {
    let phase = Arc::new(Mutex::new(DownloadPhase::Downloading { done: 0, total: 0 }));
    let cancel = Arc::new(AtomicBool::new(false));
    let phase_t = Arc::clone(&phase);
    let cancel_t = Arc::clone(&cancel);
    thread::Builder::new()
        .name("hyprcorrect-ngram-dl".into())
        .spawn(move || {
            let result = run_download(&dest, &cancel_t, &phase_t);
            if let Ok(mut g) = phase_t.lock() {
                *g = match result {
                    Ok(dir) => DownloadPhase::Done(dir),
                    Err(DlError::Cancelled) => DownloadPhase::Cancelled,
                    Err(DlError::Msg(m)) => DownloadPhase::Failed(m),
                };
            }
        })
        .ok();
    DownloadHandle { phase, cancel }
}

enum DlError {
    Cancelled,
    Msg(String),
}

impl From<String> for DlError {
    fn from(m: String) -> Self {
        DlError::Msg(m)
    }
}

fn run_download(
    dest: &Path,
    cancel: &AtomicBool,
    phase: &Mutex<DownloadPhase>,
) -> Result<PathBuf, DlError> {
    fs::create_dir_all(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    let free = fs2::available_space(dest).map_err(|e| format!("checking free space: {e}"))?;
    if free < MIN_FREE_BYTES {
        return Err(format!(
            "need ~20 GB free in {}; only {:.1} GB available",
            dest.display(),
            free as f64 / 1e9,
        )
        .into());
    }

    let zip_path = dest.join("ngrams.zip");
    download_to(&zip_path, cancel, phase)?;

    if let Ok(mut g) = phase.lock() {
        *g = DownloadPhase::Extracting;
    }
    extract_zip(&zip_path, dest, cancel)?;
    let _ = fs::remove_file(&zip_path);

    find_lang_root(dest)
        .ok_or_else(|| "unzipped, but couldn't find the en/ data folder".to_string())
        .map_err(Into::into)
}

fn download_to(
    zip_path: &Path,
    cancel: &AtomicBool,
    phase: &Mutex<DownloadPhase>,
) -> Result<(), DlError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .build();
    let resp = agent
        .get(NGRAM_URL)
        .call()
        .map_err(|e| format!("starting download: {e}"))?;
    let total = resp
        .header("Content-Length")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let mut reader = resp.into_reader();
    let mut file =
        File::create(zip_path).map_err(|e| format!("creating {}: {e}", zip_path.display()))?;
    let mut buf = vec![0u8; 1 << 20]; // 1 MiB
    let mut done = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = fs::remove_file(zip_path);
            return Err(DlError::Cancelled);
        }
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("downloading: {e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| format!("writing archive: {e}"))?;
        done += n as u64;
        if let Ok(mut g) = phase.lock() {
            *g = DownloadPhase::Downloading { done, total };
        }
    }
    Ok(())
}

fn extract_zip(zip_path: &Path, dest: &Path, cancel: &AtomicBool) -> Result<(), DlError> {
    let file = File::open(zip_path).map_err(|e| format!("opening archive: {e}"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("reading archive: {e}"))?;
    for i in 0..archive.len() {
        if cancel.load(Ordering::Relaxed) {
            let _ = fs::remove_file(zip_path);
            return Err(DlError::Cancelled);
        }
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("archive entry {i}: {e}"))?;
        // Skip entries with unsafe paths (`..`, absolute) — `enclosed_name`
        // returns `None` for those.
        let Some(rel) = entry.enclosed_name() else {
            continue;
        };
        let out = dest.join(rel);
        if entry.is_dir() {
            fs::create_dir_all(&out).map_err(|e| format!("creating {}: {e}", out.display()))?;
        } else {
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("creating {}: {e}", parent.display()))?;
            }
            let mut w =
                File::create(&out).map_err(|e| format!("creating {}: {e}", out.display()))?;
            std::io::copy(&mut entry, &mut w)
                .map_err(|e| format!("extracting {}: {e}", out.display()))?;
        }
    }
    Ok(())
}

/// The n-gram data root under `parent` (the directory containing `en/`)
/// when a download has been unpacked there, else `None`. Lets prefs tell
/// whether the app already downloaded the data, regardless of the config
/// field's contents.
pub fn data_root(parent: &Path) -> Option<PathBuf> {
    find_lang_root(parent)
}

/// Find the directory that holds the `en/` n-gram folder — usually the
/// extract root, but cope with a single wrapper directory one level down.
fn find_lang_root(root: &Path) -> Option<PathBuf> {
    if root.join("en").is_dir() {
        return Some(root.to_path_buf());
    }
    for entry in fs::read_dir(root).ok()?.flatten() {
        let p = entry.path();
        if p.is_dir() && p.join("en").is_dir() {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_lang_root_at_root_and_one_level_down() {
        let tmp = std::env::temp_dir().join(format!("hc-ngram-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);

        // en/ directly under the root.
        let flat = tmp.join("flat");
        fs::create_dir_all(flat.join("en")).unwrap();
        assert_eq!(find_lang_root(&flat), Some(flat.clone()));

        // en/ under a single wrapper directory.
        let nested = tmp.join("nested");
        fs::create_dir_all(nested.join("ngrams-en-20150817").join("en")).unwrap();
        assert_eq!(
            find_lang_root(&nested),
            Some(nested.join("ngrams-en-20150817"))
        );

        // No en/ anywhere.
        let empty = tmp.join("empty");
        fs::create_dir_all(empty.join("fr")).unwrap();
        assert_eq!(find_lang_root(&empty), None);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn extract_zip_unpacks_and_then_locates_en() {
        let tmp = std::env::temp_dir().join(format!("hc-ngram-extract-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // Craft a tiny archive shaped like the real one (en/<n>grams/...).
        let zip_path = tmp.join("t.zip");
        {
            let f = File::create(&zip_path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            zw.add_directory("en/", opts).unwrap();
            zw.start_file("en/3grams/marker.txt", opts).unwrap();
            zw.write_all(b"ngram").unwrap();
            zw.finish().unwrap();
        }

        let dest = tmp.join("out");
        let cancel = AtomicBool::new(false);
        assert!(extract_zip(&zip_path, &dest, &cancel).is_ok());
        assert!(dest.join("en/3grams/marker.txt").is_file());
        assert_eq!(find_lang_root(&dest), Some(dest.clone()));

        let _ = fs::remove_dir_all(&tmp);
    }
}
