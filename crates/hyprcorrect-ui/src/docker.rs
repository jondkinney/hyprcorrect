//! One-click LanguageTool-in-Docker installer and status probe.
//!
//! DESIGN.md keeps LanguageTool as a configure-a-URL provider — the
//! daemon never embeds it. This module is a *UX* layer on top of that:
//! it shells out to `docker` so users who don't already self-host can
//! get a working server with one click, and probes the configured URL
//! so users who *do* already self-host (any image, any name, native
//! install) see "running" instead of being nagged to install ours.
//!
//! Detection layers (in order of authority):
//! 1. HTTP probe of the configured URL — if LanguageTool answers, it
//!    is up, regardless of how it is hosted.
//! 2. `docker ps` for our named container ([`CONTAINER`]) — drives
//!    Start / Stop / Remove buttons.
//! 3. `docker ps --filter ancestor=<image>` for any container running
//!    the same image — lets us recognize an existing LT container the
//!    user runs with a different name (informational; we don't take
//!    lifecycle control of containers we didn't create).
//!
//! All checks run on a background thread via [`spawn_status_probe`];
//! the probe takes up to ~2 s on a cold URL and would stutter the UI
//! if done inline.

use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Container name we own. Predictable so we can check / start / stop
/// it without persisting a docker ID.
pub const CONTAINER: &str = "hyprcorrect-languagetool";
/// Image we pull. Most popular community image; listens on 8010.
pub const IMAGE: &str = "erikvl87/languagetool";
/// Internal port the image binds inside the container.
const IMAGE_PORT: u16 = 8010;
/// Probe timeout for the URL check. Short enough that the UI stays
/// responsive even when the configured URL points at a dead host.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Combined status of "is LanguageTool available to hyprcorrect right
/// now". The URL probe is the authoritative signal — if it passes,
/// the user's daemon will be able to reach LT regardless of what
/// (or whether) docker has anything to do with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanguageToolStatus {
    /// HTTP probe succeeded. `managed_container_running` is true if
    /// our named container ([`CONTAINER`]) is also up — in that case
    /// the UI offers Stop / Remove. False means the URL is served by
    /// something we don't manage (another container, native install,
    /// remote host) and we leave it alone.
    Reachable { managed_container_running: bool },
    /// URL doesn't answer. Drives the Install / Start UI.
    Unreachable(DockerState),
}

/// What `docker` knows about our local situation when the URL isn't
/// answering. Only consulted from inside
/// [`LanguageToolStatus::Unreachable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockerState {
    /// `docker` binary is not on PATH — user needs to install it.
    NotInstalled,
    /// docker present but the daemon isn't responding (e.g. socket
    /// permission denied, daemon stopped). Surface the raw error so
    /// the user has a hint.
    DockerUnavailable(String),
    /// docker is up; no container by [`CONTAINER`] exists, and no
    /// container running the canonical image either.
    AbsentContainer,
    /// Our named container exists but is stopped.
    ContainerStopped,
    /// Our named container exists and is running, but the URL probe
    /// still failed. Usually means a port-mapping mismatch between
    /// the configured URL and the container's `-p` flag.
    ContainerRunning,
    /// A container running [`IMAGE`] exists under a different name —
    /// the user installed LT separately, probably stopped, or it is
    /// listening on a different port than the URL. Pure informational;
    /// no lifecycle controls because we did not create it.
    ForeignContainer { name: String, running: bool },
}

/// What kind of docker operation is currently in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Install,
    Start,
    Stop,
    Remove,
}

impl OpKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Install => "Pulling image and starting container…",
            Self::Start => "Starting container…",
            Self::Stop => "Stopping container…",
            Self::Remove => "Removing container…",
        }
    }
}

/// Result reported by the background worker — `Ok(())` on success,
/// `Err(msg)` with the docker stderr or our wrapper error.
pub type OpResult = Result<(), String>;

/// Handle to a background docker operation. Drop = forget; the worker
/// thread runs to completion regardless. Use [`OpHandle::poll`] each
/// frame to pick up the result.
pub struct OpHandle {
    kind: OpKind,
    result: Arc<Mutex<Option<OpResult>>>,
}

impl OpHandle {
    pub fn kind(&self) -> OpKind {
        self.kind
    }

    /// Returns `Some(result)` once the worker thread finishes, `None`
    /// while it's still running.
    pub fn poll(&self) -> Option<OpResult> {
        self.result.lock().ok().and_then(|mut g| g.take())
    }
}

