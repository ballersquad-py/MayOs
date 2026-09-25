//! Blocking TCP sockets for kernel threads, on top of `net::tcp`.

use net::tcp::{self, Handle, State};
use net::Ipv4;

use super::{NetError, IFACE};
use crate::proc::sched;
use crate::time::uptime_ms;

/// Run `f` on the TCP stack, then transmit what it queued.
fn with<R>(f: impl FnOnce(&mut tcp::Stack, Ipv4) -> R) -> Result<R, NetError> {
    let mut g = IFACE.lock();
    let i = g.as_mut().ok_or(NetError::NoAdapter)?;
    let r = f(&mut i.tcp, i.ip);
    i.tcp_flush();
    Ok(r)
}

fn map_err(e: tcp::Error) -> NetError {
    match e {
        tcp::Error::Reset => NetError::Reset,
        tcp::Error::TimedOut => NetError::Timeout,
        tcp::Error::NotConnected => NetError::Reset,
        tcp::Error::InUse => NetError::AddressInUse,
    }
}

pub struct TcpListener {
    port: u16,
}

impl TcpListener {
    pub fn bind(port: u16) -> Result<TcpListener, NetError> {
        with(|t, _| t.listen(port))?.map_err(map_err)?;
        Ok(TcpListener { port })
    }

    /// Wait up to `timeout_ms` for a connection.
    pub fn accept(&self, timeout_ms: u64) -> Option<TcpStream> {
        let deadline = uptime_ms() + timeout_ms;
        loop {
            if let Ok(Some(h)) = with(|t, _| t.accept(self.port)) {
                return Some(TcpStream { h });
            }
            if uptime_ms() >= deadline {
                return None;
            }
            sched::sleep_ms(20);
        }
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        let _ = with(|t, _| t.unlisten(self.port));
    }
}

pub struct TcpStream {
    h: Handle,
}

impl TcpStream {
    pub fn connect(ip: Ipv4, port: u16, timeout_ms: u64) -> Result<TcpStream, NetError> {
        let h = with(|t, me| {
            if me.is_unspecified() {
                None
            } else {
                Some(t.connect(me, ip, port, uptime_ms()))
            }
        })?
        .ok_or(NetError::NotConfigured)?;
        let s = TcpStream { h };
        let deadline = uptime_ms() + timeout_ms;
        loop {
            let (state, err) = with(|t, _| (t.state(h), t.error(h)))?;
            match (state, err) {
                (State::Established | State::CloseWait, _) => return Ok(s),
                (_, Some(tcp::Error::Reset)) => return Err(NetError::Refused),
                (_, Some(e)) => return Err(map_err(e)),
                _ => {}
            }
            if uptime_ms() >= deadline {
                return Err(NetError::Timeout);
            }
            sched::sleep_ms(2);
        }
    }

    pub fn peer(&self) -> Option<(Ipv4, u16)> {
        with(|t, _| t.remote(self.h)).ok().flatten()
    }

    /// Read what is available (waiting up to `timeout_ms` for something).
    /// `Ok(0)` means the peer closed the connection.
    pub fn read(&mut self, buf: &mut [u8], timeout_ms: u64) -> Result<usize, NetError> {
        let deadline = uptime_ms() + timeout_ms;
        loop {
            match with(|t, _| t.recv(self.h, buf))? {
                Some(Ok(n)) => return Ok(n),
                Some(Err(e)) => return Err(map_err(e)),
                None => {}
            }
            if uptime_ms() >= deadline {
                return Err(NetError::Timeout);
            }
            sched::sleep_ms(1);
        }
    }

    /// Queue all of `data`, waiting while the send buffer is full.
    pub fn write_all(&mut self, mut data: &[u8], timeout_ms: u64) -> Result<(), NetError> {
        let mut deadline = uptime_ms() + timeout_ms;
        while !data.is_empty() {
            let n = with(|t, _| t.send(self.h, data))?.map_err(map_err)?;
            data = &data[n..];
            if n > 0 {
                deadline = uptime_ms() + timeout_ms;
            } else if uptime_ms() >= deadline {
                return Err(NetError::Timeout);
            } else {
                sched::sleep_ms(1);
            }
        }
        Ok(())
    }

    /// Wait until everything written has been acknowledged.
    pub fn flush(&mut self, timeout_ms: u64) -> Result<(), NetError> {
        let deadline = uptime_ms() + timeout_ms;
        loop {
            let (queued, err) = with(|t, _| (t.send_queued(self.h), t.error(self.h)))?;
            if let Some(e) = err {
                return Err(map_err(e));
            }
            if queued == 0 {
                return Ok(());
            }
            if uptime_ms() >= deadline {
                return Err(NetError::Timeout);
            }
            sched::sleep_ms(2);
        }
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        let _ = with(|t, _| t.close(self.h));
    }
}
