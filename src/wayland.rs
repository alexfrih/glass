//! Wayland layer-shell client with KDE blur, icons, and pointer input.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use wayland_client::{
    globals::{registry_queue_init, GlobalList},
    protocol::{wl_output, wl_pointer, wl_region, wl_seat, wl_shm, wl_surface},
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};

use crate::blur_api::{org_kde_kwin_blur, org_kde_kwin_blur_manager};

// ---- public types ----

pub enum Event {
    Notify {
        id: u32,
        app_name: String,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        timeout_ms: u32,
    },
    Close {
        id: u32,
    },
}

pub enum Reply {
    Closed { id: u32, reason: u32 },
    ActionInvoked { id: u32, action: String },
}

// ---- constants ----

const WIDTH: u32 = 380;
const HEIGHT: u32 = 94;
const RADIUS: f32 = 16.0;
const MARGIN_TOP: i32 = 14;
const MARGIN_RIGHT: i32 = 14;
const SCALE: i32 = 2;
const ICON_LOGICAL: u32 = 18;

// ---- state ----

struct Slot {
    surface: LayerSurface,
    blur: org_kde_kwin_blur::OrgKdeKwinBlur,
    configured: bool,
    expires_at: Option<Instant>,
    summary: String,
    body: String,
    app_name: String,
    has_default_action: bool,
    icon_rgba: Option<Vec<u8>>, // RGBA pixels at ICON_LOGICAL*SCALE square
}

struct AppState {
    registry: RegistryState,
    output: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    seat: SeatState,
    blur_manager: org_kde_kwin_blur_manager::OrgKdeKwinBlurManager,
    slots: HashMap<u32, Slot>,
    reply_tx: UnboundedSender<Reply>,
    hovered_surface: Option<wayland_client::protocol::wl_surface::WlSurface>,
}

// ---- entry point ----

