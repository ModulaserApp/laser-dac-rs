//! Main simulator application with egui UI.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use eframe::egui;

use crate::fps_estimator::FpsEstimator;
use crate::persistence_buffer::{PersistenceBuffer, BUFFER_RESOLUTION};
use crate::protocol_handler::RenderPoint;
use crate::renderer::{self, RenderSettings};
use crate::server::ServerEvent;
use crate::settings::{AckErrorOption, BeamStyle, ColorMode, RenderMode, SimulatorSettings};
use crate::timing::{TimedPoint, TimestampUnwrapper};

/// Maximum stream time to keep in buffer (seconds).
/// Points older than this are trimmed.
const BUFFER_RETENTION_SECS: f64 = 5.0;

pub struct SimulatorApp {
    event_rx: mpsc::Receiver<ServerEvent>,
    running: Arc<AtomicBool>,
    settings: Arc<RwLock<SimulatorSettings>>,

    /// Ring buffer of timed points (recent stream history).
    point_buffer: VecDeque<TimedPoint>,
    /// Timestamp unwrapper for converting u32 timestamps to monotonic u64.
    timestamp_unwrapper: TimestampUnwrapper,

    /// Maximum stream timestamp received so far (the "leading edge" of received data).
    max_received_stream_us: u64,

    /// Current playback position in stream time (microseconds).
    /// This advances in real-time to simulate actual beam scanning.
    playback_stream_us: u64,
    /// Real time when playback started (for calculating elapsed time).
    playback_start_real: Option<Instant>,
    /// Stream timestamp when playback started.
    playback_start_stream_us: u64,

    /// Last real time that was painted (for decay calculation).
    last_paint_real: Option<Instant>,
    /// Index into point_buffer of next point to process.
    next_point_idx: usize,

    /// Persistence buffer for time-based rendering.
    persistence_buffer: PersistenceBuffer,
    /// Texture handle for displaying the persistence buffer.
    texture_handle: Option<egui::TextureHandle>,

    chunks_received: u64,
    client_address: Option<SocketAddr>,
    /// Local copy of ACK error selection for UI
    ack_error_selection: AckErrorOption,

    // Stats tracking
    /// Points received since last stats reset
    points_in_window: u64,
    /// Chunks received since last stats reset
    chunks_in_window: u64,
    /// Time of last stats reset
    stats_window_start: Instant,
    /// Calculated PPS (updated periodically)
    current_pps: f64,
    /// Calculated chunks per second
    current_cps: f64,
    /// Last chunk size received
    last_chunk_size: usize,

    /// FPS estimator for detecting frame cycle rate
    fps_estimator: FpsEstimator,
}

impl SimulatorApp {
    pub fn new(
        event_rx: mpsc::Receiver<ServerEvent>,
        running: Arc<AtomicBool>,
        settings: Arc<RwLock<SimulatorSettings>>,
    ) -> Self {
        Self {
            event_rx,
            running,
            settings,
            point_buffer: VecDeque::new(),
            timestamp_unwrapper: TimestampUnwrapper::new(),
            max_received_stream_us: 0,
            playback_stream_us: 0,
            playback_start_real: None,
            playback_start_stream_us: 0,
            last_paint_real: None,
            next_point_idx: 0,
            persistence_buffer: PersistenceBuffer::new(BUFFER_RESOLUTION, BUFFER_RESOLUTION),
            texture_handle: None,
            chunks_received: 0,
            client_address: None,
            ack_error_selection: AckErrorOption::Success,
            points_in_window: 0,
            chunks_in_window: 0,
            stats_window_start: Instant::now(),
            current_pps: 0.0,
            current_cps: 0.0,
            last_chunk_size: 0,
            fps_estimator: FpsEstimator::new(),
        }
    }
}

