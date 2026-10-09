//! The egui front end: a point view plus profile, fault and status panels.

use std::time::Duration;

use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use laser_dac::protocols::ether_dream::sim::Faults;
use laser_dac::protocols::ether_dream::{DacPoint, DacStatus, FirmwareProfile};

use crate::sim::{SimHandle, SimOptions};

/// Opcodes a client sends, offered as "never answer this" switches.
const SWALLOWABLE: &[(u8, &str)] = &[
    (b'?', "ping ?"),
    (b'p', "prepare p"),
    (b'b', "begin b"),
    (b'd', "data d"),
    (b'q', "rate q"),
    (b'u', "update u"),
    (b's', "stop s"),
    (b'c', "clear c"),
    (b'v', "version v"),
];

/// Editable fault knobs, mapped to [`Faults`] on every change.
#[derive(Clone, Default, PartialEq)]
struct FaultKnobs {
    reply_delay_ms: u32,
    duplicate_replies: bool,
    drop_enabled: bool,
    drop_after: usize,
    truncate_enabled: bool,
    truncate_at: usize,
    swallow: Vec<u8>,
}

impl FaultKnobs {
    fn to_faults(&self) -> Faults {
        let mut f = Faults::default();
        f.reply_delay = Duration::from_millis(self.reply_delay_ms.into());
        f.duplicate_replies = self.duplicate_replies;
        f.drop_after_replies = self.drop_enabled.then_some(self.drop_after);
        f.truncate_reply = self.truncate_enabled.then_some(self.truncate_at);
        f.swallow_opcodes = self.swallow.clone();
        f
    }
}

pub struct SimulatorApp {
    sim: Option<SimHandle>,
    options: SimOptions,
    profiles: Vec<FirmwareProfile>,
    profile_index: usize,
    start_error: Option<String>,
    knobs: FaultKnobs,
    interlock_ok: bool,
    persistence_ms: u32,
    show_blanked: bool,
}

impl SimulatorApp {
    pub fn new(
        sim: SimHandle,
        options: SimOptions,
        profiles: Vec<FirmwareProfile>,
        profile_index: usize,
    ) -> Self {
        Self {
            sim: Some(sim),
            options,
            profiles,
            profile_index,
            start_error: None,
            knobs: FaultKnobs::default(),
            interlock_ok: true,
            persistence_ms: 40,
            show_blanked: false,
        }
    }

    /// Restart the server with another profile. Connected clients see the
    /// connection drop, as they would on a power cycle.
    fn switch_profile(&mut self, index: usize) {
        self.profile_index = index;
        // Release the ports before binding them again.
        self.sim = None;
        match SimHandle::start(self.profiles[index].clone(), self.options.clone()) {
            Ok(sim) => {
                sim.server().set_faults(self.knobs.to_faults());
                sim.server()
                    .with_model(|m| m.set_interlock(self.interlock_ok));
                self.sim = Some(sim);
                self.start_error = None;
            }
            Err(e) => self.start_error = Some(format!("restart failed: {e}")),
        }
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        ui.heading("Firmware");
        let mut selected = self.profile_index;
        egui::ComboBox::from_id_salt("profile")
            .selected_text(self.profiles[selected].name)
            .show_ui(ui, |ui| {
                for (i, p) in self.profiles.iter().enumerate() {
                    ui.selectable_value(&mut selected, i, p.name);
                }
            });
        if selected != self.profile_index {
            self.switch_profile(selected);
        }
        let p = &self.profiles[self.profile_index];
        ui.label(format!("{:?}", p.provenance));
        ui.label(format!(
            "capacity {} (ring {}), max {} pps",
            p.buffer_capacity, p.ring_points, p.max_point_rate
        ));
        ui.label(format!(
            "version {}",
            p.version_string.unwrap_or("(none, 'v' unsupported)")
        ));
        if let Some(err) = &self.start_error {
            ui.colored_label(Color32::LIGHT_RED, err);
        }

        ui.separator();
        ui.heading("Faults");
        let before = self.knobs.clone();
        let k = &mut self.knobs;
        ui.add(egui::Slider::new(&mut k.reply_delay_ms, 0..=500).text("reply delay ms"));
        ui.checkbox(&mut k.duplicate_replies, "duplicate every reply");
        ui.horizontal(|ui| {
            ui.checkbox(&mut k.drop_enabled, "drop connection at reply");
            ui.add_enabled(k.drop_enabled, egui::DragValue::new(&mut k.drop_after));
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut k.truncate_enabled, "truncate reply");
            ui.add_enabled(k.truncate_enabled, egui::DragValue::new(&mut k.truncate_at));
        });
        ui.label("Never answer:");
        ui.horizontal_wrapped(|ui| {
            for &(op, label) in SWALLOWABLE {
                let mut on = k.swallow.contains(&op);
                if ui.checkbox(&mut on, label).changed() {
                    k.swallow.retain(|&o| o != op);
                    if on {
                        k.swallow.push(op);
                    }
                }
            }
        });
        if ui.button("Clear faults").clicked() {
            *k = FaultKnobs::default();
        }
        if self.knobs != before {
            if let Some(sim) = &self.sim {
                sim.server().set_faults(self.knobs.to_faults());
            }
        }

        ui.separator();
        ui.heading("Safety");
        ui.horizontal(|ui| {
            if ui.button("Trigger e-stop").clicked() {
                if let Some(sim) = &self.sim {
                    let now = sim.server().now();
                    sim.server().with_model(|m| m.trigger_estop(now));
                }
            }
            if ui
                .checkbox(&mut self.interlock_ok, "interlock closed")
                .changed()
            {
                if let Some(sim) = &self.sim {
                    let ok = self.interlock_ok;
                    sim.server().with_model(|m| m.set_interlock(ok));
                }
            }
        });

