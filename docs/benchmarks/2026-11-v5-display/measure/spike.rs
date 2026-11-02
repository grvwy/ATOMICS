//! Frame and echo measurement for the VM display check (scratch copy only, never committed).
//! ATOMICS_SPIKE_STATS=<jsonl> enables it; ATOMICS_SPIKE_SCROLL=<fx,fy> scrolls 30 points per frame
//! at that fraction of the window (reversing every 240 frames); ATOMICS_SPIKE_REPAINT repaints every
//! frame; ATOMICS_SPIKE_PROBE=<agent> types a marker into that agent's tile every 500 ms and times
//! until its screen shows the echo.
use std::io::Write;
use std::time::{Duration, Instant};

pub struct Spike {
    out: std::fs::File,
    start: Instant,
    window_start: Instant,
    last: Option<Instant>,
    intervals: Vec<f32>,
    cpu: Vec<f32>,
    frames: u32,
    repaint: bool,
    scroll_at: Option<(f32, f32)>,
    scroll_frame: u64,
    pub probe_agent: Option<String>,
    probe_seq: u32,
    probe_pending: Option<(String, Instant)>,
    probe_next: Instant,
    probe_window: Vec<f32>,
    probe_on_line: u32,
    /// ATOMICS_SPIKE_TYPE_AT=<fx,fy>: click there once, then type the markers as keyboard input.
    type_at: Option<(f32, f32)>,
    clicked: bool,
    queued: Option<String>,
    enter: bool,
}

fn quantile(sorted: &[f32], p: f32) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f32 * p).round() as usize]
}

impl Spike {
    pub fn from_env(cc: &eframe::CreationContext<'_>) -> Option<Self> {
        let path = std::env::var_os("ATOMICS_SPIKE_STATS")?;
        let mut out = std::fs::OpenOptions::new().create(true).append(true).open(path).ok()?;
        if let Some(render) = cc.wgpu_render_state.as_ref() {
            let info = render.adapter.get_info();
            let available: Vec<String> = render
                .available_adapters
                .iter()
                .map(|adapter| {
                    let info = adapter.get_info();
                    format!("{} ({:?}, {:?})", info.name, info.backend, info.device_type)
                })
                .collect();
            let line = serde_json::json!({
                "adapter": info.name,
                "backend": format!("{:?}", info.backend),
                "device_type": format!("{:?}", info.device_type),
                "driver": info.driver,
                "driver_info": info.driver_info,
                "available": available,
                "present_mode": if std::env::var_os("ATOMICS_SPIKE_NOVSYNC").is_some() { "AutoNoVsync" } else { "AutoVsync" },
                "target_format": format!("{:?}", render.target_format),
            });
            let _ = writeln!(out, "{line}");
        }
        let scroll_at = std::env::var("ATOMICS_SPIKE_SCROLL").ok().and_then(|value| {
            let (x, y) = value.split_once(',')?;
            Some((x.parse().ok()?, y.parse().ok()?))
        });
        let now = Instant::now();
        Some(Self {
            out,
            start: now,
            window_start: now,
            last: None,
            intervals: Vec::new(),
            cpu: Vec::new(),
            frames: 0,
            repaint: std::env::var_os("ATOMICS_SPIKE_REPAINT").is_some() || scroll_at.is_some(),
            scroll_at,
            scroll_frame: 0,
            probe_agent: std::env::var("ATOMICS_SPIKE_PROBE").ok().filter(|name| !name.is_empty()),
            probe_seq: 0,
            probe_pending: None,
            probe_next: now + Duration::from_secs(8),
            probe_window: Vec::new(),
            probe_on_line: 0,
            type_at: std::env::var("ATOMICS_SPIKE_TYPE_AT").ok().and_then(|value| {
                let (x, y) = value.split_once(',')?;
                Some((x.parse().ok()?, y.parse().ok()?))
            }),
            clicked: false,
            queued: None,
            enter: false,
        })
    }

