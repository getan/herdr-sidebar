//! Inline image preview for the file-preview pane.
//!
//! Raster images render through `ratatui-image`: kitty graphics on terminals
//! that speak the protocol (Ghostty, RootShell, kitty, WezTerm, …) and
//! halfblock characters as the universal fallback. Terminal detection runs at
//! most once per process; override it with
//! `HERDR_SIDEBAR_IMG_PREVIEW=auto|kitty|block|off` (`auto` by default).

use std::io::IsTerminal as _;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use image::DynamicImage;
use ratatui::layout::Size;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::{FilterType, Resize};

#[cfg(test)]
#[path = "imgpreview_tests.rs"]
mod tests;

/// Files larger than this are never decoded: previews are thumbnails, and
/// anything this big is not meant for the sidebar pane.
const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;

/// Extensions the bundled `image` decoders can read. Strong-magic formats
/// (PNG/JPEG/GIF/WebP) also preview without an extension; the rest need the
/// name to match so stray binaries are never probed.
const IMAGE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "tif", "tiff", "qoi", "pnm", "pbm", "pgm",
    "ppm", "dds", "tga", "exr", "hdr", "ff", "farbfeld",
];

/// How image files render in the preview pane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImgMode {
    /// Probe the terminal once: kitty graphics where supported, halfblocks
    /// otherwise, text placeholder when probing fails.
    #[default]
    Auto,
    /// Force kitty graphics (for SSH chains that hide terminal identity).
    Kitty,
    /// Force halfblock characters (renders on any terminal, lower fidelity).
    Block,
    /// No image rendering; binary files keep the text placeholder.
    Off,
}

/// Map an override value to a mode. Unset and unrecognized values are `Auto`
/// so a typo degrades to detection instead of silently disabling previews.
pub fn parse_mode(value: Option<&str>) -> ImgMode {
    match value {
        Some("kitty") => ImgMode::Kitty,
        Some("block") => ImgMode::Block,
        Some("off") => ImgMode::Off,
        _ => ImgMode::Auto,
    }
}

/// Active mode from `HERDR_SIDEBAR_IMG_PREVIEW`.
pub fn mode() -> ImgMode {
    parse_mode(std::env::var("HERDR_SIDEBAR_IMG_PREVIEW").ok().as_deref())
}

/// Whether the file name claims to be a decodable raster image.
pub fn is_image_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    IMAGE_EXTS
        .iter()
        .any(|ext| lower.ends_with(&format!(".{ext}")))
}

/// Whether the bytes carry a raster-image magic number strong enough to
/// preview on content alone (PNG/JPEG/GIF/WebP).
fn sniffs_strong(bytes: &[u8]) -> bool {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        return true;
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return true;
    }
    if bytes.starts_with(b"GIF8") {
        return true;
    }
    if bytes.len() >= 12 && bytes[..4] == *b"RIFF" && bytes[8..12] == *b"WEBP" {
        return true;
    }
    false
}

/// Whether the bytes carry any known raster-image magic number, including
/// short ones that still need a matching file extension to count.
pub fn sniff(bytes: &[u8]) -> bool {
    if sniffs_strong(bytes) {
        return true;
    }
    if bytes.starts_with(b"BM") {
        return true;
    }
    if bytes.starts_with(&[0, 0, 1, 0]) || bytes.starts_with(&[0, 0, 2, 0]) {
        return true;
    }
    if bytes.starts_with(&[b'I', b'I', 42, 0]) || bytes.starts_with(&[b'M', b'M', 0, 42]) {
        return true;
    }
    if bytes.starts_with(b"qoif") || bytes.starts_with(b"DDS ") {
        return true;
    }
    if bytes.starts_with(&[0x76, 0x2F, 0x31, 0x01]) {
        return true;
    }
    if bytes.starts_with(b"#?RADIANCE") || bytes.starts_with(b"#?RGBE") {
        return true;
    }
    if bytes.starts_with(b"farbfeld") {
        return true;
    }
    if bytes.len() >= 2 && bytes[0] == b'P' && matches!(bytes[1], b'1'..=b'7') {
        return true;
    }
    false
}

/// TGA has no magic number; the extension is the only signal.
fn is_tga(name: &str) -> bool {
    name.to_lowercase().ends_with(".tga")
}

/// Claimed image payload attached to a preview `Doc`. Decoding is deferred to
/// [`ImageDoc::protocol_for`] so a wrong guess here never costs more than a
/// header check.
pub struct ImageDoc {
    src: Vec<u8>,
    built_for: Option<(u16, u16)>,
    protocol: Option<Protocol>,
}

