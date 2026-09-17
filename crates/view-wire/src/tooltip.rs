//! Copied native tooltip options and concrete container style.
use super::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum TooltipPosition {
    #[default]
    Top,
    Bottom,
    Left,
    Right,
    FollowCursor,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum TooltipPreset {
    #[default]
    Transparent,
    Rounded,
    Bordered,
    Dark,
    Primary,
    Secondary,
    Success,
    Warning,
    Danger,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TooltipStyle {
    pub preset: TooltipPreset,
    pub background: Option<Rgba>,
    pub text: Option<Rgba>,
    pub border: Option<Border>,
    pub shadow: Shadow,
    pub pixel_snap: Option<bool>,
}

impl TooltipStyle {
    pub(super) fn sanitize(&mut self) {
        bound_color(&mut self.background);
        bound_color(&mut self.text);
        bound_border(&mut self.border);
        self.shadow.sanitize();
    }
}