impl eframe::App for SimulatorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now_real = Instant::now();

        // Get playback settings
        let (
            render_mode,
            beam_style,
            color_mode,
            persistence_ms,
            invert_y,
            show_blanking,
            point_size,
            vector_window_ms,
            highlight_chunk_start,
        ) = {
            let settings = self.settings.read().unwrap();
            (
                settings.render_mode,
                settings.beam_style,
                settings.color_mode,
                settings.persistence_ms,
                settings.invert_y,
                settings.show_blanking,
                settings.point_size,
                settings.vector_window_ms,
                settings.highlight_chunk_start,
            )
        };

        // Update stats periodically (every 500ms)
        let stats_elapsed = now_real
            .duration_since(self.stats_window_start)
            .as_secs_f64();
        if stats_elapsed >= 0.5 {
            self.current_pps = self.points_in_window as f64 / stats_elapsed;
            self.current_cps = self.chunks_in_window as f64 / stats_elapsed;
            self.points_in_window = 0;
            self.chunks_in_window = 0;
            self.stats_window_start = now_real;
        }

        // Process incoming events
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                ServerEvent::Chunk(chunk) => {
                    self.chunks_received += 1;
                    self.chunks_in_window += 1;

                    // Unwrap the timestamp to monotonic u64
                    let chunk_ts_us = self.timestamp_unwrapper.unwrap(chunk.timestamp_us_u32);

                    // Calculate per-point timestamps
                    let n_points = chunk.points.len();
                    let duration_us = chunk.duration_us;

                    // Track stats
                    self.points_in_window += n_points as u64;
                    self.last_chunk_size = n_points;

                    // Feed points to FPS estimator
                    // Calculate PPS from chunk duration
                    let chunk_pps = if n_points > 0 && duration_us > 0 {
                        (n_points as f64 * 1_000_000.0 / duration_us as f64) as u32
                    } else {
                        30_000 // fallback
                    };
                    let xy_points: Vec<(f32, f32)> =
                        chunk.points.iter().map(|p| (p.x, p.y)).collect();
                    self.fps_estimator.push_points(&xy_points, chunk_pps);

                    // Calculate time delta between points
                    // If duration_us is 0 or parsing fails, assume 30k PPS (~33µs per point)
                    let dt_us = if n_points > 0 && duration_us > 0 {
                        duration_us as f64 / n_points as f64
                    } else {
                        33.0 // fallback: ~30k PPS
                    };

                    // Expand chunk into timed points and track max timestamp
                    for (i, point) in chunk.points.into_iter().enumerate() {
                        let point_ts_us = chunk_ts_us + (i as f64 * dt_us) as u64;
                        self.point_buffer.push_back(TimedPoint {
                            t_us: point_ts_us,
                            p: point,
                            is_chunk_start: i == 0, // First point of chunk
                        });
                        // Track the leading edge of received data
                        self.max_received_stream_us = self.max_received_stream_us.max(point_ts_us);
                    }
                }
                ServerEvent::ClientConnected(addr) => {
                    self.client_address = Some(addr);
                }
                ServerEvent::ClientDisconnected => {
                    self.client_address = None;
                    // Reset playback state on disconnect
                    self.point_buffer.clear();
                    self.timestamp_unwrapper.reset();
                    self.max_received_stream_us = 0;
                    self.playback_stream_us = 0;
                    self.playback_start_real = None;
                    self.playback_start_stream_us = 0;
                    self.last_paint_real = None;
                    self.next_point_idx = 0;
                    self.persistence_buffer.clear();
                    // Reset stats
                    self.current_pps = 0.0;
                    self.current_cps = 0.0;
                    self.last_chunk_size = 0;
                    self.points_in_window = 0;
                    self.chunks_in_window = 0;
                    self.stats_window_start = now_real;
                    // Reset FPS estimator
                    self.fps_estimator.reset();
                }
            }
        }

        // Initialize playback timing when first data arrives
        if self.playback_start_real.is_none() && !self.point_buffer.is_empty() {
            self.playback_start_real = Some(now_real);
            // Start playback at the first point's timestamp
            self.playback_start_stream_us = self.point_buffer.front().unwrap().t_us;
            self.playback_stream_us = self.playback_start_stream_us;
        }

        // Advance playback position in real-time
        // This makes the beam scan at the actual stream rate
        let now_stream_us = if let Some(start_real) = self.playback_start_real {
            let elapsed_us = now_real.duration_since(start_real).as_micros() as u64;
            let target_stream_us = self.playback_start_stream_us + elapsed_us;
            // Don't advance past received data (avoid rendering future points)
            self.playback_stream_us = target_stream_us.min(self.max_received_stream_us);
            self.playback_stream_us
        } else {
            0
        };

        // Trim buffer: remove points older than BUFFER_RETENTION_SECS from now_stream_us
        // Also adjust next_point_idx accordingly
        let retention_us = (BUFFER_RETENTION_SECS * 1_000_000.0) as u64;
        let cutoff_us = now_stream_us.saturating_sub(retention_us);
        let mut removed = 0;
        while let Some(front) = self.point_buffer.front() {
            if front.t_us < cutoff_us {
                self.point_buffer.pop_front();
                removed += 1;
            } else {
                break;
            }
        }
        // Adjust next_point_idx for removed points
        self.next_point_idx = self.next_point_idx.saturating_sub(removed);

        // Apply decay based on real time elapsed since last paint (Beam mode only)
        // Skip if buffer is empty (optimization for idle state)
        // Track content BEFORE decay so we update texture one final time after content fades
        let had_content_before_decay = self.persistence_buffer.has_content();
        if render_mode == RenderMode::Beam && had_content_before_decay {
            if let Some(last_real) = self.last_paint_real {
                let dt_real_ms = now_real.duration_since(last_real).as_secs_f32() * 1000.0;
                if persistence_ms > 0.0 && dt_real_ms > 0.0 {
                    let decay_factor = (-dt_real_ms / persistence_ms).exp();
                    self.persistence_buffer.apply_decay(decay_factor);
                }
            }
        }

        // Process points up to now_stream_us
        // Always advance next_point_idx to stay in sync with the stream,
        // but only deposit energy into persistence buffer in Beam mode.
        // This prevents a "flash" when switching from Vector back to Beam.
        let is_beam_mode = render_mode == RenderMode::Beam;

        // Track if we integrated any points this frame (for texture upload optimization)
        let mut did_integrate_points = false;

        // Collect chunk start positions for highlighting (normalized coords)
        let mut chunk_start_positions: Vec<(f32, f32)> = Vec::new();

        let mut prev_point: Option<&TimedPoint> = if self.next_point_idx > 0 {
            self.point_buffer.get(self.next_point_idx - 1)
        } else {
            None
        };

        while self.next_point_idx < self.point_buffer.len() {
            let tp = &self.point_buffer[self.next_point_idx];

            // Only process points up to now_stream_us
            if tp.t_us > now_stream_us {
                break;
            }

            // Only deposit energy in Beam mode
            if is_beam_mode {
                did_integrate_points = true;

                // Track chunk start positions for highlighting
                if highlight_chunk_start && tp.is_chunk_start {
                    chunk_start_positions.push((tp.p.x, tp.p.y));
                }

                // Convert to pixel coordinates
                let (px, py) = self
                    .persistence_buffer
                    .norm_to_pixel(tp.p.x, tp.p.y, invert_y);

                // Calculate time delta from previous point (for energy-conserving lines)
                let dt_us = prev_point
                    .map(|prev| (tp.t_us.saturating_sub(prev.t_us)) as f32)
                    .unwrap_or(1.0)
                    .max(1.0); // Minimum 1us to avoid division issues

                // Energy scale: proportional to dwell time
                // With energy-conserving lines, energy is spread across pixels,
                // so we need a higher base scale for visibility at typical scan rates.
                // At 30k PPS, dt_us ≈ 33us. With lines ~20 pixels long, each pixel
                // gets 33/20 ≈ 1.6us worth. We want this to be visible, so scale up.
                let energy_scale = dt_us / 10.0; // 10us of dwell = 1.0 energy unit

                // Deposit energy if point is lit
                if tp.p.intensity > 0.01 {
                    // Apply color mode
                    let (r, g, b) = match color_mode {
                        ColorMode::Analog => (tp.p.r, tp.p.g, tp.p.b),
                        ColorMode::Ttl => {
                            // TTL mode: any value > 0 becomes fully on
                            let r = if tp.p.r > 0.01 { 1.0 } else { 0.0 };
                            let g = if tp.p.g > 0.01 { 1.0 } else { 0.0 };
                            let b = if tp.p.b > 0.01 { 1.0 } else { 0.0 };
                            (r, g, b)
                        }
                    };

                    let total_r = r * tp.p.intensity * energy_scale;
                    let total_g = g * tp.p.intensity * energy_scale;
                    let total_b = b * tp.p.intensity * energy_scale;

                    match beam_style {
                        BeamStyle::Dots => {
                            // Dots mode: deposit only at point location
                            self.persistence_buffer.deposit(
                                px as usize,
                                py as usize,
                                total_r,
                                total_g,
                                total_b,
                            );
                        }
                        BeamStyle::Lines => {
                            // Lines mode: draw energy-conserving line from previous point
                            if let Some(prev) = prev_point {
                                if prev.p.intensity > 0.01 {
                                    let (prev_px, prev_py) = self
                                        .persistence_buffer
                                        .norm_to_pixel(prev.p.x, prev.p.y, invert_y);
                                    // Energy is distributed across all pixels in the line
                                    self.persistence_buffer.deposit_line(
                                        prev_px, prev_py, px, py, total_r, total_g, total_b,
                                    );
                                } else {
                                    // Previous was blanked, just deposit a dot
                                    self.persistence_buffer.deposit(
                                        px as usize,
                                        py as usize,
                                        total_r,
                                        total_g,
                                        total_b,
                                    );
                                }
                            } else {
                                // No previous point, just deposit a dot
                                self.persistence_buffer.deposit(
                                    px as usize,
                                    py as usize,
                                    total_r,
                                    total_g,
                                    total_b,
                                );
                            }
                        }
                    }
                } else if show_blanking {
                    // Blanked move - optionally show faint gray
                    let blank_energy = 0.0001 * energy_scale;
                    match beam_style {
                        BeamStyle::Dots => {
                            self.persistence_buffer.deposit(
                                px as usize,
                                py as usize,
                                blank_energy,
                                blank_energy,
                                blank_energy,
                            );
                        }
                        BeamStyle::Lines => {
                            if let Some(prev) = prev_point {
                                let (prev_px, prev_py) = self
                                    .persistence_buffer
                                    .norm_to_pixel(prev.p.x, prev.p.y, invert_y);
                                self.persistence_buffer.deposit_line(
                                    prev_px,
                                    prev_py,
                                    px,
                                    py,
                                    blank_energy,
                                    blank_energy,
                                    blank_energy,
                                );
                            }
                        }
                    }
                }
            }

            prev_point = Some(tp);
            self.next_point_idx += 1;
        }

        // Update last paint time (for decay calculation)
        self.last_paint_real = Some(now_real);

        // Build vector mode points (filtered by window)
        // Also collect chunk start positions for Vector mode
        let (vector_points, vector_chunk_starts): (Vec<RenderPoint>, Vec<(f32, f32)>) =
            if render_mode == RenderMode::Vector {
                let window_us = vector_window_ms as u64 * 1000;
                let cutoff = now_stream_us.saturating_sub(window_us);

                let mut points = Vec::new();
                let mut chunk_starts = Vec::new();

                for tp in self.point_buffer.iter().filter(|tp| tp.t_us >= cutoff) {
                    // Apply color mode transformation
                    let mut p = tp.p.clone();
                    if color_mode == ColorMode::Ttl {
                        p.r = if p.r > 0.01 { 1.0 } else { 0.0 };
                        p.g = if p.g > 0.01 { 1.0 } else { 0.0 };
                        p.b = if p.b > 0.01 { 1.0 } else { 0.0 };
                    }
                    points.push(p);

                    // Track chunk starts
                    if highlight_chunk_start && tp.is_chunk_start {
                        chunk_starts.push((tp.p.x, tp.p.y));
                    }
                }

                (points, chunk_starts)
            } else {
                (Vec::new(), Vec::new())
            };

        // Adaptive repaint rate:
        // - When actively receiving data or buffer has visible content: 60Hz (16ms)
        // - When idle: slower rate (100ms) to reduce CPU usage
        let repaint_interval = if self.client_address.is_some() || had_content_before_decay {
            std::time::Duration::from_millis(16) // ~60Hz when active
        } else {
            std::time::Duration::from_millis(100) // 10Hz when idle
        };
        ctx.request_repaint_after(repaint_interval);

        // Get show_grid setting for overlay
        let show_grid = self.settings.read().unwrap().show_grid;

        // Left side panel with controls
        egui::SidePanel::left("controls")
            .default_width(180.0)
            .show(ctx, |ui| {
                ui.heading("Render Mode");
                ui.add_space(4.0);

                {
                    let mut settings = self.settings.write().unwrap();
                    let current_mode = settings.render_mode;

                    egui::ComboBox::from_id_salt("render_mode")
                        .selected_text(current_mode.label())
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut settings.render_mode,
                                RenderMode::Vector,
                                RenderMode::Vector.label(),
                            );
                            ui.selectable_value(
                                &mut settings.render_mode,
                                RenderMode::Beam,
                                RenderMode::Beam.label(),
                            );
                        });
                }

                ui.add_space(8.0);
                ui.separator();

                {
                    let mut settings = self.settings.write().unwrap();
                    let current_mode = settings.render_mode;

                    match current_mode {
                        RenderMode::Vector => {
                            ui.heading("Vector Settings");
                            ui.add_space(4.0);

                            ui.horizontal(|ui| {
                                ui.label("Window (ms):");
                                ui.add(
                                    egui::DragValue::new(&mut settings.vector_window_ms)
                                        .speed(1)
                                        .range(1..=100),
                                );
                            })
                            .response
                            .on_hover_text("Capture window - how many ms of points to display");

                            ui.horizontal(|ui| {
                                ui.label("Line width:");
                                ui.add(
                                    egui::DragValue::new(&mut settings.point_size)
                                        .speed(0.1)
                                        .range(0.5..=10.0),
                                );
                            });
                        }
                        RenderMode::Beam => {
                            ui.heading("Beam Settings");
                            ui.add_space(4.0);

                            ui.horizontal(|ui| {
                                ui.label("Style:");
                                egui::ComboBox::from_id_salt("beam_style")
                                    .selected_text(settings.beam_style.label())
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(
                                            &mut settings.beam_style,
                                            BeamStyle::Lines,
                                            BeamStyle::Lines.label(),
                                        );
                                        ui.selectable_value(
                                            &mut settings.beam_style,
                                            BeamStyle::Dots,
                                            BeamStyle::Dots.label(),
                                        );
                                    });
                            })
                            .response
                            .on_hover_text(
                                "Dots: debug point density. Lines: realistic brightness.",
                            );

                            ui.horizontal(|ui| {
                                ui.label("Persistence (ms):");
                                ui.add(
                                    egui::DragValue::new(&mut settings.persistence_ms)
                                        .speed(1.0)
                                        .range(1.0..=500.0),
                                );
                            })
                            .response
                            .on_hover_text("Persistence time constant - lower = faster decay");
                        }
                    }

                    ui.add_space(8.0);
                    ui.separator();
                    ui.heading("Display");
                    ui.add_space(4.0);

                    ui.horizontal(|ui| {
                        ui.label("Color:");
                        egui::ComboBox::from_id_salt("color_mode")
                            .selected_text(settings.color_mode.label())
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut settings.color_mode,
                                    ColorMode::Analog,
                                    ColorMode::Analog.label(),
                                );
                                ui.selectable_value(
                                    &mut settings.color_mode,
                                    ColorMode::Ttl,
                                    ColorMode::Ttl.label(),
                                );
                            });
                    })
                    .response
                    .on_hover_text("Analog: full gradients. TTL: binary on/off (8 colors max).");

                    ui.checkbox(&mut settings.highlight_chunk_start, "Highlight chunk start")
                        .on_hover_text("Draw white circle at first point of each chunk");
                    ui.checkbox(&mut settings.show_grid, "Show grid");
                    ui.checkbox(&mut settings.show_blanking, "Show blanking");
                    ui.checkbox(&mut settings.invert_y, "Invert Y axis");
                }

                ui.add_space(8.0);
                ui.separator();
                ui.heading("Device Status");
                ui.add_space(4.0);

                {
                    let mut settings = self.settings.write().unwrap();

                    ui.checkbox(&mut settings.status_malfunction, "Malfunction")
                        .on_hover_text("Sets malfunction flag in scan response");
                    ui.checkbox(&mut settings.status_offline, "Offline")
                        .on_hover_text("Device becomes invisible to discovery");
                    ui.checkbox(&mut settings.status_excluded, "Excluded")
                        .on_hover_text("Rejects all real-time messages");
                    ui.checkbox(&mut settings.status_occupied, "Occupied")
                        .on_hover_text("Rejects new clients while one is connected");
                }

                ui.add_space(8.0);
                ui.separator();
                ui.heading("Connection");
                ui.add_space(4.0);

                {
                    let mut settings = self.settings.write().unwrap();

                    ui.horizontal(|ui| {
                        ui.label("Timeout (ms):");
                        ui.add(
                            egui::DragValue::new(&mut settings.link_timeout_ms)
                                .speed(10)
                                .range(100..=5000),
                        );
                    });

                    ui.horizontal(|ui| {
                        ui.label("Latency (ms):");
                        ui.add(
                            egui::DragValue::new(&mut settings.simulated_latency_ms)
                                .speed(1)
                                .range(0..=200),
                        );
                    });

                    if ui.button("Disconnect client").clicked() {
                        settings.force_disconnect = true;
                    }
                }

                ui.add_space(8.0);
                ui.separator();
                ui.heading("ACK Injection");
                ui.add_space(4.0);

                egui::ComboBox::from_label("")
                    .selected_text(self.ack_error_selection.label())
                    .show_ui(ui, |ui| {
                        let options = [
                            AckErrorOption::Success,
                            AckErrorOption::EmptyClose,
                            AckErrorOption::SessionsOccupied,
                            AckErrorOption::GroupExcluded,
                            AckErrorOption::InvalidPayload,
                            AckErrorOption::ProcessingError,
                        ];
                        for option in options {
                            if ui
                                .selectable_value(
                                    &mut self.ack_error_selection,
                                    option,
                                    option.label(),
                                )
                                .clicked()
                            {
                                let mut settings = self.settings.write().unwrap();
                                settings.ack_error_code = option.code();
                            }
                        }
                    });
            });

        // Stats panel at bottom
        egui::TopBottomPanel::bottom("stats").show(ctx, |ui| {
            ui.horizontal(|ui| {
                // Format PPS with k suffix for thousands
                let pps_str = if self.current_pps >= 1000.0 {
                    format!("{:.1}k", self.current_pps / 1000.0)
                } else {
                    format!("{:.0}", self.current_pps)
                };
                ui.label(format!("PPS: {}", pps_str));
                ui.separator();
                ui.label(format!("CPS: {:.0}", self.current_cps));
                ui.separator();
                // Estimated FPS from FFT analysis
                let fps_str = match self.fps_estimator.get_fps() {
                    Some(fps) => format!("{:.1}", fps),
                    None => "--".to_string(),
                };
                ui.label(format!("FPS: {}", fps_str))
                    .on_hover_text("Estimated frame rate (cycle detection via FFT)");
                ui.separator();
                ui.label(format!("Chunk: {} pts", self.last_chunk_size));
                ui.separator();
                ui.label(format!("Buffer: {}", self.point_buffer.len()));
                if let Some(addr) = &self.client_address {
                    ui.separator();
                    ui.label(format!("Client: {}", addr));
                }
            });
        });

        // Main canvas - render based on mode
        egui::CentralPanel::default()
            .frame(egui::Frame::none())
            .show(ctx, |ui| {
                let canvas_rect = match render_mode {
                    RenderMode::Vector => {
                        // Vector mode: use the polyline renderer
                        let render_settings = RenderSettings {
                            point_size,
                            show_blanking,
                            invert_y,
                            show_grid,
                        };
                        let laser_rect =
                            renderer::render_laser_canvas(ui, &vector_points, &render_settings);

                        // Draw chunk start markers as white circles
                        if highlight_chunk_start {
                            let center = laser_rect.center();
                            let scale = laser_rect.width() / 2.0;
                            let y_mult = if invert_y { 1.0 } else { -1.0 };
                            for (x, y) in &vector_chunk_starts {
                                let screen_x = center.x + x * scale;
                                let screen_y = center.y + y * y_mult * scale;
                                ui.painter().circle_stroke(
                                    egui::pos2(screen_x, screen_y),
                                    6.0,
                                    egui::Stroke::new(1.5, egui::Color32::WHITE),
                                );
                            }
                        }

                        laser_rect
                    }
                    RenderMode::Beam => {
                        let available_size = ui.available_size();
                        let (response, painter) =
                            ui.allocate_painter(available_size, egui::Sense::hover());
                        let rect = response.rect;

                        // Draw black background
                        painter.rect_filled(rect, 0.0, egui::Color32::BLACK);

                        // Calculate image rect to maintain aspect ratio and center
                        // Use 0.9 scale factor (0.45 * 2) to match Vector mode padding
                        let scale = rect.width().min(rect.height()) * 0.45;
                        let img_size = scale * 2.0;

                        let img_rect = egui::Rect::from_center_size(
                            rect.center(),
                            egui::Vec2::splat(img_size),
                        );

                        // Only rebuild texture if something changed (new points or visible decay)
                        // This avoids expensive to_color_image() and texture upload when idle
                        // Use had_content_before_decay to ensure we show the final fade-to-black frame
                        let needs_texture_update = did_integrate_points || had_content_before_decay;

                        if needs_texture_update {
                            let color_image = self.persistence_buffer.to_color_image();
                            if let Some(texture) = &mut self.texture_handle {
                                texture.set(color_image, egui::TextureOptions::LINEAR);
                            } else {
                                // First-time texture creation (no clone needed)
                                self.texture_handle = Some(ctx.load_texture(
                                    "persistence",
                                    color_image,
                                    egui::TextureOptions::LINEAR,
                                ));
                            }
                        }

                        // Draw the persistence buffer image if texture exists
                        if let Some(texture) = &self.texture_handle {
                            painter.image(
                                texture.id(),
                                img_rect,
                                egui::Rect::from_min_max(
                                    egui::pos2(0.0, 0.0),
                                    egui::pos2(1.0, 1.0),
                                ),
                                egui::Color32::WHITE,
                            );
                        }

                        // Draw grid overlay on top in Beam mode (image has opaque black background)
                        if show_grid {
                            renderer::draw_grid_overlay(&painter, img_rect);
                        }

                        // Draw chunk start markers as white circles
                        if highlight_chunk_start {
                            let y_mult = if invert_y { 1.0 } else { -1.0 };
                            for (x, y) in &chunk_start_positions {
                                let screen_x = img_rect.center().x + x * scale;
                                let screen_y = img_rect.center().y + y * y_mult * scale;
                                painter.circle_stroke(
                                    egui::pos2(screen_x, screen_y),
                                    6.0,
                                    egui::Stroke::new(1.5, egui::Color32::WHITE),
                                );
                            }
                        }

                        img_rect
                    }
                };

                // canvas_rect is used for future overlays if needed
                let _ = canvas_rect;
            });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.running.store(false, Ordering::SeqCst);
    }
}

/// Test mode app - renders hardcoded test points.
pub struct TestApp {
    test_points: Vec<RenderPoint>,
}

impl TestApp {
    pub fn new(test_points: Vec<RenderPoint>) -> Self {
        Self { test_points }
    }
}

impl eframe::App for TestApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::bottom("stats").show(ctx, |ui| {
            ui.label("TEST MODE - Showing hardcoded test lines");
        });

        egui::CentralPanel::default()
            .frame(egui::Frame::none())
            .show(ctx, |ui| {
                renderer::render_laser_canvas(ui, &self.test_points, &RenderSettings::default());
            });
    }
}