    pub fn raw_input(&mut self, raw: &mut egui::RawInput) {
        if let (Some((fx, fy)), Some(rect)) = (self.type_at, raw.screen_rect) {
            // Someone typing into ATOMICS has its window focused; another app may be in front here.
            raw.focused = true;
            let pos = egui::pos2(rect.min.x + rect.width() * fx, rect.min.y + rect.height() * fy);
            if !self.clicked && Instant::now() >= self.probe_next - Duration::from_secs(2) {
                self.clicked = true;
                raw.events.push(egui::Event::PointerMoved(pos));
                for pressed in [true, false] {
                    raw.events.push(egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    });
                }
            }
            if std::mem::take(&mut self.enter) {
                raw.events.push(egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                });
            }
            if let Some(marker) = self.queued.take() {
                raw.events.push(egui::Event::Text(marker.clone()));
                self.probe_pending = Some((marker, Instant::now()));
            }
        }
        let Some((fx, fy)) = self.scroll_at else { return };
        let Some(rect) = raw.screen_rect else { return };
        let pos = egui::pos2(rect.min.x + rect.width() * fx, rect.min.y + rect.height() * fy);
        self.scroll_frame += 1;
        let direction = if (self.scroll_frame / 240) % 2 == 0 { -1.0 } else { 1.0 };
        raw.events.push(egui::Event::PointerMoved(pos));
        raw.events.push(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, 30.0 * direction),
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Wheel,
            modifiers: egui::Modifiers::NONE,
        });
    }

    pub fn frame(&mut self, ctx: &egui::Context, frame: &eframe::Frame, terminal: Option<&crate::terminal::Terminal>) {
        let now = Instant::now();
        if let Some(last) = self.last {
            self.intervals.push((now - last).as_secs_f32() * 1000.0);
        }
        self.last = Some(now);
        self.frames += 1;
        if let Some(cpu) = frame.info().cpu_usage {
            self.cpu.push(cpu * 1000.0);
        }
        if self.repaint {
            ctx.request_repaint();
        }
        if let Some(terminal) = terminal {
            match &self.probe_pending {
                Some((marker, sent)) => {
                    if terminal.screen_text(80).contains(marker.as_str()) {
                        self.probe_window.push((now - *sent).as_secs_f32() * 1000.0);
                        self.probe_pending = None;
                        self.probe_on_line += 1;
                        if self.probe_on_line >= 8 {
                            if self.type_at.is_some() {
                                self.enter = true;
                            } else {
                                terminal.record_input(Instant::now(), true);
                                terminal.write(&b"\r"[..]);
                            }
                            self.probe_on_line = 0;
                        }
                    } else if now - *sent > Duration::from_secs(5) {
                        self.probe_pending = None;
                    }
                }
                None if now >= self.probe_next && self.queued.is_none() => {
                    self.probe_seq += 1;
                    let marker = format!("q{:05}", self.probe_seq);
                    if self.type_at.is_some() {
                        // Typed on the next frame, through egui's input like a real keystroke.
                        self.queued = Some(marker);
                        ctx.request_repaint();
                    } else {
                        terminal.record_input(now, false);
                        terminal.write(marker.clone().into_bytes());
                        self.probe_pending = Some((marker, now));
                    }
                    self.probe_next = now + Duration::from_millis(500);
                }
                None => {}
            }
            ctx.request_repaint_after(self.probe_next.saturating_duration_since(now).max(Duration::from_millis(1)));
        }
        let elapsed = now - self.window_start;
        if elapsed >= Duration::from_secs(5) {
            let mut intervals = std::mem::take(&mut self.intervals);
            intervals.sort_by(f32::total_cmp);
            let mut cpu = std::mem::take(&mut self.cpu);
            cpu.sort_by(f32::total_cmp);
            let mut probes = std::mem::take(&mut self.probe_window);
            let probe_all = probes.clone();
            probes.sort_by(f32::total_cmp);
            let ppp = ctx.pixels_per_point();
            let rect = ctx.content_rect();
            let line = serde_json::json!({
                "t": (now - self.start).as_secs_f32(),
                "frames": self.frames,
                "fps": self.frames as f32 / elapsed.as_secs_f32(),
                "p50_ms": quantile(&intervals, 0.5),
                "p95_ms": quantile(&intervals, 0.95),
                "p99_ms": quantile(&intervals, 0.99),
                "max_ms": intervals.last().copied().unwrap_or(0.0),
                "over_20ms": intervals.iter().filter(|v| **v > 20.0).count(),
                "over_34ms": intervals.iter().filter(|v| **v > 34.0).count(),
                "cpu_p50_ms": quantile(&cpu, 0.5),
                "cpu_p95_ms": quantile(&cpu, 0.95),
                "ppp": ppp,
                "px": [rect.width() * ppp, rect.height() * ppp],
                "probe_n": probes.len(),
                "probe_p50_ms": quantile(&probes, 0.5),
                "probe_p95_ms": quantile(&probes, 0.95),
                "probe_max_ms": probes.last().copied().unwrap_or(0.0),
                "probe_all_ms": probe_all,
            });
            let _ = writeln!(self.out, "{line}");
            self.window_start = now;
            self.frames = 0;
        }
    }
}
