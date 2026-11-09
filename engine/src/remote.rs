//! Reading a device through a privileged helper process.
//!
//! Raw disks need administrator rights. Rather than run the whole app as
//! root, the app runs a tiny helper with those rights whose only ability is
//! to read the one device it was started for: it opens it read-only, then
//! answers "give me N bytes at offset X" requests over a Unix socket. All
//! parsing, scanning and file writing stays in the unprivileged app.
//!
//! The app listens on a socket in a private directory and the helper
//! connects to it, so the privileged process never accepts connections.
//!
//! Wire format (all integers little-endian):
//! - helper -> app, once: `b"RPH1"`, device length `u64`
//! - app -> helper: offset `u64`, length `u32`
//! - helper -> app: byte count `u32` (or `u32::MAX` on a read error), bytes

#![cfg(unix)]

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Mutex;

use crate::source::{DeviceSource, Source};

const MAGIC: &[u8; 4] = b"RPH1";
const MAX_REQUEST: u32 = 16 << 20;
const READ_ERROR: u32 = u32::MAX;

/// Helper side: opens `device` read-only and serves reads until the app
/// disconnects.
pub fn serve(device: &Path, socket: &Path) -> io::Result<()> {
    let src = DeviceSource::open(device)?;
    let mut stream = UnixStream::connect(socket)?;
    stream.write_all(MAGIC)?;
    stream.write_all(&src.len().to_le_bytes())?;
    let mut buf = Vec::new();
    loop {
        let mut req = [0u8; 12];
        match stream.read_exact(&mut req) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        }
        let offset = u64::from_le_bytes(req[..8].try_into().unwrap());
        let len = u32::from_le_bytes(req[8..].try_into().unwrap()).min(MAX_REQUEST);
        buf.resize(len as usize, 0);
        match src.read_at(offset, &mut buf) {
            Ok(n) => {
                stream.write_all(&(n as u32).to_le_bytes())?;
                stream.write_all(&buf[..n])?;
            }
            Err(_) => stream.write_all(&READ_ERROR.to_le_bytes())?,
        }
    }
}

/// App side: a [`Source`] whose reads are served by the helper.
pub struct RemoteSource {
    stream: Mutex<UnixStream>,
    len: u64,
}

impl RemoteSource {
    /// Completes the handshake on a stream the helper connected with.
    pub fn handshake(mut stream: UnixStream) -> io::Result<Self> {
        let mut hello = [0u8; 12];
        stream.read_exact(&mut hello)?;
        if &hello[..4] != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a Reperio helper",
            ));
        }
        let len = u64::from_le_bytes(hello[4..].try_into().unwrap());
        Ok(Self {
            stream: Mutex::new(stream),
            len,
        })
    }
}

impl Source for RemoteSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let mut stream = self.stream.lock().unwrap_or_else(|e| e.into_inner());
        let mut done = 0;
        while done < buf.len() {
            let want = (buf.len() - done).min(MAX_REQUEST as usize);
            let mut req = [0u8; 12];
            req[..8].copy_from_slice(&(offset + done as u64).to_le_bytes());
            req[8..].copy_from_slice(&(want as u32).to_le_bytes());
            stream.write_all(&req)?;
            let mut n = [0u8; 4];
            stream.read_exact(&mut n)?;
            let n = u32::from_le_bytes(n);
            if n == READ_ERROR {
                return Err(io::Error::other("the device could not be read here"));
            }
            let n = (n as usize).min(want);
            stream.read_exact(&mut buf[done..done + n])?;
            done += n;
            if n < want {
                break;
            }
        }
        Ok(done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn serves_reads_over_socket() {
        let dir = std::env::temp_dir().join(format!("reperio-remote-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("disk.img");
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&image, &data).unwrap();
        let socket = dir.join("s");
        let listener = UnixListener::bind(&socket).unwrap();

        let helper = std::thread::spawn({
            let (image, socket) = (image.clone(), socket.clone());
            move || serve(&image, &socket).unwrap()
        });
        let remote = RemoteSource::handshake(listener.accept().unwrap().0).unwrap();
        assert_eq!(remote.len(), data.len() as u64);
        let mut buf = vec![0u8; 5000];
        assert_eq!(remote.read_at(123_457, &mut buf).unwrap(), 5000);
        assert_eq!(buf, data[123_457..128_457]);
        assert_eq!(remote.read_at(299_000, &mut buf).unwrap(), 1000);
        drop(remote);
        helper.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
