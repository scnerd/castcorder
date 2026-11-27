//! TURN over TCP and TLS for the host (RFC 8656 section 3.1: the client may
//! reach the server over UDP, TCP or TLS and still be allocated a UDP relayed
//! address). The transport's TURN client speaks UDP only, so each stream URL
//! gets a [`Bridge`]: a loopback UDP socket the transport treats as the TURN
//! server's address, and a thread that carries every STUN message and
//! ChannelData frame it receives over one TCP (or TLS) connection to the real
//! server, and the server's frames back as datagrams.
//!
//! Framing on the stream is the TURN one: a STUN message is its 20-byte header
//! plus the length in bytes 2..4; ChannelData is a 4-byte header plus that
//! length, padded to a multiple of four. The connection opens when the first
//! datagram needs it and reopens (at most once a second) after it fails; a
//! datagram that finds no connection is dropped, and the client's own
//! retransmissions are the retry.
//!
//! One connection per client socket: TURN names an allocation by the client's
//! five-tuple, and the transport allocates from a second socket when its
//! credentials renew, beside the live allocation (a shared connection would
//! answer 437, wrong transaction or allocation mismatch).

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

/// How long the bridge waits on either side before looking at the other.
const POLL: Duration = Duration::from_millis(2);
/// Reconnect no more often than this.
const RECONNECT: Duration = Duration::from_secs(1);
/// Give up connecting (TCP and the TLS handshake) after this long.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// A stream the bridge reads and writes (TCP, or TLS over it).
trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

/// Which stream to the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// `turn:...?transport=tcp`: plain TCP.
    Tcp,
    /// `turns:`: TLS over TCP.
    Tls,
}

/// A running bridge: send TURN datagrams to [`Bridge::addr`].
#[derive(Clone, Debug)]
pub struct Bridge {
    addr: SocketAddr,
}

impl Bridge {
    /// The loopback UDP address standing in for the server.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

type Key = (String, u16, Kind);

/// The bridge for `host:port` over `kind`: started on first use and kept for
/// the life of the process (credentials renew every so often and ask again).
pub fn bridge(host: &str, port: u16, kind: Kind) -> io::Result<Bridge> {
    static ALL: OnceLock<Mutex<HashMap<Key, Bridge>>> = OnceLock::new();
    let mut all = ALL
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let key = (host.to_string(), port, kind);
    if let Some(b) = all.get(&key) {
        return Ok(b.clone());
    }
    let b = start(host, port, kind, Arc::new(web_roots()))?;
    all.insert(key, b.clone());
    Ok(b)
}

/// Start a bridge to `host:port` trusting `roots` for TLS.
pub(crate) fn start(
    host: &str,
    port: u16,
    kind: Kind,
    roots: Arc<rustls::RootCertStore>,
) -> io::Result<Bridge> {
    let udp = UdpSocket::bind("127.0.0.1:0")?;
    udp.set_read_timeout(Some(POLL))?;
    let addr = udp.local_addr()?;
    info!(%addr, %host, port, ?kind, "TURN stream bridge started");
    let name = format!("turn-{kind:?}-{host}:{port}").to_lowercase();
    let host = host.to_string();
    thread::Builder::new()
        .name(name)
        .spawn(move || run(udp, &host, port, kind, &roots))?;
    Ok(Bridge { addr })
}

fn web_roots() -> rustls::RootCertStore {
    rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    }
}

