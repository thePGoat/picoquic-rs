use super::packet::Packet;
use super::quic_ctx::{socket_addr_from_c, MicroSeconds, QuicCtx};
use connection;
use error::*;
use stream;
use ConnectionType;

use picoquic_sys::picoquic::{
    self, picoquic_close, picoquic_cnx_t, picoquic_create_client_cnx, picoquic_delete_cnx,
    picoquic_enable_keep_alive, picoquic_get_cnx_state, picoquic_get_first_cnx,
    picoquic_get_local_addr, picoquic_get_local_cnxid, picoquic_get_local_error,
    picoquic_get_next_cnx, picoquic_get_peer_addr, picoquic_get_remote_error, picoquic_is_client,
    picoquic_quic_t, picoquic_state_enum_picoquic_state_client_ready,
    picoquic_state_enum_picoquic_state_disconnected,
    picoquic_state_enum_picoquic_state_server_ready, picoquic_val64_connection_id,
    PICOQUIC_TLS_HANDSHAKE_FAILED,
};

use std::net::SocketAddr;
use std::ptr;
use std::time::Duration;

use socket2::SockAddr;

#[derive(Copy, Clone)]
pub struct Connection {
    cnx: *mut picoquic_cnx_t,
}

impl Connection {
    pub fn new(
        quic: &QuicCtx,
        server_addr: SocketAddr,
        current_time: u64,
    ) -> Result<Connection, Error> {
        assert!(
            !server_addr.ip().is_unspecified(),
            "server address must not be unspecified!"
        );

        let server_addr = SockAddr::from(server_addr);

        let cnx = unsafe {
            picoquic_create_client_cnx(
                quic.as_ptr(),
                server_addr.as_ptr() as *mut picoquic::sockaddr,
                current_time,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                None,
                ptr::null_mut(),
            )
        };

        if cnx.is_null() {
            Err(ErrorKind::Unknown)?;
        }

        Ok(Connection { cnx })
    }

    pub fn as_ptr(&self) -> *mut picoquic_cnx_t {
        self.cnx
    }

    /// Returns the peer address of this connection.
    pub fn peer_addr(&self) -> SocketAddr {
        let mut addr_len = 0;
        let mut addr: *mut picoquic::sockaddr = ptr::null_mut();

        unsafe {
            picoquic_get_peer_addr(self.cnx, &mut addr, &mut addr_len);

            socket_addr_from_c(addr, addr_len)
        }
    }

    /// Returns the local address of this connection.
    pub fn local_addr(&self) -> SocketAddr {
        let mut addr_len = 0;
        let mut addr: *mut picoquic::sockaddr = ptr::null_mut();

        unsafe {
            picoquic_get_local_addr(self.cnx, &mut addr, &mut addr_len);

            socket_addr_from_c(addr, addr_len)
        }
    }

    /// Creates and prepares a `Packet`.
    /// The `Packet` contains any data from this connection(data from streams, ACK's, ...).
    /// The `Packet` will be stored in the given buffer.
    ///
    /// # Returns
    /// The length of the `Packet` in the buffer or `None` if the package does not contains any data.
    pub fn create_and_prepare_packet(
        &self,
        buffer: &mut [u8],
        current_time: u64,
    ) -> Result<Option<usize>, Error> {
        let mut packet = Packet::create(buffer)?;
        let size = packet.prepare(current_time, self)?;

        if packet.contains_data() {
            Ok(Some(size))
        } else {
            Ok(None)
        }
    }

    /// Deletes the underlying C pointer!
    pub fn delete(self) {
        unsafe {
            picoquic_delete_cnx(self.cnx);
        }
    }

    pub fn is_disconnected(&self) -> bool {
        self.state() == picoquic_state_enum_picoquic_state_disconnected
    }

    /// Is the connection ready to be used?
    pub fn is_ready(&self) -> bool {
        let state = self.state();
        state == picoquic_state_enum_picoquic_state_client_ready
            || state == picoquic_state_enum_picoquic_state_server_ready
    }

    fn state(&self) -> u32 {
        unsafe { picoquic_get_cnx_state(self.cnx) }
    }

    pub fn close(&self) {
        //TODO maybe replace 0 with an appropriate error code
        unsafe {
            picoquic_close(self.cnx, 0);
        }
    }

    /// Generates a new `Stream` id from the given `next_id`. The `next_id` can be incremented by
    /// one, after calling this function. The resulting `Stream` id depends on `is_client` and
    /// `stype`, as both values are encoded in the first two bits of the new id.
    pub(crate) fn generate_stream_id(
        next_id: u64,
        is_client: bool,
        stype: stream::Type,
    ) -> stream::Id {
        // Stream 0, 1, 2 and 3 are reserved.
        // Client first usable stream is 4, Server first usable stream is 5.
        // Client gets even stream ids and server gets odd stream ids.
        let mut id = next_id + 1;

        id <<= 2;

        if !is_client {
            id |= 1;
        }

        // Unidirectional sets the second bit to 1
        if let stream::Type::Unidirectional = stype {
            id |= 2;
        }

        id
    }

