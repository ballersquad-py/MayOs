//! TCP (RFC 793 with the usual modern simplifications).
//!
//! The stack is "sans I/O": feed it received segments with `input`, call
//! `poll` regularly, and send whatever it queues in `out`. Sockets are
//! numbered handles. Out-of-order segments are dropped (the peer resends
//! them); lost data is resent after a timeout or three duplicate ACKs.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;

use crate::{be16, checksum, pseudo_header_sum, Ipv4, PROTO_TCP};

pub const FIN: u8 = 0x01;
pub const SYN: u8 = 0x02;
pub const RST: u8 = 0x04;
pub const PSH: u8 = 0x08;
pub const ACK: u8 = 0x10;

/// Our maximum segment size (Ethernet MTU minus IP and TCP headers).
pub const MSS: usize = 1460;
/// Receive buffer per connection (no window scaling, so at most 64 KiB).
const RECV_CAP: usize = 65535;
/// Bytes a socket buffers for sending before `send` accepts no more.
pub const SEND_CAP: usize = 256 * 1024;
const RTO_INITIAL: u64 = 400;
const RTO_MAX: u64 = 4000;
const MAX_RETRIES: u32 = 10;
const TIME_WAIT_MS: u64 = 2000;
const BACKLOG: usize = 16;

// ---------------------------------------------------------------------------
// Segments
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    pub mss: Option<u16>,
    pub payload: &'a [u8],
}

impl<'a> Segment<'a> {
    /// Parse and verify the checksum (which covers the IP pseudo-header).
    pub fn parse(b: &'a [u8], src: Ipv4, dst: Ipv4) -> Option<Segment<'a>> {
        if b.len() < 20 {
            return None;
        }
        let off = ((b[12] >> 4) as usize) * 4;
        if off < 20 || off > b.len() {
            return None;
        }
        if checksum(b, pseudo_header_sum(src, dst, PROTO_TCP, b.len())) != 0 {
            return None;
        }
        let mut mss = None;
        let mut i = 20;
        while i < off {
            match b[i] {
                0 => break,
                1 => i += 1,
                kind => {
                    let len = *b.get(i + 1)? as usize;
                    if len < 2 || i + len > off {
                        break;
                    }
                    if kind == 2 && len == 4 {
                        mss = Some(be16(b, i + 2));
                    }
                    i += len;
                }
            }
        }
        Some(Segment {
            src_port: be16(b, 0),
            dst_port: be16(b, 2),
            seq: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
            ack: u32::from_be_bytes([b[8], b[9], b[10], b[11]]),
            flags: b[13],
            window: be16(b, 14),
            mss,
            payload: &b[off..],
        })
    }
}

pub fn build_segment(src: Ipv4, dst: Ipv4, s: &Segment) -> Vec<u8> {
    let opt = if s.mss.is_some() { 4 } else { 0 };
    let len = 20 + opt + s.payload.len();
    let mut v = Vec::with_capacity(len);
    v.extend_from_slice(&s.src_port.to_be_bytes());
    v.extend_from_slice(&s.dst_port.to_be_bytes());
    v.extend_from_slice(&s.seq.to_be_bytes());
    v.extend_from_slice(&s.ack.to_be_bytes());
    v.push((((20 + opt) / 4) as u8) << 4);
    v.push(s.flags);
    v.extend_from_slice(&s.window.to_be_bytes());
    v.extend_from_slice(&[0, 0, 0, 0]);
    if let Some(m) = s.mss {
        v.extend_from_slice(&[2, 4]);
        v.extend_from_slice(&m.to_be_bytes());
    }
    v.extend_from_slice(s.payload);
    let c = checksum(&v, pseudo_header_sum(src, dst, PROTO_TCP, len));
    v[16..18].copy_from_slice(&c.to_be_bytes());
    v
}

/// `a < b` in sequence space.
fn lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

fn le(a: u32, b: u32) -> bool {
    a == b || lt(a, b)
}

// ---------------------------------------------------------------------------
// Sockets
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    SynSent,
    SynReceived,
    Established,
    /// We sent FIN, waiting for its ACK.
    FinWait1,
    /// Our FIN is acknowledged; waiting for the peer's.
    FinWait2,
    /// The peer sent FIN; we may still send.
    CloseWait,
    Closing,
    LastAck,
    TimeWait,
    Closed,
}

