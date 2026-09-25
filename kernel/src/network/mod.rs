//! The network stack: one Ethernet interface with ARP, IPv4, ICMP, UDP,
//! TCP, a DHCP client and a DNS resolver. Packet formats live in `libs/net`.
//!
//! A kernel thread polls the NIC every couple of milliseconds and runs the
//! DHCP state machine; other threads call `ping`, `resolve` etc., which
//! send packets and then wait for the thread to deliver the replies.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use net::{dhcp, dns, Arp, EthFrame, Icmp, Ipv4, Ipv4Packet, Mac, Udp};

pub mod httpd;
pub mod tcp;

use crate::drivers::e1000::E1000;
use crate::proc::sched;
use crate::sync::Spin;
use crate::time::uptime_ms;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DhcpState {
    Disabled,
    Discovering,
    Requesting,
    Bound,
    Failed,
}

#[derive(Clone, Debug)]
pub struct Status {
    pub adapter: String,
    pub mac: Mac,
    pub link_up: bool,
    pub speed_mbps: u32,
    pub ip: Ipv4,
    pub mask: Ipv4,
    pub gateway: Ipv4,
    pub dns: Vec<Ipv4>,
    pub dhcp: DhcpState,
    pub rx_packets: u64,
    pub tx_packets: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

struct Pending {
    next_hop: Ipv4,
    packet: Vec<u8>,
    since: u64,
}

struct Iface {
    nic: E1000,
    ip: Ipv4,
    mask: Ipv4,
    gateway: Ipv4,
    dns: Vec<Ipv4>,
    use_dhcp: bool,
    dhcp: DhcpState,
    dhcp_xid: u32,
    dhcp_offer: Option<dhcp::Lease>,
    dhcp_sent: u64,
    dhcp_tries: u32,
    dhcp_renew_at: u64,
    arp: BTreeMap<Ipv4, (Mac, u64)>,
    arp_asked: BTreeMap<Ipv4, u64>,
    pending: Vec<Pending>,
    ip_id: u16,
    echo_replies: VecDeque<(Ipv4, u16, u16, u64)>,
    udp: BTreeMap<u16, VecDeque<(Ipv4, u16, Vec<u8>)>>,
    tcp: net::tcp::Stack,
    stats: [u64; 4],
}

static IFACE: Spin<Option<Iface>> = Spin::new(None);
static NEXT_PORT: Spin<u16> = Spin::new(49152);

const ARP_TTL_MS: u64 = 5 * 60 * 1000;

pub fn is_present() -> bool {
    IFACE.lock().is_some()
}

/// Take ownership of the NIC and start the network thread.
pub fn init(nic: E1000) {
    let seed = (crate::arch::cpu::rdtsc() as u32) ^ 0x4d61_794f;
    *IFACE.lock() = Some(Iface {
        nic,
        ip: Ipv4::UNSPECIFIED,
        mask: Ipv4::UNSPECIFIED,
        gateway: Ipv4::UNSPECIFIED,
        dns: Vec::new(),
        use_dhcp: true,
        dhcp: DhcpState::Discovering,
        dhcp_xid: seed,
        dhcp_offer: None,
        dhcp_sent: 0,
        dhcp_tries: 0,
        dhcp_renew_at: 0,
        arp: BTreeMap::new(),
        arp_asked: BTreeMap::new(),
        pending: Vec::new(),
        ip_id: seed as u16,
        echo_replies: VecDeque::new(),
        udp: BTreeMap::new(),
        tcp: net::tcp::Stack::new(seed),
        stats: [0; 4],
    });
    sched::spawn_kernel("network", net_thread, 0);
}

extern "C" fn net_thread(_: usize) {
    loop {
        {
            let mut g = IFACE.lock();
            if let Some(i) = g.as_mut() {
                i.poll();
                i.dhcp_tick();
                i.retry_pending();
                i.tcp_flush();
            }
        }
        sched::sleep_ms(2);
    }
}

impl Iface {
    fn send_frame(&mut self, frame: &[u8]) {
        if self.nic.send(frame) {
            self.stats[1] += 1;
            self.stats[3] += frame.len() as u64;
        }
    }