    pub fn enable_keep_alive(&self, interval: Duration) {
        let interval = interval.as_micro_seconds();
        unsafe {
            picoquic_enable_keep_alive(self.cnx, interval);
        }
    }

    /// Returns the local connection id for this connection.
    pub fn local_id(&self) -> connection::Id {
        unsafe {
            let id = picoquic_get_local_cnxid(self.as_ptr());
            picoquic_val64_connection_id(id)
        }
    }

    /// Returns the type of this connection.
    pub fn con_type(&self) -> ConnectionType {
        unsafe {
            if picoquic_is_client(self.as_ptr()) == 1 {
                ConnectionType::Outgoing
            } else {
                ConnectionType::Incoming
            }
        }
    }

    /// Checks if the connection had an error.
    /// The returned closure, will always construct the same error.
    pub fn error(&self) -> Option<Box<Fn() -> Error>> {
        let error_code = unsafe {
            let error = picoquic_get_local_error(self.as_ptr());
            if error != 0 {
                error
            } else {
                picoquic_get_remote_error(self.as_ptr())
            }
        };

        if error_code == 0 {
            None
        } else {
            Some(Box::new(move || match error_code as u32 {
                PICOQUIC_TLS_HANDSHAKE_FAILED => ErrorKind::TLSHandshakeError.into(),
                _ => ErrorKind::Unknown.into(),
            }))
        }
    }
}

impl From<*mut picoquic_cnx_t> for Connection {
    fn from(cnx: *mut picoquic_cnx_t) -> Connection {
        Connection { cnx }
    }
}

pub struct ConnectionIter {
    iter: <Vec<*mut picoquic_cnx_t> as IntoIterator>::IntoIter,
}

impl ConnectionIter {
    pub fn new(quic: *mut picoquic_quic_t) -> ConnectionIter {
        // We need to build a "stable" iterator.
        // Picoquic reorders the connections internally, while working with them and that can lead
        // to an infinite loop over the connections.
        // So, we build the list once and are safe to not loop infinitely.
        let mut vec = Vec::new();
        unsafe {
            let mut current = picoquic_get_first_cnx(quic);

            while !current.is_null() {
                vec.push(current);

                current = picoquic_get_next_cnx(current);
            }
        }

        ConnectionIter {
            iter: vec.into_iter(),
        }
    }
}

impl Iterator for ConnectionIter {
    type Item = Connection;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next().map(Connection::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_bidirectional_stream_id_generation() {
        assert_eq!(
            4,
            Connection::generate_stream_id(0, true, stream::Type::Bidirectional)
        );
        assert_eq!(
            8,
            Connection::generate_stream_id(1, true, stream::Type::Bidirectional)
        );
        assert_eq!(
            12,
            Connection::generate_stream_id(2, true, stream::Type::Bidirectional)
        );
    }

    #[test]
    fn client_unidirectional_stream_id_generation() {
        assert_eq!(
            6,
            Connection::generate_stream_id(0, true, stream::Type::Unidirectional)
        );
        assert_eq!(
            10,
            Connection::generate_stream_id(1, true, stream::Type::Unidirectional)
        );
        assert_eq!(
            14,
            Connection::generate_stream_id(2, true, stream::Type::Unidirectional)
        );
    }

    #[test]
    fn server_bidirectional_stream_id_generation() {
        assert_eq!(
            5,
            Connection::generate_stream_id(0, false, stream::Type::Bidirectional)
        );
        assert_eq!(
            9,
            Connection::generate_stream_id(1, false, stream::Type::Bidirectional)
        );
        assert_eq!(
            13,
            Connection::generate_stream_id(2, false, stream::Type::Bidirectional)
        );
    }

    #[test]
    fn server_unidirectional_stream_id_generation() {
        assert_eq!(
            7,
            Connection::generate_stream_id(0, false, stream::Type::Unidirectional)
        );
        assert_eq!(
            11,
            Connection::generate_stream_id(1, false, stream::Type::Unidirectional)
        );
        assert_eq!(
            15,
            Connection::generate_stream_id(2, false, stream::Type::Unidirectional)
        );
    }

    #[test]
    #[should_panic(expected = "server address must not be unspecified!")]
    fn do_not_accept_unspecified_ip_address() {
        let _ = Connection::new(&QuicCtx::dummy(), ([0, 0, 0, 0], 12345).into(), 0);
    }
}