fn connect(
    host: &str,
    port: u16,
    kind: Kind,
    roots: &Arc<rustls::RootCertStore>,
) -> io::Result<Box<dyn Stream>> {
    let addrs: Vec<SocketAddr> = (host, port).to_socket_addrs()?.collect();
    // IPv4 first, like the UDP URLs.
    let mut last = io::Error::other("no address");
    let mut ordered = addrs.clone();
    ordered.sort_by_key(|a| !a.is_ipv4());
    for a in ordered {
        match TcpStream::connect_timeout(&a, CONNECT_TIMEOUT) {
            Ok(tcp) => return finish(tcp, host, kind, roots),
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn finish(
    tcp: TcpStream,
    host: &str,
    kind: Kind,
    roots: &Arc<rustls::RootCertStore>,
) -> io::Result<Box<dyn Stream>> {
    tcp.set_nodelay(true)?;
    tcp.set_write_timeout(Some(Duration::from_secs(10)))?;
    tcp.set_read_timeout(Some(CONNECT_TIMEOUT))?;
    let stream: Box<dyn Stream> = match kind {
        Kind::Tcp => Box::new(tcp.try_clone()?),
        Kind::Tls => {
            let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
            let cfg = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(io::Error::other)?
                .with_root_certificates(roots.clone())
                .with_no_client_auth();
            let name = rustls::pki_types::ServerName::try_from(host.to_string())
                .map_err(io::Error::other)?;
            let mut conn =
                rustls::ClientConnection::new(Arc::new(cfg), name).map_err(io::Error::other)?;
            let mut raw = tcp.try_clone()?;
            while conn.is_handshaking() {
                conn.complete_io(&mut raw)?;
            }
            Box::new(rustls::StreamOwned::new(conn, tcp.try_clone()?))
        }
    };
    // The handshake is done: from here a read waits only POLL.
    tcp.set_read_timeout(Some(POLL))?;
    Ok(stream)
}

/// One client socket's connection: TURN identifies an allocation by the
/// client's five-tuple, so each socket the transport sends from (it takes a
/// second one when credentials renew, to allocate beside the live allocation)
/// needs a TCP connection of its own, or the server answers 437.
struct Conn {
    stream: Box<dyn Stream>,
    inbuf: Vec<u8>,
    last_sent: Instant,
    /// Set once the client has sent its Refresh with lifetime 0 (deleting its
    /// allocation): the connection is dropped then, so that the next allocation
    /// from this client socket is made on a fresh connection. Reusing one that
    /// carried a deleted allocation made the server answer 437 to the next
    /// Allocate on it.
    close_at: Option<Instant>,
}

/// How long after a deallocation the connection is kept (for the answer).
const CLOSE_AFTER_DEALLOCATE: Duration = Duration::from_millis(1500);

/// Whether `d` is a TURN Refresh request with LIFETIME 0.
pub(crate) fn is_deallocation(d: &[u8]) -> bool {
    if d.len() < 20 || d[0] >> 6 != 0 || u16::from_be_bytes([d[0], d[1]]) != 0x0004 {
        return false;
    }
    let len = usize::from(u16::from_be_bytes([d[2], d[3]])).min(d.len() - 20);
    let attrs = &d[20..20 + len];
    let mut i = 0;
    while i + 4 <= attrs.len() {
        let t = u16::from_be_bytes([attrs[i], attrs[i + 1]]);
        let l = usize::from(u16::from_be_bytes([attrs[i + 2], attrs[i + 3]]));
        if t == 0x000D && l == 4 && attrs.len() >= i + 8 {
            return attrs[i + 4..i + 8] == [0, 0, 0, 0];
        }
        i += 4 + l.div_ceil(4) * 4;
    }
    false
}

/// Drop a connection whose client socket has sent nothing for this long (an
/// allocation is refreshed every few minutes while it lives).
const IDLE: Duration = Duration::from_secs(15 * 60);

fn run(udp: UdpSocket, host: &str, port: u16, kind: Kind, roots: &Arc<rustls::RootCertStore>) {
    let mut conns: HashMap<SocketAddr, Conn> = HashMap::new();
    let mut last_try: HashMap<SocketAddr, Instant> = HashMap::new();
    let mut buf = vec![0u8; 65_536];
    loop {
        // Datagram -> its client's stream.
        match udp.recv_from(&mut buf) {
            Ok((n, from)) => {
                if !conns.contains_key(&from)
                    && last_try.get(&from).is_none_or(|t| t.elapsed() >= RECONNECT)
                {
                    last_try.insert(from, Instant::now());
                    match connect(host, port, kind, roots) {
                        Ok(stream) => {
                            debug!(%host, port, ?kind, client = %from, "TURN stream connected");
                            conns.insert(
                                from,
                                Conn {
                                    stream,
                                    inbuf: Vec::new(),
                                    last_sent: Instant::now(),
                                    close_at: None,
                                },
                            );
                        }
                        Err(e) => {
                            warn!(%host, port, ?kind, error = %e, "TURN stream connect failed")
                        }
                    }
                }
                if let Some(c) = conns.get_mut(&from) {
                    c.last_sent = Instant::now();
                    if is_deallocation(&buf[..n]) {
                        debug!(client = %from, "TURN stream: deallocation seen; closing the stream soon");
                        c.close_at = Some(Instant::now() + CLOSE_AFTER_DEALLOCATE);
                    }
                    if let Some(frame) = frame_for_stream(&buf[..n])
                        && let Err(e) = c.stream.write_all(&frame).and_then(|()| c.stream.flush())
                    {
                        warn!(error = %e, "TURN stream write failed; reconnecting");
                        conns.remove(&from);
                    }
                }
            }
            Err(e) if is_timeout(&e) => {}
            // A reply to a closed peer socket surfaces here on some systems.
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            Err(e) => {
                warn!(error = %e, "TURN bridge socket failed; stopping");
                return;
            }
        }
        // Each stream -> datagrams to its client.
        let mut gone: Vec<SocketAddr> = vec![];
        for (peer, c) in conns.iter_mut() {
            if c.last_sent.elapsed() > IDLE || c.close_at.is_some_and(|t| Instant::now() >= t) {
                gone.push(*peer);
                continue;
            }
            let mut chunk = [0u8; 8192];
            match c.stream.read(&mut chunk) {
                Ok(0) => {
                    warn!(client = %peer, "TURN stream closed by the server");
                    gone.push(*peer);
                }
                Ok(n) => {
                    c.inbuf.extend_from_slice(&chunk[..n]);
                    match drain(&mut c.inbuf) {
                        Ok(frames) => {
                            for f in frames {
                                let _ = udp.send_to(&f, peer);
                            }
                        }
                        Err(()) => {
                            warn!("TURN stream carried something that is not STUN; closing it");
                            gone.push(*peer);
                        }
                    }
                }
                Err(e) if is_timeout(&e) => {}
                Err(e) => {
                    warn!(error = %e, "TURN stream read failed");
                    gone.push(*peer);
                }
            }
        }
        for p in gone {
            conns.remove(&p);
        }
    }
}

fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

/// The bytes to put on the stream for one datagram: a STUN message as it is,
/// ChannelData padded to four bytes, anything else nothing.
pub(crate) fn frame_for_stream(d: &[u8]) -> Option<Vec<u8>> {
    match d.first()? >> 6 {
        0b00 if d.len() >= 20 => Some(d.to_vec()),
        0b01 if d.len() >= 4 => {
            let mut v = d.to_vec();
            while !v.len().is_multiple_of(4) {
                v.push(0);
            }
            Some(v)
        }
        _ => None,
    }
}

/// Take every whole frame off the front of `buf` (each as the datagram it
/// travels as: ChannelData without its padding). `Err` if the stream is not
/// STUN framing.
pub(crate) fn drain(buf: &mut Vec<u8>) -> Result<Vec<Vec<u8>>, ()> {
    let mut out = vec![];
    loop {
        if buf.len() < 4 {
            return Ok(out);
        }
        let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        let (total, datagram) = match buf[0] >> 6 {
            0b00 => (20 + len, 20 + len),
            0b01 => (4 + len.div_ceil(4) * 4, 4 + len),
            _ => return Err(()),
        };
        if buf.len() < total {
            return Ok(out);
        }
        out.push(buf[..datagram].to_vec());
        buf.drain(..total);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn a_refresh_with_lifetime_zero_is_a_deallocation() {
        let refresh = |lifetime: u32| {
            let mut m = vec![0x00, 0x04, 0x00, 0x08];
            m.extend_from_slice(&[0x21, 0x12, 0xa4, 0x42]);
            m.extend_from_slice(&[7; 12]);
            m.extend_from_slice(&[0x00, 0x0D, 0x00, 0x04]);
            m.extend_from_slice(&lifetime.to_be_bytes());
            m
        };
        assert!(is_deallocation(&refresh(0)));
        assert!(!is_deallocation(&refresh(600)));
        assert!(!is_deallocation(&stun(4, 1)));
    }

    fn stun(len: usize, fill: u8) -> Vec<u8> {
        let mut m = vec![0x00, 0x01];
        m.extend_from_slice(&(len as u16).to_be_bytes());
        m.extend_from_slice(&[0x21, 0x12, 0xa4, 0x42]);
        m.extend_from_slice(&[fill; 12]);
        m.extend(std::iter::repeat_n(fill, len));
        m
    }

    fn channel_data(len: usize) -> Vec<u8> {
        let mut m = vec![0x40, 0x00];
        m.extend_from_slice(&(len as u16).to_be_bytes());
        m.extend(std::iter::repeat_n(7u8, len));
        m
    }

    #[test]
    fn channel_data_is_padded_on_the_stream_and_unpadded_off_it() {
        let c = channel_data(5);
        let framed = frame_for_stream(&c).unwrap();
        assert_eq!(framed.len(), 12);
        assert_eq!(frame_for_stream(&stun(8, 1)).unwrap(), stun(8, 1));
        assert_eq!(frame_for_stream(&[0xff; 30]), None);
        let mut b = framed;
        b.extend(stun(4, 2));
        assert_eq!(drain(&mut b).unwrap(), vec![c, stun(4, 2)]);
        assert!(b.is_empty());
    }

    #[test]
    fn frames_split_across_reads_wait_for_the_rest() {
        let m = stun(16, 3);
        let mut b = m[..10].to_vec();
        assert_eq!(drain(&mut b).unwrap(), Vec::<Vec<u8>>::new());
        b.extend_from_slice(&m[10..]);
        assert_eq!(drain(&mut b).unwrap(), vec![m]);
        let mut junk = vec![0x80, 0, 0, 0];
        assert!(drain(&mut junk).is_err());
    }

    /// A server that echoes every frame it reads, splitting its writes into
    /// small pieces so the bridge must reassemble them.
    fn echo_server() -> (SocketAddr, thread::JoinHandle<()>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let h = thread::spawn(move || {
            // Two connections: the second proves the bridge reconnects. The
            // first serves two frames, the second one.
            for quota in [2, 1] {
                let (mut s, _) = l.accept().unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 256];
                let mut served = 0;
                while served < quota {
                    let n = s.read(&mut chunk).unwrap();
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let mut whole = buf.clone();
                    if let Ok(frames) = drain(&mut whole) {
                        for f in &frames {
                            let framed = frame_for_stream(f).unwrap();
                            for piece in framed.chunks(7) {
                                s.write_all(piece).unwrap();
                                s.flush().unwrap();
                                thread::sleep(Duration::from_millis(1));
                            }
                            served += 1;
                        }
                        if !frames.is_empty() {
                            buf.clear();
                        }
                    }
                }
            }
        });
        (addr, h)
    }

    fn roundtrip(to: SocketAddr, sock: &UdpSocket, m: &[u8]) -> Vec<u8> {
        sock.send_to(m, to).unwrap();
        let mut b = vec![0u8; 2048];
        let (n, from) = sock.recv_from(&mut b).expect("a reply through the bridge");
        assert_eq!(from, to);
        b.truncate(n);
        b
    }

    #[test]
    fn datagrams_travel_over_tcp_and_back_and_the_bridge_reconnects() {
        let (server, h) = echo_server();
        let b = start(
            &server.ip().to_string(),
            server.port(),
            Kind::Tcp,
            Arc::new(rustls::RootCertStore::empty()),
        )
        .unwrap();
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        assert_eq!(roundtrip(b.addr(), &sock, &stun(24, 9)), stun(24, 9));
        assert_eq!(
            roundtrip(b.addr(), &sock, &channel_data(5)),
            channel_data(5)
        );
        // The server closed the first connection after two frames; the next
        // datagram may be lost while the bridge notices, so keep asking.
        let mut got = None;
        for _ in 0..20 {
            sock.send_to(&stun(8, 4), b.addr()).unwrap();
            let mut r = vec![0u8; 2048];
            sock.set_read_timeout(Some(Duration::from_millis(400)))
                .unwrap();
            if let Ok((n, _)) = sock.recv_from(&mut r) {
                got = Some(r[..n].to_vec());
                break;
            }
        }
        assert_eq!(got, Some(stun(8, 4)));
        h.join().unwrap();
    }

    /// TURN allocations are per five-tuple: each client socket needs its own
    /// connection (the second socket the transport takes when credentials
    /// renew must not share the first one's), and replies go to the right one.
    #[test]
    fn each_client_socket_gets_its_own_connection() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let server = l.local_addr().unwrap();
        let h = thread::spawn(move || {
            let mut handles = vec![];
            for i in 0..2u8 {
                let (mut s, _) = l.accept().unwrap();
                handles.push(thread::spawn(move || {
                    let mut head = [0u8; 28];
                    s.read_exact(&mut head).unwrap();
                    // Answer with the connection's number so a mixup shows.
                    let mut reply = head;
                    reply[20..].fill(i + 1);
                    s.write_all(&reply).unwrap();
                    s.flush().unwrap();
                    thread::sleep(Duration::from_millis(300));
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
        });
        let b = start(
            &server.ip().to_string(),
            server.port(),
            Kind::Tcp,
            Arc::new(rustls::RootCertStore::empty()),
        )
        .unwrap();
        let (x, y) = (
            UdpSocket::bind("127.0.0.1:0").unwrap(),
            UdpSocket::bind("127.0.0.1:0").unwrap(),
        );
        for s in [&x, &y] {
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        }
        x.send_to(&stun(8, 1), b.addr()).unwrap();
        y.send_to(&stun(8, 1), b.addr()).unwrap();
        let (mut rx, mut ry) = ([0u8; 64], [0u8; 64]);
        let (nx, _) = x
            .recv_from(&mut rx)
            .expect("the first socket gets its reply");
        let (ny, _) = y
            .recv_from(&mut ry)
            .expect("the second socket gets its reply");
        assert_eq!((nx, ny), (28, 28));
        let mut seen = [rx[27], ry[27]];
        seen.sort();
        assert_eq!(seen, [1, 2], "two connections, one answer each");
        h.join().unwrap();
    }

    #[test]
    fn turns_reaches_a_tls_server_that_the_roots_trust() {
        let key = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert = key.cert.der().clone();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.clone()).unwrap();
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let server_cfg = Arc::new(
            rustls::ServerConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![cert],
                    rustls::pki_types::PrivateKeyDer::Pkcs8(key.signing_key.serialize_der().into()),
                )
                .unwrap(),
        );
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = thread::spawn(move || {
            let (tcp, _) = l.accept().unwrap();
            let conn = rustls::ServerConnection::new(server_cfg).unwrap();
            let mut s = rustls::StreamOwned::new(conn, tcp);
            let mut head = [0u8; 28];
            s.read_exact(&mut head).unwrap();
            s.write_all(&head).unwrap();
            s.flush().unwrap();
            // Hold the connection until the client has what it needs.
            thread::sleep(Duration::from_millis(500));
        });
        let b = start("localhost", port, Kind::Tls, Arc::new(roots)).unwrap();
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        assert_eq!(roundtrip(b.addr(), &sock, &stun(8, 5)), stun(8, 5));
        h.join().unwrap();
    }

    #[test]
    fn a_tls_server_the_roots_do_not_trust_gets_nothing() {
        let key = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let server_cfg = Arc::new(
            rustls::ServerConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![key.cert.der().clone()],
                    rustls::pki_types::PrivateKeyDer::Pkcs8(key.signing_key.serialize_der().into()),
                )
                .unwrap(),
        );
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        thread::spawn(move || {
            if let Ok((tcp, _)) = l.accept() {
                let conn = rustls::ServerConnection::new(server_cfg).unwrap();
                let mut s = rustls::StreamOwned::new(conn, tcp);
                let mut b = [0u8; 1];
                let _ = s.read(&mut b);
            }
        });
        let b = start(
            "localhost",
            port,
            Kind::Tls,
            Arc::new(rustls::RootCertStore::empty()),
        )
        .unwrap();
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(800)))
            .unwrap();
        sock.send_to(&stun(8, 5), b.addr()).unwrap();
        let mut r = [0u8; 64];
        assert!(sock.recv_from(&mut r).is_err(), "nothing may come back");
    }
}