pub fn run(mut rx: UnboundedReceiver<Event>, reply_tx: UnboundedSender<Reply>) -> Result<()> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<AppState>(&conn)?;
    let qh = queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;
    let pool = SlotPool::new((WIDTH * HEIGHT * 4 * SCALE as u32 * SCALE as u32 * 4) as usize, &shm)?;
    let seat = SeatState::new(&globals, &qh);
    let blur_manager = bind_blur_manager(&globals, &qh)?;

    let mut state = AppState {
        registry: RegistryState::new(&globals),
        output: OutputState::new(&globals, &qh),
        compositor,
        layer_shell,
        shm,
        pool,
        seat,
        blur_manager,
        slots: HashMap::new(),
        reply_tx,
        hovered_surface: None,
    };

    tracing::info!("wayland bound, blur manager OK");

    loop {
        while let Ok(ev) = rx.try_recv() {
            state.handle_event(ev, &qh);
        }

        let now = Instant::now();
        let expired: Vec<u32> = state
            .slots
            .iter()
            .filter_map(|(id, s)| s.expires_at.filter(|e| *e <= now).map(|_| *id))
            .collect();
        for id in expired {
            state.close(id, 1); // reason 1 = expired
        }

        queue.roundtrip(&mut state)?;
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn bind_blur_manager(
    globals: &GlobalList,
    qh: &QueueHandle<AppState>,
) -> Result<org_kde_kwin_blur_manager::OrgKdeKwinBlurManager> {
    globals
        .bind::<org_kde_kwin_blur_manager::OrgKdeKwinBlurManager, _, _>(qh, 1..=1, ())
        .map_err(|e| anyhow!("org_kde_kwin_blur_manager not available: {e}"))
}

// ---- AppState impl ----

impl AppState {
    fn handle_event(&mut self, ev: Event, qh: &QueueHandle<AppState>) {
        match ev {
            Event::Notify {
                id,
                app_name,
                app_icon,
                summary,
                body,
                actions,
                timeout_ms,
            } => self.open(id, app_name, app_icon, summary, body, actions, timeout_ms, qh),
            Event::Close { id } => self.close(id, 3), // reason 3 = closed by CloseNotification
        }
    }

    fn open(
        &mut self,
        id: u32,
        app_name: String,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        timeout_ms: u32,
        qh: &QueueHandle<AppState>,
    ) {
        self.close(id, 0); // silent close if replacing

        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some(format!("glass-{id}")),
            None,
        );
        layer.set_anchor(Anchor::TOP | Anchor::RIGHT);
        layer.set_margin(MARGIN_TOP, MARGIN_RIGHT, 0, 0);
        layer.set_size(WIDTH, HEIGHT);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();

        let wl_surface = layer.wl_surface().clone();
        let blur = self.blur_manager.create(&wl_surface, qh, ());
        let region = self.compositor.wl_compositor().create_region(qh, ());
        add_rounded_region(&region, WIDTH as i32, HEIGHT as i32, RADIUS as i32);
        blur.set_region(Some(&region));
        blur.commit();
        region.destroy();

        let expires_at = if timeout_ms == 0 {
            None
        } else {
            Some(Instant::now() + Duration::from_millis(timeout_ms as u64))
        };

        let has_default_action = actions.iter().any(|a| a == "default");
        let icon_rgba = load_icon(&app_icon, &app_name);

        self.slots.insert(
            id,
            Slot {
                surface: layer,
                blur,
                configured: false,
                expires_at,
                summary,
                body,
                app_name,
                has_default_action,
                icon_rgba,
            },
        );
    }

    fn close(&mut self, id: u32, reason: u32) {
        if let Some(slot) = self.slots.remove(&id) {
            slot.blur.release();
            drop(slot);
            if reason > 0 {
                let _ = self.reply_tx.send(Reply::Closed { id, reason });
            }
        }
    }

    fn find_id_by_surface(&self, surface: &wl_surface::WlSurface) -> Option<u32> {
        let target = surface.id();
        self.slots
            .iter()
            .find_map(|(id, s)| (s.surface.wl_surface().id() == target).then_some(*id))
    }

    fn click(&mut self, surface: &wl_surface::WlSurface, button: u32) {
        let Some(id) = self.find_id_by_surface(surface) else {
            return;
        };
        let is_left = button == 0x110; // BTN_LEFT
        if is_left {
            // Check if app registered a "default" action
            let has_action = self.slots.get(&id).map(|s| s.has_default_action).unwrap_or(false);
            if has_action {
                let _ = self.reply_tx.send(Reply::ActionInvoked {
                    id,
                    action: "default".to_string(),
                });
            }
        }
        // Any click dismisses
        self.close(id, 2); // reason 2 = dismissed by user
    }

    fn draw(&mut self, id: u32) {
        let Some(slot) = self.slots.get_mut(&id) else {
            return;
        };
        if !slot.configured {
            return;
        }
        let buf_w = WIDTH as i32 * SCALE;
        let buf_h = HEIGHT as i32 * SCALE;
        let stride = buf_w * 4;
        let Ok((buffer, canvas)) =
            self.pool
                .create_buffer(buf_w, buf_h, stride, wl_shm::Format::Argb8888)
        else {
            tracing::error!("failed to allocate buffer");
            return;
        };

        let summary = slot.summary.clone();
        let body = slot.body.clone();
        let app = slot.app_name.clone();
        let icon = slot.icon_rgba.clone();
        render_card(canvas, buf_w as u32, buf_h as u32, RADIUS * SCALE as f32, &app, &summary, &body, icon.as_deref());

        let wl = slot.surface.wl_surface();
        wl.set_buffer_scale(SCALE);
        wl.damage_buffer(0, 0, buf_w, buf_h);
        if let Err(e) = buffer.attach_to(wl) {
            tracing::error!(error = %e, "attach_to");
            return;
        }
        slot.surface.commit();
    }
}

// ---- icon loading ----

fn load_icon(app_icon: &str, app_name: &str) -> Option<Vec<u8>> {
    let path = find_icon_path(app_icon, app_name)?;
    let size = ICON_LOGICAL * SCALE as u32;
    match image::open(&path) {
        Ok(img) => {
            let resized = img.resize_exact(size, size, image::imageops::FilterType::Lanczos3);
            Some(resized.to_rgba8().into_raw())
        }
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "failed to load icon");
            None
        }
    }
}