pub type Handle = usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The peer reset the connection (or it could not be established).
    Reset,
    /// The connection timed out.
    TimedOut,
    /// No such socket, or it is not connected.
    NotConnected,
    /// The port is already in use.
    InUse,
}

struct Sock {
    state: State,
    local_ip: Ipv4,
    local_port: u16,
    remote_ip: Ipv4,
    remote_port: u16,
    /// Oldest unacknowledged sequence number; `send_buf[0]` has this number.
    snd_una: u32,
    /// Next sequence number to send.
    snd_nxt: u32,
    /// Peer's advertised window.
    snd_wnd: u32,
    peer_mss: usize,
    send_buf: VecDeque<u8>,
    rcv_nxt: u32,
    recv_buf: VecDeque<u8>,
    /// Window we last advertised.
    adv_wnd: usize,
    /// The application has no more data to send (FIN once drained).
    fin_queued: bool,
    /// Sequence number of our FIN once sent.
    fin_seq: Option<u32>,
    /// The peer has closed its side.
    peer_fin: bool,
    ack_due: bool,
    rto: u64,
    rto_at: Option<u64>,
    retries: u32,
    dup_acks: u32,
    /// For SYN(-ACK) retransmission.
    syn_seq: u32,
    error: Option<Error>,
    /// The application closed its handle; free the socket when done.
    released: bool,
    /// Listening port this connection arrived on, until accepted.
    from_listener: Option<u16>,
    time_wait_until: u64,
}

impl Sock {
    fn in_flight(&self) -> u32 {
        self.snd_nxt.wrapping_sub(self.snd_una)
    }

    fn window(&self) -> u16 {
        (RECV_CAP - self.recv_buf.len()).min(65535) as u16
    }

    fn can_send_data(&self) -> bool {
        matches!(self.state, State::Established | State::CloseWait)
    }
}

pub struct Stack {
    socks: BTreeMap<Handle, Sock>,
    /// Listening port -> connections waiting to be accepted.
    listeners: BTreeMap<u16, VecDeque<Handle>>,
    next_handle: Handle,
    next_port: u16,
    iss: u32,
    /// Segments to transmit: (source, destination, TCP segment bytes).
    pub out: Vec<(Ipv4, Ipv4, Vec<u8>)>,
}

impl Stack {
    pub fn new(seed: u32) -> Stack {
        Stack {
            socks: BTreeMap::new(),
            listeners: BTreeMap::new(),
            next_handle: 1,
            next_port: 49152 + (seed % 8000) as u16,
            iss: seed,
            out: Vec::new(),
        }
    }

    fn new_iss(&mut self) -> u32 {
        self.iss = self.iss.wrapping_mul(1_103_515_245).wrapping_add(12345 + 64_000);
        self.iss
    }

    pub fn listen(&mut self, port: u16) -> Result<(), Error> {
        if self.listeners.contains_key(&port) {
            return Err(Error::InUse);
        }
        self.listeners.insert(port, VecDeque::new());
        Ok(())
    }

    pub fn unlisten(&mut self, port: u16) {
        if let Some(q) = self.listeners.remove(&port) {
            for h in q {
                self.abort(h);
            }
        }
    }

    /// A fully established connection on `port`, if any.
    pub fn accept(&mut self, port: u16) -> Option<Handle> {
        let q = self.listeners.get_mut(&port)?;
        let pos = q.iter().position(|h| self.socks.get(h).map(|s| s.state != State::SynReceived).unwrap_or(false))?;
        let h = q.remove(pos)?;
        if let Some(s) = self.socks.get_mut(&h) {
            s.from_listener = None;
        }
        Some(h)
    }

    /// Start connecting; poll `state` until it is `Established`.
    pub fn connect(&mut self, local_ip: Ipv4, remote_ip: Ipv4, remote_port: u16, now: u64) -> Handle {
        self.next_port = if self.next_port >= 65000 { 49152 } else { self.next_port + 1 };
        let iss = self.new_iss();
        let h = self.alloc(Sock::new(State::SynSent, local_ip, self.next_port, remote_ip, remote_port, iss, 0));
        let s = self.socks.get_mut(&h).unwrap();
        s.rto_at = Some(now + s.rto);
        self.send_syn(h, false);
        h
    }

