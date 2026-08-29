//! Download + unzip LanguageTool's English n-gram dataset (~8.4 GB) on a
//! background thread, for the Providers panel's "Download n-grams" button.
//!
//! The dataset is what lets LanguageTool catch real-word confusions
//! (`wear`/`where`); the `erikvl87` image doesn't fetch it, so the app
//! does. egui-free: the UI polls a [`DownloadHandle`] for progress and,
//! on completion, points the container's `langtool_languageModel` at the
//! extracted folder (via `docker::enable_ngrams`).

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use hyprcorrect_core::secure_fs;
use sha2::{Digest, Sha256};

/// LanguageTool's English n-gram archive — a stable dated release.
const NGRAM_URL: &str = "https://languagetool.org/download/ngram-data/ngrams-en-20150817.zip";
/// Independently recorded archive digest. The same value is used by the
/// YunoHost LanguageTool package rather than being fetched beside the asset.
const NGRAM_SHA256: &str = "10e548731d9f58189fc36a553f7f685703be30da0d9bb42d1f7b5bf5f8bb232c";
/// Bytes that must be free in the target before we start: the archive is
/// ~9 GB and unzips to ~16 GB, so require safe headroom for both at once.
const MIN_FREE_BYTES: u64 = 30 * 1024 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(60);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);
const MAX_ARCHIVE_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = 18 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 100_000;
const MAX_REDIRECTS: usize = 3;
const ALLOWED_DOWNLOAD_HOST: &str = "languagetool.org";

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
    let parent = dest
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", dest.display()))?;
    secure_fs::ensure_private_directory(parent, true)
        .map_err(|error| format!("secure {}: {error}", parent.display()))?;
    if dest.exists() {
        return Err(format!(
            "{} already exists; remove the previous data before downloading again",
            dest.display()
        )
        .into());
    }
    let free = fs2::available_space(parent).map_err(|e| format!("checking free space: {e}"))?;
    if free < MIN_FREE_BYTES {
        return Err(format!(
            "need ~30 GB free in {}; only {:.1} GB available",
            parent.display(),
            free as f64 / 1e9,
        )
        .into());
    }

    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|error| format!("randomness unavailable: {error}"))?;
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let staging = parent.join(format!(".hyprcorrect-ngrams.{suffix}"));
    secure_fs::ensure_private_directory(&staging, true)
        .map_err(|error| format!("create private staging directory: {error}"))?;

    let result = (|| {
        let zip_path = staging.join("ngrams.zip");
        let mut archive_file = secure_fs::create_new_file(&zip_path, 0o600)
            .map_err(|error| format!("create secure archive staging file: {error}"))?;
        download_to(&mut archive_file, cancel, phase)?;

        if let Ok(mut g) = phase.lock() {
            *g = DownloadPhase::Extracting;
        }
        extract_zip(&mut archive_file, &staging, cancel)?;
        drop(archive_file);
        secure_fs::remove_file(&zip_path)
            .map_err(|error| format!("remove verified archive: {error}"))?;
        if find_lang_root(&staging).as_deref() != Some(staging.as_path()) {
            return Err(
                "verified archive did not produce the expected en/ data folder"
                    .to_string()
                    .into(),
            );
        }
        secure_fs::publish_directory(&staging, dest)
            .map_err(|error| format!("publish n-gram data: {error}"))?;
        Ok(dest.to_path_buf())
    })();
    if result.is_err() {
        let _ = secure_fs::remove_directory_tree(&staging);
    }
    result
}