    fn poll(&mut self) {
        for _ in 0..64 {
            let Some(frame) = self.nic.recv() else { break };
            if frame.is_empty() {
                continue;
            }
            self.stats[0] += 1;
            self.stats[2] += frame.len() as u64;
            self.handle_frame(&frame);
        }
    }

    fn handle_frame(&mut self, frame: &[u8]) {
        let Some(eth) = EthFrame::parse(frame) else { return };
        if eth.dst != self.nic.mac && eth.dst != Mac::BROADCAST {
            return;
        }
        match eth.ethertype {
            net::ETH_ARP => self.handle_arp(eth.payload),
            net::ETH_IPV4 => self.handle_ipv4(eth.payload),
            _ => {}
        }
    }

    fn handle_arp(&mut self, b: &[u8]) {
        let Some(a) = Arp::parse(b) else { return };
        if !a.sender_ip.is_unspecified() {
            self.arp.insert(a.sender_ip, (a.sender_mac, uptime_ms()));
        }
        if a.op == net::ARP_REQUEST && !self.ip.is_unspecified() && a.target_ip == self.ip {
            let reply = Arp {
                op: net::ARP_REPLY,
                sender_mac: self.nic.mac,
                sender_ip: self.ip,
                target_mac: a.sender_mac,
                target_ip: a.sender_ip,
            };
            let f = net::build_eth(a.sender_mac, self.nic.mac, net::ETH_ARP, &reply.build());
            self.send_frame(&f);
        }
        self.flush_pending_for(a.sender_ip);
    }

    fn handle_ipv4(&mut self, b: &[u8]) {
        let Some(p) = Ipv4Packet::parse(b) else { return };
        let for_us = p.dst == self.ip || p.dst == Ipv4::BROADCAST || self.ip.is_unspecified();
        if !for_us {
            return;
        }
        match p.protocol {
            net::PROTO_ICMP => {
                let Some(icmp) = Icmp::parse(p.payload) else { return };
                match icmp.kind {
                    net::ICMP_ECHO_REQUEST if !self.ip.is_unspecified() => {
                        let reply = net::build_icmp_echo(net::ICMP_ECHO_REPLY, icmp.id, icmp.seq, icmp.data);
                        self.send_ip(p.src, net::PROTO_ICMP, &reply);
                    }
                    net::ICMP_ECHO_REPLY => {
                        if self.echo_replies.len() < 64 {
                            self.echo_replies.push_back((p.src, icmp.id, icmp.seq, crate::time::uptime_us()));
                        }
                    }
                    _ => {}
                }
            }
            net::PROTO_UDP => {
                let Some(u) = Udp::parse(p.payload, p.src, p.dst) else { return };
                if u.dst_port == dhcp::CLIENT_PORT {
                    self.handle_dhcp(u.payload);
                } else if let Some(q) = self.udp.get_mut(&u.dst_port)
                    && q.len() < 64
                {
                    q.push_back((p.src, u.src_port, u.payload.to_vec()));
                }
            }
            net::PROTO_TCP if !self.ip.is_unspecified() && p.dst == self.ip => {
                self.tcp.input(p.src, p.dst, p.payload, uptime_ms());
            }
            _ => {}
        }
    }

    /// Run TCP timers and transmit its queued segments.
    fn tcp_flush(&mut self) {
        self.tcp.poll(uptime_ms());
        for (_, dst, seg) in core::mem::take(&mut self.tcp.out) {
            self.send_ip(dst, net::PROTO_TCP, &seg);
        }
    }

    fn next_hop(&self, dst: Ipv4) -> Ipv4 {
        if dst == Ipv4::BROADCAST || self.mask.is_unspecified() || dst.same_subnet(self.ip, self.mask) {
            dst
        } else {
            self.gateway
        }
    }