fn find_icon_path(app_icon: &str, app_name: &str) -> Option<PathBuf> {
    // 1. Direct file path
    if app_icon.starts_with('/') {
        let p = PathBuf::from(app_icon);
        if p.exists() {
            return Some(p);
        }
    }

    let name = if !app_icon.is_empty() && !app_icon.starts_with('/') {
        app_icon
    } else {
        app_name
    };
    let lower = name.to_lowercase();

    // 2. hicolor theme
    for size in &["256x256", "128x128", "64x64", "48x48", "32x32"] {
        let p = PathBuf::from(format!("/usr/share/icons/hicolor/{size}/apps/{lower}.png"));
        if p.exists() {
            return Some(p);
        }
    }

    // 3. pixmaps
    let p = PathBuf::from(format!("/usr/share/pixmaps/{lower}.png"));
    if p.exists() {
        return Some(p);
    }

    // 4. /opt/{name}/{name}.png (Electron apps)
    let p = PathBuf::from(format!("/opt/{lower}/{lower}.png"));
    if p.exists() {
        return Some(p);
    }

    // 5. Desktop entry lookup
    let desktop = format!("/usr/share/applications/{lower}.desktop");
    if let Ok(content) = std::fs::read_to_string(&desktop) {
        for line in content.lines() {
            if let Some(icon_name) = line.strip_prefix("Icon=") {
                let icon_name = icon_name.trim();
                if icon_name != name {
                    return find_icon_path(icon_name, "");
                }
            }
        }
    }

    None
}

// ---- rendering ----