fn download_to(
    output: &mut File,
    cancel: &AtomicBool,
    phase: &Mutex<DownloadPhase>,
) -> Result<(), DlError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .timeout_write(READ_TIMEOUT)
        .timeout(TOTAL_TIMEOUT)
        .redirects(0)
        .https_only(true)
        .build();
    let mut current = url::Url::parse(NGRAM_URL)
        .map_err(|error| format!("invalid pinned n-gram URL: {error}"))?;
    let started = std::time::Instant::now();
    let mut redirect_count = 0usize;
    let resp = loop {
        validate_download_url(&current)?;
        let response = agent
            .get(current.as_str())
            .set("Accept", "application/zip")
            .set("Accept-Encoding", "identity")
            .call()
            .map_err(|e| format!("starting download: {e}"))?;
        if matches!(response.status(), 301 | 302 | 303 | 307 | 308) {
            if redirect_count == MAX_REDIRECTS {
                return Err(format!("n-gram download exceeded {MAX_REDIRECTS} redirects").into());
            }
            let location = response
                .header("Location")
                .ok_or_else(|| "n-gram redirect omitted Location".to_string())?;
            let next = current
                .join(location)
                .map_err(|error| format!("invalid n-gram redirect: {error}"))?;
            validate_download_url(&next)?;
            redirect_count += 1;
            current = next;
            continue;
        }
        if response.status() != 200 {
            return Err(format!("n-gram download returned HTTP {}", response.status()).into());
        }
        break response;
    };
    let total = resp
        .header("Content-Length")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    if total > MAX_ARCHIVE_BYTES {
        return Err(format!("archive exceeds the {MAX_ARCHIVE_BYTES}-byte limit").into());
    }
    let mut reader = resp.into_reader();
    let mut buf = vec![0u8; 1 << 20]; // 1 MiB
    let mut done = 0u64;
    let mut digest = Sha256::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(DlError::Cancelled);
        }
        if started.elapsed() > TOTAL_TIMEOUT {
            return Err(format!("download exceeded {} seconds", TOTAL_TIMEOUT.as_secs()).into());
        }
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("downloading: {e}"))?;
        if n == 0 {
            break;
        }
        done += n as u64;
        if done > MAX_ARCHIVE_BYTES {
            return Err(format!("archive exceeds the {MAX_ARCHIVE_BYTES}-byte limit").into());
        }
        output
            .write_all(&buf[..n])
            .map_err(|e| format!("writing archive: {e}"))?;
        digest.update(&buf[..n]);
        if let Ok(mut g) = phase.lock() {
            *g = DownloadPhase::Downloading { done, total };
        }
    }
    output
        .sync_all()
        .map_err(|error| format!("syncing archive: {error}"))?;
    let actual = format!("{:x}", digest.finalize());
    if actual != NGRAM_SHA256 {
        return Err(format!(
            "n-gram archive digest mismatch; expected {NGRAM_SHA256}, got {actual}"
        )
        .into());
    }
    Ok(())
}

fn validate_download_url(url: &url::Url) -> Result<(), DlError> {
    if url.scheme() != "https"
        || url.host_str() != Some(ALLOWED_DOWNLOAD_HOST)
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.port(), None | Some(443))
    {
        return Err(format!("refusing unexpected n-gram download URL: {url}").into());
    }
    Ok(())
}

