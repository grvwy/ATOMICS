//! Native local listeners. One open action accepts a session or no session for future machine views.
use std::collections::HashMap;
use std::io;
use std::net::TcpListener;

use super::relay::Relay;
use atomics_core::preview::Preview;
use atomics_core::wire::{MAX_TUNNELS, Message};
use atomics_ui::Command;
use atomics_web::remote::RemoteBackend;

struct Listening {
    socket: TcpListener,
    session: Option<String>,
    preview: Preview,
}

#[derive(Default)]
pub struct Client {
    listeners: HashMap<u16, Listening>,
    relay: Relay,
    next: u64,
}

impl Client {
    /// Return the plain-HTTP browser URL after the loopback listener is ready.
    #[cfg(test)]
    pub fn open(&mut self, session: Option<String>, preview: Preview) -> io::Result<String> {
        let local = self.listen(session, preview.clone())?;
        Ok(preview.url(local))
    }

    /// The local port forwarding `preview`, listening once this returns.
    fn listen(&mut self, session: Option<String>, preview: Preview) -> io::Result<u16> {
        preview.validate().map_err(io::Error::other)?;
        if !self.listeners.contains_key(&preview.port) {
            if self.listeners.len() >= MAX_TUNNELS {
                return Err(io::Error::other("too many previews"));
            }
            let socket = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, preview.port))
                .or_else(|_| TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)))?;
            socket.set_nonblocking(true)?;
            self.listeners.insert(preview.port, Listening { socket, session, preview: preview.clone() });
        }
        Ok(self.listeners[&preview.port].socket.local_addr()?.port())
    }

    pub fn active(&self) -> bool {
        !self.listeners.is_empty()
    }

    pub fn pump(&mut self, remote: &RemoteBackend) {
        if !remote.tunnels() {
            self.listeners.clear();
            self.relay = Relay::default();
            remote.set_active_previews(Vec::new());
            return;
        }
        for action in remote.take_preview_actions() {
            match action {
                Command::OpenPreview { session, preview } => match self.listen(session, preview.clone()) {
                    // The probe goes through this listener, so it waits for later pumps: off this thread.
                    Ok(local) => {
                        std::thread::spawn(move || {
                            let _ = open_browser(&preview.url_with(local, speaks_tls(local)));
                        });
                    }
                    Err(error) => remote.preview_error(format!("Could not open Preview: {error}")),
                },
                Command::StopPreview { port } => {
                    self.listeners.remove(&port);
                }
                _ => {}
            }
        }
        self.listeners.retain(|port, l| {
            (l.session.is_none() && remote.machine_previewable(*port))
                || remote.with_mirror(|m| {
                    m.sessions.values().any(|s| {
                        l.session.as_ref().is_none_or(|id| s.id == *id)
                            && s.status != atomics_core::session::AgentStatus::Exited
                            && s.previews.iter().any(|p| p.port == *port)
                    })
                })
        });
        for message in remote.take_tunnel_messages() {
            if let Message::TunnelRefused { reason, .. } = &message {
                remote.preview_error(format!("Preview: {reason}"));
            }
            let id = match &message {
                Message::TunnelOpened { id }
                | Message::TunnelRefused { id, .. }
                | Message::TunnelData { id, .. }
                | Message::TunnelAck { id, .. }
                | Message::TunnelClose { id, .. } => id.clone(),
                _ => continue,
            };
            if self.relay.receive(message).is_err() {
                self.relay.remove(&id);
                remote.send_tunnel(Message::TunnelClose { id, abort: true });
            }
        }
        for l in self.listeners.values() {
            while self.relay.len() < MAX_TUNNELS {
                match l.socket.accept() {
                    Ok((stream, _)) => {
                        self.next += 1;
                        let id = format!("preview-{}", self.next);
                        if self.relay.accept(id.clone(), l.preview.port, stream).is_ok() {
                            remote.send_tunnel(Message::TunnelOpen { id, port: l.preview.port });
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        for message in self.relay.poll(|port| self.listeners.contains_key(&port)) {
            remote.send_tunnel(message);
        }
        remote.set_active_previews(self.listeners.keys().copied().collect());
    }
}

/// Whether the server on this machine's loopback `port` answers a TLS ClientHello, so the browser should
/// open it over https. Asked once, when someone opens the preview; discovery never probes. A TLS server
/// answers with a handshake or an alert record; a plain HTTP server with text, or by closing.
pub fn speaks_tls(port: u16) -> bool {
    use std::io::{Read, Write};
    use std::time::Duration;
    let address = (std::net::Ipv4Addr::LOCALHOST, port).into();
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    if stream.write_all(&client_hello("localhost")).is_err() {
        return false;
    }
    let mut first = [0u8; 1];
    matches!(stream.read(&mut first), Ok(1) if first[0] == 0x16 || first[0] == 0x15)
}

/// A minimal TLS 1.2 ClientHello naming `host`: enough for a server to answer with its own handshake
/// record, or an alert, either of which says it speaks TLS.
fn client_hello(host: &str) -> Vec<u8> {
    let name = host.as_bytes();
    let mut server_name = vec![0x00, 0x00]; // extension: server_name
    server_name.extend(((name.len() + 5) as u16).to_be_bytes());
    server_name.extend(((name.len() + 3) as u16).to_be_bytes());
    server_name.push(0x00); // host_name
    server_name.extend((name.len() as u16).to_be_bytes());
    server_name.extend(name);
    let mut body = vec![0x03, 0x03]; // TLS 1.2
    body.extend([0u8; 32]); // random
    body.push(0x00); // no session id
    body.extend([0x00, 0x06, 0xc0, 0x2f, 0xc0, 0x2b, 0x13, 0x01]); // ECDHE-RSA/ECDSA-AES128-GCM, TLS_AES_128_GCM
    body.extend([0x01, 0x00]); // null compression
    body.extend((server_name.len() as u16).to_be_bytes());
    body.extend(server_name);
    let mut handshake = vec![0x01]; // ClientHello
    handshake.extend(&(body.len() as u32).to_be_bytes()[1..]);
    handshake.extend(body);
    let mut record = vec![0x16, 0x03, 0x01]; // handshake record
    record.extend((handshake.len() as u16).to_be_bytes());
    record.extend(handshake);
    record
}

pub fn open_browser(url: &str) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";
    std::process::Command::new(program).arg(url).spawn()?.wait()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A one-connection server that reads the first bytes, then answers `reply` and hangs up.
    fn serve(reply: &'static [u8]) -> u16 {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut first = [0u8; 5];
            let _ = stream.read_exact(&mut first);
            let _ = stream.write_all(reply);
        });
        port
    }

    #[test]
    fn a_tls_server_is_opened_over_https_and_a_plain_one_over_http() {
        assert!(speaks_tls(serve(&[0x16, 0x03, 0x03, 0x00, 0x2a])), "a ServerHello record");
        assert!(speaks_tls(serve(&[0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x28])), "a handshake_failure alert");
        assert!(!speaks_tls(serve(b"HTTP/1.1 400 Bad Request\r\n\r\n")), "plain HTTP");
        assert!(!speaks_tls(serve(b"")), "a server that just hangs up");
        let closed = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
        assert!(!speaks_tls(closed), "nothing listening");
    }

    #[test]
    fn the_client_hello_is_one_well_formed_record() {
        let hello = client_hello("localhost");
        assert_eq!(&hello[..3], &[0x16, 0x03, 0x01]);
        assert_eq!(u16::from_be_bytes([hello[3], hello[4]]) as usize, hello.len() - 5, "record length");
        assert_eq!(hello[5], 0x01, "ClientHello");
        assert_eq!(u32::from_be_bytes([0, hello[6], hello[7], hello[8]]) as usize, hello.len() - 9, "handshake length");
        assert!(hello.windows(9).any(|w| w == b"localhost"), "names the host");
    }
}
