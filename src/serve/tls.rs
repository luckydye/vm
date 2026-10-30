//! TLS for `varmint serve --cert FILE --key FILE`, so browsers on other
//! machines get a secure context. Two threads per connection move bytes
//! between the TLS session and one end of a socket pair, whose other end the
//! server handles like a plain TCP connection.

use crate::protocol;
use rustls::{
    ServerConfig, ServerConnection,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
};
use std::{
    io::{self, Read, Write},
    net::{Shutdown, TcpStream},
    os::unix::net::UnixStream,
    path::Path,
    sync::{Arc, Mutex},
    thread,
};

/// Below rustls' buffer limits, so a chunk is always taken whole.
const CHUNK_BYTES: usize = 16 * 1024;

/// The server config for a PEM certificate chain and private key.
pub fn config(cert: &Path, key: &Path) -> Result<Arc<ServerConfig>, String> {
    let chain = CertificateDer::pem_file_iter(cert)
        .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
        .map_err(|error| format!("failed to read {}: {error}", cert.display()))?;
    let key = PrivateKeyDer::from_pem_file(key).map_err(|error| format!("failed to read {}: {error}", key.display()))?;
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|error| format!("invalid certificate or key: {error}"))?;
    Ok(Arc::new(config))
}

/// Runs `handle` on the plaintext of `connection` and returns its result, or
/// the TLS error that ended the connection first.
pub fn wrap(
    connection: TcpStream,
    config: Arc<ServerConfig>,
    handle: impl FnOnce(UnixStream) -> io::Result<()>,
) -> io::Result<()> {
    let session = Mutex::new(ServerConnection::new(config).map_err(io::Error::other)?);
    let (plaintext, inner) = UnixStream::pair()?;
    protocol::set_socket_option(&plaintext, libc::SOL_SOCKET, libc::SO_SNDBUF, protocol::STREAM_QUEUE_BYTES)?;
    let inner_writer = inner.try_clone()?;
    thread::scope(|scope| {
        let decrypted = scope.spawn(|| {
            let result = decrypt(&session, &connection, &inner_writer);
            let _ = inner_writer.shutdown(Shutdown::Write);
            result
        });
        scope.spawn(|| {
            let _ = encrypt(&session, inner, &connection);
            let _ = connection.shutdown(Shutdown::Both);
        });
        let handled = handle(plaintext);
        decrypted.join().expect("the TLS thread panicked").and(handled)
    })
}

/// Passes what the client sends, decrypted, to `inner` until the client closes.
fn decrypt(session: &Mutex<ServerConnection>, mut network: &TcpStream, mut inner: &UnixStream) -> io::Result<()> {
    let mut buffer = vec![0; CHUNK_BYTES];
    let mut plaintext = Vec::new();
    loop {
        let count = network.read(&mut buffer)?;
        if count == 0 {
            return Ok(());
        }
        let mut closed = false;
        {
            let mut session = session.lock().expect("the TLS session is poisoned");
            let mut received = &buffer[..count];
            while !received.is_empty() {
                session.read_tls(&mut received)?;
                let state = session.process_new_packets().map_err(|error| {
                    // Tells the client why, as an alert.
                    let _ = session.write_tls(&mut network);
                    io::Error::other(format!("TLS: {error}"))
                })?;
                let start = plaintext.len();
                plaintext.resize(start + state.plaintext_bytes_to_read(), 0);
                session.reader().read_exact(&mut plaintext[start..])?;
                closed |= state.peer_has_closed();
            }
            while session.wants_write() {
                session.write_tls(&mut network)?;
            }
        }
        inner.write_all(&plaintext)?;
        plaintext.clear();
        if closed {
            return Ok(());
        }
    }
}

/// Sends what the server writes to `inner`, encrypted, until it closes.
fn encrypt(session: &Mutex<ServerConnection>, mut inner: UnixStream, mut network: &TcpStream) -> io::Result<()> {
    let mut buffer = vec![0; CHUNK_BYTES];
    loop {
        let count = inner.read(&mut buffer)?;
        let mut session = session.lock().expect("the TLS session is poisoned");
        match count {
            0 => session.send_close_notify(),
            _ => session.writer().write_all(&buffer[..count])?,
        }
        while session.wants_write() {
            session.write_tls(&mut network)?;
        }
        if count == 0 {
            return Ok(());
        }
    }
}