/// Handle to a background status probe (URL probe + docker
/// inspection). Same poll pattern as [`OpHandle`].
pub struct StatusHandle {
    result: Arc<Mutex<Option<LanguageToolStatus>>>,
}

impl StatusHandle {
    pub fn poll(&self) -> Option<LanguageToolStatus> {
        self.result.lock().ok().and_then(|mut g| g.take())
    }
}

/// Spawn a background probe: HTTP-checks `url`, then if needed asks
/// docker. Returns immediately; poll the handle each frame.
pub fn spawn_status_probe(url: String) -> StatusHandle {
    let result = Arc::new(Mutex::new(None));
    let result_for_thread = Arc::clone(&result);
    thread::Builder::new()
        .name("hyprcorrect-lt-probe".into())
        .spawn(move || {
            let status = probe_status_blocking(&url);
            if let Ok(mut g) = result_for_thread.lock() {
                *g = Some(status);
            }
        })
        .ok();
    StatusHandle { result }
}

fn probe_status_blocking(url: &str) -> LanguageToolStatus {
    if probe_url(url) {
        let managed_container_running =
            matches!(check_docker_state(), DockerState::ContainerRunning);
        LanguageToolStatus::Reachable {
            managed_container_running,
        }
    } else {
        LanguageToolStatus::Unreachable(check_docker_state())
    }
}

/// Hit `<url>/v2/languages` — LanguageTool's no-parameter GET endpoint
/// — and return true if the response looks like LT. Used as the
/// authoritative "is something there" signal regardless of how the
/// server is hosted.
fn probe_url(url: &str) -> bool {
    let base = url.trim().trim_end_matches('/');
    if base.is_empty() {
        return false;
    }
    let endpoint = format!("{base}/v2/languages");
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(PROBE_TIMEOUT)
        .timeout_read(PROBE_TIMEOUT)
        .timeout_write(PROBE_TIMEOUT)
        .build();
    match agent.get(&endpoint).call() {
        Ok(resp) => resp.status() == 200,
        Err(_) => false,
    }
}

/// Inspect docker for our container (and, as a fallback, any
/// container running the canonical LT image). Cheap; safe to call on
/// every probe.
fn check_docker_state() -> DockerState {
    // `docker version` is the canonical "is the daemon reachable"
    // probe — it both verifies the binary is on PATH and that we can
    // talk to the socket.
    let probe = Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .stdin(Stdio::null())
        .output();
    let probe = match probe {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return DockerState::NotInstalled;
        }
        Err(e) => return DockerState::DockerUnavailable(e.to_string()),
    };
    if !probe.status.success() {
        let stderr = String::from_utf8_lossy(&probe.stderr);
        let msg = stderr.lines().next().unwrap_or("").trim().to_string();
        let msg = if msg.is_empty() {
            "docker daemon not reachable".into()
        } else {
            msg
        };
        return DockerState::DockerUnavailable(msg);
    }

    // First: is *our* container around? `name=^CONTAINER$` anchors
    // so a partial match like `hyprcorrect-languagetool-old` doesn't
    // hijack the slot.
    if let Some(state) = inspect_container_state(&format!("name=^{CONTAINER}$")) {
        return match state.as_str() {
            "running" => DockerState::ContainerRunning,
            _ => DockerState::ContainerStopped,
        };
    }

    // Otherwise: is *any* container running the canonical image?
    // Catches the case the user opened — they installed LT under a
    // different container name and we'd otherwise miss it.
    if let Some(found) = find_container_by_image(IMAGE) {
        return DockerState::ForeignContainer {
            name: found.name,
            running: found.running,
        };
    }

    DockerState::AbsentContainer
}