fn render_card(
    canvas: &mut [u8],
    w: u32,
    h: u32,
    radius: f32,
    app: &str,
    summary: &str,
    body: &str,
    icon_rgba: Option<&[u8]>,
) {
    use cosmic_text::{
        Attrs, Buffer, Color as CColor, Family, FontSystem, Metrics, Shaping, SwashCache, Weight,
    };
    use tiny_skia::{Color, FillRule, Paint, Pixmap, Transform};

    let mut pm = Pixmap::new(w, h).expect("pixmap alloc");
    pm.fill(Color::TRANSPARENT);

    let s = w as f32 / WIDTH as f32;
    let path = rounded_rect_path_at(0.0, 0.0, w as f32, h as f32, radius).expect("build path");

    let mut paint = Paint::default();
    paint.set_color_rgba8(22, 22, 38, 245);
    paint.anti_alias = true;
    pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);

    let icon_size = (ICON_LOGICAL as f32 * s) as i32;
    let icon_x = (14.0 * s) as i32;
    let icon_y = (12.0 * s) as i32;
    let text_left = icon_x + icon_size + (8.0 * s) as i32;
    let text_w = w as i32 - text_left - (14.0 * s) as i32;
    let right_edge = w as i32 - (14.0 * s) as i32;

    // icon
    if let Some(rgba) = icon_rgba {
        draw_icon_circle(pm.data_mut(), w as i32, icon_x, icon_y, icon_size, rgba);
    } else {
        let color = app_icon_color(app);
        draw_circle(pm.data_mut(), w as i32, icon_x + icon_size / 2, icon_y + icon_size / 2, icon_size / 2, color);
    }

    // text
    let mut font_system = FontSystem::new();
    let mut swash_cache = SwashCache::new();

    let app_text = if app.is_empty() { "notification" } else { app };
    let row1_metrics = Metrics::new(11.0 * s, 14.0 * s);

    let mut app_buf = Buffer::new(&mut font_system, row1_metrics);
    app_buf.set_size(&mut font_system, Some(text_w as f32 * 0.6), None);
    app_buf.set_text(&mut font_system, app_text, &Attrs::new().family(Family::SansSerif).weight(Weight::SEMIBOLD), Shaping::Advanced);
    app_buf.shape_until_scroll(&mut font_system, false);

    let mut now_buf = Buffer::new(&mut font_system, row1_metrics);
    now_buf.set_size(&mut font_system, Some((60.0 * s) as f32), None);
    now_buf.set_text(&mut font_system, "now", &Attrs::new().family(Family::SansSerif).weight(Weight::NORMAL), Shaping::Advanced);
    now_buf.shape_until_scroll(&mut font_system, false);

    let white_dim = CColor::rgba(255, 255, 255, 160);
    let white = CColor::rgba(255, 255, 255, 245);
    let white_body = CColor::rgba(255, 255, 255, 200);

    let pm_data = pm.data_mut();
    let pw = w as i32;
    let row1_y = (14.0 * s) as i32;

    draw_text_buffer(pm_data, pw, &mut font_system, &mut swash_cache, &app_buf, text_left, row1_y, white_dim);
    let now_x = right_edge - (26.0 * s) as i32;
    draw_text_buffer(pm_data, pw, &mut font_system, &mut swash_cache, &now_buf, now_x, row1_y, white_dim);

    let sum_metrics = Metrics::new(13.5 * s, 17.0 * s);
    let mut sum_buf = Buffer::new(&mut font_system, sum_metrics);
    sum_buf.set_size(&mut font_system, Some(text_w as f32), None);
    sum_buf.set_text(&mut font_system, summary, &Attrs::new().family(Family::SansSerif).weight(Weight::BOLD), Shaping::Advanced);
    sum_buf.shape_until_scroll(&mut font_system, false);
    let row2_y = row1_y + (16.0 * s) as i32;
    draw_text_buffer(pm_data, pw, &mut font_system, &mut swash_cache, &sum_buf, text_left, row2_y, white);

    let body_metrics = Metrics::new(12.5 * s, 16.0 * s);
    let mut body_buf = Buffer::new(&mut font_system, body_metrics);
    body_buf.set_size(&mut font_system, Some(text_w as f32), None);
    body_buf.set_text(&mut font_system, body, &Attrs::new().family(Family::SansSerif).weight(Weight::NORMAL), Shaping::Advanced);
    body_buf.shape_until_scroll(&mut font_system, false);
    let row3_y = row2_y + (18.0 * s) as i32;
    draw_text_buffer(pm_data, pw, &mut font_system, &mut swash_cache, &body_buf, text_left, row3_y, white_body);

    // copy RGBA -> BGRA
    let src = pm.data();
    for (dst, chunk) in canvas.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
        dst[0] = chunk[2];
        dst[1] = chunk[1];
        dst[2] = chunk[0];
        dst[3] = chunk[3];
    }
}

// ---- drawing helpers ----

fn draw_text_buffer(
    pixels: &mut [u8],
    pw: i32,
    font_system: &mut cosmic_text::FontSystem,
    swash_cache: &mut cosmic_text::SwashCache,
    buffer: &cosmic_text::Buffer,
    offset_x: i32,
    offset_y: i32,
    color: cosmic_text::Color,
) {
    let cr = color.r();
    let cg = color.g();
    let cb = color.b();

    buffer.draw(font_system, swash_cache, cosmic_text::Color::rgba(cr, cg, cb, 255), |x, y, _w, _h, color| {
        let px = x + offset_x;
        let py = y + offset_y;
        if px < 0 || py < 0 || px >= pw {
            return;
        }
        let i = ((py * pw + px) * 4) as usize;
        if i + 3 >= pixels.len() {
            return;
        }
        let a = color.a();
        if a == 0 {
            return;
        }
        let src_a = a as u32;
        let inv_a = 255 - src_a;
        let sr = (cr as u32 * src_a / 255) as u8;
        let sg = (cg as u32 * src_a / 255) as u8;
        let sb = (cb as u32 * src_a / 255) as u8;
        let sa = (src_a + (pixels[i + 3] as u32 * inv_a / 255)) as u8;
        pixels[i]     = (sb as u32 + pixels[i] as u32 * inv_a / 255).min(sa as u32) as u8;
        pixels[i + 1] = (sg as u32 + pixels[i + 1] as u32 * inv_a / 255).min(sa as u32) as u8;
        pixels[i + 2] = (sr as u32 + pixels[i + 2] as u32 * inv_a / 255).min(sa as u32) as u8;
        pixels[i + 3] = sa;
    });
}

