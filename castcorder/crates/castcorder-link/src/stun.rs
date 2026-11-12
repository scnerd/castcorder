//! Just enough STUN (RFC 5389) to learn a server-reflexive address: a Binding
//! request with no attributes, and a Binding success response carrying
//! XOR-MAPPED-ADDRESS. Other attributes in a request (as browsers may send)
//! are ignored; a response's other attributes are skipped.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// The fixed magic cookie.
pub const MAGIC: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const XOR_MAPPED_ADDRESS: u16 = 0x0020;

/// A transaction id.
pub type TxId = [u8; 12];

fn header(kind: u16, len: u16, txid: &TxId) -> Vec<u8> {
    let mut b = Vec::with_capacity(20 + len as usize);
    b.extend(kind.to_be_bytes());
    b.extend(len.to_be_bytes());
    b.extend(MAGIC.to_be_bytes());
    b.extend(txid);
    b
}

fn parse_header(b: &[u8]) -> Option<(u16, &[u8], TxId)> {
    if b.len() < 20 || b[0] & 0xC0 != 0 {
        return None;
    }
    let kind = u16::from_be_bytes([b[0], b[1]]);
    let len = u16::from_be_bytes([b[2], b[3]]) as usize;
    if u32::from_be_bytes([b[4], b[5], b[6], b[7]]) != MAGIC
        || b.len() < 20 + len
        || !len.is_multiple_of(4)
    {
        return None;
    }
    Some((kind, &b[20..20 + len], b[8..20].try_into().ok()?))
}

/// A Binding request.
pub fn binding_request(txid: TxId) -> Vec<u8> {
    header(BINDING_REQUEST, 0, &txid)
}

/// The transaction id of a Binding request, if `b` is one.
pub fn parse_binding_request(b: &[u8]) -> Option<TxId> {
    match parse_header(b)? {
        (BINDING_REQUEST, _, txid) => Some(txid),
        _ => None,
    }
}

/// A Binding success response telling the requester its address `mapped`.
pub fn binding_success(txid: TxId, mapped: SocketAddr) -> Vec<u8> {
    let port = mapped.port() ^ (MAGIC >> 16) as u16;
    let (family, addr): (u8, Vec<u8>) = match mapped.ip() {
        IpAddr::V4(ip) => (1, (u32::from(ip) ^ MAGIC).to_be_bytes().to_vec()),
        IpAddr::V6(ip) => {
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&MAGIC.to_be_bytes());
            mask[4..].copy_from_slice(&txid);
            let o = ip.octets();
            (2, (0..16).map(|i| o[i] ^ mask[i]).collect())
        }
    };
    let value_len = 4 + addr.len();
    let mut b = header(BINDING_SUCCESS, (4 + value_len) as u16, &txid);
    b.extend(XOR_MAPPED_ADDRESS.to_be_bytes());
    b.extend((value_len as u16).to_be_bytes());
    b.extend([0, family]);
    b.extend(port.to_be_bytes());
    b.extend(addr);
    b
}

/// The transaction id and mapped address of a Binding success response, if
/// `b` is one carrying XOR-MAPPED-ADDRESS.
pub fn parse_binding_success(b: &[u8]) -> Option<(TxId, SocketAddr)> {
    let (kind, mut attrs, txid) = parse_header(b)?;
    if kind != BINDING_SUCCESS {
        return None;
    }
    while attrs.len() >= 4 {
        let t = u16::from_be_bytes([attrs[0], attrs[1]]);
        let l = u16::from_be_bytes([attrs[2], attrs[3]]) as usize;
        let padded = l.div_ceil(4) * 4;
        if attrs.len() < 4 + padded {
            return None;
        }
        let v = &attrs[4..4 + l];
        if t == XOR_MAPPED_ADDRESS && l >= 8 {
            let port = u16::from_be_bytes([v[2], v[3]]) ^ (MAGIC >> 16) as u16;
            let ip = match (v[1], l) {
                (1, 8) => {
                    let x = u32::from_be_bytes([v[4], v[5], v[6], v[7]]) ^ MAGIC;
                    IpAddr::V4(Ipv4Addr::from(x))
                }
                (2, 20) => {
                    let mut mask = [0u8; 16];
                    mask[..4].copy_from_slice(&MAGIC.to_be_bytes());
                    mask[4..].copy_from_slice(&txid);
                    let mut o = [0u8; 16];
                    for i in 0..16 {
                        o[i] = v[4 + i] ^ mask[i];
                    }
                    IpAddr::V6(Ipv6Addr::from(o))
                }
                _ => return None,
            };
            return Some((txid, SocketAddr::new(ip, port)));
        }
        attrs = &attrs[4 + padded..];
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_round_trips_v4_and_v6() {
        let tx = [3; 12];
        let req = binding_request(tx);
        assert_eq!(parse_binding_request(&req), Some(tx));
        assert_eq!(parse_binding_success(&req), None);
        for addr in ["203.0.113.7:51234", "[2001:db8::1]:4000"] {
            let a: SocketAddr = addr.parse().unwrap();
            let resp = binding_success(tx, a);
            assert_eq!(parse_binding_success(&resp), Some((tx, a)));
            assert_eq!(parse_binding_request(&resp), None);
        }
    }

    #[test]
    fn known_vector_from_rfc5769() {
        // RFC 5769 2.2: IPv4 response, XOR-MAPPED-ADDRESS 192.0.2.1:32853
        // (other attributes stripped and the length adjusted).
        let tx: TxId = [
            0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
        ];
        let mut b = vec![0x01, 0x01, 0x00, 0x0c, 0x21, 0x12, 0xa4, 0x42];
        b.extend(tx);
        b.extend([
            0x00, 0x20, 0x00, 0x08, 0x00, 0x01, 0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43,
        ]);
        assert_eq!(
            parse_binding_success(&b),
            Some((tx, "192.0.2.1:32853".parse().unwrap()))
        );
    }

    #[test]
    fn junk_is_not_stun() {
        assert_eq!(parse_binding_request(b"hello"), None);
        assert_eq!(parse_binding_success(&[0x80; 40]), None);
        let mut req = binding_request([1; 12]);
        req[4] = 0;
        assert_eq!(parse_binding_request(&req), None);
    }
}
