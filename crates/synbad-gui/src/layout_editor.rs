//! Drag-and-drop screen layout editor.
//!
//! Screens are rendered as draggable rectangles on a panable canvas. After
//! any drag, links are recomputed from spatial adjacency — when two screens
//! share an edge (within `SNAP_PX`) and their perpendicular ranges overlap,
//! we emit a directional link in both directions. This means the user
//! never edits links explicitly; the layout *is* the link set.

use egui::{Color32, Pos2, Rect, Sense, Stroke, Vec2};

use synbad_config::{Config, Link, Side};

const SNAP_PX: f32 = 14.0;
const GRID: f32 = 20.0;

pub struct LayoutEditor {
    pan: Vec2,
    dragging: Option<DragState>,
    pending_delete: Option<usize>,
}

struct DragState {
    idx: usize,
    offset_in_screen: Vec2,
}

impl Default for LayoutEditor {
    fn default() -> Self {
        LayoutEditor {
            pan: Vec2::ZERO,
            dragging: None,
            pending_delete: None,
        }
    }
}

impl LayoutEditor {
    /// Recenter the canvas. Cheap escape hatch when the user has panned
    /// the screens off-view and lost sight of them.
    pub fn reset_view(&mut self) {
        self.pan = Vec2::ZERO;
    }

    /// Returns `true` if the config changed (drag, delete).
    pub fn show(&mut self, ui: &mut egui::Ui, config: &mut Config) -> bool {
        let mut dirty = false;
        let available = ui.available_rect_before_wrap();
        let (response, painter) = ui.allocate_painter(available.size(), Sense::click_and_drag());
        let canvas = response.rect;
        let origin = canvas.center() + self.pan;

        // Background grid.
        painter.rect_filled(canvas, 0.0, Color32::from_gray(20));
        draw_grid(&painter, canvas, origin);

        // Middle-button (or right-button without target) drag pans.
        if response.dragged_by(egui::PointerButton::Middle) {
            self.pan += response.drag_delta();
        }

        // Build screen rects in screen space and pick the hovered screen.
        let mut rects: Vec<Rect> = Vec::with_capacity(config.screens.len());
        for s in &config.screens {
            let r = Rect::from_min_size(
                origin + Vec2::new(s.position.x as f32, s.position.y as f32),
                Vec2::new(s.position.w as f32, s.position.h as f32),
            );
            rects.push(r);
        }
        let hovered_idx = ui.input(|i| i.pointer.hover_pos()).and_then(|p| {
            rects
                .iter()
                .enumerate()
                .rev()
                .find_map(|(i, r)| r.contains(p).then_some(i))
        });

        // Begin / continue / end drag.
        if response.drag_started_by(egui::PointerButton::Primary) {
            if let Some(idx) = hovered_idx {
                if let Some(p) = ui.input(|i| i.pointer.press_origin()) {
                    self.dragging = Some(DragState {
                        idx,
                        offset_in_screen: p - rects[idx].min,
                    });
                }
            }
        }
        if let Some(drag) = &self.dragging {
            if let Some(pos) = ui.input(|i| i.pointer.hover_pos()) {
                let new_min = pos - drag.offset_in_screen - origin.to_vec2();
                let snapped = Vec2::new(snap(new_min.x, GRID), snap(new_min.y, GRID));
                let s = &mut config.screens[drag.idx];
                if snapped.x as i32 != s.position.x || snapped.y as i32 != s.position.y {
                    s.position.x = snapped.x as i32;
                    s.position.y = snapped.y as i32;
                    dirty = true;
                }
            }
        }
        if response.drag_stopped() && self.dragging.take().is_some() {
            // Recompute rectangles after drag so adjacency uses final position.
            for (r, s) in rects.iter_mut().zip(config.screens.iter()) {
                *r = Rect::from_min_size(
                    origin + Vec2::new(s.position.x as f32, s.position.y as f32),
                    Vec2::new(s.position.w as f32, s.position.h as f32),
                );
            }
            dirty = true;
        }

        // Right-click → context menu (rename via name field outside, delete here).
        response.context_menu(|ui| {
            if let Some(idx) = hovered_idx {
                let name = config.screens[idx].name.clone();
                ui.label(format!("Screen: {}", name));
                if ui.button("Delete").clicked() {
                    self.pending_delete = Some(idx);
                    ui.close_menu();
                }
            } else {
                ui.label("Right-click a screen to delete it.");
            }
        });

        if let Some(idx) = self.pending_delete.take() {
            let removed = config.screens.remove(idx).name;
            config
                .links
                .retain(|l| l.from != removed && l.to != removed);
            if config.server_name == removed {
                config.server_name = config
                    .screens
                    .first()
                    .map(|s| s.name.clone())
                    .unwrap_or_default();
            }
            dirty = true;
        }

        // Recompute links from spatial adjacency every frame. Cheap (N^2 in
        // screen count) and keeps the link set in sync with what's drawn.
        let new_links = derive_links(&config.screens, &rects);
        if new_links != config.links {
            config.links = new_links;
            dirty = true;
        }

        // Draw screens.
        for (i, r) in rects.iter().enumerate() {
            let is_server = config.screens[i].name == config.server_name;
            let fill = if is_server {
                Color32::from_rgb(46, 92, 60)
            } else {
                Color32::from_rgb(40, 50, 70)
            };
            let stroke_color = if Some(i) == hovered_idx {
                Color32::YELLOW
            } else {
                Color32::from_gray(180)
            };
            painter.rect(*r, 4.0, fill, Stroke::new(1.5, stroke_color));
            painter.text(
                r.center(),
                egui::Align2::CENTER_CENTER,
                &config.screens[i].name,
                egui::FontId::proportional(14.0),
                Color32::WHITE,
            );
            if is_server {
                painter.text(
                    r.left_top() + Vec2::new(6.0, 4.0),
                    egui::Align2::LEFT_TOP,
                    "server",
                    egui::FontId::proportional(10.0),
                    Color32::LIGHT_GREEN,
                );
            }
        }

        // Draw link indicators on adjacent edges.
        for link in &config.links {
            let from = config.screens.iter().position(|s| s.name == link.from);
            let to = config.screens.iter().position(|s| s.name == link.to);
            if let (Some(a), Some(b)) = (from, to) {
                draw_link_edge(&painter, &rects[a], &rects[b], link.side);
            }
        }

        dirty
    }
}

