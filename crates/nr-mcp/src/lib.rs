//! Bounded, allocation-free datagram framing for MCP JSON-RPC messages.
//!
//! Standard MCP remains HTTP-facing at the host gateway. This crate defines
//! the small authenticated-network-side protocol between that gateway and a
//! NightRun device. It fragments JSON-RPC messages so they stay below an
//! Ethernet MTU and reassembles one bounded message at a time.

#![cfg_attr(not(feature = "std"), no_std)]

use core::fmt;

pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 24;
pub const MAX_DATAGRAM: usize = 1200;
pub const MAX_FRAGMENT_PAYLOAD: usize = MAX_DATAGRAM - HEADER_LEN;
const MAGIC: [u8; 4] = *b"NRMP";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Kind {
    /// An HTTP client is calling the MCP server exposed by NightRun.
    HostRequest = 1,
    /// NightRun's response to an HTTP client.
    DeviceResponse = 2,
    /// NightRun is calling a remote MCP server through the gateway.
    DeviceRequest = 3,
    /// A remote MCP server's response to NightRun.
    HostResponse = 4,
    /// Device presence announcement used to establish the UDP return path.
    Hello = 5,
}

impl TryFrom<u8> for Kind {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::HostRequest),
            2 => Ok(Self::DeviceResponse),
            3 => Ok(Self::DeviceRequest),
            4 => Ok(Self::HostResponse),
            5 => Ok(Self::Hello),
            _ => Err(Error::UnknownKind),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    BufferTooSmall,
    Truncated,
    BadMagic,
    UnsupportedVersion,
    UnknownKind,
    InvalidFragment,
    MessageTooLarge,
    ChecksumMismatch,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Fragment<'a> {
    pub kind: Kind,
    pub message_id: u32,
    pub index: u16,
    pub count: u16,
    pub total_len: u32,
    pub checksum: u32,
    pub payload: &'a [u8],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompleteMessage<'a> {
    pub kind: Kind,
    pub message_id: u32,
    pub payload: &'a [u8],
}

pub fn fragment_count(message_len: usize) -> Result<u16, Error> {
    if message_len == 0 {
        return Err(Error::InvalidFragment);
    }
    let count = message_len.max(1).div_ceil(MAX_FRAGMENT_PAYLOAD);
    if count > u16::MAX as usize || count > 64 {
        return Err(Error::MessageTooLarge);
    }
    Ok(count as u16)
}

pub fn encode_fragment(
    kind: Kind,
    message_id: u32,
    message: &[u8],
    index: u16,
    output: &mut [u8],
) -> Result<usize, Error> {
    let count = fragment_count(message.len())?;
    if index >= count {
        return Err(Error::InvalidFragment);
    }
    let start = index as usize * MAX_FRAGMENT_PAYLOAD;
    let end = (start + MAX_FRAGMENT_PAYLOAD).min(message.len());
    let payload = &message[start..end];
    let len = HEADER_LEN + payload.len();
    if output.len() < len {
        return Err(Error::BufferTooSmall);
    }
    output[..len].fill(0);
    output[0..4].copy_from_slice(&MAGIC);
    output[4] = VERSION;
    output[5] = kind as u8;
    put16(&mut output[6..8], index);
    put32(&mut output[8..12], message_id);
    put16(&mut output[12..14], count);
    put16(&mut output[14..16], payload.len() as u16);
    put32(&mut output[16..20], message.len() as u32);
    put32(&mut output[20..24], crc32(message));
    output[HEADER_LEN..len].copy_from_slice(payload);
    Ok(len)
}

pub fn parse_fragment(datagram: &[u8]) -> Result<Fragment<'_>, Error> {
    if datagram.len() < HEADER_LEN {
        return Err(Error::Truncated);
    }
    if datagram[0..4] != MAGIC {
        return Err(Error::BadMagic);
    }
    if datagram[4] != VERSION {
        return Err(Error::UnsupportedVersion);
    }
    let kind = Kind::try_from(datagram[5])?;
    let index = be16(&datagram[6..8]);
    let message_id = be32(&datagram[8..12]);
    let count = be16(&datagram[12..14]);
    let payload_len = be16(&datagram[14..16]) as usize;
    let total_len = be32(&datagram[16..20]);
    let checksum = be32(&datagram[20..24]);
    if count == 0
        || count > 64
        || index >= count
        || payload_len > MAX_FRAGMENT_PAYLOAD
        || datagram.len() != HEADER_LEN + payload_len
        || total_len == 0
        || count as usize != (total_len as usize).div_ceil(MAX_FRAGMENT_PAYLOAD)
    {
        return Err(Error::InvalidFragment);
    }
    let expected_len = if index + 1 == count {
        total_len as usize - index as usize * MAX_FRAGMENT_PAYLOAD
    } else {
        MAX_FRAGMENT_PAYLOAD
    };
    if payload_len != expected_len {
        return Err(Error::InvalidFragment);
    }
    Ok(Fragment {
        kind,
        message_id,
        index,
        count,
        total_len,
        checksum,
        payload: &datagram[HEADER_LEN..],
    })
}

