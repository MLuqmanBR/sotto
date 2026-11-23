//! The recording overlay, drawn natively rather than with a webview.
//!
//! ## Why this is not HTML
//!
//! The overlay was originally a webview window, like the settings page. That
//! cannot work here, and the reason is measured rather than assumed: on this
//! stack **WebKitGTK does not paint at all on a `wlr-layer-shell` surface**. The
//! window maps, reports the correct size, and renders nothing.
//!
//! The evidence, in order:
//!
//! 1. An ordinary Tauri window asked for `(40, 40)` and landed at `(953, 604)` —
//!    the compositor ignores `set_position` for toplevels on Wayland.
//! 2. A standalone C program with plain GTK widgets on a layer surface drew
//!    exactly where it was anchored.
//! 3. The same C program with a `WebKitWebView` on a layer surface drew nothing,
//!    while the identical webview as an ordinary window drew fine.
//! 4. Every documented workaround (`WEBKIT_DISABLE_COMPOSITING_MODE`,
//!    `WEBKIT_DISABLE_DMABUF_RENDERER`, `LIBGL_ALWAYS_SOFTWARE`, hardware
//!    acceleration set to `NEVER`) produced the same empty result.
//!
//! Layer-shell is not optional, because it is the only mechanism Wayland gives a
//! client to choose its own position. "Bottom centre, just above the taskbar" is
//! therefore only reachable with native GTK. So the overlay is built from GTK
//! widgets, styled with CSS, with the level meter drawn in cairo.
//!
//! ## Structure
//!
//! [`Source`] is what the overlay displays: one function for the input level and
//! one for the recognised words. Putting them behind functions means the same
//! overlay serves both real recording and the settings preview, and keeps this
//! module independent of the pipeline.
//!
//! Everything here must run on the GTK main thread; the module enforces that by
//! keeping its widgets in a thread-local and refusing to touch them from
//! anywhere else.

use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_int, c_void, CString};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

// ── layer-shell bindings ────────────────────────────────────────────────────
//
// `libgtk-layer-shell` is loaded with `dlopen` rather than linked, for three
// reasons: the development headers are not installed on this machine (only the
// shared object), a missing library must degrade instead of preventing startup,
// and it is Wayland-only — on X11 the symbol is simply absent and the ordinary
// positioning path is correct.

/// `GtkLayerShellEdge` values, in the order the header defines them.
const EDGE_LEFT: c_int = 0;
const EDGE_RIGHT: c_int = 1;
const EDGE_TOP: c_int = 2;
const EDGE_BOTTOM: c_int = 3;

/// `GtkLayerShellLayer::GTK_LAYER_SHELL_LAYER_OVERLAY` — above normal windows
/// and above the panel, so the pill is never hidden behind the taskbar.
const LAYER_OVERLAY: c_int = 3;

struct Api {
    // Kept alive for the life of the process: dropping the library while a
    // window is still managed by layer-shell would break the compositor
    // connection.
    _lib: libloading::Library,
    is_supported: unsafe extern "C" fn() -> c_int,
    init_for_window: unsafe extern "C" fn(*mut c_void),
    set_namespace: unsafe extern "C" fn(*mut c_void, *const c_char),
    set_layer: unsafe extern "C" fn(*mut c_void, c_int),
    set_anchor: unsafe extern "C" fn(*mut c_void, c_int, c_int),
    set_margin: unsafe extern "C" fn(*mut c_void, c_int, c_int),
    set_keyboard_interactivity: unsafe extern "C" fn(*mut c_void, c_int),
    set_exclusive_zone: unsafe extern "C" fn(*mut c_void, c_int),
    is_layer_window: unsafe extern "C" fn(*mut c_void) -> c_int,
    // Present from v0.5; optional so an older library still works.
    set_keyboard_mode: Option<unsafe extern "C" fn(*mut c_void, c_int)>,
    /// Which output the surface belongs on. Optional for the same reason.
    ///
    /// Without it the compositor chooses, which on a multi-monitor desk can put
    /// the pill on a screen the user is not looking at.
    set_monitor: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
}

static API: OnceLock<Option<Api>> = OnceLock::new();

fn api() -> &'static Option<Api> {
    API.get_or_init(|| unsafe {
        // The versioned soname is what is installed; there is no unversioned
        // symlink without the -devel package.
        let lib = ["libgtk-layer-shell.so.0", "libgtk-layer-shell.so"]
            .iter()
            .find_map(|name| libloading::Library::new(*name).ok())?;

        // Each symbol is fetched individually, so a partial library is detected
        // instead of being called through a null pointer.
        macro_rules! sym {
            ($name:literal) => {
                *lib.get::<unsafe extern "C" fn()>($name).ok()?
            };
        }
        let is_supported = std::mem::transmute(sym!(b"gtk_layer_is_supported\0"));
        let init_for_window = std::mem::transmute(sym!(b"gtk_layer_init_for_window\0"));
        let set_namespace = std::mem::transmute(sym!(b"gtk_layer_set_namespace\0"));
        let set_layer = std::mem::transmute(sym!(b"gtk_layer_set_layer\0"));
        let set_anchor = std::mem::transmute(sym!(b"gtk_layer_set_anchor\0"));
        let set_margin = std::mem::transmute(sym!(b"gtk_layer_set_margin\0"));
        let set_keyboard_interactivity =
            std::mem::transmute(sym!(b"gtk_layer_set_keyboard_interactivity\0"));
        let set_exclusive_zone = std::mem::transmute(sym!(b"gtk_layer_set_exclusive_zone\0"));
        let is_layer_window = std::mem::transmute(sym!(b"gtk_layer_is_layer_window\0"));

        let set_keyboard_mode = lib
            .get::<unsafe extern "C" fn()>(b"gtk_layer_set_keyboard_mode\0")
            .ok()
            .map(|s| std::mem::transmute(*s));

        let set_monitor = lib
            .get::<unsafe extern "C" fn()>(b"gtk_layer_set_monitor\0")
            .ok()
            .map(|s| std::mem::transmute(*s));

        Some(Api {
            _lib: lib,
            is_supported,
            init_for_window,
            set_namespace,
            set_layer,
            set_anchor,
            set_margin,
            set_keyboard_interactivity,
            set_exclusive_zone,
            is_layer_window,
            set_keyboard_mode,
            set_monitor,
        })
    })
}

