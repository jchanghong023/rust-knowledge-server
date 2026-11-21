use std::env;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::os::unix::net::UnixStream as BlockingStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

use crate::data_root::{LINUX_ROOT, check_private_directory};
use crate::settings::DAEMON_CHILD;

const SOCKET: &str = "daemon.sock";
const LOG: &str = "daemon.log";
const START_TIMEOUT: Duration = Duration::from_secs(30);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(3);
const STATUS: u8 = b'?';
const STOP: u8 = b'q';
const STOPPED: &str = "Stopped";

pub(crate) fn start() -> io::Result<()> {
    let (reader, writer) = BlockingStream::pair()?;
    reader.set_read_timeout(Some(START_TIMEOUT))?;
    let mut command = Command::new(env::current_exe()?);
    command
        .arg(DAEMON_CHILD)
        .args(env::args_os().skip(2))
        .stdin(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(writer)))
        .stderr(Stdio::null());
    // SAFETY: the callback runs between fork and exec and only invokes setsid/reads errno.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    // The Command retains its own copy of stdout until dropped.
    drop(command);
    let result = startup_result(reader);
    let address = match result {
        Ok(address) => address,
        Err(error) => {
            // This is the Child we own and have not reaped, never a PID from a file.
            let _ = child.kill();
            child.wait()?;
            return Err(error);
        }
    };
    println!("{}", running(child.id(), address));
    Ok(())
}

fn startup_result(reader: BlockingStream) -> io::Result<SocketAddr> {
    let mut line = String::new();
    match BufReader::new(reader).read_line(&mut line) {
        Ok(0) => Err(io::Error::other(
            "background service exited before reporting readiness",
        )),
        Ok(_) => {
            let line = line.trim_end();
            if let Some(error) = line.strip_prefix("error ") {
                return Err(io::Error::other(error.to_owned()));
            }
            line.strip_prefix("ready ")
                .and_then(|address| address.parse().ok())
                .ok_or_else(|| io::Error::other("invalid background startup response"))
        }
        Err(error) => Err(io::Error::other(format!(
            "waiting for background startup: {error}"
        ))),
    }
}

pub(crate) fn report_start_error(error: &dyn std::error::Error) {
    let _ = writeln!(io::stdout().lock(), "error {error}");
}

pub(crate) fn report_ready(address: SocketAddr) -> io::Result<()> {
    writeln!(io::stdout().lock(), "ready {address}")
}

pub(crate) fn redirect_log(root: &Path) -> io::Result<()> {
    let log = File::options()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(root.join(LOG))?;
    if !log.metadata()?.is_file() {
        return Err(io::Error::other("daemon.log is not a regular file"));
    }
    // SAFETY: log holds a valid descriptor, and the child reserves stderr for this log.
    if unsafe { libc::dup2(log.as_raw_fd(), libc::STDERR_FILENO) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn status() -> io::Result<()> {
    println!("{}", request(STATUS)?);
    Ok(())
}

pub(crate) fn stop() -> io::Result<()> {
    if request(STOP)? != STOPPED {
        return Err(io::Error::other(
            "background service did not confirm a clean stop",
        ));
    }
    println!("{STOPPED}");
    Ok(())
}

fn request(action: u8) -> io::Result<String> {
    let root = Path::new(LINUX_ROOT);
    check_private_directory(root).map_err(not_running)?;
    let path = root.join(SOCKET);
    if !fs::symlink_metadata(&path)
        .map_err(not_running)?
        .file_type()
        .is_socket()
    {
        return Err(io::Error::other("background control path is not a socket"));
    }
    let mut stream = BlockingStream::connect(path).map_err(not_running)?;
    stream.set_write_timeout(Some(CONTROL_TIMEOUT))?;
    if action == STATUS {
        stream.set_read_timeout(Some(CONTROL_TIMEOUT))?;
    }
    stream.write_all(&[action])?;
    let mut line = String::new();
    if BufReader::new(stream).read_line(&mut line)? == 0 {
        return Err(io::Error::other(
            "background control closed without a response",
        ));
    }
    line.truncate(line.trim_end().len());
    Ok(line)
}

fn not_running(error: io::Error) -> io::Error {
    match error.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
            io::Error::new(io::ErrorKind::NotFound, "background service is not running")
        }
        _ => error,
    }
}

fn running(pid: u32, address: SocketAddr) -> String {
    format!("Running: PID {pid}, http://{address}")
}

pub(crate) struct Control {
    path: PathBuf,
    listener: UnixListener,
}

impl Control {
    /// Called while the data root lock is held, including during stale socket cleanup.
    pub(crate) fn bind(root: &Path) -> io::Result<Self> {
        let path = root.join(SOCKET);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(&path)?,
            Ok(_) => return Err(io::Error::other("background control path is not a socket")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(&path)?;
        Ok(Self { path, listener })
    }

    pub(crate) async fn wait(
        self,
        address: SocketAddr,
        shutdown: CancellationToken,
    ) -> io::Result<(Self, Option<UnixStream>)> {
        let reply = tokio::select! {
            _ = shutdown.cancelled() => None,
            result = self.wait_for_stop(address) => {
                shutdown.cancel();
                Some(result?)
            }
        };
        Ok((self, reply))
    }

    async fn wait_for_stop(&self, address: SocketAddr) -> io::Result<UnixStream> {
        let status = running(process::id(), address) + "\n";
        loop {
            let (mut stream, _) = self.listener.accept().await?;
            match tokio::time::timeout(CONTROL_TIMEOUT, stream.read_u8()).await {
                Ok(Ok(STOP)) => return Ok(stream),
                Ok(Ok(STATUS)) => {
                    let _ =
                        tokio::time::timeout(CONTROL_TIMEOUT, stream.write_all(status.as_bytes()))
                            .await;
                }
                _ => {}
            }
        }
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            let _ = writeln!(
                io::stderr().lock(),
                "remove {}: {error}",
                self.path.display()
            );
        }
    }
}

pub(crate) async fn confirm_stop(mut stream: UnixStream) {
    // A caller may disconnect while the service is draining; it cannot undo the stop.
    let _ = stream.write_all(b"Stopped\n").await;
}