    fn alloc(&mut self, s: Sock) -> Handle {
        let h = self.next_handle;
        self.next_handle += 1;
        self.socks.insert(h, s);
        h
    }

    pub fn state(&self, h: Handle) -> State {
        self.socks.get(&h).map(|s| s.state).unwrap_or(State::Closed)
    }

    pub fn error(&self, h: Handle) -> Option<Error> {
        match self.socks.get(&h) {
            Some(s) => s.error,
            None => Some(Error::NotConnected),
        }
    }

    pub fn remote(&self, h: Handle) -> Option<(Ipv4, u16)> {
        self.socks.get(&h).map(|s| (s.remote_ip, s.remote_port))
    }

    /// Queue data; returns how many bytes were accepted.
    pub fn send(&mut self, h: Handle, data: &[u8]) -> Result<usize, Error> {
        let s = self.socks.get_mut(&h).ok_or(Error::NotConnected)?;
        if let Some(e) = s.error {
            return Err(e);
        }
        if s.fin_queued || !matches!(s.state, State::SynSent | State::SynReceived | State::Established | State::CloseWait) {
            return Err(Error::NotConnected);
        }
        let n = data.len().min(SEND_CAP.saturating_sub(s.send_buf.len()));
        s.send_buf.extend(&data[..n]);
        Ok(n)
    }

    /// Bytes queued but not yet acknowledged by the peer.
    pub fn send_queued(&self, h: Handle) -> usize {
        self.socks.get(&h).map(|s| s.send_buf.len()).unwrap_or(0)
    }

    /// Read received data. `Ok(0)` with a non-empty buffer means the peer
    /// closed its side (end of stream); `None` means nothing yet.
    pub fn recv(&mut self, h: Handle, buf: &mut [u8]) -> Option<Result<usize, Error>> {
        let s = self.socks.get_mut(&h)?;
        if !s.recv_buf.is_empty() {
            let n = buf.len().min(s.recv_buf.len());
            for (d, b) in buf[..n].iter_mut().zip(s.recv_buf.drain(..n)) {
                *d = b;
            }
            // Tell the peer when a nearly closed window opens up again.
            if (s.window() as usize) >= s.adv_wnd + 2 * MSS {
                s.ack_due = true;
            }
            return Some(Ok(n));
        }
        if let Some(e) = s.error {
            return Some(Err(e));
        }
        if s.peer_fin || s.state == State::Closed {
            return Some(Ok(0));
        }
        None
    }

    /// Graceful close: send what is queued, then FIN. The handle becomes
    /// invalid; the socket lingers until the close handshake is done.
    pub fn close(&mut self, h: Handle) {
        let Some(s) = self.socks.get_mut(&h) else { return };
        s.released = true;
        match s.state {
            State::SynSent | State::Closed | State::TimeWait => {
                s.state = State::Closed;
            }
            _ => s.fin_queued = true,
        }
    }

    /// Drop the connection immediately (sends RST).
    pub fn abort(&mut self, h: Handle) {
        let Some(s) = self.socks.remove(&h) else { return };
        if !matches!(s.state, State::Closed | State::TimeWait | State::SynSent) {
            let seg = Segment {
                src_port: s.local_port,
                dst_port: s.remote_port,
                seq: s.snd_nxt,
                ack: s.rcv_nxt,
                flags: RST | ACK,
                window: 0,
                mss: None,
                payload: &[],
            };
            self.out.push((s.local_ip, s.remote_ip, build_segment(s.local_ip, s.remote_ip, &seg)));
        }
    }

    // --- input ------------------------------------------------------

    /// Process one received segment addressed to `dst`.
    pub fn input(&mut self, src: Ipv4, dst: Ipv4, bytes: &[u8], now: u64) {
        let Some(seg) = Segment::parse(bytes, src, dst) else { return };
        let found = self
            .socks
            .iter()
            .find(|(_, s)| {
                s.local_port == seg.dst_port && s.remote_port == seg.src_port && s.remote_ip == src && s.state != State::Closed
            })
            .map(|(h, _)| *h);
        match found {
            Some(h) => self.input_sock(h, &seg, now),
            None => {
                if seg.flags & (SYN | ACK | RST) == SYN && self.listeners.contains_key(&seg.dst_port) {
                    self.passive_open(src, dst, &seg, now);
                } else if seg.flags & RST == 0 {
                    self.reset_reply(src, dst, &seg);
                }
            }
        }
    }

