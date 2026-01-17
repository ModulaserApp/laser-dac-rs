//! Simulator settings and configuration types.

/// Render mode selection.
#[derive(Clone, Copy, PartialEq, Default)]
pub enum RenderMode {
    /// Immediate polyline rendering (stable geometry/debug view).
    #[default]
    Vector,
    /// Time-based persistence buffer rendering (realistic scan/flicker behavior).
    Beam,
}

impl RenderMode {
    pub fn label(&self) -> &'static str {
        match self {
            RenderMode::Vector => "Vector (Instant)",
            RenderMode::Beam => "Beam (Persistence)",
        }
    }
}

/// Beam mode stroke style - controls how energy is deposited into the persistence buffer.
#[derive(Clone, Copy, PartialEq, Default)]
pub enum BeamStyle {
    /// Deposit energy only at point locations. Best for debugging point density and dwell behavior.
    Dots,
    /// Draw lines between points with energy conserved per segment.
    /// Longer/faster segments appear dimmer (energy spread over more pixels).
    #[default]
    Lines,
}

impl BeamStyle {
    pub fn label(&self) -> &'static str {
        match self {
            BeamStyle::Dots => "Dots",
            BeamStyle::Lines => "Lines",
        }
    }
}

/// Color mode for beam rendering - how RGB values are interpreted.
#[derive(Clone, Copy, PartialEq, Default)]
pub enum ColorMode {
    /// Full color gradients - RGB values are used as-is (0.0-1.0 range).
    #[default]
    Analog,
    /// TTL/binary colors - each channel is either fully on or off.
    /// Any value > 0 becomes 1.0, giving 8 possible colors.
    Ttl,
}

impl ColorMode {
    pub fn label(&self) -> &'static str {
        match self {
            ColorMode::Analog => "Analog",
            ColorMode::Ttl => "TTL",
        }
    }
}

/// Shared settings between UI and server.
#[derive(Clone)]
pub struct SimulatorSettings {
    // Render mode
    pub render_mode: RenderMode,

    // Visualization (shared)
    pub point_size: f32,
    pub show_grid: bool,
    pub show_blanking: bool,
    pub invert_y: bool,
    pub color_mode: ColorMode,       // Analog vs TTL
    pub highlight_chunk_start: bool, // Draw circle at first point of each chunk

    // Time-based rendering (persistence buffer) - Beam mode
    pub beam_style: BeamStyle,
    pub persistence_ms: f32, // Persistence time constant (ms)

    // Vector mode settings
    pub vector_window_ms: u32, // Capture window for Vector mode (ms)

    // Device status flags (for scan response)
    pub status_malfunction: bool,
    pub status_offline: bool,
    pub status_excluded: bool,
    pub status_occupied: bool,

    // Connection
    pub link_timeout_ms: u32,
    pub simulated_latency_ms: u32,
    pub force_disconnect: bool,

    // ACK error injection
    pub ack_error_code: u8,
}

impl Default for SimulatorSettings {
    fn default() -> Self {
        Self {
            render_mode: RenderMode::default(),
            point_size: 2.0,
            show_grid: true,
            show_blanking: false,
            invert_y: false,
            color_mode: ColorMode::default(),
            highlight_chunk_start: false, // Off by default
            beam_style: BeamStyle::default(),
            persistence_ms: 50.0, // Default 50ms persistence
            vector_window_ms: 16, // Default 16ms (~one 60Hz frame)
            status_malfunction: false,
            status_offline: false,
            status_excluded: false,
            status_occupied: false,
            link_timeout_ms: 1000,
            simulated_latency_ms: 0,
            force_disconnect: false,
            ack_error_code: 0x00,
        }
    }
}

/// ACK error code options for the dropdown.
#[derive(Clone, Copy, PartialEq)]
pub enum AckErrorOption {
    Success,
    EmptyClose,
    SessionsOccupied,
    GroupExcluded,
    InvalidPayload,
    ProcessingError,
}

impl AckErrorOption {
    pub fn code(&self) -> u8 {
        match self {
            AckErrorOption::Success => 0x00,
            AckErrorOption::EmptyClose => 0xEB,
            AckErrorOption::SessionsOccupied => 0xEC,
            AckErrorOption::GroupExcluded => 0xED,
            AckErrorOption::InvalidPayload => 0xEE,
            AckErrorOption::ProcessingError => 0xEF,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            AckErrorOption::Success => "Success (0x00)",
            AckErrorOption::EmptyClose => "Empty close (0xEB)",
            AckErrorOption::SessionsOccupied => "Sessions occupied (0xEC)",
            AckErrorOption::GroupExcluded => "Group excluded (0xED)",
            AckErrorOption::InvalidPayload => "Invalid payload (0xEE)",
            AckErrorOption::ProcessingError => "Processing error (0xEF)",
        }
    }
}