    fn send_ip(&mut self, dst: Ipv4, proto: u8, payload: &[u8]) {
        self.ip_id = self.ip_id.wrapping_add(1);
        let packet = net::build_ipv4(self.ip, dst, proto, self.ip_id, payload);
        let hop = self.next_hop(dst);
        if dst == Ipv4::BROADCAST {
            let f = net::build_eth(Mac::BROADCAST, self.nic.mac, net::ETH_IPV4, &packet);
            self.send_frame(&f);
            return;
        }
        if let Some(&(mac, t)) = self.arp.get(&hop)
            && uptime_ms() - t < ARP_TTL_MS
        {
            let f = net::build_eth(mac, self.nic.mac, net::ETH_IPV4, &packet);
            self.send_frame(&f);
            return;
        }
        if self.pending.len() < 256 {
            self.pending.push(Pending { next_hop: hop, packet, since: uptime_ms() });
        }
        self.arp_request(hop);
    }

    fn arp_request(&mut self, ip: Ipv4) {
        let now = uptime_ms();
        if let Some(&t) = self.arp_asked.get(&ip)
            && now - t < 500
        {
            return;
        }
        self.arp_asked.insert(ip, now);
        let a = Arp { op: net::ARP_REQUEST, sender_mac: self.nic.mac, sender_ip: self.ip, target_mac: Mac::ZERO, target_ip: ip };
        let f = net::build_eth(Mac::BROADCAST, self.nic.mac, net::ETH_ARP, &a.build());
        self.send_frame(&f);
    }

    fn flush_pending_for(&mut self, ip: Ipv4) {
        let Some(&(mac, _)) = self.arp.get(&ip) else { return };
        let ready: Vec<Pending> = {
            let (r, keep): (Vec<_>, Vec<_>) = core::mem::take(&mut self.pending).into_iter().partition(|p| p.next_hop == ip);
            self.pending = keep;
            r
        };
        for p in ready {
            let f = net::build_eth(mac, self.nic.mac, net::ETH_IPV4, &p.packet);
            self.send_frame(&f);
        }
    }

    fn retry_pending(&mut self) {
        let now = uptime_ms();
        self.pending.retain(|p| now - p.since < 3000);
        let hops: Vec<Ipv4> = self.pending.iter().map(|p| p.next_hop).collect();
        for h in hops {
            self.arp_request(h);
        }
    }

    // --- DHCP ---------------------------------------------------------

    fn dhcp_send(&mut self, kind: u8, requested: Option<(Ipv4, Ipv4)>) {
        let msg = dhcp::build(kind, self.dhcp_xid, self.nic.mac, requested, "mayos");
        let udp = net::build_udp(Ipv4::UNSPECIFIED, Ipv4::BROADCAST, dhcp::CLIENT_PORT, dhcp::SERVER_PORT, &msg);
        let ip = net::build_ipv4(Ipv4::UNSPECIFIED, Ipv4::BROADCAST, net::PROTO_UDP, 0, &udp);
        let f = net::build_eth(Mac::BROADCAST, self.nic.mac, net::ETH_IPV4, &ip);
        self.send_frame(&f);
        self.dhcp_sent = uptime_ms();
    }

    fn dhcp_tick(&mut self) {
        if !self.use_dhcp {
            return;
        }
        let now = uptime_ms();
        match self.dhcp {
            DhcpState::Discovering | DhcpState::Requesting if now - self.dhcp_sent > 2500 => {
                if self.dhcp_tries >= 6 {
                    self.dhcp = DhcpState::Failed;
                    crate::kprintln!("net: no DHCP server answered");
                    return;
                }
                self.dhcp_tries += 1;
                self.dhcp = DhcpState::Discovering;
                self.dhcp_xid = self.dhcp_xid.wrapping_add(1);
                self.dhcp_send(dhcp::DISCOVER, None);
            }
            DhcpState::Bound if now >= self.dhcp_renew_at => self.restart_dhcp(),
            _ => {}
        }
    }

    fn restart_dhcp(&mut self) {
        self.dhcp = DhcpState::Discovering;
        self.dhcp_tries = 0;
        self.dhcp_sent = 0;
    }