impl ImageDoc {
    /// Claim `bytes` for image preview when the mode, name, size and magic
    /// agree.
    pub fn try_new(name: &str, bytes: &[u8]) -> Option<Self> {
        Self::try_new_with(name, bytes, mode())
    }

    /// [`ImageDoc::try_new`] with an explicit mode, keeping unit tests free
    /// of process-environment mutation.
    pub fn try_new_with(name: &str, bytes: &[u8], img_mode: ImgMode) -> Option<Self> {
        if img_mode == ImgMode::Off || bytes.len() > MAX_IMAGE_BYTES {
            return None;
        }
        let named = is_image_name(name);
        if sniffs_strong(bytes) || (named && (sniff(bytes) || is_tga(name))) {
            Some(Self {
                src: bytes.to_vec(),
                built_for: None,
                protocol: None,
            })
        } else {
            None
        }
    }

    /// Render protocol for a `cols`×`rows` cell body, rebuilding when the
    /// pane was resized since the last call. `None` when decoding fails —
    /// the caller falls back to the text placeholder.
    pub fn protocol_for(&mut self, cols: u16, rows: u16) -> Option<&Protocol> {
        let guard = cached_picker(mode())?;
        self.protocol_for_with(cols, rows, &guard.1)
    }

    /// [`ImageDoc::protocol_for`] with an explicit picker. Unit tests pass
    /// [`Picker::halfblocks`], which performs no terminal I/O.
    pub fn protocol_for_with(
        &mut self,
        cols: u16,
        rows: u16,
        picker: &Picker,
    ) -> Option<&Protocol> {
        let size = (cols.max(1), rows.max(1));
        if self.built_for != Some(size) {
            let image: DynamicImage = image::load_from_memory(&self.src).ok()?;
            let area = Size {
                width: size.0,
                height: size.1,
            };
            let resize = Resize::Fit(Some(FilterType::Triangle));
            self.protocol = picker.new_protocol(image, area, resize).ok();
            self.built_for = Some(size);
        }
        self.protocol.as_ref()
    }
}

/// Process-wide picker. Terminal probing (a stdio query/response round trip)
/// happens at most once; the answer is reused for every later preview.
fn cached_picker(img_mode: ImgMode) -> Option<MutexGuard<'static, (ImgMode, Picker)>> {
    static PICKER: OnceLock<Mutex<(ImgMode, Picker)>> = OnceLock::new();
    let mut guard = PICKER
        .get_or_init(|| Mutex::new((ImgMode::Auto, Picker::halfblocks())))
        .lock()
        .ok()?;
    if guard.0 != img_mode {
        guard.1 = build_picker(img_mode)?;
        guard.0 = img_mode;
    }
    Some(guard)
}

/// Build a picker for `img_mode`. A silent terminal degrades to halfblocks
/// (no I/O) so previews never stall waiting for an answer.
fn build_picker(img_mode: ImgMode) -> Option<Picker> {
    match img_mode {
        ImgMode::Off => None,
        ImgMode::Block => Some(Picker::halfblocks()),
        ImgMode::Kitty => {
            // `from_query_stdio` still answers on silent terminals (ioctl
            // font-size fallback); only the protocol is forced here.
            let mut picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
            picker.set_protocol_type(ProtocolType::Kitty);
            Some(picker)
        }
        ImgMode::Auto => Some(Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())),
    }
}

/// Whether `protocol` paints outside the cell grid (kitty/sixel/iTerm2) and
/// therefore needs an explicit delete once it leaves the screen.
pub fn needs_clear(protocol: &Protocol) -> bool {
    !matches!(protocol, Protocol::Halfblocks(_))
}

/// Latch set after a graphics frame is drawn and consumed by
/// [`clear_graphics`]. It keeps doc switches self-healing: the next text
/// frame deletes the stale image even if its own close path was missed.
static GRAPHICS_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Remember that a graphics frame is on screen.
pub fn mark_graphics_active() {
    GRAPHICS_ACTIVE.store(true, Ordering::SeqCst);
}

/// Delete leftover terminal graphics. One-shot per activation, and skipped
/// when stdout is not a tty so unit tests never emit escape sequences.
pub fn clear_graphics() {
    if !GRAPHICS_ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    if !std::io::stdout().is_terminal() {
        return;
    }
    let _ = write!(std::io::stdout(), "\u{1b}_Ga=d\u{1b}\\");
    let _ = std::io::stdout().flush();
}
