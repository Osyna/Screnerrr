//! Curated one-click frames. Each look is a complete decision (backdrop, margin, corners, edge,
//! shadow) authored for an 800 px short side and scaled to the screenshot it is applied to, so a
//! phone capture and a 4K desktop both get the same proportions. Layout (canvas and output size)
//! is never part of a look.

use crate::presentation::{Background, MAX_BORDER, MAX_PADDING, MAX_RADIUS, Presentation};

const REFERENCE_SIDE: f64 = 800.0;
const NONE: [f64; 4] = [1.0, 1.0, 1.0, 1.0];
const INK: [f64; 4] = [0.07, 0.07, 0.08, 1.0];
const WHITE: [f64; 4] = [1.0, 1.0, 1.0, 1.0];

pub struct Look {
    pub group: &'static str,
    pub name: &'static str,
    pub note: &'static str,
    padding: u32,
    radius: f64,
    shadow: f64,
    border: u32,
    edge: [f64; 4],
    background: Background,
}

const fn c(r: f64, g: f64, b: f64) -> [f64; 4] {
    [r, g, b, 1.0]
}

const fn grad(from: [f64; 4], to: [f64; 4]) -> Background {
    Background::Gradient(from, to)
}

const fn look(
    group: &'static str,
    name: &'static str,
    note: &'static str,
    (padding, radius, shadow): (u32, f64, f64),
    (border, edge): (u32, [f64; 4]),
    background: Background,
) -> Look {
    Look {
        group,
        name,
        note,
        padding,
        radius,
        shadow,
        border,
        edge,
        background,
    }
}

