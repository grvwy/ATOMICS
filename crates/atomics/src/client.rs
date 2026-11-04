//! `atomics --connect <address>` (and `[client] connect`): the native window as a client of an
//! `atomics host` over `atomics.v1` (phase 3 in docs/daemon.md). The host owns agents, terminals, plans,
//! approvals, worktrees and git; this window owns only what a client does (selection, tabs, layout,
//! drafts). Closing it leaves every agent running on the host, and opening it again shows them as they
//! are.
//!
//! It is the browser client's own path, run natively: atomics-web's `Driver` (connection, reconnect and
//! backoff), `RemoteBackend` (the board, diffs, terminals, leases and mutations) and views (the desktop's
//! frame drawn by atomics-ui, the phone layout on a narrow window), with this module carrying out the
//! driver's outputs. Instead of the browser's broker and Noise channel, it dials the host's server
//! directly ([`link`]), with the host's bearer token: the local host's from its `protocol.json`, or a
//! paired one's ([`target`]). Instead of the page's input overlay, [`input`] routes the window's keys and
//! pointer to the terminal that has the keyboard.

pub mod input;
pub mod link;
pub mod target;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use atomics_ui::Client;
use atomics_web::driver::{Driver, Output};
use atomics_web::views::{self, Place, View};

use self::link::{Endpoint, Event, Link};
use self::target::Target;

/// What a launch asks of the window beyond showing the host: `--open AGENT:FOLDER` (started on the host
/// once its board is in, as the + New picker starts them) and `--screenshot` (a PNG after a delay, then
/// the window closes and the host goes on).
#[derive(Debug, Clone, Default)]
pub struct Launch {
    pub open: Vec<(String, std::path::PathBuf)>,
    pub screenshot: Option<(std::path::PathBuf, f32)>,
}

/// Finds the host's endpoint for one dial, on the dialing thread (it may take a moment).
pub type Resolve = Box<dyn FnOnce() -> anyhow::Result<Endpoint> + Send>;

/// The window as a client of the host `target` names.
pub struct ClientApp {
    driver: Driver,
    client: Client,
    view: View,
    target: Target,
    keyboard: input::Keyboard,
    events: Receiver<Event>,
    events_tx: SyncSender<Event>,
    link: Option<(u64, Link)>,
    wake: Arc<dyn Fn() + Send + Sync>,
    started: Instant,
    /// Why the last dial could not find the host, for the status line's hover and the log.
    unreachable: Arc<Mutex<Option<String>>>,
    /// Agents to start once the board is in.
    open: Vec<(String, std::path::PathBuf)>,
    /// `--screenshot`: where, when (seconds after the start), and whether it was taken.
    screenshot: Option<(std::path::PathBuf, f64, Arc<AtomicBool>)>,
    shot_requested: bool,
    /// Tests reach a host of their own making.
    #[cfg(test)]
    fixed: Option<Arc<dyn Fn() -> anyhow::Result<Endpoint> + Send + Sync>>,
}

/// Frames the link may read ahead of the window. A window that stops drawing makes the link stop
/// reading, and the host then treats it as the slow client it is (a fresh snapshot, or a drop), rather
/// than this process holding an unbounded backlog.
const READ_AHEAD: usize = 512;

/// A random prefix for this window's operation IDs, so a retried operation is recognised.
fn op_prefix() -> String {
    format!("desk-{}", &uuid::Uuid::new_v4().simple().to_string()[..16])
}