    fn handle_dhcp(&mut self, payload: &[u8]) {
        let Some(l) = dhcp::parse(payload) else { return };
        if l.xid != self.dhcp_xid || !self.use_dhcp {
            return;
        }
        match (l.message, &self.dhcp) {
            (dhcp::OFFER, DhcpState::Discovering) => {
                self.dhcp_send(dhcp::REQUEST, Some((l.your_ip, l.server)));
                self.dhcp_offer = Some(l);
                self.dhcp = DhcpState::Requesting;
            }
            (dhcp::ACK, DhcpState::Requesting) | (dhcp::ACK, DhcpState::Discovering) => {
                self.ip = l.your_ip;
                self.mask = l.mask.unwrap_or(Ipv4([255, 255, 255, 0]));
                self.gateway = l.router.unwrap_or(Ipv4::UNSPECIFIED);
                self.dns = l.dns.clone();
                let lease = l.lease_secs.unwrap_or(3600).clamp(60, 7 * 24 * 3600) as u64;
                self.dhcp_renew_at = uptime_ms() + lease * 1000 / 2;
                self.dhcp = DhcpState::Bound;
                crate::kprintln!(
                    "net: DHCP lease {}/{} gateway {} dns {:?}",
                    self.ip,
                    self.mask.prefix_len(),
                    self.gateway,
                    self.dns
                );
            }
            (dhcp::NAK, _) => self.restart_dhcp(),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn status() -> Option<Status> {
    let g = IFACE.lock();
    let i = g.as_ref()?;
    Some(Status {
        adapter: String::from(i.nic.model),
        mac: i.nic.mac,
        link_up: i.nic.link_up(),
        speed_mbps: i.nic.speed_mbps(),
        ip: i.ip,
        mask: i.mask,
        gateway: i.gateway,
        dns: i.dns.clone(),
        dhcp: if i.use_dhcp { i.dhcp.clone() } else { DhcpState::Disabled },
        rx_packets: i.stats[0],
        tx_packets: i.stats[1],
        rx_bytes: i.stats[2],
        tx_bytes: i.stats[3],
    })
}

/// Switch to DHCP and request a new lease.
pub fn use_dhcp() {
    if let Some(i) = IFACE.lock().as_mut() {
        i.use_dhcp = true;
        i.ip = Ipv4::UNSPECIFIED;
        i.restart_dhcp();
    }
}

pub fn set_static(ip: Ipv4, mask: Ipv4, gateway: Ipv4, dns: Vec<Ipv4>) {
    if let Some(i) = IFACE.lock().as_mut() {
        i.use_dhcp = false;
        i.ip = ip;
        i.mask = mask;
        i.gateway = gateway;
        i.dns = dns;
        i.arp.clear();
    }
}

/// Wait up to `ms` for an address (DHCP).
pub fn wait_configured(ms: u64) -> bool {
    let start = uptime_ms();
    loop {
        if let Some(s) = status()
            && !s.ip.is_unspecified()
        {
            return true;
        }
        if uptime_ms() - start > ms {
            return false;
        }
        sched::sleep_ms(20);
    }
}

#[derive(Debug)]
pub enum NetError {
    NoAdapter,
    NotConfigured,
    Timeout,
    NameNotFound,
    BadName,
    Refused,
    Reset,
    AddressInUse,
}

impl core::fmt::Display for NetError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            NetError::NoAdapter => "no network adapter",
            NetError::NotConfigured => "no IP address (is DHCP still running?)",
            NetError::Timeout => "request timed out",
            NetError::NameNotFound => "host name not found",
            NetError::BadName => "invalid host name",
            NetError::Refused => "connection refused",
            NetError::Reset => "connection reset by peer",
            NetError::AddressInUse => "port already in use",
        })
    }
}

