use ab_glyph::{Font, FontRef, GlyphId, PxScale, ScaleFont};
use std::sync::OnceLock;
use std::time::Duration;
use thiserror::Error;
use tiny_skia::{
    Color, GradientStop, LineCap, LinearGradient, Paint, PathBuilder, Pixmap, Point, SpreadMode,
    Stroke, Transform,
};

static FONT_DATA: &[u8] = include_bytes!("NotoSansSC-status-subset.ttf");

const WIDTH: u32 = 1600;
const HEIGHT: u32 = 900;

fn title_suffix() -> &'static str {
    mibot_i18n::tr(" · Status", " · 运行状态")
}
fn label_online() -> &'static str {
    mibot_i18n::tr("Online", "在线")
}
fn health_alert() -> &'static str {
    mibot_i18n::tr("Alert", "资源告警")
}
fn health_warning() -> &'static str {
    mibot_i18n::tr("Attention", "需要关注")
}
fn health_normal() -> &'static str {
    mibot_i18n::tr("Healthy", "运行正常")
}
fn gauge_cpu() -> &'static str {
    "CPU"
}
fn gauge_memory() -> &'static str {
    mibot_i18n::tr("Memory", "内存")
}
fn gauge_disk() -> &'static str {
    mibot_i18n::tr("Disk", "磁盘")
}
fn gauge_swap() -> &'static str {
    "Swap"
}

#[derive(Debug, Error)]
pub enum CardError {
    #[error("load statuscard font: {0}")]
    Font(String),
    #[error("allocate pixmap")]
    Pixmap,
    #[error("encode PNG: {0}")]
    Encode(String),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Gauge {
    pub percent: f64,
    pub known: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Card {
    pub name: String,
    pub uptime: Duration,
    pub cpu: Gauge,
    pub memory: Gauge,
    pub disk: Gauge,
    pub swap: Gauge,
    pub footer: String,
}

static PARSED_FONT: OnceLock<Result<FontRef<'static>, String>> = OnceLock::new();

fn load_font() -> Result<&'static FontRef<'static>, CardError> {
    let res = PARSED_FONT.get_or_init(|| FontRef::try_from_slice(FONT_DATA).map_err(|e| e.to_string()));
    res.as_ref().map_err(|e| CardError::Font(e.clone()))
}

fn hex_color(value: u32, alpha: f32) -> Color {
    let r = ((value >> 16) & 0xff) as f32 / 255.0;
    let g = ((value >> 8) & 0xff) as f32 / 255.0;
    let b = (value & 0xff) as f32 / 255.0;
    Color::from_rgba(r, g, b, alpha).unwrap_or(Color::BLACK)
}

fn hex_rgb(value: u32) -> [u8; 3] {
    [
        ((value >> 16) & 0xff) as u8,
        ((value >> 8) & 0xff) as u8,
        (value & 0xff) as u8,
    ]
}

pub fn health(card: &Card) -> (&'static str, u32) {
    let mut highest = 0.0_f64;
    for g in [card.cpu, card.memory, card.disk, card.swap] {
        if g.known {
            highest = highest.max(g.percent);
        }
    }
    if highest >= 90.0 {
        (health_alert(), 0xfb7185)
    } else if highest >= 75.0 {
        (health_warning(), 0xfbbf24)
    } else {
        (health_normal(), 0x34f59a)
    }
}

fn gauge_color(gauge: Gauge, normal: u32) -> u32 {
    if gauge.known && gauge.percent >= 90.0 {
        0xfb7185
    } else if gauge.known && gauge.percent >= 75.0 {
        0xfbbf24
    } else {
        normal
    }
}

#[derive(Clone, Copy)]
enum Align {
    Left,
    Center,
}

#[derive(Clone, Copy)]
struct RectBox {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    r: f32,
}

#[derive(Clone, Copy)]
struct ArcSpec {
    cx: f32,
    cy: f32,
    r: f32,
    a0: f32,
    a1: f32,
}

struct Canvas<'a> {
    pixmap: Pixmap,
    font: &'a FontRef<'static>,
    scale_factor: f32,
}

impl<'a> Canvas<'a> {
    fn new(font: &'a FontRef<'static>) -> Result<Self, CardError> {
        let pixmap = Pixmap::new(WIDTH, HEIGHT).ok_or(CardError::Pixmap)?;
        let scale_factor = font.height_unscaled() / font.units_per_em().unwrap_or(1000.0);
        Ok(Self {
            pixmap,
            font,
            scale_factor,
        })
    }