    fn reset_reply(&mut self, src: Ipv4, dst: Ipv4, seg: &Segment) {
        let seg_len = seg.payload.len() as u32 + (seg.flags & SYN != 0) as u32 + (seg.flags & FIN != 0) as u32;
        let (seq, ack, flags) = if seg.flags & ACK != 0 { (seg.ack, 0, RST) } else { (0, seg.seq.wrapping_add(seg_len), RST | ACK) };
        let r = Segment { src_port: seg.dst_port, dst_port: seg.src_port, seq, ack, flags, window: 0, mss: None, payload: &[] };
        self.out.push((dst, src, build_segment(dst, src, &r)));
    }

    fn passive_open(&mut self, src: Ipv4, dst: Ipv4, seg: &Segment, now: u64) {
        let port = seg.dst_port;
        if self.listeners.get(&port).map(|q| q.len() >= BACKLOG).unwrap_or(true) {
            return;
        }
        let iss = self.new_iss();
        let mut s = Sock::new(State::SynReceived, dst, port, src, seg.src_port, iss, seg.seq.wrapping_add(1));
        s.snd_wnd = seg.window as u32;
        s.peer_mss = seg.mss.map(|m| m as usize).unwrap_or(536).clamp(64, MSS);
        s.from_listener = Some(port);
        s.rto_at = Some(now + s.rto);
        let h = self.alloc(s);
        self.listeners.get_mut(&port).unwrap().push_back(h);
        self.send_syn(h, true);
    }

    fn send_syn(&mut self, h: Handle, ack: bool) {
        let s = &self.socks[&h];
        let seg = Segment {
            src_port: s.local_port,
            dst_port: s.remote_port,
            seq: s.syn_seq,
            ack: if ack { s.rcv_nxt } else { 0 },
            flags: if ack { SYN | ACK } else { SYN },
            window: s.window(),
            mss: Some(MSS as u16),
            payload: &[],
        };
        let pkt = build_segment(s.local_ip, s.remote_ip, &seg);
        self.out.push((s.local_ip, s.remote_ip, pkt));
    }

