//! Interactive terminals in sandboxes.
//!
//! A sandbox has one terminal session: a login shell on a pseudo-terminal in the guest, kept
//! outside the journal. A client attaches over a dedicated connection (gateway, host, guest)
//! that carries [`Frame`]s after the request's reply line. The guest keeps the last [`BUFFER`]
//! bytes of output with their byte offsets, so a client that reconnects resumes from the offset
//! it has seen; a session nobody is attached to ends after [`DETACHED`].
use std::{
    collections::VecDeque,
    io::{Read, Write},
    os::fd::{AsRawFd, OwnedFd},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::Context;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::Notify,
};

/// Output a session keeps for clients that attach later.
pub const BUFFER: usize = 256 * 1024;
/// How long a session lives with nobody attached.
pub const DETACHED: Duration = Duration::from_secs(600);
/// Largest frame on the wire.
const MAX_FRAME: u32 = 1 << 20;

/// What flows on a terminal connection: input and output bytes, a window size, the client
/// ending the session, or the shell's exit status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    Data(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Close,
    Exit(i32),
}

const DATA: u8 = 0;
const RESIZE: u8 = 1;
const CLOSE: u8 = 2;
const EXIT: u8 = 3;

/// The next frame, or None at end of stream.
pub async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> std::io::Result<Option<Frame>> {
    let kind = match reader.read_u8().await {
        Ok(kind) => kind,
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    };
    let len = reader.read_u32().await?;
    let invalid = || std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid terminal frame");
    if len > MAX_FRAME {
        return Err(invalid());
    }
    let mut payload = vec![0; len as usize];
    reader.read_exact(&mut payload).await?;
    Ok(Some(match (kind, payload.as_slice()) {
        (DATA, _) => Frame::Data(payload),
        (RESIZE, [c0, c1, r0, r1]) => Frame::Resize {
            cols: u16::from_be_bytes([*c0, *c1]),
            rows: u16::from_be_bytes([*r0, *r1]),
        },
        (CLOSE, []) => Frame::Close,
        (EXIT, [a, b, c, d]) => Frame::Exit(i32::from_be_bytes([*a, *b, *c, *d])),
        _ => return Err(invalid()),
    }))
}

pub async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    frame: &Frame,
) -> std::io::Result<()> {
    let (kind, payload): (u8, Vec<u8>) = match frame {
        Frame::Data(bytes) => (DATA, bytes.clone()),
        Frame::Resize { cols, rows } => (RESIZE, [cols.to_be_bytes(), rows.to_be_bytes()].concat()),
        Frame::Close => (CLOSE, vec![]),
        Frame::Exit(code) => (EXIT, code.to_be_bytes().to_vec()),
    };
    let mut bytes = Vec::with_capacity(payload.len() + 5);
    bytes.push(kind);
    bytes.extend((payload.len() as u32).to_be_bytes());
    bytes.extend(payload);
    writer.write_all(&bytes).await?;
    writer.flush().await
}

struct Output {
    buffer: VecDeque<u8>,
    /// Offset of `buffer[0]` in everything the session has written.
    start: u64,
    exit: Option<i32>,
}

struct Session {
    master: Mutex<std::fs::File>,
    /// The shell's process group.
    pid: i32,
    output: Mutex<Output>,
    changed: Notify,
    attached: AtomicUsize,
    /// Counts attachments, so a detach timer knows whether someone came and went since.
    generation: AtomicU64,
}

impl Session {
    fn resize(&self, cols: u16, rows: u16) {
        let size = libc::winsize {
            ws_row: rows.max(1),
            ws_col: cols.max(1),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let master = self.master.lock().unwrap();
        // SAFETY: a valid descriptor and a `winsize` that outlives the call.
        unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ as _, &size) };
    }

    fn end(&self) {
        // SAFETY: signalling the shell's own process group.
        unsafe { libc::killpg(self.pid, libc::SIGKILL) };
    }
}

/// A sandbox's terminal session.
pub struct Terminals {
    /// Where shells start.
    cwd: std::path::PathBuf,
    session: Mutex<Option<Arc<Session>>>,
}

/// What a client sees on attaching: the session, and the oldest offset still buffered.
pub struct Attached {
    session: Arc<Session>,
    pub start: u64,
}