    fn px_scale(&self, size: f32) -> PxScale {
        PxScale::from(size * self.scale_factor)
    }

    fn background(&mut self) {
        let stops = vec![
            GradientStop::new(0.0, hex_color(0x071927, 1.0)),
            GradientStop::new(0.58, hex_color(0x071520, 1.0)),
            GradientStop::new(1.0, hex_color(0x05111a, 1.0)),
        ];
        if let Some(shader) = LinearGradient::new(
            Point::from_xy(0.0, 0.0),
            Point::from_xy(WIDTH as f32, HEIGHT as f32),
            stops,
            SpreadMode::Pad,
            Transform::identity(),
        ) {
            let paint = Paint {
                shader,
                ..Default::default()
            };
            if let Some(rect) = tiny_skia::Rect::from_xywh(0.0, 0.0, WIDTH as f32, HEIGHT as f32) {
                self.pixmap
                    .fill_rect(rect, &paint, Transform::identity(), None);
            }
        }
    }

    fn grid(&mut self) {
        let mut paint = Paint::default();
        paint.set_color_rgba8(39, 103, 139, (255.0 * 0.14) as u8);
        paint.anti_alias = true;
        let stroke = Stroke {
            width: 1.0,
            ..Default::default()
        };
        let mut pb = PathBuilder::new();
        for x in (48..WIDTH).step_by(64) {
            pb.move_to(x as f32, 48.0);
            pb.line_to(x as f32, (HEIGHT - 48) as f32);
        }
        for y in (48..HEIGHT).step_by(64) {
            pb.move_to(48.0, y as f32);
            pb.line_to((WIDTH - 48) as f32, y as f32);
        }
        if let Some(path) = pb.finish() {
            self.pixmap
                .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }

    fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, color: Color) {
        let mut pb = PathBuilder::new();
        pb.move_to(x0, y0);
        pb.line_to(x1, y1);
        if let Some(path) = pb.finish() {
            let mut paint = Paint::default();
            paint.set_color(color);
            paint.anti_alias = true;
            let stroke = Stroke {
                width,
                line_cap: LineCap::Butt,
                ..Default::default()
            };
            self.pixmap
                .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }

    fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, color: Color) {
        if let Some(path) = PathBuilder::from_circle(cx, cy, r) {
            let mut paint = Paint::default();
            paint.set_color(color);
            paint.anti_alias = true;
            self.pixmap.fill_path(
                &path,
                &paint,
                tiny_skia::FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }

    fn rounded_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
        let r = r.min(w / 2.0).min(h / 2.0);
        let k = 0.552_284_8 * r;
        let mut pb = PathBuilder::new();
        pb.move_to(x + r, y);
        pb.line_to(x + w - r, y);
        pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
        pb.line_to(x + w, y + h - r);
        pb.cubic_to(
            x + w,
            y + h - r + k,
            x + w - r + k,
            y + h,
            x + w - r,
            y + h,
        );
        pb.line_to(x + r, y + h);
        pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
        pb.line_to(x, y + r);
        pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
        pb.close();
        pb.finish()
    }

    fn fill_rounded_rect(&mut self, rect: RectBox, color: Color) {
        if let Some(path) = Self::rounded_rect_path(rect.x, rect.y, rect.w, rect.h, rect.r) {
            let mut paint = Paint::default();
            paint.set_color(color);
            paint.anti_alias = true;
            self.pixmap.fill_path(
                &path,
                &paint,
                tiny_skia::FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }

    fn stroke_rounded_rect(&mut self, rect: RectBox, line_width: f32, color: Color) {
        if let Some(path) = Self::rounded_rect_path(rect.x, rect.y, rect.w, rect.h, rect.r) {
            let mut paint = Paint::default();
            paint.set_color(color);
            paint.anti_alias = true;
            let stroke = Stroke {
                width: line_width,
                ..Default::default()
            };
            self.pixmap
                .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }

    fn stroke_arc(&mut self, arc: ArcSpec, line_width: f32, color: Color, round: bool) {
        let steps = ((arc.a1 - arc.a0).abs() * arc.r / 4.0).max(16.0) as usize;
        let mut pb = PathBuilder::new();
        for step in 0..=steps {
            let angle = arc.a0 + (arc.a1 - arc.a0) * (step as f32 / steps as f32);
            let px = arc.cx + arc.r * angle.cos();
            let py = arc.cy + arc.r * angle.sin();
            if step == 0 {
                pb.move_to(px, py);
            } else {
                pb.line_to(px, py);
            }
        }
        if let Some(path) = pb.finish() {
            let mut paint = Paint::default();
            paint.set_color(color);
            paint.anti_alias = true;
            let stroke = Stroke {
                width: line_width,
                line_cap: if round {
                    LineCap::Round
                } else {
                    LineCap::Butt
                },
                ..Default::default()
            };
            self.pixmap
                .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }

    fn radio_mark(&mut self, x: f32, y: f32) {
        let tone = hex_color(0x22d3ee, 1.0);
        let pi = std::f32::consts::PI;
        self.stroke_arc(
            ArcSpec { cx: x, cy: y, r: 40.0, a0: pi * 1.08, a1: pi * 1.55 },
            9.0,
            tone,
            true,
        );
        self.stroke_arc(
            ArcSpec { cx: x, cy: y, r: 22.0, a0: pi * 1.08, a1: pi * 1.55 },
            9.0,
            tone,
            true,
        );
        self.fill_circle(x - 35.0, y + 32.0, 7.0, hex_color(0xe2e8f0, 1.0));
    }

    fn measure_width(&self, text: &str, size: f32) -> f32 {
        let scaled = self.font.as_scaled(self.px_scale(size));
        let mut width = 0.0_f32;
        let mut prev: Option<GlyphId> = None;
        for ch in text.chars() {
            let gid = self.font.glyph_id(ch);
            if let Some(p) = prev {
                width += scaled.kern(p, gid);
            }
            width += scaled.h_advance(gid);
            prev = Some(gid);
        }
        width
    }

    fn fitted(&self, value: &str, max_width: f32, maximum: f32, minimum: f32) -> (String, f32) {
        let mut size = maximum;
        while size >= minimum {
            if self.measure_width(value, size).round() <= max_width {
                return (value.to_owned(), size);
            }
            size -= 2.0;
        }
        let mut chars: Vec<char> = value.chars().collect();
        while !chars.is_empty() {
            let mut candidate: String = chars.iter().collect();
            candidate.push('…');
            if self.measure_width(&candidate, minimum).round() <= max_width {
                return (candidate, minimum);
            }
            chars.pop();
        }
        ("…".to_owned(), minimum)
    }

    fn draw_text(&mut self, value: &str, size: f32, mut x: f32, y: f32, rgb: [u8; 3], align: Align) {
        let px_scale = self.px_scale(size);
        let font = self.font;
        let scaled = font.as_scaled(px_scale);
        let baseline = y + (scaled.ascent() + scaled.descent()) / 2.0;
        if matches!(align, Align::Center) {
            x -= self.measure_width(value, size) / 2.0;
        }

        let pw = self.pixmap.width() as i32;
        let ph = self.pixmap.height() as i32;
        let data = self.pixmap.data_mut();
        let mut pen_x = x;
        let mut prev: Option<GlyphId> = None;

        for ch in value.chars() {
            let gid = font.glyph_id(ch);
            if let Some(p) = prev {
                pen_x += scaled.kern(p, gid);
            }
            let glyph = gid.with_scale_and_position(
                px_scale,
                ab_glyph::point(pen_x, baseline),
            );
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                let ox = bounds.min.x as i32;
                let oy = bounds.min.y as i32;
                outlined.draw(|gx, gy, cov| {
                    let px = ox + gx as i32;
                    let py = oy + gy as i32;
                    if px < 0 || py < 0 || px >= pw || py >= ph {
                        return;
                    }
                    let alpha = cov.clamp(0.0, 1.0);
                    if alpha <= 0.0 {
                        return;
                    }
                    let idx = ((py * pw + px) * 4) as usize;
                    let inv = 1.0 - alpha;
                    data[idx] = (f32::from(rgb[0]) * alpha + f32::from(data[idx]) * inv)
                        .round()
                        .clamp(0.0, 255.0) as u8;
                    data[idx + 1] = (f32::from(rgb[1]) * alpha + f32::from(data[idx + 1]) * inv)
                        .round()
                        .clamp(0.0, 255.0) as u8;
                    data[idx + 2] = (f32::from(rgb[2]) * alpha + f32::from(data[idx + 2]) * inv)
                        .round()
                        .clamp(0.0, 255.0) as u8;
                    data[idx + 3] = (255.0 * alpha + f32::from(data[idx + 3]) * inv)
                        .round()
                        .clamp(0.0, 255.0) as u8;
                });
            }
            pen_x += scaled.h_advance(gid);
            prev = Some(gid);
        }
    }

    fn gauge(&mut self, x: f32, y: f32, label: &str, value: Gauge, tone_hex: u32) {
        const BOX_W: f32 = 336.0;
        const BOX_H: f32 = 250.0;
        let box_rect = RectBox { x, y, w: BOX_W, h: BOX_H, r: 28.0 };
        self.fill_rounded_rect(
            box_rect,
            Color::from_rgba8(10, 35, 53, (255.0 * 0.86) as u8),
        );
        self.stroke_rounded_rect(
            box_rect,
            2.0,
            Color::from_rgba8(56, 139, 190, (255.0 * 0.48) as u8),
        );
        let cx = x + 104.0;
        let cy = y + 125.0;
        let radius = 68.0;
        let tau = std::f32::consts::TAU;
        self.stroke_arc(
            ArcSpec { cx, cy, r: radius, a0: 0.0, a1: tau },
            18.0,
            hex_color(0x213a4d, 1.0),
            false,
        );
        if value.known && value.percent > 0.0 {
            let start = -std::f32::consts::FRAC_PI_2;
            let end = start + tau * (value.percent.min(100.0) as f32) / 100.0;
            let arc = ArcSpec { cx, cy, r: radius, a0: start, a1: end };
            for layer in (1..=3).rev() {
                self.stroke_arc(arc, 18.0 + layer as f32 * 8.0, hex_color(tone_hex, 0.07), true);
            }
            self.stroke_arc(arc, 18.0, hex_color(tone_hex, 1.0), true);
        }
        let percent = if value.known {
            format!("{}%", value.percent.round() as i64)
        } else {
            "--".to_owned()
        };
        self.draw_text(&percent, 38.0, cx, cy + 2.0, hex_rgb(0xf1f5f9), Align::Center);
        self.line(
            x + 205.0,
            y + 61.0,
            x + 205.0,
            y + BOX_H - 61.0,
            2.0,
            Color::from_rgba8(63, 132, 170, (255.0 * 0.55) as u8),
        );
        let maximum = if label == gauge_swap() { 43.0 } else { 42.0 };
        let (name, size) = self.fitted(label, 88.0, maximum, 20.0);
        self.draw_text(&name, size, x + 232.0, cy + 2.0, hex_rgb(0xf1f5f9), Align::Left);
    }
}

pub fn render(card: &Card) -> Result<Vec<u8>, CardError> {
    let font = load_font()?;
    let mut c = Canvas::new(font)?;

    c.background();
    c.grid();
    c.stroke_rounded_rect(
        RectBox {
            x: 30.0,
            y: 30.0,
            w: (WIDTH - 60) as f32,
            h: (HEIGHT - 60) as f32,
            r: 34.0,
        },
        4.0,
        hex_color(0x0e83bd, 1.0),
    );
    c.radio_mark(120.0, 140.0);

    let (title, title_size) = c.fitted(&format!("{}{}", card.name, title_suffix()), 595.0, 68.0, 42.0);
    c.draw_text(&title, title_size, 180.0, 150.0, hex_rgb(0xf1f5f9), Align::Left);
    c.line(82.0, 235.0, 755.0, 235.0, 3.0, hex_color(0x215a78, 1.0));

    let (label, tone) = health(card);
    for layer in (1..=4).rev() {
        let radius = 42.0 + layer as f32 * 6.0;
        c.fill_circle(128.0, 370.0, radius, hex_color(tone, 0.08));
    }
    c.fill_circle(128.0, 370.0, 42.0, hex_color(tone, 1.0));
    let (health_txt, health_size) = c.fitted(label, 520.0, 68.0, 44.0);
    c.draw_text(&health_txt, health_size, 205.0, 374.0, hex_rgb(tone), Align::Left);

    let (online, online_size) = c.fitted(label_online(), 100.0, 42.0, 24.0);
    c.draw_text(&online, online_size, 88.0, 555.0, hex_rgb(0x9fb2c5), Align::Left);

    let (uptime, uptime_size) =
        c.fitted(&mibot_sysinfo::format_uptime(card.uptime), 500.0, 68.0, 48.0);
    c.draw_text(&uptime, uptime_size, 205.0, 555.0, hex_rgb(0xf1f5f9), Align::Left);

    c.gauge(810.0, 115.0, gauge_cpu(), card.cpu, gauge_color(card.cpu, 0x34f59a));
    c.gauge(1174.0, 115.0, gauge_memory(), card.memory, gauge_color(card.memory, 0x34f59a));
    c.gauge(810.0, 395.0, gauge_disk(), card.disk, gauge_color(card.disk, 0xfbbf24));
    c.gauge(1174.0, 395.0, gauge_swap(), card.swap, gauge_color(card.swap, 0x22d3ee));

    c.line(82.0, 732.0, 1518.0, 732.0, 3.0, hex_color(0x215a78, 1.0));
    let (footer, footer_size) = c.fitted(&card.footer, 1370.0, 34.0, 24.0);
    c.draw_text(
        &footer,
        footer_size,
        WIDTH as f32 / 2.0,
        805.0,
        hex_rgb(0xb8c7d7),
        Align::Center,
    );

    c.pixmap
        .encode_png()
        .map_err(|e| CardError::Encode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_covers_card_text() {
        let font = load_font().expect("load embedded subset font");
        for lang in [mibot_i18n::Lang::En, mibot_i18n::Lang::Zh] {
            mibot_i18n::with_lang(lang, || {
                let samples = [
                    "MiBot Lite v0.3.11  ·  Rust 1.99.1  ·  grammers 0.12.0",
                    title_suffix(),
                    label_online(),
                    health_alert(),
                    health_warning(),
                    health_normal(),
                    gauge_cpu(),
                    gauge_memory(),
                    gauge_disk(),
                    gauge_swap(),
                    "0123456789d天 :%-…",
                ];
                for text in samples {
                    for ch in text.chars() {
                        assert_ne!(
                            font.glyph_id(ch),
                            GlyphId(0),
                            "missing glyph '{ch}' (U+{:04X}) in subset font",
                            ch as u32
                        );
                    }
                }
            });
        }
    }

    #[test]
    fn render_status_card_and_health_dot() {
        let card = Card {
            name: "MiBot Lite".into(),
            uptime: Duration::from_secs(50 * 3600 + 3 * 60 + 4),
            cpu: Gauge {
                percent: 12.0,
                known: true,
            },
            memory: Gauge {
                percent: 80.0,
                known: true,
            },
            disk: Gauge {
                percent: 95.0,
                known: true,
            },
            swap: Gauge::default(),
            footer: "MiBot Lite 0.3.11 · Rust 1.99.1 · grammers 0.12.0".into(),
        };
        let png = render(&card).expect("render card");
        let pixmap = Pixmap::decode_png(&png).expect("decode PNG");
        assert_eq!((pixmap.width(), pixmap.height()), (1600, 900));
        assert_eq!(health(&card).0, "Alert");
        let idx = ((370 * pixmap.width() + 128) * 4) as usize;
        let px = &pixmap.data()[idx..idx + 4];
        assert_eq!((px[0], px[1], px[2]), (0xfb, 0x71, 0x85));
    }

    #[test]
    fn health_thresholds() {
        assert_eq!(
            health(&Card {
                cpu: Gauge {
                    percent: 10.0,
                    known: true
                },
                ..Default::default()
            })
            .0,
            "Healthy"
        );
        assert_eq!(
            health(&Card {
                cpu: Gauge {
                    percent: 75.0,
                    known: true
                },
                ..Default::default()
            })
            .0,
            "Attention"
        );
        assert_eq!(
            health(&Card {
                cpu: Gauge {
                    percent: 90.0,
                    known: true
                },
                ..Default::default()
            })
            .0,
            "Alert"
        );
        assert_eq!(health(&Card::default()).0, "Healthy");
    }
}
