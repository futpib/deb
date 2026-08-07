use std::{
    io::{IoSlice, IoSliceMut},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    os::unix::process::CommandExt,
    process::Command,
};

use nix::{
    fcntl::{FcntlArg, FdFlag, fcntl},
    sys::socket::{
        AddressFamily, ControlMessage, ControlMessageOwned, MsgFlags, SockFlag, SockType, recvmsg,
        sendmsg, socketpair,
    },
};
use prost::Message;
use thiserror::Error;

pub mod wire {
    include!(concat!(env!("OUT_DIR"), "/deb.shell.rs"));
}

pub const MAX_PACKET_BYTES: usize = 256 * 1024;
pub const CHILD_CONTROL_FD: RawFd = 3;
pub const MAX_PACKET_FDS: usize = 5;

pub fn is_valid_profile_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().enumerate().all(|(index, byte)| match byte {
            b'a'..=b'z' | b'0'..=b'9' => true,
            b'-' | b'_' => index != 0,
            _ => false,
        })
}

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
    #[error("shell protocol ancillary data was truncated")]
    AncillaryTruncated,
    #[error("shell protocol packet has {actual} file descriptors; maximum is {maximum}")]
    TooManyFileDescriptors { actual: usize, maximum: usize },
    #[error("shell protocol packet unexpectedly carried {0} file descriptors")]
    UnexpectedFileDescriptors(usize),
    #[error("shell protocol socket closed")]
    Closed,
}

pub struct Transport {
    socket: OwnedFd,
}

pub struct ReceivedPacket {
    pub packet: wire::Packet,
    pub file_descriptors: Vec<OwnedFd>,
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
        self.send_with_fds(packet, &[])
    }

    pub fn send_with_fds(
        &self,
        packet: &wire::Packet,
        file_descriptors: &[RawFd],
    ) -> Result<(), ProtocolError> {
        let mut payload = Vec::with_capacity(packet.encoded_len());
        packet.encode(&mut payload)?;
        if payload.len() > MAX_PACKET_BYTES {
            return Err(ProtocolError::PacketTooLarge {
                actual: payload.len(),
                maximum: MAX_PACKET_BYTES,
            });
        }
        if file_descriptors.len() > MAX_PACKET_FDS {
            return Err(ProtocolError::TooManyFileDescriptors {
                actual: file_descriptors.len(),
                maximum: MAX_PACKET_FDS,
            });
        }
        let payload = [IoSlice::new(&payload)];
        let control =
            (!file_descriptors.is_empty()).then_some(ControlMessage::ScmRights(file_descriptors));
        let control = control.as_slice();
        let written = sendmsg::<()>(
            self.socket.as_raw_fd(),
            &payload,
            control,
            MsgFlags::MSG_NOSIGNAL,
            None,
        )?;
        if written != payload[0].len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "sequenced packet was not written atomically",
            )
            .into());
        }
        Ok(())
    }

    pub fn receive(&self) -> Result<wire::Packet, ProtocolError> {
        let received = self.receive_with_fds()?;
        if !received.file_descriptors.is_empty() {
            return Err(ProtocolError::UnexpectedFileDescriptors(
                received.file_descriptors.len(),
            ));
        }
        Ok(received.packet)
    }

    pub fn receive_with_fds(&self) -> Result<ReceivedPacket, ProtocolError> {
        let mut payload = vec![0; MAX_PACKET_BYTES];
        let mut control = nix::cmsg_space!([RawFd; MAX_PACKET_FDS]);
        let (bytes, flags, file_descriptors) = {
            let mut vectors = [IoSliceMut::new(&mut payload)];
            let message = recvmsg::<()>(
                self.socket.as_raw_fd(),
                &mut vectors,
                Some(&mut control),
                MsgFlags::MSG_TRUNC | MsgFlags::MSG_CMSG_CLOEXEC,
            )?;
            let mut file_descriptors = Vec::new();
            for control_message in message.cmsgs()? {
                if let ControlMessageOwned::ScmRights(descriptors) = control_message {
                    file_descriptors.extend(descriptors);
                }
            }
            (message.bytes, message.flags, file_descriptors)
        };
        if bytes == 0 {
            return Err(ProtocolError::Closed);
        }
        if bytes > payload.len() || flags.contains(MsgFlags::MSG_TRUNC) {
            return Err(ProtocolError::Truncated);
        }
        if flags.contains(MsgFlags::MSG_CTRUNC) {
            return Err(ProtocolError::AncillaryTruncated);
        }
        if file_descriptors.len() > MAX_PACKET_FDS {
            return Err(ProtocolError::TooManyFileDescriptors {
                actual: file_descriptors.len(),
                maximum: MAX_PACKET_FDS,
            });
        }
        let file_descriptors = file_descriptors
            .into_iter()
            .map(|descriptor| unsafe { OwnedFd::from_raw_fd(descriptor) })
            .collect();
        Ok(ReceivedPacket {
            packet: wire::Packet::decode(&payload[..bytes])?,
            file_descriptors,
        })
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
    use super::{MAX_PACKET_BYTES, ProtocolError, Transport, is_valid_profile_id, wire};
    use nix::sys::socket::{MsgFlags, sendmsg};
    use nix::unistd::pipe;
    use prost::Message;
    use std::io::IoSlice;
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
    fn validates_filesystem_safe_profile_ids() {
        assert!(is_valid_profile_id("default"));
        assert!(is_valid_profile_id("work-2"));
        assert!(is_valid_profile_id("account_one"));
        assert!(!is_valid_profile_id(""));
        assert!(!is_valid_profile_id("Work"));
        assert!(!is_valid_profile_id("../work"));
        assert!(!is_valid_profile_id("-work"));
        assert!(!is_valid_profile_id(&"a".repeat(65)));
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
    fn transports_file_descriptors_with_their_packet() {
        let (sender, receiver) = Transport::pair().unwrap();
        let (read_end, _write_end) = pipe().unwrap();
        sender
            .send_with_fds(&hello_packet(), &[read_end.as_raw_fd()])
            .unwrap();
        let received = receiver.receive_with_fds().unwrap();
        assert_eq!(received.file_descriptors.len(), 1);
        assert!(matches!(
            received.packet.body,
            Some(wire::packet::Body::Hello(_))
        ));
    }

    #[test]
    fn rejects_malformed_packets() {
        let (sender, receiver) = Transport::pair().unwrap();
        let payload = [IoSlice::new(&[0xff])];
        sendmsg::<()>(
            sender.socket.as_raw_fd(),
            &payload,
            &[],
            MsgFlags::MSG_NOSIGNAL,
            None,
        )
        .unwrap();
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