/// Draw a PNG icon clipped to a circle.
fn draw_icon_circle(data: &mut [u8], pw: i32, x0: i32, y0: i32, size: i32, rgba: &[u8]) {
    let r = size as f32 / 2.0;
    let cx = r;
    let cy = r;
    let r2 = r * r;

    for iy in 0..size {
        for ix in 0..size {
            let dx = ix as f32 + 0.5 - cx;
            let dy = iy as f32 + 0.5 - cy;
            if dx * dx + dy * dy > r2 {
                continue;
            }
            let src_i = ((iy * size + ix) * 4) as usize;
            if src_i + 3 >= rgba.len() {
                continue;
            }
            let sr = rgba[src_i];
            let sg = rgba[src_i + 1];
            let sb = rgba[src_i + 2];
            let sa = rgba[src_i + 3];
            if sa == 0 {
                continue;
            }

            let dst_x = x0 + ix;
            let dst_y = y0 + iy;
            if dst_x < 0 || dst_y < 0 || dst_x >= pw {
                continue;
            }
            let di = ((dst_y * pw + dst_x) * 4) as usize;
            if di + 3 >= data.len() {
                continue;
            }

            let a = sa as u16;
            let inv = 255 - a;
            // premultiply source and blend onto dest (also premul)
            data[di]     = ((sr as u16 * a / 255 + data[di] as u16 * inv / 255) as u8);
            data[di + 1] = ((sg as u16 * a / 255 + data[di + 1] as u16 * inv / 255) as u8);
            data[di + 2] = ((sb as u16 * a / 255 + data[di + 2] as u16 * inv / 255) as u8);
            data[di + 3] = ((a + data[di + 3] as u16 * inv / 255) as u8);
        }
    }
}

fn app_icon_color(app: &str) -> [u8; 4] {
    match app.to_lowercase().as_str() {
        "discord" | "vesktop" => [88, 101, 242, 255],
        "spotify" => [30, 215, 96, 255],
        "firefox" | "brave" => [255, 122, 0, 255],
        "telegram" => [42, 171, 238, 255],
        "slack" => [74, 21, 75, 255],
        _ => [137, 180, 250, 255],
    }
}

fn draw_circle(data: &mut [u8], pw: i32, cx: i32, cy: i32, r: i32, color: [u8; 4]) {
    let [cr, cg, cb, ca] = color;
    let r2 = (r * r) as f32;
    for dy in -r..=r {
        for dx in -r..=r {
            let dist2 = (dx * dx + dy * dy) as f32;
            if dist2 > r2 {
                continue;
            }
            let px = cx + dx;
            let py = cy + dy;
            if px < 0 || py < 0 || px >= pw {
                continue;
            }
            let i = ((py * pw + px) * 4) as usize;
            if i + 3 >= data.len() {
                continue;
            }
            let edge = (r2 - dist2).sqrt().min(1.0);
            let a = (ca as f32 * edge) as u16;
            let inv = 255 - a;
            data[i]     = (cr as u16 * a / 255 + data[i] as u16 * inv / 255) as u8;
            data[i + 1] = (cg as u16 * a / 255 + data[i + 1] as u16 * inv / 255) as u8;
            data[i + 2] = (cb as u16 * a / 255 + data[i + 2] as u16 * inv / 255) as u8;
            data[i + 3] = (a + data[i + 3] as u16 * inv / 255) as u8;
        }
    }
}

fn add_rounded_region(region: &wl_region::WlRegion, w: i32, h: i32, r: i32) {
    const INSET: i32 = 4;
    let iw = w - 2 * INSET;
    let ih = h - 2 * INSET;
    let ir = (r - INSET).max(1);
    for yy in 0..ih {
        let (xl, xr) = if yy < ir {
            let dy = (ir - yy) as f32;
            let dx = ((ir * ir) as f32 - dy * dy).max(0.0).sqrt() as i32;
            (ir - dx, iw - ir + dx)
        } else if yy >= ih - ir {
            let dy = (yy - (ih - ir)) as f32;
            let dx = ((ir * ir) as f32 - dy * dy).max(0.0).sqrt() as i32;
            (ir - dx, iw - ir + dx)
        } else {
            (0, iw)
        };
        if xr > xl {
            region.add(xl + INSET, yy + INSET, xr - xl, 1);
        }
    }
}