/// Send one ICMP echo request and return the round-trip time in µs.
pub fn ping(dst: Ipv4, seq: u16, timeout_ms: u64) -> Result<u64, NetError> {
    let id = 0x4d4f; // "MO"
    let sent = {
        let mut g = IFACE.lock();
        let i = g.as_mut().ok_or(NetError::NoAdapter)?;
        if i.ip.is_unspecified() {
            return Err(NetError::NotConfigured);
        }
        i.echo_replies.retain(|r| !(r.1 == id && r.2 == seq));
        let payload: Vec<u8> = (0..56u8).map(|b| b.wrapping_add(0x20)).collect();
        let pkt = net::build_icmp_echo(net::ICMP_ECHO_REQUEST, id, seq, &payload);
        let t = crate::time::uptime_us();
        i.send_ip(dst, net::PROTO_ICMP, &pkt);
        t
    };
    let deadline = uptime_ms() + timeout_ms;
    loop {
        {
            let mut g = IFACE.lock();
            let i = g.as_mut().ok_or(NetError::NoAdapter)?;
            if let Some(pos) = i.echo_replies.iter().position(|r| r.0 == dst && r.1 == id && r.2 == seq) {
                let (_, _, _, at) = i.echo_replies.remove(pos).unwrap();
                return Ok(at.saturating_sub(sent));
            }
        }
        if uptime_ms() > deadline {
            return Err(NetError::Timeout);
        }
        sched::sleep_ms(2);
    }
}

fn udp_bind() -> u16 {
    let mut p = NEXT_PORT.lock();
    *p = if *p >= 65000 { 49152 } else { *p + 1 };
    let port = *p;
    drop(p);
    if let Some(i) = IFACE.lock().as_mut() {
        i.udp.insert(port, VecDeque::new());
    }
    port
}

fn udp_unbind(port: u16) {
    if let Some(i) = IFACE.lock().as_mut() {
        i.udp.remove(&port);
    }
}

fn udp_send(src_port: u16, dst: Ipv4, dst_port: u16, data: &[u8]) {
    if let Some(i) = IFACE.lock().as_mut() {
        let u = net::build_udp(i.ip, dst, src_port, dst_port, data);
        i.send_ip(dst, net::PROTO_UDP, &u);
    }
}

fn udp_recv(port: u16, timeout_ms: u64) -> Option<(Ipv4, u16, Vec<u8>)> {
    let deadline = uptime_ms() + timeout_ms;
    loop {
        if let Some(i) = IFACE.lock().as_mut()
            && let Some(q) = i.udp.get_mut(&port)
            && let Some(m) = q.pop_front()
        {
            return Some(m);
        }
        if uptime_ms() > deadline {
            return None;
        }
        sched::sleep_ms(2);
    }
}

/// Resolve a host name (or dotted address) to an IPv4 address.
pub fn resolve(name: &str) -> Result<Ipv4, NetError> {
    if let Some(ip) = Ipv4::parse(name) {
        return Ok(ip);
    }
    let s = status().ok_or(NetError::NoAdapter)?;
    if s.ip.is_unspecified() {
        return Err(NetError::NotConfigured);
    }
    let servers = if s.dns.is_empty() { alloc::vec![Ipv4([1, 1, 1, 1])] } else { s.dns };
    let id = (crate::arch::cpu::rdtsc() & 0xffff) as u16;
    let query = dns::build_query(id, name).ok_or(NetError::BadName)?;
    let port = udp_bind();
    let mut result = Err(NetError::Timeout);
    'outer: for server in servers.iter().take(2) {
        for _ in 0..2 {
            udp_send(port, *server, dns::PORT, &query);
            let deadline = uptime_ms() + 2000;
            while uptime_ms() < deadline {
                let Some((from, _, data)) = udp_recv(port, deadline - uptime_ms()) else { break };
                if from != *server {
                    continue;
                }
                match dns::parse_response(&data, id) {
                    Some(dns::Answer::Addresses(a)) if !a.is_empty() => {
                        result = Ok(a[0]);
                        break 'outer;
                    }
                    Some(_) => {
                        result = Err(NetError::NameNotFound);
                        break 'outer;
                    }
                    None => {}
                }
            }
        }
    }
    udp_unbind(port);
    result
}

pub fn describe(s: &Status) -> String {
    format!(
        "{} ({}), link {}, {}/{} via {}",
        s.adapter,
        s.mac,
        if s.link_up { "up" } else { "down" },
        s.ip,
        s.mask.prefix_len(),
        s.gateway
    )
}