/// Whether this session can honour a requested position.
///
/// True only on a Wayland session whose compositor implements layer-shell (KDE
/// and wlroots compositors do). False under X11, where ordinary window
/// positioning works instead.
pub fn supported() -> bool {
    match api() {
        Some(a) => unsafe { (a.is_supported)() != 0 },
        None => false,
    }
}

// ── placement ───────────────────────────────────────────────────────────────

/// Where the user wants the overlay, mirroring `sotto_core::config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    TopCentre,
    BottomCentre,
}

impl Placement {
    /// Map the saved setting onto a placement.
    pub fn from_config(p: sotto_core::config::OverlayPosition) -> Self {
        use sotto_core::config::OverlayPosition as P;
        match p {
            P::TopCentre => Placement::TopCentre,
            P::BottomCentre => Placement::BottomCentre,
        }
    }
}

/// A concrete anchoring plan: which edges to pin to, and by how much.
///
/// Layer-shell has no coordinates, only edges and margins, so a position is
/// expressed as "pin to these edges, then offset by these margins". This is a
/// plain data type so the arithmetic can be tested without a display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
    pub margin_left: i32,
    pub margin_right: i32,
    pub margin_top: i32,
    pub margin_bottom: i32,
}

/// Space between the pill and the nearest screen edge or panel.
pub const GAP: i32 = 24;

/// Compute the anchoring plan for a placement.
///
/// `screen_w` and `win_w` are in logical pixels.
///
/// The margin is [`GAP`] and nothing more. It is tempting to add the taskbar
/// height here, and that would be wrong: the compositor already lays layer
/// surfaces out inside the work area, so a panel is excluded before the margin
/// is applied. Measured on this machine, a bottom margin of 24 put the pill just
/// above the taskbar with the panel's height *not* added. Adding it would lift
/// the pill by a whole panel height, floating it in the middle of the desktop.
///
/// Centring pins *both* horizontal edges with a margin of half the leftover
/// width. That matters for more than symmetry: pinning both edges makes the
/// compositor size the window to exactly `win_w`, so the pill hugs its content
/// instead of stretching to a guessed width and leaving dead space. Recomputing
/// the plan whenever the content grows is what keeps it centred.
pub fn plan(placement: Placement, screen_w: i32, win_w: i32) -> Plan {
    let centred = ((screen_w - win_w) / 2).max(0);
    let above_panel = GAP;
    match placement {
        Placement::TopCentre => Plan {
            left: true,
            right: true,
            top: true,
            bottom: false,
            margin_left: centred,
            margin_right: centred,
            margin_top: GAP,
            margin_bottom: 0,
        },
        Placement::BottomCentre => Plan {
            left: true,
            right: true,
            top: false,
            bottom: true,
            margin_left: centred,
            margin_right: centred,
            margin_top: 0,
            margin_bottom: above_panel,
        },
    }
}

/// The monitor the overlay belongs on, in logical pixels.
///
/// `x` and `y` are the monitor's **origin in the global desktop**, which is not
/// zero on a multi-monitor setup. Carrying the origin is not cosmetic: on X11 the
/// window is positioned in global coordinates, so a monitor-relative position
/// lands on the wrong screen whenever the chosen monitor is not the top-left one.
/// On a two-monitor desk with the primary on the right, a pill computed as
/// "centred on 2560 wide" would appear at global x=1280 — the middle of the *left*
/// monitor, a whole screen away from where the user is typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Monitor {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// Height of a panel (taskbar) reserving space at the bottom, if any.
    pub panel: i32,
}

/// Where to put the window when layer-shell is unavailable (X11).
///
/// Returns the top-left corner in **global** logical pixels, offset by the
/// monitor's origin. On Wayland this request is ignored by the compositor, which
/// is exactly why layer-shell exists.
pub fn fallback_origin(
    placement: Placement,
    screen: Monitor,
    win_w: i32,
    win_h: i32,
) -> (i32, i32) {
    let local_x = ((screen.w - win_w) / 2).max(0);
    let local_y = match placement {
        Placement::TopCentre => GAP,
        Placement::BottomCentre => (screen.h - win_h - screen.panel - GAP).max(GAP),
    };
    (screen.x + local_x, screen.y + local_y)
}

// ── appearance ──────────────────────────────────────────────────────────────

/// The accent colour for each palette id.
///
/// These are deliberately distinct hues rather than shades of one blue, so the
/// choice is meaningful. `None` means the user has not picked yet, and the
/// default blue is used.
pub fn accent_hex(palette: Option<&str>) -> &'static str {
    match palette {
        Some("green") => "#2FB56B",
        Some("orange") => "#F08C2E",
        Some("pink") => "#E8558F",
        Some("violet") => "#9B6BFF",
        Some("red") => "#E5533D",
        // Blue, and anything unrecognised: the palette is validated on save, so
        // an unknown value can only come from a hand-edited file.
        _ => "#4C8DFF",
    }
}

/// The pill's geometry, in logical pixels.
///
/// These are the single source of truth: they are used both to draw the CSS and
/// to size the window, so the two can never disagree. The previous version
/// guessed a window size instead, which left the content occupying about 1% of
/// the window and a large empty strip on the right.
pub const PAD_X: i32 = 14;
pub const PAD_Y: i32 = 10;
pub const BORDER: i32 = 1;
pub const SPACING: i32 = 10;
pub const METER_W: i32 = 28;
pub const METER_H: i32 = 18;
/// The tallest the words may be drawn before they are ellipsized.
pub const MAX_TEXT_W: i32 = 300;

/// How many lines of recognised words the Live pill shows.
///
/// Two is deliberate. Dictation is read while still speaking, so the newest
/// words matter most; a taller pill would cover more of the screen for text the
/// user has already seen.
pub const MAX_LINES: i32 = 2;