fn snap(v: f32, step: f32) -> f32 {
    (v / step).round() * step
}

fn draw_grid(painter: &egui::Painter, rect: Rect, origin: Pos2) {
    let stroke = Stroke::new(1.0, Color32::from_rgb(35, 35, 40));
    let step = GRID;
    let start_x = origin.x - ((origin.x - rect.left()) / step).floor() * step;
    let start_y = origin.y - ((origin.y - rect.top()) / step).floor() * step;
    let mut x = start_x;
    while x < rect.right() {
        painter.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            stroke,
        );
        x += step;
    }
    let mut y = start_y;
    while y < rect.bottom() {
        painter.line_segment(
            [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
            stroke,
        );
        y += step;
    }
    // Origin crosshair.
    let cross = Stroke::new(1.0, Color32::from_gray(80));
    painter.line_segment(
        [
            Pos2::new(rect.left(), origin.y),
            Pos2::new(rect.right(), origin.y),
        ],
        cross,
    );
    painter.line_segment(
        [
            Pos2::new(origin.x, rect.top()),
            Pos2::new(origin.x, rect.bottom()),
        ],
        cross,
    );
}

fn draw_link_edge(painter: &egui::Painter, a: &Rect, b: &Rect, side: Side) {
    // The edge of `a` that links into `b`. Render a thicker stroke segment
    // over the overlap range so the user can see linked edges.
    let stroke = Stroke::new(3.0, Color32::from_rgb(120, 200, 255));
    match side {
        Side::Right => {
            let y0 = a.top().max(b.top());
            let y1 = a.bottom().min(b.bottom());
            painter.line_segment([Pos2::new(a.right(), y0), Pos2::new(a.right(), y1)], stroke);
        }
        Side::Left => {
            let y0 = a.top().max(b.top());
            let y1 = a.bottom().min(b.bottom());
            painter.line_segment([Pos2::new(a.left(), y0), Pos2::new(a.left(), y1)], stroke);
        }
        Side::Down => {
            let x0 = a.left().max(b.left());
            let x1 = a.right().min(b.right());
            painter.line_segment(
                [Pos2::new(x0, a.bottom()), Pos2::new(x1, a.bottom())],
                stroke,
            );
        }
        Side::Up => {
            let x0 = a.left().max(b.left());
            let x1 = a.right().min(b.right());
            painter.line_segment([Pos2::new(x0, a.top()), Pos2::new(x1, a.top())], stroke);
        }
    }
}

/// Walk all pairs and emit a directional link per adjacent edge.
/// `Config::generate_synergy_conf` adds the reverse, so we only emit the
/// "outgoing" side per pair.
fn derive_links(screens: &[synbad_config::Screen], rects: &[Rect]) -> Vec<synbad_config::Link> {
    let mut links = Vec::new();
    for i in 0..screens.len() {
        for j in 0..screens.len() {
            if i == j {
                continue;
            }
            let a = &rects[i];
            let b = &rects[j];
            // a.right shares b.left
            if (a.right() - b.left()).abs() < SNAP_PX
                && range_overlap(a.top(), a.bottom(), b.top(), b.bottom()) > 0.0
            {
                links.push(Link {
                    from: screens[i].name.clone(),
                    side: Side::Right,
                    to: screens[j].name.clone(),
                });
            } else if (a.bottom() - b.top()).abs() < SNAP_PX
                && range_overlap(a.left(), a.right(), b.left(), b.right()) > 0.0
            {
                links.push(Link {
                    from: screens[i].name.clone(),
                    side: Side::Down,
                    to: screens[j].name.clone(),
                });
            }
            // We deliberately skip Left/Up here — those are the reverse of
            // a Right/Down emitted by the partner. `generate_synergy_conf`
            // adds the reverse direction in the rendered .conf.
        }
    }
    links
}

fn range_overlap(a0: f32, a1: f32, b0: f32, b1: f32) -> f32 {
    (a1.min(b1) - a0.max(b0)).max(0.0)
}
