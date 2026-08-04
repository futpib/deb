use std::{
    io::{IoSlice, IoSliceMut},
    os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
    os::unix::process::CommandExt,
    process::Command,
};

use nix::{
    cmsg_space,
    fcntl::{FcntlArg, FdFlag, fcntl},
    sys::socket::{
        AddressFamily, ControlMessage, ControlMessageOwned, MsgFlags, SockFlag, SockType, recvmsg,
        sendmsg, socketpair,
    },
};
use prost::Message;
use thiserror::Error;

pub mod wire {
    include!(concat!(env!("OUT_DIR"), "/dual_engine.shell.v1.rs"));
}

pub const PROTOCOL_MAJOR: u32 = 1;
pub const PROTOCOL_MINOR: u32 = 0;
pub const MAX_PACKET_BYTES: usize = 256 * 1024;
pub const MAX_ATTACHED_FILES: usize = 8;
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
    #[error("shell protocol packet declared {declared} files but carried {received}")]
    AttachedFileCount { declared: usize, received: usize },
    #[error("shell protocol packet carried too many file descriptors")]
    TooManyAttachedFiles,
    #[error("shell protocol attached-file index {actual} is invalid; expected {expected}")]
    InvalidAttachedFileIndex { actual: u32, expected: usize },
    #[error("shell protocol socket closed")]
    Closed,
}

pub struct ReceivedPacket {
    pub packet: wire::Packet,
    pub files: Vec<OwnedFd>,
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
        self.send_with_files(packet, &[])
    }

    pub fn send_with_files(
        &self,
        packet: &wire::Packet,
        files: &[BorrowedFd<'_>],
    ) -> Result<(), ProtocolError> {
        if files.len() > MAX_ATTACHED_FILES {
            return Err(ProtocolError::TooManyAttachedFiles);
        }
        if packet.attached_files.len() != files.len() {
            return Err(ProtocolError::AttachedFileCount {
                declared: packet.attached_files.len(),
                received: files.len(),
            });
        }
        let mut payload = Vec::with_capacity(packet.encoded_len());
        packet.encode(&mut payload)?;
        if payload.len() > MAX_PACKET_BYTES {
            return Err(ProtocolError::PacketTooLarge {
                actual: payload.len(),
                maximum: MAX_PACKET_BYTES,
            });
        }
        let vectors = [IoSlice::new(&payload)];
        let raw_files = files.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>();
        let control = if raw_files.is_empty() {
            Vec::new()
        } else {
            vec![ControlMessage::ScmRights(&raw_files)]
        };
        let written = sendmsg::<()>(
            self.socket.as_raw_fd(),
            &vectors,
            &control,
            MsgFlags::MSG_NOSIGNAL,
            None,
        )?;
        if written != payload.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "sequenced packet was not written atomically",
            )
            .into());
        }
        Ok(())
    }

    pub fn receive(&self) -> Result<ReceivedPacket, ProtocolError> {
        let mut payload = vec![0; MAX_PACKET_BYTES];
        let mut vectors = [IoSliceMut::new(&mut payload)];
        let mut control = cmsg_space!([RawFd; MAX_ATTACHED_FILES]);
        let message = recvmsg::<()>(
            self.socket.as_raw_fd(),
            &mut vectors,
            Some(&mut control),
            MsgFlags::MSG_CMSG_CLOEXEC,
        )?;
        if message.bytes == 0 {
            return Err(ProtocolError::Closed);
        }
        if message
            .flags
            .intersects(MsgFlags::MSG_TRUNC | MsgFlags::MSG_CTRUNC)
        {
            return Err(ProtocolError::Truncated);
        }
        let bytes = message.bytes;
        let mut files = Vec::new();
        for control in message.cmsgs()? {
            if let ControlMessageOwned::ScmRights(received) = control {
                for fd in received {
                    if files.len() == MAX_ATTACHED_FILES {
                        return Err(ProtocolError::TooManyAttachedFiles);
                    }
                    files.push(unsafe { OwnedFd::from_raw_fd(fd) });
                }
            }
        }
        let packet = wire::Packet::decode(&payload[..bytes])?;
        if packet.attached_files.len() != files.len() {
            return Err(ProtocolError::AttachedFileCount {
                declared: packet.attached_files.len(),
                received: files.len(),
            });
        }
        for (index, descriptor) in packet.attached_files.iter().enumerate() {
            if descriptor.index as usize != index {
                return Err(ProtocolError::InvalidAttachedFileIndex {
                    actual: descriptor.index,
                    expected: index,
                });
            }
        }
        Ok(ReceivedPacket { packet, files })
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
    use super::{MAX_PACKET_BYTES, PROTOCOL_MAJOR, ProtocolError, Transport, wire};
    use nix::sys::socket::{MsgFlags, send};
    use prost::Message;
    use std::os::fd::{AsFd, AsRawFd};

    fn hello_packet() -> wire::Packet {
        wire::Packet {
            request_id: 1,
            attached_files: Vec::new(),
            body: Some(wire::packet::Body::Hello(wire::Hello {
                minimum_major: PROTOCOL_MAJOR,
                maximum_major: PROTOCOL_MAJOR,
                maximum_packet_bytes: MAX_PACKET_BYTES as u32,
                requested_capabilities: Vec::new(),
            })),
        }
    }

    #[test]
    fn transports_a_versioned_packet() {
        let (sender, receiver) = Transport::pair().unwrap();
        sender.send(&hello_packet()).unwrap();
        let received = receiver.receive().unwrap();
        assert_eq!(received.packet.request_id, 1);
        assert!(matches!(
            received.packet.body,
            Some(wire::packet::Body::Hello(wire::Hello {
                minimum_major: PROTOCOL_MAJOR,
                maximum_major: PROTOCOL_MAJOR,
                ..
            }))
        ));
    }

    #[test]
    fn transports_an_attached_file() {
        let (sender, receiver) = Transport::pair().unwrap();
        let file = std::fs::File::open("/dev/null").unwrap();
        let mut packet = hello_packet();
        packet.attached_files.push(wire::AttachedFile {
            index: 0,
            size: 0,
            purpose: "test".to_owned(),
        });
        sender.send_with_files(&packet, &[file.as_fd()]).unwrap();
        let received = receiver.receive().unwrap();
        assert_eq!(received.files.len(), 1);
    }

    #[test]
    fn rejects_mismatched_file_metadata() {
        let (sender, _) = Transport::pair().unwrap();
        let mut packet = hello_packet();
        packet.attached_files.push(wire::AttachedFile {
            index: 0,
            size: 0,
            purpose: "missing".to_owned(),
        });
        assert!(matches!(
            sender.send(&packet),
            Err(ProtocolError::AttachedFileCount {
                declared: 1,
                received: 0
            })
        ));
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