fn rounded_rect_path_at(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    use tiny_skia::PathBuilder;
    const KAPPA: f32 = 0.5522847498;
    let x0 = x;
    let y0 = y;
    let x1 = x + w;
    let y1 = y + h;
    let c = r * KAPPA;
    let mut pb = PathBuilder::new();
    pb.move_to(x0 + r, y0);
    pb.line_to(x1 - r, y0);
    pb.cubic_to(x1 - r + c, y0, x1, y0 + r - c, x1, y0 + r);
    pb.line_to(x1, y1 - r);
    pb.cubic_to(x1, y1 - r + c, x1 - r + c, y1, x1 - r, y1);
    pb.line_to(x0 + r, y1);
    pb.cubic_to(x0 + r - c, y1, x0, y1 - r + c, x0, y1 - r);
    pb.line_to(x0, y0 + r);
    pb.cubic_to(x0, y0 + r - c, x0 + r - c, y0, x0 + r, y0);
    pb.close();
    pb.finish()
}

// ---- smithay handler impls ----

impl CompositorHandler for AppState {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for AppState {
    fn output_state(&mut self) -> &mut OutputState { &mut self.output }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for AppState {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if let Some(id) = self.find_id_by_surface(layer.wl_surface()) {
            self.close(id, 2);
        }
    }
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface, _: LayerSurfaceConfigure, _: u32) {
        let target = layer.wl_surface().id();
        let id = self.slots.iter().find_map(|(id, s)| (s.surface.wl_surface().id() == target).then_some(*id));
        if let Some(id) = id {
            if let Some(s) = self.slots.get_mut(&id) {
                s.configured = true;
            }
            tracing::debug!(id, "configured, drawing");
            self.draw(id);
        }
    }
}

impl SeatHandler for AppState {
    fn seat_state(&mut self) -> &mut SeatState { &mut self.seat }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Pointer {
            let _ = self.seat.get_pointer(qh, &seat);
            tracing::debug!("pointer capability acquired");
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, _: Capability) {}
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for AppState {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            match event.kind {
                PointerEventKind::Enter { .. } => {
                    self.hovered_surface = Some(event.surface.clone());
                }
                PointerEventKind::Leave { .. } => {
                    self.hovered_surface = None;
                }
                PointerEventKind::Press { button, .. } => {
                    if let Some(surf) = self.hovered_surface.clone() {
                        self.click(&surf, button);
                    }
                }
                _ => {}
            }
        }
    }
}

impl ShmHandler for AppState {
    fn shm_state(&mut self) -> &mut Shm { &mut self.shm }
}

impl ProvidesRegistryState for AppState {
    fn registry(&mut self) -> &mut RegistryState { &mut self.registry }
    registry_handlers![OutputState, SeatState];
}

impl Dispatch<wl_region::WlRegion, ()> for AppState {
    fn event(_: &mut Self, _: &wl_region::WlRegion, _: wl_region::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<org_kde_kwin_blur_manager::OrgKdeKwinBlurManager, ()> for AppState {
    fn event(_: &mut Self, _: &org_kde_kwin_blur_manager::OrgKdeKwinBlurManager, _: org_kde_kwin_blur_manager::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<org_kde_kwin_blur::OrgKdeKwinBlur, ()> for AppState {
    fn event(_: &mut Self, _: &org_kde_kwin_blur::OrgKdeKwinBlur, _: org_kde_kwin_blur::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

delegate_compositor!(AppState);
delegate_output!(AppState);
delegate_layer!(AppState);
delegate_shm!(AppState);
delegate_registry!(AppState);
delegate_seat!(AppState);
delegate_pointer!(AppState);