    fn input_sock(&mut self, h: Handle, seg: &Segment, now: u64) {
        let s = self.socks.get_mut(&h).unwrap();
        if seg.flags & RST != 0 {
            // Accept resets that fall in the window (or answer our SYN).
            let ok = if s.state == State::SynSent { seg.flags & ACK != 0 && seg.ack == s.syn_seq.wrapping_add(1) } else { true };
            if ok {
                s.state = State::Closed;
                s.error = Some(Error::Reset);
                s.rto_at = None;
            }
            return;
        }
        match s.state {
            State::SynSent => {
                if seg.flags & (SYN | ACK) == SYN | ACK && seg.ack == s.syn_seq.wrapping_add(1) {
                    s.rcv_nxt = seg.seq.wrapping_add(1);
                    s.snd_una = seg.ack;
                    s.snd_nxt = seg.ack;
                    s.snd_wnd = seg.window as u32;
                    s.peer_mss = seg.mss.map(|m| m as usize).unwrap_or(536).clamp(64, MSS);
                    s.state = State::Established;
                    s.rto_at = None;
                    s.retries = 0;
                    s.ack_due = true;
                }
                return;
            }
            State::SynReceived => {
                if seg.flags & SYN != 0 && seg.flags & ACK == 0 {
                    // Retransmitted SYN: answer again.
                    self.send_syn(h, true);
                    return;
                }
                if seg.flags & ACK == 0 || seg.ack != s.syn_seq.wrapping_add(1) {
                    return;
                }
                s.snd_una = seg.ack;
                s.snd_nxt = seg.ack;
                s.state = State::Established;
                s.rto_at = None;
                s.retries = 0;
            }
            State::Closed => return,
            _ => {}
        }
        let s = self.socks.get_mut(&h).unwrap();
        if seg.flags & SYN != 0 {
            // A stray SYN on a synchronised connection: re-ACK.
            s.ack_due = true;
            return;
        }

        // ACK processing.
        if seg.flags & ACK != 0 {
            if lt(s.snd_una, seg.ack) && le(seg.ack, s.snd_nxt) {
                let acked = seg.ack.wrapping_sub(s.snd_una) as usize;
                let data_acked = acked.min(s.send_buf.len());
                s.send_buf.drain(..data_acked);
                s.snd_una = seg.ack;
                s.retries = 0;
                s.dup_acks = 0;
                s.rto = RTO_INITIAL;
                s.rto_at = if s.in_flight() > 0 { Some(now + s.rto) } else { None };
                if let Some(f) = s.fin_seq
                    && lt(f, seg.ack)
                {
                    s.state = match s.state {
                        State::FinWait1 => State::FinWait2,
                        State::Closing => {
                            s.time_wait_until = now + TIME_WAIT_MS;
                            State::TimeWait
                        }
                        State::LastAck => State::Closed,
                        other => other,
                    };
                }
            } else if seg.ack == s.snd_una && seg.payload.is_empty() && s.in_flight() > 0 && seg.window as u32 == s.snd_wnd {
                s.dup_acks += 1;
                if s.dup_acks == 3 {
                    // Fast retransmit: go back to the first unacked byte.
                    s.snd_nxt = s.snd_una;
                    s.fin_seq = None;
                }
            }
            s.snd_wnd = seg.window as u32;
        }

        // Data.
        if !seg.payload.is_empty() && matches!(s.state, State::Established | State::FinWait1 | State::FinWait2) {
            let mut data = seg.payload;
            let mut seq = seg.seq;
            if lt(seq, s.rcv_nxt) {
                let skip = s.rcv_nxt.wrapping_sub(seq) as usize;
                data = if skip < data.len() { &data[skip..] } else { &[] };
                seq = s.rcv_nxt;
            }
            if seq == s.rcv_nxt {
                let room = RECV_CAP - s.recv_buf.len();
                let n = data.len().min(room);
                s.recv_buf.extend(&data[..n]);
                s.rcv_nxt = s.rcv_nxt.wrapping_add(n as u32);
            }
            s.ack_due = true;
        }

        // FIN.
        if seg.flags & FIN != 0 {
            let fin_at = seg.seq.wrapping_add(seg.payload.len() as u32);
            if fin_at == s.rcv_nxt && !s.peer_fin {
                s.rcv_nxt = s.rcv_nxt.wrapping_add(1);
                s.peer_fin = true;
                s.state = match s.state {
                    State::Established => State::CloseWait,
                    State::FinWait1 => State::Closing,
                    State::FinWait2 => {
                        s.time_wait_until = now + TIME_WAIT_MS;
                        State::TimeWait
                    }
                    other => other,
                };
            }
            s.ack_due = true;
        }
    }

    // --- output -----------------------------------------------------

    /// Run timers and transmit what can be sent. Call every few ms and
    /// after `send`/`recv`/`close`.
    pub fn poll(&mut self, now: u64) {
        let handles: Vec<Handle> = self.socks.keys().copied().collect();
        for h in handles {
            self.poll_sock(h, now);
        }
        // Free finished sockets whose handles were released.
        self.socks.retain(|_, s| {
            let done = s.state == State::Closed || (s.state == State::TimeWait && now >= s.time_wait_until);
            !(done && (s.released || s.error.is_some() && s.from_listener.is_some()))
        });
        let socks = &self.socks;
        for q in self.listeners.values_mut() {
            q.retain(|h| socks.contains_key(h));
        }
    }