/// The compact pill's height. The meter is its tallest element, so this is a
/// single row.
pub const PILL_H: i32 = METER_H + 2 * (PAD_Y + BORDER);

/// The pill's height for a style, given the measured height of its text.
///
/// Live was previously a fixed [`PILL_H`], which left 18 px for text when one
/// 13 px line needs about 17 and two need about 34. A second sentence was
/// therefore clipped. The height now follows the text, capped at [`MAX_LINES`].
pub fn pill_height(live: bool, text_h: i32) -> i32 {
    let content = if live { text_h.max(METER_H) } else { METER_H };
    content + 2 * (PAD_Y + BORDER)
}

/// The pill's width for a style and word count.
///
/// "Pill" is the meter and its padding, so it is genuinely compact. "Live" adds
/// the measured width of the words, up to [`MAX_TEXT_W`], so the pill grows with
/// the text instead of reserving space it may never use.
pub fn pill_width(live: bool, text_w: i32) -> i32 {
    let base = METER_W + 2 * (PAD_X + BORDER);
    if live {
        base + SPACING + text_w.clamp(0, MAX_TEXT_W)
    } else {
        base
    }
}

/// The bar response curve, so the meter looks like a meter rather than five
/// identical sticks. Indexed left to right.
const CURVE: [f32; 5] = [0.45, 0.72, 1.0, 0.82, 0.55];
const BAR_MIN: f64 = 4.0;
const BAR_MAX: f64 = 18.0;

/// Draw a filled rounded rectangle, used for the meter bars.
fn rounded_rect(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
}

// ── what the overlay shows ──────────────────────────────────────────────────

/// The data behind the overlay.
///
/// Both fields are functions rather than values so the overlay can poll them on
/// a timer, and so a preview can substitute a synthetic sweep without any
/// recording taking place.
#[derive(Clone)]
pub struct Source {
    level: Arc<dyn Fn() -> f32 + Send + Sync>,
    words: Arc<dyn Fn() -> String + Send + Sync>,
}

impl Source {
    /// A source backed by real capture.
    pub fn live(
        level: Arc<dyn Fn() -> f32 + Send + Sync>,
        words: Arc<dyn Fn() -> String + Send + Sync>,
    ) -> Self {
        Self { level, words }
    }

    /// A source that fakes a speaking voice, for the settings preview.
    ///
    /// It uses a syllable-like envelope inside a slower phrase envelope so the
    /// meter moves the way speech does, and walks a sample sentence in word by
    /// word for the Live style. Nothing here touches the microphone.
    pub fn synthetic() -> Self {
        let start = Instant::now();
        let level = Arc::new(move || {
            let t = start.elapsed().as_secs_f32();
            // ~4 Hz syllables inside a ~0.25 Hz phrase.
            let phrase = 0.55 + 0.45 * (t * 1.6).sin();
            let syllable = 0.5 + 0.5 * (t * 26.0).sin();
            (phrase * syllable * 0.9).clamp(0.0, 1.0)
        });
        let words = Arc::new(move || {
            const PHRASE: [&str; 6] = ["and", "then", "we", "can", "send", "it"];
            let shown = (start.elapsed().as_millis() / 420) as usize + 1;
            PHRASE[..shown.min(PHRASE.len())].join(" ")
        });
        Self { level, words }
    }

    fn level_now(&self) -> f32 {
        (self.level)().clamp(0.0, 1.0)
    }

    fn words_now(&self) -> String {
        (self.words)()
    }
}

// ── the overlay itself ──────────────────────────────────────────────────────

struct Overlay {
    window: gtk::Window,
    /// The pill container. Held so `reveal` can show it explicitly; `show_all`
    /// is deliberately avoided because it re-shows hidden children.
    pill: gtk::Box,
    meter: gtk::DrawingArea,
    words: gtk::Label,
    provider: gtk::CssProvider,
    /// Whether the Live style is active. Shared rather than a plain `bool`
    /// because the redraw timer reads it on every tick: capturing it by value
    /// meant that reusing the overlay for a different style left the timer with
    /// the old style, which collapsed Live to the Pill width and clipped the
    /// words.
    live: Rc<Cell<bool>>,
    /// Smoothed level, kept between frames so the meter falls gradually.
    smooth: Rc<Cell<f32>>,
    /// The redraw timer; removed when the overlay is hidden.
    timer: RefCell<Option<gtk::glib::SourceId>>,
    /// The level and words to display.
    ///
    /// Shared with the redraw timer rather than copied into it: `Source` is
    /// `Clone`, so handing the timer a clone would freeze it on whatever was
    /// current when the timer started. That was a real bug — reusing the overlay
    /// for a different style left it showing the previous style's text forever.
    source: Rc<RefCell<Source>>,
    /// Whether layer-shell actually claimed this window.
    layered: bool,
    /// Where the overlay should sit. Kept so the window can be re-centred
    /// whenever its width changes, which it does as Live text grows.
    placement: Rc<Cell<Placement>>,
    /// The monitor the overlay is on: its origin, size and panel height.
    metrics: Rc<Cell<Monitor>>,
    /// The GDK monitor object, which is what layer-shell needs to be told which
    /// output the surface belongs on. Separate from `metrics` because that is a
    /// plain geometry type that can be tested without a display.
    gdk_monitor: Rc<RefCell<Option<gtk::gdk::Monitor>>>,
    /// The current window size, so the X11 fallback can position it.
    size: Rc<Cell<(i32, i32)>>,
}

thread_local! {
    /// The single overlay window. GTK objects are not `Send`, so they live in a
    /// thread-local and are only ever touched from the main thread.
    static OVERLAY: RefCell<Option<Overlay>> = const { RefCell::new(None) };
}