impl Terminals {
    pub fn new(cwd: impl Into<std::path::PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            cwd: cwd.into(),
            session: Mutex::new(None),
        })
    }

    /// Attach to the session, starting a shell sized `cols`×`rows` if there is none (or it has
    /// exited).
    pub fn open(self: &Arc<Self>, cols: u16, rows: u16) -> anyhow::Result<Attached> {
        let mut current = self.session.lock().unwrap();
        let live = current
            .as_ref()
            .filter(|s| s.output.lock().unwrap().exit.is_none())
            .cloned();
        let session = match live {
            Some(session) => session,
            None => {
                let session = spawn(&self.cwd, cols, rows)?;
                *current = Some(session.clone());
                session
            }
        };
        session.resize(cols, rows);
        session.attached.fetch_add(1, Ordering::SeqCst);
        session.generation.fetch_add(1, Ordering::SeqCst);
        let start = session.output.lock().unwrap().start;
        Ok(Attached { session, start })
    }

    /// Serve an attachment: output from offset `since` on, and the client's input, until the
    /// client goes or the shell exits.
    pub async fn serve(
        self: &Arc<Self>,
        attached: Attached,
        since: u64,
        mut read: impl AsyncRead + Unpin,
        mut write: impl AsyncWrite + Unpin,
    ) -> anyhow::Result<()> {
        let session = attached.session.clone();
        let input = async {
            while let Some(frame) = read_frame(&mut read).await? {
                match frame {
                    Frame::Data(bytes) => {
                        let session = session.clone();
                        tokio::task::spawn_blocking(move || {
                            session.master.lock().unwrap().write_all(&bytes)
                        })
                        .await??;
                    }
                    Frame::Resize { cols, rows } => session.resize(cols, rows),
                    Frame::Close => {
                        session.end();
                        break;
                    }
                    Frame::Exit(_) => {}
                }
            }
            anyhow::Ok(())
        };
        let output = async {
            let mut offset = since;
            loop {
                let notified = session.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let (chunk, exit) = {
                    let output = session.output.lock().unwrap();
                    offset = offset.max(output.start);
                    let skip = (offset - output.start) as usize;
                    let chunk: Vec<u8> = output
                        .buffer
                        .iter()
                        .skip(skip)
                        .take(64 * 1024)
                        .copied()
                        .collect();
                    (chunk, output.exit)
                };
                if !chunk.is_empty() {
                    offset += chunk.len() as u64;
                    write_frame(&mut write, &Frame::Data(chunk)).await?;
                    continue;
                }
                if let Some(code) = exit {
                    write_frame(&mut write, &Frame::Exit(code)).await?;
                    return anyhow::Ok(());
                }
                notified.await;
            }
        };
        let result = tokio::select! {
            result = input => result,
            result = output => result,
        };
        self.detach(attached);
        result
    }

    fn detach(self: &Arc<Self>, attached: Attached) {
        let session = attached.session;
        if session.attached.fetch_sub(1, Ordering::SeqCst) != 1 {
            return;
        }
        let generation = session.generation.load(Ordering::SeqCst);
        let this = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(DETACHED).await;
            if session.attached.load(Ordering::SeqCst) == 0
                && session.generation.load(Ordering::SeqCst) == generation
            {
                session.end();
                let mut current = this.session.lock().unwrap();
                if current.as_ref().is_some_and(|s| Arc::ptr_eq(s, &session)) {
                    *current = None;
                }
            }
        });
    }

    /// End the session (the sandbox is stopping).
    pub fn end(&self) {
        if let Some(session) = self.session.lock().unwrap().take() {
            session.end();
        }
    }
}