/// `docker ps -a --filter <filter> --format {{.State}}` — returns the
/// State string for the first matching container, or `None` when
/// nothing matches.
fn inspect_container_state(filter: &str) -> Option<String> {
    let output = Command::new("docker")
        .args(["ps", "-a", "--filter", filter, "--format", "{{.State}}"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let first = text.lines().next()?.trim().to_string();
    if first.is_empty() { None } else { Some(first) }
}

struct ForeignContainer {
    name: String,
    running: bool,
}

/// Find the first container whose ancestor image matches `image`.
/// `ancestor=` is docker's own image-filter — covers any name the
/// user happens to have given the container.
fn find_container_by_image(image: &str) -> Option<ForeignContainer> {
    let output = Command::new("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("ancestor={image}"),
            "--format",
            "{{.Names}}\t{{.State}}",
        ])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (name, state) = text.lines().next()?.split_once('\t')?;
    Some(ForeignContainer {
        name: name.trim().to_string(),
        running: state.trim() == "running",
    })
}

/// Spawn `docker run -d` on a background thread, returning a handle
/// the caller can poll. Pulls the image implicitly the first time.
///
/// When `ngram_dir` is set, the container mounts that host folder and
/// points LanguageTool at it (`langtool_languageModel`), enabling the
/// n-gram confusion rules (wear/where). The folder must be the unzipped
/// n-gram data — the directory holding `en/`.
pub fn install(host_port: u16, ngram_dir: Option<&str>) -> OpHandle {
    let port_map = format!("{host_port}:{IMAGE_PORT}");
    let ngram = ngram_dir.map(str::to_string);
    spawn_op(OpKind::Install, move || {
        let mut args: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            CONTAINER.into(),
            "--restart=unless-stopped".into(),
            "-p".into(),
            port_map,
        ];
        // Options must precede the image name in `docker run`.
        if let Some(dir) = ngram.as_deref().filter(|d| !d.trim().is_empty()) {
            args.push("-v".into());
            args.push(format!("{dir}:/ngrams"));
            args.push("-e".into());
            args.push("langtool_languageModel=/ngrams".into());
        }
        args.push(IMAGE.into());
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run_command("docker", &refs)
    })
}

/// Start an existing stopped container.
pub fn start() -> OpHandle {
    spawn_op(OpKind::Start, || {
        run_command("docker", &["start", CONTAINER])
    })
}

/// Stop a running container.
pub fn stop() -> OpHandle {
    spawn_op(OpKind::Stop, || run_command("docker", &["stop", CONTAINER]))
}

/// Force-remove the container (whether running or stopped). The image
/// is left in the local cache so a subsequent re-install is fast.
pub fn remove() -> OpHandle {
    spawn_op(OpKind::Remove, || {
        run_command("docker", &["rm", "-f", CONTAINER])
    })
}

fn spawn_op<F>(kind: OpKind, op: F) -> OpHandle
where
    F: FnOnce() -> OpResult + Send + 'static,
{
    let result = Arc::new(Mutex::new(None));
    let result_for_thread = Arc::clone(&result);
    thread::Builder::new()
        .name(format!("hyprcorrect-docker-{kind:?}"))
        .spawn(move || {
            let out = op();
            if let Ok(mut g) = result_for_thread.lock() {
                *g = Some(out);
            }
        })
        .ok();
    OpHandle { kind, result }
}

fn run_command(program: &str, args: &[&str]) -> OpResult {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("failed to launch `{program}`: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let msg = stderr
        .lines()
        .last()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("docker command failed")
        .to_string();
    Err(msg)
}

/// Extract the host port from a configured LanguageTool URL.
/// Returns `None` if the URL doesn't carry an explicit port — we'd
/// rather make the user fix the URL than guess wrong and bind to 80.
pub fn host_port_from_url(url: &str) -> Option<u16> {
    let trimmed = url.trim();
    // Strip scheme.
    let after_scheme = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    // Take the authority component (before the first `/`).
    let authority = after_scheme.split('/').next().unwrap_or(after_scheme);
    // IPv6 addresses live in brackets: `[::1]:8081`. Cope with that
    // before splitting on `:`.
    let port_part = if let Some(rest) = authority.strip_prefix('[') {
        rest.split_once("]:").map(|(_, p)| p)?
    } else {
        authority.rsplit_once(':').map(|(_, p)| p)?
    };
    port_part.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_port_from_typical_urls() {
        assert_eq!(host_port_from_url("http://localhost:8081"), Some(8081));
        assert_eq!(
            host_port_from_url("http://localhost:8081/v2/check"),
            Some(8081)
        );
        assert_eq!(
            host_port_from_url("https://lt.example.com:9000"),
            Some(9000)
        );
        assert_eq!(host_port_from_url("http://[::1]:8081"), Some(8081));
    }

    #[test]
    fn returns_none_without_explicit_port() {
        assert!(host_port_from_url("http://localhost").is_none());
        assert!(host_port_from_url("").is_none());
        assert!(host_port_from_url("not a url").is_none());
    }
}