/// Groups are contiguous: the designer starts a new heading whenever `group` changes.
#[rustfmt::skip]
pub static LOOKS: [Look; 28] = [
    look("Featured", "Snow", "White backdrop, soft shadow", (56, 14.0, 0.5), (0, NONE), Background::Solid(c(1.0, 1.0, 1.0))),
    look("Featured", "Ocean", "Blue to aqua", (64, 16.0, 0.9), (0, NONE), grad(c(0.13, 0.55, 0.95), c(0.40, 0.85, 0.90))),
    look("Featured", "Midnight", "Near-black backdrop with a faint edge light", (72, 16.0, 1.0), (1, [1.0, 1.0, 1.0, 0.14]), Background::Solid(c(0.05, 0.07, 0.10))),
    look("Featured", "Sunset", "Orange to raspberry", (64, 16.0, 0.9), (0, NONE), grad(c(1.0, 0.55, 0.30), c(0.85, 0.20, 0.55))),
    look("Studio", "Plain", "The screenshot alone, no frame", (0, 0.0, 0.0), (0, NONE), Background::Transparent),
    look("Studio", "Float", "Transparent backdrop with a soft drop shadow", (40, 12.0, 0.8), (0, NONE), Background::Transparent),
    look("Studio", "Paper", "Warm off-white backdrop for documents", (64, 10.0, 0.4), (0, NONE), Background::Solid(c(0.96, 0.94, 0.90))),
    look("Studio", "Mist", "Cool grey backdrop for product UI", (64, 12.0, 0.6), (0, NONE), Background::Solid(c(0.88, 0.91, 0.94))),
    look("Studio", "Gallery", "Wide mat with a hairline keyline", (96, 2.0, 0.0), (1, c(0.78, 0.78, 0.76)), Background::Solid(c(0.98, 0.98, 0.97))),
    look("Studio", "Graphite", "Dark backdrop, deep shadow", (64, 14.0, 1.0), (0, NONE), Background::Solid(c(0.13, 0.15, 0.17))),
    look("Gradients", "Violet", "Indigo to pink", (64, 16.0, 0.9), (0, NONE), grad(c(0.45, 0.30, 0.95), c(0.95, 0.45, 0.85))),
    look("Gradients", "Mint", "Green to lime", (64, 16.0, 0.8), (0, NONE), grad(c(0.20, 0.80, 0.60), c(0.80, 0.95, 0.50))),
    look("Gradients", "Aurora", "Teal to deep blue", (72, 18.0, 1.0), (0, NONE), grad(c(0.10, 0.75, 0.65), c(0.20, 0.35, 0.80))),
    look("Gradients", "Peach", "Light apricot to rose", (56, 14.0, 0.6), (0, NONE), grad(c(1.0, 0.80, 0.65), c(0.98, 0.60, 0.70))),
    look("Gradients", "Dusk", "Night plum to copper", (72, 16.0, 1.0), (0, NONE), grad(c(0.18, 0.15, 0.35), c(0.75, 0.35, 0.45))),
    look("Gradients", "Forest", "Deep green to leaf", (64, 14.0, 1.0), (0, NONE), grad(c(0.05, 0.30, 0.28), c(0.45, 0.75, 0.45))),
    look("Gradients", "Citrus", "Lemon to orange", (56, 14.0, 0.7), (0, NONE), grad(c(1.0, 0.85, 0.30), c(0.95, 0.50, 0.20))),
    look("Gradients", "Steel", "Slate to silver", (64, 12.0, 0.9), (0, NONE), grad(c(0.20, 0.25, 0.30), c(0.55, 0.62, 0.68))),
    look("Bold", "Poster", "Yellow block with a heavy black edge", (56, 0.0, 0.0), (6, INK), Background::Solid(c(1.0, 0.82, 0.15))),
    look("Bold", "Pop", "Pink block, black outline", (56, 6.0, 0.0), (4, INK), Background::Solid(c(1.0, 0.40, 0.62))),
    look("Bold", "Signal", "Red backdrop with a white outline", (48, 10.0, 0.5), (4, WHITE), Background::Solid(c(0.92, 0.18, 0.18))),
    look("Bold", "Blueprint", "Royal blue with a crisp white edge", (56, 4.0, 0.0), (2, WHITE), Background::Solid(c(0.10, 0.25, 0.75))),
    look("Bold", "Ink", "Black backdrop with a white outline", (48, 0.0, 0.0), (3, WHITE), Background::Solid(INK)),
    look("Soft", "Sky", "Pale blue, generous corners", (72, 24.0, 0.5), (0, NONE), Background::Solid(c(0.82, 0.90, 0.98))),
    look("Soft", "Blush", "Pale pink wash", (72, 22.0, 0.5), (0, NONE), grad(c(0.99, 0.91, 0.90), c(0.98, 0.82, 0.88))),
    look("Soft", "Sage", "Muted green, calm shadow", (72, 20.0, 0.4), (0, NONE), Background::Solid(c(0.84, 0.90, 0.84))),
    look("Soft", "Haze", "Lavender to rose", (72, 22.0, 0.5), (0, NONE), grad(c(0.88, 0.84, 0.97), c(0.97, 0.86, 0.93))),
    look("Soft", "Tight Cream", "Slim cream margin for crowded feeds", (28, 12.0, 0.3), (0, NONE), Background::Solid(c(0.99, 0.97, 0.92))),
];

impl Look {
    /// The frame for a `scene` (the cropped screenshot) of this size; canvas and output stay unset.
    pub fn frame(&self, (w, h): (u32, u32)) -> Presentation {
        let k = (f64::from(w.min(h)) / REFERENCE_SIDE).clamp(0.4, 3.0);
        let px = |v: u32, max: u32| {
            if v == 0 {
                0
            } else {
                ((f64::from(v) * k).round() as u32).clamp(1, max)
            }
        };
        Presentation {
            padding: px(self.padding, MAX_PADDING),
            border: px(self.border, MAX_BORDER),
            border_color: self.edge,
            radius: (self.radius * k).min(MAX_RADIUS),
            shadow: self.shadow,
            background: self.background,
            ..Presentation::default()
        }
    }

    /// Whether `p` is this look as it would be applied to `scene`; the edge colour only counts
    /// when there is an edge.
    pub fn matches(&self, scene: (u32, u32), p: &Presentation) -> bool {
        let f = self.frame(scene);
        f.padding == p.padding
            && f.radius == p.radius
            && f.shadow == p.shadow
            && f.border == p.border
            && (f.border == 0 || f.border_color == p.border_color)
            && f.background == p.background
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_a_frame_clears_its_matching_look() {
        let scene = (1280, 720);
        let last = &LOOKS[LOOKS.len() - 1];
        let mut pres = last.frame(scene);
        assert!(last.matches(scene, &pres));
        pres.padding += 1;
        assert!(!last.matches(scene, &pres));
    }
}
