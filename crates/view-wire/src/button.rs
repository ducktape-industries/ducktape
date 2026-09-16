//! Copied native button preset and recipe values.
use super::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum ButtonPreset {
    #[default]
    Primary,
    Secondary,
    Success,
    Warning,
    Danger,
    Text,
    Background,
    Subtle,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ButtonRecipe {
    pub base: Face,
    pub hover_background: Option<Rgba>,
    pub pressed_background: Option<Rgba>,
    pub disabled_background: Option<Rgba>,
    pub disabled_text: Option<Rgba>,
    pub disabled_opacity: Option<f32>,
    pub focus_ring: Option<Rgba>,
    pub text_size: Option<f32>,
    pub line_height: Option<f32>,
    pub font: Option<NamedFont>,
}

impl ButtonRecipe {
    pub(super) fn sanitize(&mut self, text_budget: &mut usize) {
        if let Some(font) = &mut self.font {
            font.sanitize(text_budget);
        }
        for color in [
            &mut self.base.background,
            &mut self.base.text,
            &mut self.hover_background,
            &mut self.pressed_background,
            &mut self.disabled_background,
            &mut self.disabled_text,
            &mut self.focus_ring,
        ] {
            bound_color(color);
        }
        bound_border(&mut self.base.border);
        if let Some(value) = &mut self.disabled_opacity {
            *value = bounded(*value).min(1.0);
        }
        if let Some(value) = &mut self.text_size {
            *value = bounded(*value).min(MAX_TEXT_PIXELS);
        }
        if let Some(value) = &mut self.line_height {
            *value = bounded(*value).clamp(f32::EPSILON, MAX_PIXELS / MAX_TEXT_PIXELS);
        }
    }
}
