//! The palette: greys for everything, one warm accent for what satisfies a
//! goal, one red for what failed. State reads from brightness and shape,
//! not from hue.

use gpui::{Hsla, Rgba, rgb};

/// Window background.
pub(crate) fn background() -> Rgba {
    rgb(0x000a_0a0b)
}

/// Selected / emphasised text.
pub(crate) fn bright() -> Rgba {
    rgb(0x00ed_edef)
}

/// Body text.
pub(crate) fn text() -> Rgba {
    rgb(0x008e_8e93)
}

/// Quiet text.
pub(crate) fn quiet() -> Rgba {
    rgb(0x0052_5257)
}

/// Barely there: idle edges, pruned nodes.
pub(crate) fn ghost() -> Rgba {
    rgb(0x002a_2a2e)
}

/// Satisfies a goal.
pub(crate) fn accent() -> Rgba {
    rgb(0x00f5_a524)
}

/// Failed.
pub(crate) fn failure() -> Rgba {
    rgb(0x00e5_484d)
}

/// `color` at `alpha`.
pub(crate) fn alpha(color: Rgba, alpha: f32) -> Hsla {
    Hsla::from(color).opacity(alpha)
}
