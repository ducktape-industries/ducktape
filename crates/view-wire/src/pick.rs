//! Copied native pick-list metrics and handles. Fonts remain host-owned.
use super::*;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PickOptions {
    pub menu_height: Option<Length>,
    pub padding: Option<f32>,
    pub text_size: Option<f32>,
    pub line_height: Option<f32>,
    pub shaping: Option<Shaping>,
    pub font: Option<NamedFont>,
    pub handle: Option<PickHandle>,
    pub on_open: Option<u32>,
    pub on_close: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PickHandle {
    Arrow { size: Option<f32> },
    Static(PickIcon),
    Dynamic { closed: PickIcon, open: PickIcon },
    None,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PickIcon {
    pub code_point: char,
    pub font: Option<NamedFont>,
    pub size: Option<f32>,
    pub line_height: Option<f32>,
    pub shaping: Option<Shaping>,
}

pub(super) fn text_size(value: &mut Option<f32>) {
    if let Some(value) = value {
        *value = bounded(*value).clamp(f32::EPSILON, MAX_TEXT_PIXELS);
    }
}
pub(super) fn line_height(value: &mut Option<f32>) {
    if let Some(value) = value {
        *value = bounded(*value).clamp(f32::EPSILON, MAX_PIXELS / MAX_TEXT_PIXELS);
    }
}
impl PickIcon {
    fn sanitize(&mut self, text: &mut usize) {
        text_size(&mut self.size);
        line_height(&mut self.line_height);
        if let Some(font) = &mut self.font {
            font.sanitize(text);
        }
    }
}
impl PickOptions {
    pub(super) fn sanitize(&mut self, text: &mut usize) {
        if let Some(Length::Fixed(value)) = &mut self.menu_height {
            *value = bounded(*value);
        }
        bound_optional(&mut self.padding);
        text_size(&mut self.text_size);
        line_height(&mut self.line_height);
        if let Some(font) = &mut self.font {
            font.sanitize(text);
        }
        match &mut self.handle {
            Some(PickHandle::Arrow { size }) => text_size(size),
            Some(PickHandle::Static(icon)) => icon.sanitize(text),
            Some(PickHandle::Dynamic { closed, open }) => {
                closed.sanitize(text);
                open.sanitize(text);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pick_metrics_are_bounded_and_fonts_share_text_budget() {
        let font = NamedFont {
            family: FontFamily::Named("ééé".into()),
            weight: Weight::Normal,
            stretch: FontStretch::Normal,
            style: FontStyle::Normal,
        };
        let icon = PickIcon {
            code_point: '▼',
            font: Some(font.clone()),
            size: Some(f32::MAX),
            line_height: Some(f32::MAX),
            shaping: None,
        };
        let mut options = PickOptions {
            menu_height: Some(Length::Fixed(f32::INFINITY)),
            padding: Some(f32::NAN),
            text_size: Some(f32::MAX),
            line_height: Some(-1.0),
            font: Some(font),
            handle: Some(PickHandle::Dynamic {
                closed: icon.clone(),
                open: icon,
            }),
            ..Default::default()
        };
        let mut budget = 9;
        options.sanitize(&mut budget);
        assert_eq!(options.menu_height, Some(Length::Fixed(MAX_PIXELS)));
        assert_eq!(options.padding, Some(0.0));
        assert_eq!(options.text_size, Some(MAX_TEXT_PIXELS));
        assert_eq!(options.line_height, Some(f32::EPSILON));
        let Some(PickHandle::Dynamic { closed, open }) = options.handle else {
            panic!()
        };
        assert_eq!(closed.size, Some(MAX_TEXT_PIXELS));
        assert_eq!(closed.line_height, Some(MAX_PIXELS / MAX_TEXT_PIXELS));
        assert_eq!(closed.font.unwrap().family, FontFamily::Named("é".into()));
        assert_eq!(open.font.unwrap().family, FontFamily::Named(String::new()));
        assert_eq!(budget, 1);
    }
}