/// Show the overlay, creating it the first time.
///
/// Must be called on the GTK main thread. Calling it while the overlay is
/// already visible updates the style, colour and position in place, which is
/// what the settings preview needs when the user changes the palette.
pub fn show(
    style: sotto_core::config::OverlayStyle,
    palette: Option<&str>,
    placement: Placement,
    source: Source,
) {
    use sotto_core::config::OverlayStyle;

    // "None" means the user asked for no overlay at all.
    let live = match style {
        OverlayStyle::Live => true,
        OverlayStyle::Minimal => false,
        OverlayStyle::None => {
            hide();
            return;
        }
    };

    let accent = accent_hex(palette);

    // The monitor's work area is what is left after panels reserve space. When
    // the compositor does not report a panel the difference is zero, and the
    // gap alone positions the pill.
    let screen = monitor_metrics();

    OVERLAY.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(o) = slot.as_mut() {
            // Reuse: restyle, re-fit, reposition, show.
            o.live.set(live);
            o.set_css(accent, live);
            *o.source.borrow_mut() = source;
            o.placement.set(placement);
            o.metrics.set(screen);
            *o.gdk_monitor.borrow_mut() = overlay_monitor();
            o.fit();
            o.reveal();
            o.start_timer();
            return;
        }

        match build(live, accent, placement, screen, source) {
            Some(o) => {
                o.reveal();
                o.start_timer();
                *slot = Some(o);
            }
            None => eprintln!("overlay: could not create the window"),
        }
    });
}

/// Hide the overlay, if it is showing. Safe to call at any time.
pub fn hide() {
    OVERLAY.with(|slot| {
        if let Some(o) = slot.borrow_mut().as_mut() {
            o.stop_timer();
            o.window.hide();
        }
    });
}

/// Whether the overlay window currently exists.
pub fn exists() -> bool {
    OVERLAY.with(|slot| slot.borrow().is_some())
}

/// The monitor the overlay should appear on.
///
/// The monitor under the **pointer**, because that is where the user is looking
/// and working. A dictation app that always used the primary monitor would put
/// its pill on the wrong screen for anyone with a second display — the words
/// appear where you are typing, but the confirmation of *what it heard* would
/// appear somewhere else entirely.
///
/// Falls back to the primary monitor, then to the first one, then to a sane
/// default, so a session that reports no pointer still works.
fn overlay_monitor() -> Option<gtk::gdk::Monitor> {
    let display = gtk::gdk::Display::default()?;

    // The monitor containing the pointer, found by comparing rectangles rather
    // than by asking GDK.
    //
    // `gdk_display_get_monitor_at_point` is the obvious call and it is not
    // reliable: measured on this machine with two monitors defined over one
    // output, it returned monitor 0 for a point at x=1920 that is plainly inside
    // monitor 1 (1280..2560). Comparing the rectangles gives the right answer
    // because it depends only on the geometry GDK reports, not on how it resolves
    // the lookup.
    if let Some(seat) = display.default_seat() {
        if let Some(pointer) = seat.pointer() {
            let (_, x, y) = pointer.position();
            let mut best: Option<gtk::gdk::Monitor> = None;
            for i in 0..display.n_monitors() {
                let Some(monitor) = display.monitor(i) else {
                    continue;
                };
                let g = monitor.geometry();
                if x >= g.x() && x < g.x() + g.width() && y >= g.y() && y < g.y() + g.height() {
                    return Some(monitor);
                }
                // Keep a fallback: the pointer can be outside every monitor when
                // the layout has gaps, which a mixed-DPI desk does.
                if best.is_none() {
                    best = Some(monitor);
                }
            }
            if let Some(m) = best {
                return Some(m);
            }
        }
    }

    display.primary_monitor().or_else(|| display.monitor(0))
}

/// The overlay monitor's geometry, in logical pixels, with its global origin.
fn monitor_metrics() -> Monitor {
    let Some(monitor) = overlay_monitor() else {
        return Monitor {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
            panel: 0,
        };
    };
    let geo = monitor.geometry();
    let work = monitor.workarea();
    // A bottom panel shows up as work area height smaller than the geometry. The
    // panel belongs to the *bottom* edge of this monitor, so the offset is
    // measured from the monitor's own bottom, not from the global origin.
    let panel = ((geo.y() + geo.height()) - (work.y() + work.height())).max(0);
    Monitor {
        x: geo.x(),
        y: geo.y(),
        w: geo.width(),
        h: geo.height(),
        panel,
    }
}

