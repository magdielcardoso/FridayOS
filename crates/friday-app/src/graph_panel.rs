//! Knowledge-graph panel.
//!
//! A standalone borderless window that draws the friday-brain graph: nodes as
//! concentric rings, edges as hairlines, highlighted nodes in full red. Static
//! and deterministic — no physics, no timers, no interaction.
//!
//! Layout is a golden-angle spiral by node index, fitted to the canvas, with a
//! fixed zoom factor and pan offsets applied on top.

use std::collections::{HashMap, HashSet};

use friday_brain::{Edge, Node};
use gpui::prelude::*;
use gpui::{
    canvas, div, point, px, rgb, rgba, size, App, Bounds, Context, PathBuilder, Render,
    SharedString, Window, WindowBounds, WindowHandle, WindowKind, WindowOptions,
};

const BG: u32 = 0x050505;
const RED: u32 = 0xff2f2f;
const RED_DIM: u32 = 0x5e1414;
const RED_GHOST: u32 = 0x3b0d0d;

const TAU: f32 = std::f32::consts::TAU;
/// Golden angle, the same one sunflowers use.
const GOLDEN_ANGLE: f32 = 2.399_963_2;

const PANEL_WIDTH: f32 = 900.0;
const PANEL_HEIGHT: f32 = 700.0;

pub struct GraphPanel {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    highlight: HashSet<String>,
    zoom: f32,
    pan_x: f32,
    pan_y: f32,
}

impl GraphPanel {
    fn new(nodes: Vec<Node>, edges: Vec<Edge>, highlight: Vec<String>) -> Self {
        // Highlight entries may arrive as ids (from `resolve`) or as human
        // titles (from a manual call); keep both forms in lower case.
        let mut set = HashSet::new();
        for entry in highlight {
            let entry = entry.trim().to_lowercase();
            if !entry.is_empty() {
                set.insert(entry);
            }
        }
        Self {
            nodes,
            edges,
            highlight: set,
            zoom: 1.0,
            pan_x: 0.0,
            pan_y: 0.0,
        }
    }

    fn marked(&self, node: &Node) -> bool {
        self.highlight.contains(&node.id.to_lowercase())
            || self.highlight.contains(&node.title.to_lowercase())
    }

    fn highlighted_titles(&self) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|node| self.marked(node))
            .map(|node| node.title.clone())
            .collect()
    }
}

/// Opens the graph panel with the given node ids/titles highlighted.
pub fn open(cx: &mut App, highlight: Vec<String>) -> Result<WindowHandle<GraphPanel>, String> {
    // The knowledge store may not exist yet on a fresh machine; an empty panel
    // is more useful than a failed window.
    let nodes = friday_brain::all_nodes().unwrap_or_default();
    let edges = friday_brain::all_edges().unwrap_or_default();

    let bounds = Bounds::centered(None, size(px(PANEL_WIDTH), px(PANEL_HEIGHT)), cx);

    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: None,
            kind: WindowKind::PopUp,
            focus: false,
            show: true,
            is_movable: true,
            is_resizable: false,
            is_minimizable: false,
            window_background: gpui::WindowBackgroundAppearance::Opaque,
            ..Default::default()
        },
        move |_window, cx| cx.new(|_cx| GraphPanel::new(nodes, edges, highlight)),
    )
    .map_err(|error| format!("grafo: não foi possível abrir a janela: {error}"))
}

/// Resolves a free-form name to node ids, then opens the panel.
pub fn open_from_query(cx: &mut App, query: &str) -> Result<WindowHandle<GraphPanel>, String> {
    let mut ids = friday_brain::resolve(query).map_err(|error| error.to_string())?;
    if ids.is_empty() {
        // Nothing indexed under that name: fall back to a title match so the
        // panel still tries to spotlight it.
        ids.push(query.trim().to_string());
    }
    open(cx, ids)
}

