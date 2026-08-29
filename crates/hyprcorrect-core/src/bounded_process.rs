//! Deadline- and byte-bounded collection of child-process output.

use std::io::{self, Read};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Captured child status and output, analogous to [`std::process::Output`].
#[derive(Debug)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

/// Spawn `command`, drain both pipes concurrently, and kill it as soon as a
/// sentinel byte or the total deadline is reached.
pub fn output(
    command: &mut Command,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> io::Result<Output> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let (overflow_tx, overflow_rx) = mpsc::sync_channel(2);
    let stdout = reader(
        child.stdout.take().expect("piped stdout"),
        stdout_limit,
        Stream::Stdout,
        overflow_tx.clone(),
    );
    let stderr = reader(
        child.stderr.take().expect("piped stderr"),
        stderr_limit,
        Stream::Stderr,
        overflow_tx,
    );
    let deadline = Instant::now() + timeout;
    let mut failure = None;
    let status = loop {
        if let Ok(stream) = overflow_rx.try_recv() {
            failure = Some(match stream {
                Stream::Stdout => format!("child stdout exceeded its {stdout_limit}-byte limit"),
                Stream::Stderr => format!("child stderr exceeded its {stderr_limit}-byte limit"),
            });
            let _ = child.kill();
            break child.wait()?;
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            failure = Some(format!(
                "child process exceeded its {}-millisecond deadline",
                timeout.as_millis()
            ));
            let _ = child.kill();
            break child.wait()?;
        }
        thread::sleep(Duration::from_millis(25));
    };

    let stdout = stdout
        .join()
        .map_err(|_| io::Error::other("stdout reader panicked"))??;
    let stderr = stderr
        .join()
        .map_err(|_| io::Error::other("stderr reader panicked"))??;
    if let Some(failure) = failure {
        return Err(io::Error::new(io::ErrorKind::InvalidData, failure));
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn reader<R: Read + Send + 'static>(
    mut input: R,
    limit: usize,
    stream: Stream,
    overflow: mpsc::SyncSender<Stream>,
) -> thread::JoinHandle<io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        (&mut input)
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            let _ = overflow.send(stream);
            bytes.truncate(limit);
        }
        Ok(bytes)
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn accepts_exact_limit_and_kills_nonterminating_overflow() {
        const LIMIT: usize = 16 * 1024;
        let exact_script = format!("import os; os.write(1, b'x' * {LIMIT})");
        let exact = output(
            Command::new("/usr/bin/python3")
                .args(["-c", &exact_script])
                .stdin(Stdio::null()),
            Duration::from_secs(3),
            LIMIT,
            LIMIT,
        )
        .unwrap();
        assert!(exact.status.success());
        assert_eq!(exact.stdout.len(), LIMIT);

        let overflow_script = format!(
            "import os,time; os.write(1, b'x' * {}); time.sleep(30)",
            LIMIT + 1
        );
        let started = Instant::now();
        let error = output(
            Command::new("/usr/bin/python3")
                .args(["-c", &overflow_script])
                .stdin(Stdio::null()),
            Duration::from_secs(10),
            LIMIT,
            LIMIT,
        )
        .expect_err("sentinel byte must abort the process");
        assert!(error.to_string().contains("exceeded"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
