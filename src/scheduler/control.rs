//! Local wakeups interrupt deadline-based waits; task data stays in SQLite.

use anyhow::Context;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::Path;
use std::time::Duration;

pub(super) fn lock(path: &Path) -> anyhow::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path.with_extension("control"))?;
    file.lock().context("lock scheduler lifecycle")?;
    Ok(file)
}

pub(super) fn wake(path: &Path) -> anyhow::Result<()> {
    let file = match File::open(path.with_extension("wake")) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut port = String::new();
    file.take(16).read_to_string(&mut port)?;
    // Startup publishes the endpoint before reading the saved task schedule.
    if port.trim().is_empty() {
        return Ok(());
    }
    let port: u16 = port.trim().parse().context("read scheduler wakeup port")?;
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
    socket.send_to(&[1], (Ipv4Addr::LOCALHOST, port))?;
    Ok(())
}

pub(super) struct Waiter {
    socket: UdpSocket,
    address: SocketAddr,
}

impl Waiter {
    pub(super) fn new(path: &Path) -> anyhow::Result<Self> {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
        let address = socket.local_addr()?;
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        // Windows denies reads through other handles to the exclusively locked instance file.
        let mut endpoint = options.open(path.with_extension("wake"))?;
        writeln!(endpoint, "{}", address.port())?;
        Ok(Self { socket, address })
    }

    pub(super) fn clear(path: &Path) -> anyhow::Result<()> {
        match std::fs::remove_file(path.with_extension("wake")) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn notify(&self) -> anyhow::Result<()> {
        self.socket.send_to(&[1], self.address)?;
        Ok(())
    }

    pub(super) fn wait(&self, timeout: Option<Duration>) -> anyhow::Result<()> {
        self.socket.set_read_timeout(timeout)?;
        match self.socket.recv(&mut [0; 1]) {
            Ok(_) => Ok(()),
            Err(error) if matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wakeup_sent_before_waiting_is_not_lost() {
        let path = std::env::temp_dir().join(format!("datacraft-wakeup-{}.sqlite", uuid::Uuid::new_v4()));
        let file = OpenOptions::new().read(true).write(true).create_new(true).open(path.with_extension("lock")).unwrap();
        file.lock().unwrap();
        let waiter = Waiter::new(&path).unwrap();
        wake(&path).unwrap();
        waiter.socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        assert_eq!(waiter.socket.recv(&mut [0; 1]).unwrap(), 1);
        waiter.notify().unwrap();
        assert_eq!(waiter.socket.recv(&mut [0; 1]).unwrap(), 1);
        Waiter::clear(&path).unwrap();
        wake(&path).unwrap();
        drop(file);
        std::fs::remove_file(path.with_extension("lock")).unwrap();
        wake(&path).unwrap();
    }
}
