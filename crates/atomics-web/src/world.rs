//! The frame, its hosting page, its stored devices and a [`FakeHost`], joined by a synchronous network.

use std::collections::HashMap;

use atomics_core::noise::{Access, Pairing, Secret};
use atomics_core::tunnel;
use atomics_core::wire::Message;
use atomics_ui::{Backend, Command};
use serde_json::{Value, json};

use crate::boot::{self, ParentOrigin};
use crate::device::Device;
use crate::driver::{Driver, Output};
use crate::fake_host::{FakeHost, HOST_ID};
use crate::keep::{Ask, Sealed};

pub const PARENT: &str = "https://app.example";
pub const BROKER: &str = "wss://app.example/atomics/tunnel";

/// A message sent is received before the next is sent.
pub struct World {
    pub driver: Driver,
    pub host: FakeHost,
    pub store: HashMap<String, Device>,
    pub posted: Vec<&'static str>,
    pub socket: Option<u64>,
    /// The subprotocols of every dial.
    pub dials: Vec<Vec<String>>,
    /// Codes this client closed sockets with.
    pub closed: Vec<u16>,
    pub now: u64,
    /// The hosting page keeps sealed records ([`boot::KEEP`]), and those it keeps, by host.
    pub keeps: bool,
    pub page: HashMap<String, Sealed>,
    /// Whether the page answers a keep, and how; `None`: it stays silent.
    pub page_answers: Option<bool>,
    /// What `atomics.paired` said each time.
    pub kept: Vec<bool>,
    /// What the passkey button was last asked to do.
    pub ask: Option<Ask>,
}

impl World {
    pub fn new(host: FakeHost) -> Self {
        let driver = Driver::new(ParentOrigin::parse(PARENT), "op", "Safari on iPhone");
        let mut world = World {
            driver,
            host,
            store: HashMap::new(),
            posted: Vec::new(),
            socket: None,
            dials: Vec::new(),
            closed: Vec::new(),
            now: 1_000_000,
            keeps: false,
            page: HashMap::new(),
            page_answers: Some(true),
            kept: Vec::new(),
            ask: None,
        };
        world.driver.tick(world.now);
        world.pump();
        world
    }

    /// A world whose browser paired with the host earlier.
    pub fn paired(host: FakeHost) -> Self {
        let mut world = World::new(host);
        let device = Secret::generate().unwrap();
        world.host.allowed.push((device.public(), Access::Control));
        let host_key = world.host.public();
        world.store.insert(HOST_ID.into(), Device { host_id: HOST_ID.into(), host_key, secret: device });
        world
    }

    pub fn boot_data(&self, pairing: Option<&Pairing>) -> Value {
        let mut data = json!({
            "type": boot::BOOT, "v": 1, "host_id": HOST_ID, "ticket": "ticket-1", "broker": BROKER,
            "display_name": "A Person", "theme": "dark", "scheme": "dark", "locale": "en",
        });
        if self.keeps {
            data["keeps"] = json!(true);
            data["sealed"] = self.page.get(HOST_ID).map_or(Value::Null, Sealed::to_json);
        }
        if let Some(pairing) = pairing {
            let fragment = pairing.fragment();
            let parts: Vec<&str> = fragment.split('.').collect();
            data["mode"] = json!("pair");
            data["pair"] = json!({ "v": 1, "host_noise_pubkey": parts[2], "psk": parts[3] });
        }
        data
    }

    pub fn boot(&mut self, pairing: Option<&Pairing>) {
        let data = self.boot_data(pairing);
        self.driver.message(PARENT, true, &data).unwrap();
        self.pump();
    }

    pub fn ticket(&mut self, ticket: &str) {
        let data = json!({ "type": boot::TICKET, "v": 1, "host_id": HOST_ID, "ticket": ticket });
        self.driver.message(PARENT, true, &data).unwrap();
        self.pump();
    }

    pub fn deliver(&mut self, messages: Vec<Vec<u8>>) {
        let socket = self.socket.expect("a socket is open");
        for message in messages {
            self.driver.socket_message(socket, &message);
        }
        self.pump();
    }

    pub fn drop_socket(&mut self, code: u16) {
        let socket = self.socket.take().expect("a socket is open");
        self.host.left();
        self.driver.socket_closed(socket, code);
        self.pump();
    }

    pub fn advance(&mut self, ms: u64) {
        self.now += ms;
        self.driver.tick(self.now);
        self.pump();
    }

    pub fn send(&mut self, command: Command) {
        self.driver.remote.send(command);
        self.driver.flush();
        self.pump();
    }

    pub fn pump(&mut self) {
        loop {
            let outputs = self.driver.take_outputs();
            if outputs.is_empty() {
                return;
            }
            for output in outputs {
                match output {
                    Output::Post(kind) => self.posted.push(kind),
                    Output::Dial { socket, url, protocols } => {
                        assert_eq!(url, BROKER);
                        self.dials.push(protocols.iter().map(|p| p.to_string()).collect());
                        self.socket = Some(socket);
                        self.host.join();
                        self.driver.socket_open(socket, tunnel::SUBPROTOCOL);
                    }
                    // The world is a browser's; a direct client's tests drive their own host.
                    Output::DialDirect { socket } => self.driver.socket_closed(socket, 1006),
                    Output::Send { socket, message } => {
                        if Some(socket) != self.socket {
                            continue;
                        }
                        match self.host.receive(&message) {
                            Ok(replies) => {
                                for reply in replies {
                                    self.driver.socket_message(socket, &reply);
                                }
                                if let Some(code) = self.host.take_close()
                                    && self.socket == Some(socket)
                                {
                                    self.socket = None;
                                    self.host.left();
                                    self.driver.socket_closed(socket, code);
                                }
                            }
                            Err(code) => {
                                self.socket = None;
                                self.host.left();
                                self.driver.socket_closed(socket, code);
                            }
                        }
                    }
                    Output::Close { socket, code } => {
                        if Some(socket) == self.socket {
                            self.socket = None;
                            self.host.left();
                        }
                        self.closed.push(code);
                    }
                    Output::LoadDevice(host) => {
                        let device = self.store.get(&host).cloned();
                        self.driver.device_loaded(Ok(device));
                    }
                    Output::SaveDevice(device) => {
                        self.store.insert(device.host_id.clone(), device);
                        self.driver.device_saved(Ok(()));
                    }
                    Output::Paired { kept } => {
                        self.posted.push(boot::PAIRED);
                        self.kept.push(kept);
                    }
                    Output::Keep(sealed) => {
                        let host_id = sealed.host_id.clone();
                        if self.page_answers == Some(true) {
                            self.page.insert(host_id.clone(), sealed);
                        }
                        if let Some(ok) = self.page_answers {
                            let data = json!({ "type": boot::KEPT, "v": 1, "host_id": host_id, "ok": ok });
                            self.driver.message(PARENT, true, &data).unwrap();
                        }
                    }
                    Output::Forget(host) => {
                        self.page.remove(&host);
                    }
                    Output::Passkey(Ask::Done) => self.ask = None,
                    Output::Passkey(ask) => self.ask = Some(ask),
                    Output::ForgetDevice(host) => {
                        self.store.remove(&host);
                    }
                }
            }
        }
    }

    pub fn sent(&self) -> Vec<&Message> {
        self.host.received.iter().collect()
    }
}