impl Render for GraphPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let nodes = self.nodes.clone();
        let edges = self.edges.clone();
        let highlight = self.highlight.clone();
        let zoom = self.zoom;
        let pan_x = self.pan_x;
        let pan_y = self.pan_y;

        let node_count = self.nodes.len();
        let edge_count = self.edges.len();
        let mut titles = self.highlighted_titles();
        titles.truncate(10);
        let hidden = self.highlighted_titles().len().saturating_sub(titles.len());

        let graph = canvas(
            |_bounds, _window, _cx| {},
            move |bounds, _, window, _cx| {
                let width = f32::from(bounds.size.width);
                let height = f32::from(bounds.size.height);
                let origin_x = f32::from(bounds.origin.x);
                let origin_y = f32::from(bounds.origin.y);
                let center_x = origin_x + width / 2.0 + pan_x;
                let center_y = origin_y + height / 2.0 + pan_y;

                if nodes.is_empty() {
                    return;
                }

                // Unit layout: index 0 sits at the centre, the rest unwind on a
                // golden-angle spiral with r = sqrt(i).
                let mut positions: Vec<(f32, f32)> = Vec::with_capacity(nodes.len());
                let mut max_radius = 0.0f32;
                for index in 0..nodes.len() {
                    let angle = index as f32 * GOLDEN_ANGLE;
                    let radius = (index as f32).sqrt();
                    let x = radius * angle.cos();
                    let y = radius * angle.sin();
                    max_radius = max_radius.max(x.hypot(y));
                    positions.push((x, y));
                }
                if max_radius <= f32::EPSILON {
                    max_radius = 1.0;
                }

                // Fit the spiral inside the canvas, then apply the panel zoom.
                let fit = (width.min(height) * 0.40) / max_radius * zoom.max(0.05);
                let mut placed: HashMap<&str, (f32, f32)> = HashMap::with_capacity(nodes.len());
                for (node, (x, y)) in nodes.iter().zip(positions.iter()) {
                    placed.insert(node.id.as_str(), (center_x + x * fit, center_y + y * fit));
                }

                let marked: HashSet<&str> = nodes
                    .iter()
                    .filter(|node| {
                        highlight.contains(&node.id.to_lowercase())
                            || highlight.contains(&node.title.to_lowercase())
                    })
                    .map(|node| node.id.as_str())
                    .collect();

                for edge in &edges {
                    let (Some(&(x1, y1)), Some(&(x2, y2))) =
                        (placed.get(edge.src.as_str()), placed.get(edge.dst.as_str()))
                    else {
                        continue;
                    };
                    let both =
                        marked.contains(edge.src.as_str()) && marked.contains(edge.dst.as_str());
                    let either =
                        marked.contains(edge.src.as_str()) || marked.contains(edge.dst.as_str());
                    let (color, width) = if both {
                        (RED, 1.8)
                    } else if either {
                        (0x8a1c1c, 1.1)
                    } else {
                        (RED_DIM, 0.8)
                    };

                    let mut builder = PathBuilder::stroke(px(width));
                    builder.move_to(point(px(x1), px(y1)));
                    builder.line_to(point(px(x2), px(y2)));
                    if let Ok(path) = builder.build() {
                        window.paint_path(path, rgb(color));
                    }
                }

                for node in &nodes {
                    let Some(&(x, y)) = placed.get(node.id.as_str()) else {
                        continue;
                    };
                    if marked.contains(node.id.as_str()) {
                        paint_circle(window, x, y, 6.0, 1.8, RED);
                        paint_circle(window, x, y, 11.0, 0.8, RED_GHOST);
                        paint_circle(window, x, y, 1.4, 2.8, RED);
                    } else {
                        paint_circle(window, x, y, 3.0, 1.0, RED_DIM);
                    }
                }
            },
        )
        .size_full();

        let mut legend =
            div()
                .absolute()
                .top(px(14.0))
                .left(px(14.0))
                .flex()
                .flex_col()
                .gap(px(5.0))
                .px(px(12.0))
                .py(px(10.0))
                .bg(rgba(0x0a0505e6))
                .border_1()
                .border_color(rgb(RED_GHOST))
                .child(
                    div()
                        .text_size(px(10.5))
                        .text_color(rgb(RED))
                        .child(SharedString::from("▌ GRAFO DE CONHECIMENTO")),
                )
                .child(div().text_size(px(9.0)).text_color(rgb(RED_DIM)).child(
                    SharedString::from(format!(
                        "{node_count} nós · {edge_count} arestas · zoom {zoom:.2}"
                    )),
                ));

        if titles.is_empty() {
            legend = legend.child(
                div()
                    .text_size(px(9.0))
                    .text_color(rgb(0x7a1a1a))
                    .child(SharedString::from("nenhum nó em destaque")),
            );
        } else {
            for title in titles {
                legend = legend.child(
                    div()
                        .text_size(px(9.5))
                        .text_color(rgb(RED))
                        .child(SharedString::from(format!("◆ {title}"))),
                );
            }
            if hidden > 0 {
                legend = legend.child(
                    div()
                        .text_size(px(8.5))
                        .text_color(rgb(RED_DIM))
                        .child(SharedString::from(format!("+{hidden} outros"))),
                );
            }
        }

        legend = legend.child(
            div()
                .mt(px(4.0))
                .pt(px(6.0))
                .border_t_1()
                .border_color(rgb(RED_GHOST))
                .text_size(px(8.5))
                .text_color(rgb(0x7a1a1a))
                .child(SharedString::from(
                    "estático · círculos = nós · linhas = relações",
                )),
        );

        div()
            .relative()
            .size_full()
            .bg(rgb(BG))
            .font_family("Menlo")
            .child(div().absolute().inset_0().child(graph))
            .child(legend)
    }
}

fn paint_circle(window: &mut Window, cx: f32, cy: f32, radius: f32, width: f32, color: u32) {
    if radius <= 0.2 || width <= 0.0 {
        return;
    }
    let segments = ((radius * 3.0) as usize).clamp(16, 48);
    let mut builder = PathBuilder::stroke(px(width));
    for index in 0..=segments {
        let angle = index as f32 / segments as f32 * TAU;
        let p = point(px(cx + radius * angle.cos()), px(cy + radius * angle.sin()));
        if index == 0 {
            builder.move_to(p);
        } else {
            builder.line_to(p);
        }
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, rgb(color));
    }
}