    fn poll_sock(&mut self, h: Handle, now: u64) {
        let s = self.socks.get_mut(&h).unwrap();
        // Retransmission timer.
        if let Some(t) = s.rto_at
            && now >= t
        {
            s.retries += 1;
            if s.retries > MAX_RETRIES {
                s.state = State::Closed;
                s.error = Some(Error::TimedOut);
                s.rto_at = None;
                return;
            }
            s.rto = (s.rto * 2).min(RTO_MAX);
            s.rto_at = Some(now + s.rto);
            match s.state {
                State::SynSent => return self.send_syn(h, false),
                State::SynReceived => return self.send_syn(h, true),
                _ => {
                    s.snd_nxt = s.snd_una;
                    s.dup_acks = 0;
                    s.fin_seq = None;
                    // Zero-window probe: allow one byte.
                    if s.snd_wnd == 0 {
                        s.snd_wnd = 1;
                    }
                }
            }
        }
        if matches!(s.state, State::SynSent | State::SynReceived | State::Closed | State::TimeWait) {
            if s.ack_due && s.state == State::TimeWait {
                self.send_flags(h, ACK, 0, 0);
            }
            return;
        }
        // Data segments.
        let mut sent_any = false;
        loop {
            let s = self.socks.get_mut(&h).unwrap();
            let offset = s.snd_nxt.wrapping_sub(s.snd_una) as usize;
            let unsent = s.send_buf.len().saturating_sub(offset);
            let wnd_left = (s.snd_wnd as usize).saturating_sub(offset);
            let n = unsent.min(wnd_left).min(s.peer_mss);
            let fin_now = s.fin_queued && s.fin_seq.is_none() && offset + n == s.send_buf.len();
            if n == 0 && !fin_now {
                break;
            }
            if !s.can_send_data() && n > 0 && !matches!(s.state, State::FinWait1 | State::Closing | State::LastAck) {
                break;
            }
            let mut flags = ACK;
            if n > 0 && offset + n == s.send_buf.len() {
                flags |= PSH;
            }
            if fin_now {
                flags |= FIN;
            }
            self.send_flags(h, flags, offset, n);
            sent_any = true;
            let s = self.socks.get_mut(&h).unwrap();
            s.snd_nxt = s.snd_nxt.wrapping_add(n as u32);
            if fin_now {
                s.fin_seq = Some(s.snd_nxt);
                s.snd_nxt = s.snd_nxt.wrapping_add(1);
                s.state = match s.state {
                    State::Established => State::FinWait1,
                    State::CloseWait => State::LastAck,
                    other => other,
                };
            }
            if s.rto_at.is_none() {
                s.rto_at = Some(now + s.rto);
            }
            if fin_now {
                break;
            }
        }
        let s = self.socks.get_mut(&h).unwrap();
        if s.ack_due && !sent_any {
            self.send_flags(h, ACK, 0, 0);
        }
    }

    /// Send a segment carrying `n` bytes of `send_buf` from `offset`.
    fn send_flags(&mut self, h: Handle, flags: u8, offset: usize, n: usize) {
        let s = self.socks.get_mut(&h).unwrap();
        let payload: Vec<u8> = s.send_buf.range(offset..offset + n).copied().collect();
        let seq = s.snd_una.wrapping_add(offset as u32);
        let w = s.window();
        s.adv_wnd = w as usize;
        s.ack_due = false;
        let seg = Segment {
            src_port: s.local_port,
            dst_port: s.remote_port,
            seq,
            ack: s.rcv_nxt,
            flags,
            window: w,
            mss: None,
            payload: &payload,
        };
        let pkt = build_segment(s.local_ip, s.remote_ip, &seg);
        self.out.push((s.local_ip, s.remote_ip, pkt));
    }
}

