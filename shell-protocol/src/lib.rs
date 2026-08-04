use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    os::unix::process::CommandExt,
    process::Command,
};

use nix::{
    fcntl::{FcntlArg, FdFlag, fcntl},
    sys::socket::{AddressFamily, MsgFlags, SockFlag, SockType, recv, send, socketpair},
};
use prost::Message;
use thiserror::Error;

pub mod wire {
    include!(concat!(env!("OUT_DIR"), "/dual_engine.shell.rs"));
}

pub const MAX_PACKET_BYTES: usize = 256 * 1024;
pub const CHILD_CONTROL_FD: RawFd = 3;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("shell protocol I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("shell protocol socket failed: {0}")]
    Socket(#[from] nix::Error),
    #[error("shell protocol encoding failed: {0}")]
    Encode(#[from] prost::EncodeError),
    #[error("shell protocol decoding failed: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("shell protocol packet is {actual} bytes; maximum is {maximum}")]
    PacketTooLarge { actual: usize, maximum: usize },
    #[error("shell protocol packet was truncated")]
    Truncated,
    #[error("shell protocol socket closed")]
    Closed,
}

pub struct Transport {
    socket: OwnedFd,
}

impl Transport {
    pub fn pair() -> Result<(Self, Self), ProtocolError> {
        let (left, right) = socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::SOCK_CLOEXEC,
        )?;
        Ok((Self { socket: left }, Self { socket: right }))
    }

    /// # Safety
    ///
    /// `fd` must be an open, uniquely owned `AF_UNIX/SOCK_SEQPACKET` socket.
    pub unsafe fn from_raw_fd(fd: RawFd) -> Result<Self, ProtocolError> {
        let socket = unsafe { OwnedFd::from_raw_fd(fd) };
        fcntl(&socket, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
        Ok(Self { socket })
    }

    pub fn try_clone(&self) -> Result<Self, ProtocolError> {
        Ok(Self {
            socket: self.socket.try_clone()?,
        })
    }

    pub fn send(&self, packet: &wire::Packet) -> Result<(), ProtocolError> {
        let mut payload = Vec::with_capacity(packet.encoded_len());
        packet.encode(&mut payload)?;
        if payload.len() > MAX_PACKET_BYTES {
            return Err(ProtocolError::PacketTooLarge {
                actual: payload.len(),
                maximum: MAX_PACKET_BYTES,
            });
        }
        let written = send(self.socket.as_raw_fd(), &payload, MsgFlags::MSG_NOSIGNAL)?;
        if written != payload.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "sequenced packet was not written atomically",
            )
            .into());
        }
        Ok(())
    }

    pub fn receive(&self) -> Result<wire::Packet, ProtocolError> {
        let mut payload = vec![0; MAX_PACKET_BYTES];
        let bytes = recv(self.socket.as_raw_fd(), &mut payload, MsgFlags::MSG_TRUNC)?;
        if bytes == 0 {
            return Err(ProtocolError::Closed);
        }
        if bytes > payload.len() {
            return Err(ProtocolError::Truncated);
        }
        Ok(wire::Packet::decode(&payload[..bytes])?)
    }
}

pub fn configure_child_command(command: &mut Command, transport: &Transport) {
    let source = transport.socket.as_raw_fd();
    command
        .arg("--control-fd")
        .arg(CHILD_CONTROL_FD.to_string());
    unsafe {
        command.pre_exec(move || {
            if source == CHILD_CONTROL_FD {
                let flags = nix::libc::fcntl(source, nix::libc::F_GETFD);
                if flags < 0
                    || nix::libc::fcntl(source, nix::libc::F_SETFD, flags & !nix::libc::FD_CLOEXEC)
                        < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
            } else if nix::libc::dup2(source, CHILD_CONTROL_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_PACKET_BYTES, ProtocolError, Transport, wire};
    use nix::sys::socket::{MsgFlags, send};
    use prost::Message;
    use std::os::fd::AsRawFd;

    fn hello_packet() -> wire::Packet {
        wire::Packet {
            request_id: 1,
            body: Some(wire::packet::Body::Hello(wire::Hello {
                maximum_packet_bytes: MAX_PACKET_BYTES as u32,
                requested_capabilities: Vec::new(),
            })),
        }
    }

    #[test]
    fn transports_a_packet() {
        let (sender, receiver) = Transport::pair().unwrap();
        sender.send(&hello_packet()).unwrap();
        let received = receiver.receive().unwrap();
        assert_eq!(received.request_id, 1);
        assert!(matches!(received.body, Some(wire::packet::Body::Hello(_))));
    }

    #[test]
    fn rejects_malformed_packets() {
        let (sender, receiver) = Transport::pair().unwrap();
        send(sender.socket.as_raw_fd(), &[0xff], MsgFlags::MSG_NOSIGNAL).unwrap();
        assert!(matches!(receiver.receive(), Err(ProtocolError::Decode(_))));
    }

    #[test]
    fn ignores_unknown_protobuf_fields() {
        let mut encoded = hello_packet().encode_to_vec();
        encoded.extend_from_slice(&[0xa0, 0x06, 0x07]);
        let decoded = wire::Packet::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.request_id, 1);
        assert!(matches!(decoded.body, Some(wire::packet::Body::Hello(_))));
    }
}