/// Start a login shell on a new pseudo-terminal, and the thread that buffers its output.
fn spawn(cwd: &std::path::Path, cols: u16, rows: u16) -> anyhow::Result<Arc<Session>> {
    let size = nix::pty::Winsize {
        ws_row: rows.max(1),
        ws_col: cols.max(1),
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = nix::pty::openpty(Some(&size), None)?;
    let (master, slave): (OwnedFd, OwnedFd) = (pty.master, pty.slave);
    let shell = if std::path::Path::new("/bin/bash").exists() {
        "/bin/bash"
    } else {
        "/bin/sh"
    };
    let mut command = std::process::Command::new(shell);
    command
        .arg("-l")
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", "/home/agent")
        .env("TERM", "xterm-256color")
        .envs(crate::egress::child_env())
        .stdin(std::process::Stdio::from(slave.try_clone()?))
        .stdout(std::process::Stdio::from(slave.try_clone()?))
        .stderr(std::process::Stdio::from(slave));
    // SAFETY: between fork and exec only async-signal-safe calls, allocating nothing: a new
    // session with the terminal as its controlling terminal.
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            libc::setsid();
            libc::ioctl(0, libc::TIOCSCTTY as _, 0);
            Ok(())
        });
    }
    let mut command = tokio::process::Command::from(command);
    crate::guest::killable_first(&mut command);
    let mut child = command
        .kill_on_drop(false)
        .spawn()
        .context("starting the terminal shell")?;
    let pid = child.id().context("terminal shell pid")? as i32;
    let reader = std::fs::File::from(master.try_clone()?);
    let session = Arc::new(Session {
        master: Mutex::new(std::fs::File::from(master)),
        pid,
        output: Mutex::new(Output {
            buffer: VecDeque::new(),
            start: 0,
            exit: None,
        }),
        changed: Notify::new(),
        attached: AtomicUsize::new(0),
        generation: AtomicU64::new(0),
    });
    let buffered = session.clone();
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut chunk = [0u8; 8192];
        // Read errors with EIO once the shell and its children have all closed the terminal.
        while let Ok(read) = reader.read(&mut chunk) {
            if read == 0 {
                break;
            }
            let mut output = buffered.output.lock().unwrap();
            output.buffer.extend(&chunk[..read]);
            let excess = output.buffer.len().saturating_sub(BUFFER);
            output.buffer.drain(..excess);
            output.start += excess as u64;
            drop(output);
            buffered.changed.notify_waiters();
        }
    });
    let exited = session.clone();
    tokio::spawn(async move {
        let code = match child.wait().await {
            Ok(status) => status.code().unwrap_or(128 + 9),
            Err(_) => -1,
        };
        // Let the reader drain what the shell wrote last.
        tokio::time::sleep(Duration::from_millis(100)).await;
        exited.output.lock().unwrap().exit = Some(code);
        exited.changed.notify_waiters();
    });
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_shell_runs_input_streams_output_and_resumes_from_an_offset() {
        let dir = tempfile::tempdir().unwrap();
        let terminals = Terminals::new(dir.path());
        let (client, server) = tokio::io::duplex(1 << 16);
        let (server_read, server_write) = tokio::io::split(server);
        let attached = terminals.open(80, 24).unwrap();
        let serving = tokio::spawn({
            let terminals = terminals.clone();
            async move {
                terminals
                    .serve(attached, 0, server_read, server_write)
                    .await
            }
        });
        let (mut from_shell, mut to_shell) = tokio::io::split(client);
        write_frame(
            &mut to_shell,
            &Frame::Data(b"echo marker-$((40+2))\r".to_vec()),
        )
        .await
        .unwrap();
        let mut seen = Vec::new();
        while !String::from_utf8_lossy(&seen).contains("marker-42") {
            match tokio::time::timeout(Duration::from_secs(10), read_frame(&mut from_shell))
                .await
                .unwrap()
                .unwrap()
            {
                Some(Frame::Data(bytes)) => seen.extend(bytes),
                other => panic!("{other:?}"),
            }
        }
        // Leaving keeps the session: a new attachment replays from the offset it asks for.
        drop((from_shell, to_shell));
        serving.await.unwrap().ok();
        let again = terminals.open(100, 30).unwrap();
        assert_eq!(again.start, 0);
        let (client, server) = tokio::io::duplex(1 << 16);
        let (server_read, server_write) = tokio::io::split(server);
        let serving = tokio::spawn({
            let terminals = terminals.clone();
            async move { terminals.serve(again, 0, server_read, server_write).await }
        });
        let (mut from_shell, mut to_shell) = tokio::io::split(client);
        let mut replayed = Vec::new();
        while !String::from_utf8_lossy(&replayed).contains("marker-42") {
            if let Some(Frame::Data(bytes)) = read_frame(&mut from_shell).await.unwrap() {
                replayed.extend(bytes);
            }
        }
        write_frame(&mut to_shell, &Frame::Data(b"exit 3\r".to_vec()))
            .await
            .unwrap();
        let exit = loop {
            match tokio::time::timeout(Duration::from_secs(10), read_frame(&mut from_shell))
                .await
                .unwrap()
                .unwrap()
            {
                Some(Frame::Exit(code)) => break code,
                Some(_) => continue,
                None => panic!("closed before exit"),
            }
        };
        assert_eq!(exit, 3);
        serving.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn frames_round_trip_and_malformed_ones_are_refused() {
        let frames = [
            Frame::Data(b"ls -la\r".to_vec()),
            Frame::Resize {
                cols: 120,
                rows: 40,
            },
            Frame::Close,
            Frame::Exit(130),
        ];
        let mut wire = Vec::new();
        for frame in &frames {
            write_frame(&mut wire, frame).await.unwrap();
        }
        let mut reader = wire.as_slice();
        for frame in &frames {
            assert_eq!(read_frame(&mut reader).await.unwrap().as_ref(), Some(frame));
        }
        assert_eq!(read_frame(&mut reader).await.unwrap(), None);
        let mut bad: &[u8] = &[RESIZE, 0, 0, 0, 1, 7];
        assert!(read_frame(&mut bad).await.is_err());
        let mut huge: &[u8] = &[DATA, 0xff, 0xff, 0xff, 0xff];
        assert!(read_frame(&mut huge).await.is_err());
    }
}