impl Sock {
    fn new(state: State, local_ip: Ipv4, local_port: u16, remote_ip: Ipv4, remote_port: u16, iss: u32, rcv_nxt: u32) -> Sock {
        Sock {
            state,
            local_ip,
            local_port,
            remote_ip,
            remote_port,
            snd_una: iss.wrapping_add(1),
            snd_nxt: iss.wrapping_add(1),
            snd_wnd: 0,
            peer_mss: 536,
            send_buf: VecDeque::new(),
            rcv_nxt,
            recv_buf: VecDeque::new(),
            adv_wnd: RECV_CAP,
            fin_queued: false,
            fin_seq: None,
            peer_fin: false,
            ack_due: false,
            rto: RTO_INITIAL,
            rto_at: None,
            retries: 0,
            dup_acks: 0,
            syn_seq: iss,
            error: None,
            released: false,
            from_listener: None,
            time_wait_until: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    const A: Ipv4 = Ipv4([10, 0, 0, 1]);
    const B: Ipv4 = Ipv4([10, 0, 0, 2]);

    /// Deliver queued segments between two stacks, dropping about one in
    /// `drop_every` at random (0 = lossless).
    fn pump(a: &mut Stack, b: &mut Stack, now: u64, drop_every: usize, counter: &mut usize) {
        for _ in 0..50 {
            a.poll(now);
            b.poll(now);
            let from_a = core::mem::take(&mut a.out);
            let from_b = core::mem::take(&mut b.out);
            if from_a.is_empty() && from_b.is_empty() {
                return;
            }
            for (src, dst, seg) in from_a {
                *counter = counter.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                if drop_every == 0 || (*counter >> 33) % drop_every != 0 {
                    b.input(src, dst, &seg, now);
                }
            }
            for (src, dst, seg) in from_b {
                *counter = counter.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                if drop_every == 0 || (*counter >> 33) % drop_every != 0 {
                    a.input(src, dst, &seg, now);
                }
            }
        }
    }

    fn transfer(drop_every: usize) {
        let (mut a, mut b) = (Stack::new(1), Stack::new(99));
        b.listen(80).unwrap();
        let mut now = 0;
        let mut counter = 0;
        let c = a.connect(A, B, 80, now);
        let mut server = None;
        let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 + i / 251) as u8).collect();
        let reply: Vec<u8> = (0..100_000u32).map(|i| (i * 13) as u8).collect();
        let (mut sent, mut rsent) = (0, 0);
        let (mut got, mut rgot) = (Vec::new(), Vec::new());
        let mut buf = vec![0u8; 4096];
        let mut closed = false;
        let mut server_closed = false;
        for _ in 0..200_000 {
            now += 5;
            pump(&mut a, &mut b, now, drop_every, &mut counter);
            if server.is_none() {
                server = b.accept(80);
            }
            if a.state(c) == State::Established || a.state(c) == State::CloseWait {
                if sent < data.len() {
                    sent += a.send(c, &data[sent..]).unwrap();
                } else if !closed && a.send_queued(c) == 0 {
                    // Keep reading the reply before closing.
                }
            }
            if let Some(s) = server {
                while let Some(Ok(n)) = b.recv(s, &mut buf) {
                    if n == 0 {
                        break;
                    }
                    got.extend_from_slice(&buf[..n]);
                }
                if rsent < reply.len() {
                    if let Ok(n) = b.send(s, &reply[rsent..]) {
                        rsent += n;
                    }
                }
                if got.len() == data.len() && rsent == reply.len() && !server_closed {
                    b.close(s);
                    server_closed = true;
                }
            }
            while let Some(r) = a.recv(c, &mut buf) {
                match r {
                    Ok(0) => {
                        if !closed {
                            a.close(c);
                            closed = true;
                        }
                        break;
                    }
                    Ok(n) => rgot.extend_from_slice(&buf[..n]),
                    Err(e) => panic!("client error {:?} drop={} sent={} got={} rsent={} rgot={} srv={:?}", e, drop_every, sent, got.len(), rsent, rgot.len(), server.map(|s| b.state(s))),
                }
            }
            if closed && server_closed && a.socks.is_empty() && b.socks.is_empty() {
                break;
            }
        }
        assert_eq!(got.len(), data.len());
        assert!(got == data, "server received corrupted data");
        assert!(rgot == reply, "client received corrupted data");
        // Let TIME_WAIT expire.
        for _ in 0..1000 {
            now += 10;
            pump(&mut a, &mut b, now, drop_every, &mut counter);
        }
        assert!(a.socks.is_empty() && b.socks.is_empty(), "sockets not freed: {:?} {:?}", a.socks.len(), b.socks.len());
    }

    #[test]
    fn lossless_transfer() {
        transfer(0);
    }

    #[test]
    fn lossy_transfer() {
        transfer(7);
        transfer(3);
    }

    #[test]
    fn segment_roundtrip_and_reset() {
        let s = Segment { src_port: 1234, dst_port: 80, seq: 5, ack: 9, flags: SYN, window: 1000, mss: Some(1460), payload: b"" };
        let b = build_segment(A, B, &s);
        assert_eq!(Segment::parse(&b, A, B).unwrap(), s);
        assert!(Segment::parse(&b, A, Ipv4([1, 1, 1, 1])).is_none());
        // Connecting to a closed port is refused with RST.
        let (mut a, mut bb) = (Stack::new(5), Stack::new(6));
        let c = a.connect(A, B, 81, 0);
        let mut n = 0;
        pump(&mut a, &mut bb, 1, 0, &mut n);
        assert_eq!(a.error(c), Some(Error::Reset));
    }
}