impl ClientApp {
    pub fn new(ctx: &egui::Context, target: Target, icon: Option<egui::TextureHandle>, launch: Launch) -> Self {
        let (events_tx, events) = mpsc::sync_channel(READ_AHEAD);
        let repaint = ctx.clone();
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || repaint.request_repaint());
        let mut client = Client::default();
        let mut view = View::default();
        // The configured theme, as the desktop would draw it; the window's state stays its own.
        let config = atomics_core::config::Config::load(&atomics_core::paths::config_file()).unwrap_or_default();
        let resolved = atomics_core::theme::load(
            &config.theme,
            &atomics_core::theme::themes_dir(&atomics_core::paths::config_file()),
        );
        view.looks = views::Looks::new(&resolved.tokens);
        client.palette = view.looks.desktop.clone();
        client.palette.apply(ctx, &views::BundledFonts);
        client.icon = icon;
        view.place = Some(Place {
            text: "Agents on the host".into(),
            hover: format!(
                "This window shows an ATOMICS host ({}): its agents run there, and closing the window leaves them \
                 running.",
                match &target {
                    Target::Address { addr } => format!("ws://{addr}"),
                    _ => "atomics host on this machine".into(),
                }
            ),
            // That host owns this machine's ATOMICS home, so this window cannot become the host too.
            action: None,
        });
        Self {
            open: launch.open,
            screenshot: launch.screenshot.map(|(path, delay)| (path, f64::from(delay), Arc::default())),
            shot_requested: false,
            driver: Driver::direct(&op_prefix()),
            client,
            view,
            target,
            keyboard: input::Keyboard::default(),
            events,
            events_tx,
            link: None,
            wake,
            started: Instant::now(),
            unreachable: Arc::default(),
            #[cfg(test)]
            fixed: None,
        }
    }

    /// A client of whatever `resolve` finds, for tests.
    #[cfg(test)]
    pub(crate) fn fixed(
        ctx: &egui::Context,
        resolve: impl Fn() -> anyhow::Result<Endpoint> + Send + Sync + 'static,
    ) -> Self {
        let mut app = Self::new(ctx, Target::Local, None, Launch::default());
        app.fixed = Some(Arc::new(resolve));
        // The first dial went out before the override: dial again with it.
        app.link = None;
        app.driver = Driver::direct(&op_prefix());
        app
    }

    #[cfg(test)]
    pub(crate) fn driver(&self) -> &Driver {
        &self.driver
    }

    #[cfg(test)]
    pub(crate) fn driver_mut(&mut self) -> &mut Driver {
        &mut self.driver
    }

    /// The client's own work for one turn, with or without a frame: the clock, what the link reported,
    /// and what the driver asked for since.
    pub(crate) fn pump(&mut self) {
        let now = self.now_ms();
        self.driver.tick(now);
        self.take_events();
        self.start_agents();
        self.run_outputs();
        self.view.now_ms = now as f64;
    }

    /// `--open`: once the host's board is in, each agent is started there, as the + New picker does.
    fn start_agents(&mut self) {
        let remote = &self.driver.remote;
        if self.open.is_empty() || !remote.live() || remote.with_mirror(|m| m.epoch.is_none()) {
            return;
        }
        for (agent, folder) in std::mem::take(&mut self.open) {
            atomics_ui::Backend::send(remote, atomics_ui::Command::Start { agent, folder: Some(folder) });
        }
        self.driver.flush();
    }

    /// `--screenshot`: the window as it is after the delay, then it closes.
    fn screenshot(&mut self, ctx: &egui::Context) {
        let Some((path, at, done)) = &self.screenshot else { return };
        if done.load(Ordering::Acquire) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if !self.shot_requested && self.started.elapsed().as_secs_f64() >= *at {
            self.shot_requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaintWhileHidden);
            let (path, done, repaint) = (path.clone(), done.clone(), ctx.clone());
            ctx.request_screenshot(move |image| {
                if let Err(error) = save_png(&path, &image) {
                    eprintln!("atomics: the screenshot failed: {error:#}");
                }
                done.store(true, Ordering::Release);
                repaint.request_repaint();
            });
        }
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    /// One frame of the window, after the terminal took its share of the input.
    pub(crate) fn draw(&mut self, ui: &mut egui::Ui) {
        views::draw(ui, &mut self.driver, &mut self.client, &mut self.view);
        self.run_outputs();
        self.keyboard.follow(&mut self.view.term.overlay, &mut self.view.term.inputs);
        self.view.place_clicked = false;
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// How the next dial finds the host.
    fn resolver(&self) -> Resolve {
        #[cfg(test)]
        if let Some(fixed) = self.fixed.clone() {
            return Box::new(move || fixed());
        }
        match self.target.clone() {
            Target::Local => Box::new(target::local),
            Target::Address { addr } => Box::new(move || target::address(addr)),
            Target::Vm => Box::new(|| anyhow::bail!("this build cannot reach the VM's host")),
        }
    }

    fn run_outputs(&mut self) {
        loop {
            let outputs = self.driver.take_outputs();
            if outputs.is_empty() {
                return;
            }
            for output in outputs {
                match output {
                    Output::DialDirect { socket } => {
                        let resolve = self.resolver();
                        let unreachable = self.unreachable.clone();
                        let resolve: Resolve = Box::new(move || {
                            let found = resolve();
                            *unreachable.lock().unwrap() = found.as_ref().err().map(|e| format!("{e:#}"));
                            found
                        });
                        let link = Link::open(socket, resolve, self.events_tx.clone(), self.wake.clone());
                        self.link = Some((socket, link));
                    }
                    Output::Send { socket, message } => {
                        if let Some((_, link)) = self.link.as_ref().filter(|(id, _)| *id == socket) {
                            link.send(String::from_utf8_lossy(&message).into_owned());
                        }
                    }
                    Output::Close { socket, .. } => {
                        if let Some((_, link)) = self.link.take_if(|(id, _)| *id == socket) {
                            link.close();
                        }
                    }
                    // A direct client has no hosting page and no stored device.
                    Output::Dial { socket, .. } => self.driver.socket_closed(socket, 1006),
                    Output::Post(_) | Output::LoadDevice(_) | Output::SaveDevice(_) | Output::ForgetDevice(_) => {}
                }
            }
        }
    }

    /// What the link reported since the last frame, in order.
    fn take_events(&mut self) {
        for event in self.events.try_iter().collect::<Vec<_>>() {
            match event {
                Event::Open { socket, protocol } => self.driver.socket_open(socket, &protocol),
                Event::Data { socket, bytes } => self.driver.socket_message(socket, &bytes),
                Event::Closed { socket, code } => {
                    if self.link.as_ref().is_some_and(|(id, _)| *id == socket) {
                        self.link = None;
                    }
                    self.driver.socket_closed(socket, code);
                }
            }
        }
    }
}