/// Reassembles one in-flight message without heap allocation. A fragment for a
/// different message replaces the previous incomplete message.
pub struct Reassembler<const MAX: usize> {
    buffer: [u8; MAX],
    active: bool,
    kind: Kind,
    message_id: u32,
    count: u16,
    total_len: usize,
    checksum: u32,
    received: u64,
}

impl<const MAX: usize> Default for Reassembler<MAX> {
    fn default() -> Self {
        Self {
            buffer: [0; MAX],
            active: false,
            kind: Kind::HostRequest,
            message_id: 0,
            count: 0,
            total_len: 0,
            checksum: 0,
            received: 0,
        }
    }
}

impl<const MAX: usize> Reassembler<MAX> {
    pub fn push<'a>(&'a mut self, datagram: &[u8]) -> Result<Option<CompleteMessage<'a>>, Error> {
        let fragment = parse_fragment(datagram)?;
        if fragment.total_len as usize > MAX {
            return Err(Error::MessageTooLarge);
        }
        let same_message = self.active
            && self.kind == fragment.kind
            && self.message_id == fragment.message_id
            && self.count == fragment.count
            && self.total_len == fragment.total_len as usize
            && self.checksum == fragment.checksum;
        if !same_message {
            self.active = true;
            self.kind = fragment.kind;
            self.message_id = fragment.message_id;
            self.count = fragment.count;
            self.total_len = fragment.total_len as usize;
            self.checksum = fragment.checksum;
            self.received = 0;
        }
        let start = fragment.index as usize * MAX_FRAGMENT_PAYLOAD;
        self.buffer[start..start + fragment.payload.len()].copy_from_slice(fragment.payload);
        self.received |= 1u64 << fragment.index;
        let complete_mask = if self.count == 64 {
            u64::MAX
        } else {
            (1u64 << self.count) - 1
        };
        if self.received != complete_mask {
            return Ok(None);
        }
        self.active = false;
        if crc32(&self.buffer[..self.total_len]) != self.checksum {
            return Err(Error::ChecksumMismatch);
        }
        Ok(Some(CompleteMessage {
            kind: self.kind,
            message_id: self.message_id,
            payload: &self.buffer[..self.total_len],
        }))
    }
}

pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn be16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn put16(bytes: &mut [u8], value: u16) {
    bytes.copy_from_slice(&value.to_be_bytes());
}

fn put32(bytes: &mut [u8], value: u32) {
    bytes.copy_from_slice(&value.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_fragment_round_trip() {
        let message = br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let mut datagram = [0; MAX_DATAGRAM];
        let len = encode_fragment(Kind::HostRequest, 7, message, 0, &mut datagram).unwrap();
        let mut rx = Reassembler::<4096>::default();
        let complete = rx.push(&datagram[..len]).unwrap().unwrap();
        assert_eq!(complete.kind, Kind::HostRequest);
        assert_eq!(complete.message_id, 7);
        assert_eq!(complete.payload, message);
    }

    #[test]
    fn fragments_reassemble_out_of_order() {
        let message = [0x5a; MAX_FRAGMENT_PAYLOAD * 2 + 19];
        let mut packets = [[0; MAX_DATAGRAM]; 3];
        let mut lengths = [0; 3];
        for index in 0..3 {
            lengths[index] = encode_fragment(
                Kind::DeviceResponse,
                99,
                &message,
                index as u16,
                &mut packets[index],
            )
            .unwrap();
        }
        let mut rx = Reassembler::<4096>::default();
        assert!(rx.push(&packets[2][..lengths[2]]).unwrap().is_none());
        assert!(rx.push(&packets[0][..lengths[0]]).unwrap().is_none());
        let complete = rx.push(&packets[1][..lengths[1]]).unwrap().unwrap();
        assert_eq!(complete.payload, message);
    }

    #[test]
    fn checksum_detects_corruption() {
        let message = [7; 1300];
        let mut first = [0; MAX_DATAGRAM];
        let mut second = [0; MAX_DATAGRAM];
        let first_len = encode_fragment(Kind::HostRequest, 4, &message, 0, &mut first).unwrap();
        let second_len = encode_fragment(Kind::HostRequest, 4, &message, 1, &mut second).unwrap();
        second[HEADER_LEN] ^= 1;
        let mut rx = Reassembler::<2048>::default();
        assert!(rx.push(&first[..first_len]).unwrap().is_none());
        assert_eq!(rx.push(&second[..second_len]), Err(Error::ChecksumMismatch));
    }

    #[test]
    fn oversized_message_is_rejected_by_small_receiver() {
        let message = [0; 1300];
        let mut datagram = [0; MAX_DATAGRAM];
        let len = encode_fragment(Kind::DeviceRequest, 1, &message, 0, &mut datagram).unwrap();
        let mut rx = Reassembler::<1000>::default();
        assert_eq!(rx.push(&datagram[..len]), Err(Error::MessageTooLarge));
    }
}