        ui.separator();
        ui.heading("View");
        ui.add(egui::Slider::new(&mut self.persistence_ms, 5..=500).text("persistence ms"));
        ui.checkbox(&mut self.show_blanked, "show blanked moves");
        if ui.button("Clear view").clicked() {
            if let Some(sim) = &self.sim {
                sim.clear_history();
            }
        }

        ui.separator();
        self.status(ui);
    }

    fn status(&self, ui: &mut egui::Ui) {
        ui.heading("Status");
        let Some(sim) = &self.sim else {
            ui.label("not running");
            return;
        };
        let server = sim.server();
        let (st, accepted, played, underflows) = server.with_model(|m| {
            (
                m.status(),
                m.accepted_total(),
                m.played_total(),
                m.underflows(),
            )
        });
        egui::Grid::new("status").num_columns(2).show(ui, |ui| {
            let mut row = |k: &str, v: String| {
                ui.label(k);
                ui.monospace(v);
                ui.end_row();
            };
            row("listening", sim.addr().to_string());
            row(
                "clients",
                format!(
                    "{} open, {} total",
                    server.active_connections(),
                    server.total_connections()
                ),
            );
            row(
                "light engine",
                light_engine_name(st.light_engine_state).into(),
            );
            row("playback", playback_name(st.playback_state).into());
            row("fullness", st.buffer_fullness.to_string());
            row("point rate", st.point_rate.to_string());
            row("point count", st.point_count.to_string());
            row(
                "flags",
                format!(
                    "le {:#x} pb {:#06x} src {:#x}",
                    st.light_engine_flags, st.playback_flags, st.source_flags
                ),
            );
            row("accepted", accepted.to_string());
            row("played", played.to_string());
            row("underflows", underflows.to_string());
        });
    }

    fn view(&self, ui: &mut egui::Ui) {
        let side = ui.available_size().min_elem();
        let (resp, painter) = ui.allocate_painter(Vec2::splat(side), Sense::hover());
        let rect = resp.rect;
        painter.rect_filled(rect, 0.0, Color32::from_gray(8));
        let Some(sim) = &self.sim else { return };
        let rate = sim.server().status().point_rate.max(1000) as u64;
        let n = (rate * u64::from(self.persistence_ms) / 1000) as usize;
        let points = sim.recent(n.max(2));
        draw_points(&painter, rect, &points, self.show_blanked);
    }
}

fn to_screen(rect: Rect, p: &DacPoint) -> Pos2 {
    let nx = (f32::from(p.x) + 32768.0) / 65535.0;
    let ny = (f32::from(p.y) + 32768.0) / 65535.0;
    Pos2::new(
        rect.left() + nx * rect.width(),
        rect.bottom() - ny * rect.height(),
    )
}

fn color(p: &DacPoint) -> Option<Color32> {
    let c = |v: u16| (v >> 8) as u8;
    if p.r == 0 && p.g == 0 && p.b == 0 {
        return None;
    }
    Some(Color32::from_rgb(c(p.r), c(p.g), c(p.b)))
}

fn draw_points(painter: &egui::Painter, rect: Rect, points: &[DacPoint], show_blanked: bool) {
    for w in points.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        let (pa, pb) = (to_screen(rect, a), to_screen(rect, b));
        match color(b) {
            Some(c) => {
                painter.line_segment([pa, pb], Stroke::new(1.5, c));
            }
            None if show_blanked => {
                painter.line_segment([pa, pb], Stroke::new(0.5, Color32::from_gray(60)));
            }
            None => {}
        }
    }
}

fn light_engine_name(s: u8) -> &'static str {
    match s {
        DacStatus::LIGHT_ENGINE_READY => "ready",
        DacStatus::LIGHT_ENGINE_WARMUP => "warmup",
        DacStatus::LIGHT_ENGINE_COOLDOWN => "cooldown",
        DacStatus::LIGHT_ENGINE_EMERGENCY_STOP => "e-stop",
        _ => "unknown",
    }
}

fn playback_name(s: u8) -> &'static str {
    match s {
        DacStatus::PLAYBACK_IDLE => "idle",
        DacStatus::PLAYBACK_PREPARED => "prepared",
        DacStatus::PLAYBACK_PLAYING => "playing",
        _ => "unknown",
    }
}

impl eframe::App for SimulatorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::SidePanel::left("controls")
            .resizable(false)
            .min_width(300.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.controls(ui));
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::BLACK))
            .show(ctx, |ui| {
                ui.centered_and_justified(|ui| self.view(ui));
            });
        ctx.request_repaint_after(Duration::from_millis(16));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knobs_map_onto_faults() {
        assert_eq!(FaultKnobs::default().to_faults(), Faults::default());
        let knobs = FaultKnobs {
            reply_delay_ms: 20,
            duplicate_replies: true,
            drop_enabled: false,
            drop_after: 7,
            truncate_enabled: true,
            truncate_at: 3,
            swallow: vec![b'b'],
        };
        let f = knobs.to_faults();
        assert_eq!(f.reply_delay, Duration::from_millis(20));
        assert!(f.duplicate_replies);
        assert_eq!(f.drop_after_replies, None);
        assert_eq!(f.truncate_reply, Some(3));
        assert_eq!(f.swallow_opcodes, vec![b'b']);
    }
}