fn extract_zip(file: &mut File, dest: &Path, cancel: &AtomicBool) -> Result<(), DlError> {
    file.seek(SeekFrom::Start(0))
        .map_err(|e| format!("rewinding archive: {e}"))?;
    let held = file
        .try_clone()
        .map_err(|e| format!("holding archive descriptor: {e}"))?;
    let mut archive = zip::ZipArchive::new(held).map_err(|e| format!("reading archive: {e}"))?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(format!("archive exceeds {MAX_ARCHIVE_ENTRIES} entries").into());
    }
    let mut paths = std::collections::HashSet::with_capacity(archive.len());
    let mut expanded = 0u64;
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .map_err(|e| format!("archive entry {i}: {e}"))?;
        let rel = safe_archive_path(&entry, i)?;
        if !paths.insert(rel) {
            return Err(format!("archive contains a duplicate path at entry {i}").into());
        }
        expanded = expanded
            .checked_add(entry.size())
            .filter(|size| *size <= MAX_EXTRACTED_BYTES)
            .ok_or_else(|| format!("archive expands beyond {MAX_EXTRACTED_BYTES} bytes"))?;
    }
    for i in 0..archive.len() {
        if cancel.load(Ordering::Relaxed) {
            return Err(DlError::Cancelled);
        }
        let entry = archive
            .by_index(i)
            .map_err(|e| format!("archive entry {i}: {e}"))?;
        let rel = safe_archive_path(&entry, i)?;
        let out = dest.join(rel);
        if entry.is_dir() {
            secure_fs::ensure_private_directory(&out, true)
                .map_err(|e| format!("creating {}: {e}", out.display()))?;
        } else {
            if let Some(parent) = out.parent() {
                secure_fs::ensure_private_directory(parent, true)
                    .map_err(|e| format!("creating {}: {e}", parent.display()))?;
            }
            let mut output = secure_fs::create_new_file(&out, 0o600)
                .map_err(|e| format!("creating {}: {e}", out.display()))?;
            let expected = entry.size();
            let copied = std::io::copy(&mut entry.take(expected + 1), &mut output)
                .map_err(|e| format!("extracting {}: {e}", out.display()))?;
            if copied != expected {
                return Err(format!("archive entry {} changed size", out.display()).into());
            }
            output
                .sync_all()
                .map_err(|e| format!("syncing {}: {e}", out.display()))?;
        }
    }
    Ok(())
}

fn safe_archive_path(entry: &zip::read::ZipFile<'_>, index: usize) -> Result<PathBuf, DlError> {
    if entry.name().len() > 4096 {
        return Err(format!("archive entry {index} has an oversized name").into());
    }
    if entry
        .unix_mode()
        .is_some_and(|mode| mode & 0o170000 == 0o120000)
    {
        return Err(format!("archive entry {index} is a symbolic link").into());
    }
    let rel = entry
        .enclosed_name()
        .ok_or_else(|| format!("archive entry {index} has an unsafe path"))?;
    let mut components = rel.components();
    if !matches!(components.next(), Some(std::path::Component::Normal(name)) if name == "en")
        || components.any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(format!("archive entry {index} is outside en/").into());
    }
    Ok(rel)
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

    fn test_dir(name: &str) -> PathBuf {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        root.join(format!("hc-ngram-{name}-{}", std::process::id()))
    }

    #[test]
    fn find_lang_root_at_root_and_one_level_down() {
        let tmp = test_dir("test");
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
        let tmp = test_dir("extract");
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
        let mut archive = File::open(&zip_path).unwrap();
        assert!(extract_zip(&mut archive, &dest, &cancel).is_ok());
        assert!(dest.join("en/3grams/marker.txt").is_file());
        assert_eq!(find_lang_root(&dest), Some(dest.clone()));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn extract_zip_refuses_a_planted_symlink_parent() {
        use std::os::unix::fs::symlink;

        let tmp = test_dir("link");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let zip_path = tmp.join("t.zip");
        {
            let file = File::create(&zip_path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            writer.start_file("en/3grams/marker.txt", options).unwrap();
            writer.write_all(b"ngram").unwrap();
            writer.finish().unwrap();
        }
        let destination = tmp.join("out");
        let elsewhere = tmp.join("elsewhere");
        fs::create_dir(&destination).unwrap();
        fs::create_dir(&elsewhere).unwrap();
        symlink(&elsewhere, destination.join("en")).unwrap();

        let mut archive = File::open(&zip_path).unwrap();
        let cancel = AtomicBool::new(false);
        assert!(extract_zip(&mut archive, &destination, &cancel).is_err());
        assert!(!elsewhere.join("3grams/marker.txt").exists());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn download_url_rejects_downgrades_and_foreign_hosts() {
        assert!(validate_download_url(&url::Url::parse(NGRAM_URL).unwrap()).is_ok());
        assert!(
            validate_download_url(
                &url::Url::parse("http://languagetool.org/download/ngrams.zip").unwrap()
            )
            .is_err()
        );
        assert!(
            validate_download_url(&url::Url::parse("https://evil.example/ngrams.zip").unwrap())
                .is_err()
        );
    }
}