fn build(
    live: bool,
    accent: &str,
    placement: Placement,
    screen: Monitor,
    source: Source,
) -> Option<Overlay> {
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("Sotto overlay");
    window.set_decorated(false);
    // Not resizable by the user, but the app sizes it to the content.
    window.set_resizable(false);
    window.set_skip_taskbar_hint(true);
    window.set_skip_pager_hint(true);
    window.set_accept_focus(false);
    window.set_focus_on_map(false);
    window.set_keep_above(true);

    // Transparency: without an RGBA visual the window has no alpha channel and
    // the area outside the rounded pill would paint opaque black.
    window.set_app_paintable(true);
    // The GDK screen (the whole desktop), which is not the monitor this overlay
    // belongs on. Named apart so the two cannot be mixed up.
    let gdk_screen = gtk::prelude::GtkWindowExt::screen(&window)?;
    if let Some(visual) = gdk_screen.rgba_visual() {
        window.set_visual(Some(&visual));
    }

    // Layer-shell has to be initialised before GTK realises the surface. The
    // window is not realised yet at this point, which is what makes it work.
    let mut layered = false;
    if supported() {
        if let Some(a) = api() {
            let ptr = window.as_ptr() as *mut c_void;
            unsafe {
                (a.init_for_window)(ptr);
                if let Ok(ns) = CString::new("sotto-overlay") {
                    (a.set_namespace)(ptr, ns.as_ptr());
                }
                (a.set_layer)(ptr, LAYER_OVERLAY);
                // Never take keyboard focus: the whole point is to keep typing
                // in whatever app the user is dictating into.
                (a.set_keyboard_interactivity)(ptr, 0);
                if let Some(mode) = a.set_keyboard_mode {
                    (mode)(ptr, 0);
                }
                // Float over whatever is there; reserve no space.
                (a.set_exclusive_zone)(ptr, 0);
                layered = (a.is_layer_window)(ptr) != 0;
            }
        }
    }

    let provider = gtk::CssProvider::new();
    gtk::StyleContext::add_provider_for_screen(
        &gdk_screen,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    // The pill: a horizontal row with the meter and, in Live style, the words.
    // No size request on the pill: it must take exactly the space its content
    // needs. A fixed width here is what previously left dead space on the right.
    let pill = gtk::Box::new(gtk::Orientation::Horizontal, SPACING);
    pill.set_widget_name("pill");

    let meter = gtk::DrawingArea::new();
    meter.set_widget_name("meter");
    meter.set_size_request(METER_W, METER_H);
    pill.pack_start(&meter, false, false, 0);

    let words = gtk::Label::new(None);
    words.set_widget_name("words");
    words.set_line_wrap(true);
    words.set_lines(2);
    words.set_ellipsize(gtk::pango::EllipsizeMode::End);
    words.set_xalign(0.0);
    words.set_visible(live);
    // The pill grows with the text, up to MAX_TEXT_W; past that it ellipsizes.
    words.set_max_width_chars(0);
    pill.pack_start(&words, false, false, 0);

    window.add(&pill);

    let smooth = Rc::new(Cell::new(0.0f32));
    {
        let smooth = smooth.clone();
        meter.connect_draw(move |area, cr| {
            draw_meter(area, cr, &smooth);
            gtk::glib::Propagation::Proceed
        });
    }

    let o = Overlay {
        window,
        pill,
        meter,
        words,
        provider,
        live: Rc::new(Cell::new(live)),
        smooth,
        timer: RefCell::new(None),
        source: Rc::new(RefCell::new(source)),
        layered,
        placement: Rc::new(Cell::new(placement)),
        metrics: Rc::new(Cell::new(screen)),
        gdk_monitor: Rc::new(RefCell::new(overlay_monitor())),
        size: Rc::new(Cell::new((0, 0))),
    };
    o.set_css(accent, live);
    o.fit();
    Some(o)
}

/// Paint the five-bar level meter.
///
/// The accent colour is read from the widget's own style, so changing the
/// palette only has to reload the CSS.
fn draw_meter(area: &gtk::DrawingArea, cr: &gtk::cairo::Context, smooth: &Rc<Cell<f32>>) {
    let w = f64::from(area.allocated_width());
    let h = f64::from(area.allocated_height());
    let bars = CURVE.len() as f64;
    let bar_w = 3.0;
    let gap = 3.0;
    let total = bars * bar_w + (bars - 1.0) * gap;
    let mut x = (w - total) / 2.0;

    // Read the accent from the widget's own style so a palette change is picked
    // up without restarting the overlay.
    let ctx = area.style_context();
    let colour = ctx.color(gtk::StateFlags::NORMAL);
    cr.set_source_rgba(
        f64::from(colour.red()),
        f64::from(colour.green()),
        f64::from(colour.blue()),
        0.9,
    );

    let level = smooth.get();
    for i in 0..CURVE.len() {
        let amp = f64::from(level * CURVE[i]);
        let bh = BAR_MIN + (BAR_MAX - BAR_MIN) * amp.min(1.0);
        let y = (h - bh) / 2.0;
        rounded_rect(cr, x, y, bar_w, bh, bar_w / 2.0);
        let _ = cr.fill();
        x += bar_w + gap;
    }
}

/// Size the window to its content and pin it in place.
///
/// A free function rather than a method because the redraw timer owns clones of
/// the widgets, not the `Overlay`, and a growing transcript has to be able to
/// re-fit without borrowing the whole struct.
fn layout_pill(
    window: &gtk::Window,
    words: &gtk::Label,
    live: bool,
    placement: Placement,
    metrics: Monitor,
    gdk_monitor: &RefCell<Option<gtk::gdk::Monitor>>,
    size: &Cell<(i32, i32)>,
    layered: bool,
) {
    // Give the label exactly the space its text needs and no more. Without the
    // width constraint the label's natural width can exceed the visible text,
    // which reserves space that is never drawn; without the height the pill
    // stays single-row and clips the second line.
    let (text_w, text_h) = if live { measured_text(words) } else { (0, 0) };
    words.set_size_request(text_w, -1);

    let w = pill_width(live, text_w);
    let h = pill_height(live, text_h);
    size.set((w, h));
    window.set_size_request(w, h);

    place_window(window, layered, placement, metrics, gdk_monitor, w, h, size);
}

/// The size the current words need, in logical pixels.
///
/// Measured with a Pango layout using the label's real font, so the pill is as
/// wide as the text actually is at this machine's DPI rather than an estimate
/// that would be wrong on a different font stack.
///
/// The width is capped at [`MAX_TEXT_W`] and the layout is asked to wrap there,
/// so a long sentence produces *two* lines rather than one line running off the
/// screen. The height is capped at [`MAX_LINES`] lines; beyond that the oldest
/// words are ellipsized.
///
/// Returning the height as well as the width matters: the pill used to be a
/// fixed single-row height, so a second line was clipped away.
fn measured_text(words: &gtk::Label) -> (i32, i32) {
    let text = words.text();
    if text.is_empty() {
        return (0, 0);
    }

    let layout = words.create_pango_layout(Some(&text));
    // Both inputs to a Pango layout are in Pango units, not pixels: the width
    // and the height must be multiplied by `SCALE`. Passing pixels directly made
    // the height cap a fraction of a line, so a long transcript stayed on one
    // line and was ellipsized instead of wrapping to two. `pixel_size()` returns
    // device pixels, which is what the rest of this module works in.
    layout.set_width(MAX_TEXT_W * gtk::pango::SCALE);
    layout.set_wrap(gtk::pango::WrapMode::WordChar);
    layout.set_ellipsize(gtk::pango::EllipsizeMode::End);
    // Capping the height is what limits the pill to MAX_LINES: Pango stops
    // laying out beyond it and ellipsizes instead of growing without bound.
    layout.set_height(line_height(words) * MAX_LINES * gtk::pango::SCALE);

    let (w, h) = layout.pixel_size();
    (w.clamp(0, MAX_TEXT_W), h.max(0))
}

/// The height of one line of the label's text, in device pixels.
///
/// Measured rather than assumed, because it depends on the font, its size and
/// the DPI. Used both to cap the layout and to size the pill.
fn line_height(words: &gtk::Label) -> i32 {
    let layout = words.create_pango_layout(Some("Ag"));
    let (_w, h) = layout.pixel_size();
    h.max(1)
}

/// Pin the window in place for a given width.
fn place_window(
    window: &gtk::Window,
    layered: bool,
    placement: Placement,
    screen: Monitor,
    gdk_monitor: &RefCell<Option<gtk::gdk::Monitor>>,
    win_w: i32,
    win_h: i32,
    size: &Cell<(i32, i32)>,
) {
    let p = plan(placement, screen.w, win_w);
    if layered {
        if let Some(a) = api() {
            let ptr = window.as_ptr() as *mut c_void;

            // Which output. Left to the compositor, a multi-monitor desk can put
            // the pill on a screen the user is not looking at; the monitor under
            // the pointer is the one they are working on.
            if let (Some(set_monitor), Ok(monitor)) = (a.set_monitor, gdk_monitor.try_borrow()) {
                if let Some(m) = monitor.as_ref() {
                    unsafe { set_monitor(ptr, m.as_ptr() as *mut c_void) };
                }
            }

            unsafe {
                for edge in [EDGE_LEFT, EDGE_RIGHT, EDGE_TOP, EDGE_BOTTOM] {
                    (a.set_anchor)(ptr, edge, 0);
                    (a.set_margin)(ptr, edge, 0);
                }
                if p.left {
                    (a.set_anchor)(ptr, EDGE_LEFT, 1);
                    (a.set_margin)(ptr, EDGE_LEFT, p.margin_left);
                }
                if p.right {
                    (a.set_anchor)(ptr, EDGE_RIGHT, 1);
                    (a.set_margin)(ptr, EDGE_RIGHT, p.margin_right);
                }
                if p.top {
                    (a.set_anchor)(ptr, EDGE_TOP, 1);
                    (a.set_margin)(ptr, EDGE_TOP, p.margin_top);
                }
                if p.bottom {
                    (a.set_anchor)(ptr, EDGE_BOTTOM, 1);
                    (a.set_margin)(ptr, EDGE_BOTTOM, p.margin_bottom);
                }
            }
        }
    } else {
        // X11, or a compositor without layer-shell: move the window. Under
        // Wayland this request is ignored, which is why the layer path exists.
        let _ = size;
        let (x, y) = fallback_origin(placement, screen, win_w, win_h);
        window.move_(x, y);
    }
}

impl Overlay {
    /// Apply the palette and layout rules.
    fn set_css(&self, accent: &str, live: bool) {
        let words_css = if live {
            "#words { color: #EDEFF4; font-size: 13px; }"
        } else {
            "#words { color: transparent; }"
        };
        // Every number here comes from the same constants that size the window,
        // so the drawn pill and the requested window can never disagree — which
        // is what previously produced a pill with an empty strip on the right.
        let css = format!(
            "window {{ background-color: transparent; }}\n\
             #pill {{\n\
               background-color: rgba(14, 17, 23, 0.92);\n\
               border: {BORDER}px solid rgba(255, 255, 255, 0.10);\n\
               border-radius: {radius}px;\n\
               padding: {PAD_Y}px {PAD_X}px;\n\
             }}\n\
             #meter {{ color: {accent}; }}\n\
             {words_css}\n",
            radius = PILL_H / 2
        );
        let _ = self.provider.load_from_data(css.as_bytes());
    }

    /// Make the overlay visible, showing only the widgets this style uses.
    ///
    /// Deliberately not `show_all()`: that recurses into every child and makes
    /// it visible, which undid `words.set_visible(live)`. In Pill style the
    /// hidden words label reappeared, so the window was sized around two lines
    /// of invisible text — 91x60 logical instead of 58x40. That phantom label
    /// was the empty space on the right and the reason the pill was not round.
    ///
    /// Visibility is therefore set explicitly, and the pill is re-fitted after,
    /// so the size is computed from what is actually on screen.
    fn reveal(&self) {
        self.pill.show();
        self.meter.show();
        // Set explicitly, and after the pill is shown: this is the line that
        // `show_all()` used to override.
        self.words.set_visible(self.live.get());
        self.window.show();
        // Size only once everything is visible, so the measurement reflects what
        // is actually drawn.
        self.fit();
    }

    /// Size the window to exactly what the pill needs, then reposition it.
    ///
    /// This is what removes the dead space. The width is the meter plus the
    /// *measured* width of the words, so a short take gives a short pill. The
    /// previous version used a fixed width, which is why both styles showed an
    /// empty strip on the right whenever the content was smaller than the
    /// reservation.
    ///
    /// Re-centring is part of the same operation: the width has just changed, so
    /// a margin computed for the previous width would leave the pill off centre.
    fn fit(&self) {
        layout_pill(
            &self.window,
            &self.words,
            self.live.get(),
            self.placement.get(),
            self.metrics.get(),
            &self.gdk_monitor,
            &self.size,
            self.layered,
        );
    }

    /// Start the redraw timer. Idempotent: a second call does not add a second
    /// timer, which would make the meter animate at double speed.
    fn start_timer(&self) {
        let mut timer = self.timer.borrow_mut();
        if timer.is_some() {
            return;
        }
        let smooth = self.smooth.clone();
        let meter = self.meter.clone();
        let words = self.words.clone();
        let source = self.source.clone();
        // Needed to re-fit the pill when the transcript grows: without this the
        // window would keep the width it had when the first word arrived and
        // clip everything after it.
        let window = self.window.clone();
        let placement = self.placement.clone();
        let metrics = self.metrics.clone();
        let gdk_monitor = self.gdk_monitor.clone();
        let size = self.size.clone();
        // Shared, so a style change is picked up on the next tick. A captured
        // `bool` here was the bug that clipped Live text to the Pill width.
        let live = self.live.clone();
        let layered = self.layered;

        let id = gtk::glib::timeout_add_local(Duration::from_millis(50), move || {
            let target = source.borrow().level_now();

            // Attack immediately, release slowly: matches how speech arrives,
            // and stops the meter twitching on every consonant.
            let prev = smooth.get();
            let next = if target > prev {
                target
            } else {
                prev * 0.82 + target * 0.18
            };
            smooth.set(next);
            meter.queue_draw();

            // The words only change a few times a second, so they are refreshed
            // on a slower cadence than the meter. A change also re-fits the pill,
            // because the width is derived from the text: the window grows with
            // the sentence instead of reserving a fixed width up front.
            if words.is_visible() {
                let text = source.borrow().words_now();
                if words.text().as_str() != text {
                    words.set_text(&text);
                    layout_pill(
                        &window,
                        &words,
                        live.get(),
                        placement.get(),
                        metrics.get(),
                        &gdk_monitor,
                        &size,
                        layered,
                    );
                }
            }
            gtk::glib::ControlFlow::Continue
        });
        *timer = Some(id);
    }

    fn stop_timer(&self) {
        if let Some(id) = self.timer.borrow_mut().take() {
            id.remove();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_constants_match_the_c_header() {
        // These are the ABI. If they are wrong the overlay lands on the wrong
        // edge, so pin them explicitly.
        assert_eq!(EDGE_LEFT, 0);
        assert_eq!(EDGE_RIGHT, 1);
        assert_eq!(EDGE_TOP, 2);
        assert_eq!(EDGE_BOTTOM, 3);
        assert_eq!(LAYER_OVERLAY, 3);
    }

    #[test]
    fn bottom_centre_is_horizontally_centred() {
        // Both horizontal edges are pinned with an equal margin. That is what
        // makes the compositor size the window to exactly `win_w`, so the pill
        // hugs its content instead of stretching and leaving dead space on the
        // right. Pinning only the left edge would leave the width up to the
        // window's own request, which is how the empty strip appeared.
        let p = plan(Placement::BottomCentre, 2327, 300);
        assert!(p.left && p.right && p.bottom);
        assert!(!p.top);
        // Equal margins on both sides is what centres it.
        assert_eq!(p.margin_left, (2327 - 300) / 2);
        assert_eq!(p.margin_left, p.margin_right);
        assert_eq!(p.margin_bottom, GAP);
    }

    #[test]
    fn the_margin_does_not_include_the_panel() {
        // The compositor already excludes the panel from a layer surface's
        // available area, so the margin must be the gap alone. Measured: with
        // this margin the pill lands just above the taskbar. If the panel height
        // were added here, the pill would float a whole panel higher.
        let p = plan(Placement::BottomCentre, 2327, 300);
        assert_eq!(p.margin_bottom, GAP);
        assert!(p.margin_bottom < 56, "must not include a panel height");
    }

    #[test]
    fn centring_never_goes_negative() {
        // A window wider than the screen would otherwise get a negative margin,
        // which layer-shell would reject.
        let p = plan(Placement::BottomCentre, 200, 460);
        assert_eq!(p.margin_left, 0);
    }

    #[test]
    fn top_centre_anchors_to_the_top() {
        let p = plan(Placement::TopCentre, 2327, 300);
        assert!(p.top && p.left);
        assert!(!p.bottom);
        // A top placement ignores the bottom entirely.
        assert_eq!(p.margin_top, GAP);
        assert_eq!(p.margin_bottom, 0);
    }

    fn single() -> Monitor {
        Monitor {
            x: 0,
            y: 0,
            w: 2327,
            h: 1455,
            panel: 56,
        }
    }

    #[test]
    fn fallback_origin_keeps_the_window_on_screen() {
        // X11 path: the real pill size on a 2327x1455 screen above a 56 px panel.
        let w = pill_width(false, 0);
        let (x, y) = fallback_origin(Placement::BottomCentre, single(), w, PILL_H);
        assert_eq!(x, (2327 - w) / 2);
        assert_eq!(y, 1455 - PILL_H - 56 - GAP);
        assert!(y > 0);
    }

    #[test]
    fn fallback_origin_clamps_a_tiny_screen() {
        let tiny = Monitor {
            x: 0,
            y: 0,
            w: 100,
            h: 100,
            panel: 56,
        };
        let (x, y) = fallback_origin(Placement::BottomCentre, tiny, 300, 96);
        assert_eq!(x, 0);
        assert!(y >= GAP);
    }

    // ── multi-monitor ────────────────────────────────────────────────────

    /// A second monitor to the **right** of the primary, as most desks have it.
    fn right_of_primary() -> Monitor {
        Monitor {
            x: 2327,
            y: 0,
            w: 1920,
            h: 1080,
            panel: 0,
        }
    }

    /// A second monitor to the **left**, which makes the primary's origin
    /// non-zero — the case a monitor-relative calculation gets wrong.
    fn primary_on_the_right() -> Monitor {
        Monitor {
            x: 1920,
            y: 0,
            w: 2327,
            h: 1455,
            panel: 56,
        }
    }

    #[test]
    fn the_window_is_centred_on_the_monitor_it_belongs_to() {
        // The pill must sit in the middle of *its* monitor, not of the desktop.
        let m = right_of_primary();
        let w = pill_width(false, 0);
        let (x, _) = fallback_origin(Placement::BottomCentre, m, w, PILL_H);
        assert_eq!(
            x,
            m.x + (m.w - w) / 2,
            "the pill should be centred within the second monitor"
        );
        assert!(
            x >= m.x && x + w <= m.x + m.w,
            "the pill must be inside its own monitor, got x={x} for monitor at {}..{}",
            m.x,
            m.x + m.w
        );
    }

    #[test]
    fn a_monitor_that_does_not_start_at_the_origin_still_gets_the_pill() {
        // This is the bug the origin exists to prevent. The primary sits at
        // x=1920 because a second monitor is to its left. Computing the position
        // relative to the monitor and then passing it to X11 — which wants global
        // coordinates — would put the pill at global x~960, in the middle of the
        // *other* screen.
        let m = primary_on_the_right();
        let w = pill_width(false, 0);
        let (x, y) = fallback_origin(Placement::BottomCentre, m, w, PILL_H);

        assert!(
            x >= m.x,
            "the pill must not be to the left of its monitor: x={x}, monitor starts at {}",
            m.x
        );
        assert_eq!(x, m.x + (m.w - w) / 2);
        assert!(y > m.y, "the pill must be below the top edge of its monitor");
        assert!(
            y < m.y + m.h,
            "the pill must be above the bottom edge of its monitor"
        );
    }

    #[test]
    fn the_pill_stays_within_a_second_monitor() {
        // A monitor that is not at the origin: both axes must be measured from
        // that monitor, not from zero.
        let m = right_of_primary();
        let w = pill_width(false, 0);
        let (x, y) = fallback_origin(Placement::BottomCentre, m, w, PILL_H);
        assert_eq!(x, m.x + (m.w - w) / 2, "centred on the second monitor");
        assert_eq!(y, m.y + m.h - PILL_H - m.panel - GAP, "bottom edge of it");

        let (tx, ty) = fallback_origin(Placement::TopCentre, m, w, PILL_H);
        assert_eq!(tx, m.x + (m.w - w) / 2);
        assert_eq!(ty, m.y + GAP, "top edge of the second monitor");
    }

    #[test]
    fn a_monitor_above_the_primary_is_handled() {
        // Stacked vertically, the monitor's origin is negative in y.
        let above = Monitor {
            x: 0,
            y: -1080,
            w: 1920,
            h: 1080,
            panel: 0,
        };
        let w = pill_width(false, 0);
        let (_, y) = fallback_origin(Placement::BottomCentre, above, w, PILL_H);
        assert_eq!(y, above.y + above.h - PILL_H - GAP);
        assert!(
            y < 0,
            "a monitor above the primary has negative y; the pill must follow it"
        );
    }

    #[test]
    fn a_panel_on_a_lower_monitor_is_not_counted_twice() {
        // `panel` is measured from the monitor's own bottom edge, so a monitor
        // below another one must not have its panel offset applied twice.
        let lower = Monitor {
            x: 0,
            y: 1455,
            w: 1920,
            h: 1080,
            panel: 40,
        };
        let (_, y) = fallback_origin(Placement::BottomCentre, lower, 200, PILL_H);
        assert_eq!(y, lower.y + lower.h - PILL_H - lower.panel - GAP);
    }

    #[test]
    fn every_palette_is_a_distinct_colour() {
        // The whole point of the picker is that the choices differ. A refactor
        // that collapsed them to one hue would still compile, so check.
        let all = ["blue", "green", "orange", "pink", "violet", "red"];
        let mut seen = std::collections::HashSet::new();
        for name in all {
            let hex = accent_hex(Some(name));
            assert!(hex.starts_with('#'), "{name} is not a hex colour");
            assert_eq!(hex.len(), 7, "{name} is not a full hex colour");
            assert!(seen.insert(hex), "{name} duplicates another palette colour");
        }
        assert_eq!(seen.len(), all.len());
    }

    #[test]
    fn an_unset_palette_falls_back_to_blue() {
        assert_eq!(accent_hex(None), accent_hex(Some("blue")));
        // A hand-edited file could contain anything; it must not panic.
        assert_eq!(accent_hex(Some("chartreuse")), accent_hex(Some("blue")));
    }

    #[test]
    fn the_pill_has_no_padding_beyond_its_geometry() {
        // The regression this guards: the window used to be 300x96 around a
        // 30x16 meter, so the content filled about 1% of it and there was a
        // wide empty strip on the right. Height must now be exactly the meter
        // plus its padding, with nothing left over.
        assert_eq!(PILL_H, METER_H + 2 * (PAD_Y + BORDER));
        let w = pill_width(false, 0);
        assert_eq!(w, METER_W + 2 * (PAD_X + BORDER));
        // Sanity: compact, not a 300 px box.
        assert!(w < 100, "compact pill should be under 100 px wide, got {w}");
        assert!(PILL_H < 50, "pill should be under 50 px tall, got {PILL_H}");
    }

    #[test]
    fn live_grows_with_the_text_instead_of_reserving_space() {
        let empty = pill_width(true, 0);
        let short = pill_width(true, 60);
        let long = pill_width(true, 240);
        assert!(short > empty, "text must widen the pill");
        assert!(long > short, "more text must widen it further");
        // The dead-space bug: a fixed width meant a two-word take left a big
        // empty strip. A short take must now be much narrower than a long one.
        assert!(
            long - short > 100,
            "width must track the text, not be fixed"
        );
    }

    #[test]
    fn live_width_is_capped_so_it_never_spans_the_screen() {
        // Beyond the cap the words are ellipsized rather than the pill growing
        // without bound.
        let huge = pill_width(true, 10_000);
        assert_eq!(huge, pill_width(true, MAX_TEXT_W));
        assert!(huge < 500, "capped pill should stay compact, got {huge}");
    }

    #[test]
    fn live_is_wider_than_pill_when_it_has_words() {
        let pill = pill_width(false, 0);
        let live = pill_width(true, 120);
        assert!(live > pill);
    }

    #[test]
    fn placement_maps_from_config() {
        use sotto_core::config::OverlayPosition as P;
        assert_eq!(Placement::from_config(P::TopCentre), Placement::TopCentre);
        assert_eq!(
            Placement::from_config(P::BottomCentre),
            Placement::BottomCentre
        );
    }

    #[test]
    fn synthetic_source_moves_and_walks_words_in() {
        // The settings preview relies on both, and a broken closure would only
        // show up as a dead overlay, so check the behaviour directly.
        let s = Source::synthetic();
        let first = s.words_now();
        assert!(!first.is_empty());
        // The level must stay inside the range the meter expects.
        for _ in 0..50 {
            let l = s.level_now();
            assert!((0.0..=1.0).contains(&l), "level out of range: {l}");
        }
    }

    #[test]
    fn supported_reports_without_panicking() {
        // Called during startup on every machine, including X11 and headless
        // CI, where the library or the protocol may be absent.
        let _ = supported();
    }

    #[test]
    fn exists_is_false_before_anything_is_shown() {
        assert!(!exists());
    }
}