impl eframe::App for ClientApp {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        let text_box = ctx.memory(|m| m.focused().is_some());
        let overlay = self.view.term.overlay.clone();
        let now = self.now_ms() as f64;
        let mut copy = |text: String| ctx.copy_text(text);
        self.keyboard.route(raw, &overlay, text_box, now, &mut self.view.term.inputs, &mut copy);
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump();
        self.screenshot(ctx);
        if let Some(wait) = self.driver.wake_in() {
            ctx.request_repaint_after(Duration::from_millis(wait.max(16)));
        }
        if let Some(due) = self.view.term.due_ms() {
            ctx.request_repaint_after(Duration::from_millis((due - self.view.now_ms).max(16.0) as u64));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }
}

fn save_png(path: &std::path::Path, image: &egui::ColorImage) -> anyhow::Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut encoder = png::Encoder::new(file, image.size[0] as u32, image.size[1] as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let bytes: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
    encoder.write_header()?.write_image_data(&bytes)?;
    Ok(())
}

/// Run the window as a client of `target` until it is closed. The host and its agents go on.
pub fn run(target: Target, launch: Launch, native: eframe::NativeOptions) -> eframe::Result {
    let icon = native.viewport.icon.clone();
    eframe::run_native(
        "ATOMICS",
        native,
        Box::new(move |cc| {
            let texture = icon.map(|icon| {
                let image =
                    egui::ColorImage::from_rgba_unmultiplied([icon.width as usize, icon.height as usize], &icon.rgba);
                cc.egui_ctx.load_texture("atomics-icon", image, egui::TextureOptions::LINEAR)
            });
            Ok(Box::new(ClientApp::new(&cc.egui_ctx, target, texture, launch)))
        }),
    )
}

#[cfg(test)]
mod tests;
