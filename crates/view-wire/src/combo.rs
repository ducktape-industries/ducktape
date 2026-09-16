//! Copied combo presentation and routes; the host retains native search state.
use super::*;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ComboOptions {
    pub menu_height: Option<Length>,
    pub padding: Option<f32>,
    pub text_size: Option<f32>,
    pub line_height: Option<f32>,
    pub shaping: Option<Shaping>,
    pub font: Option<NamedFont>,
    pub icon: Option<ComboIcon>,
    pub input: Option<u32>,
    pub hover: Option<u32>,
    pub open: Option<u32>,
    pub close: Option<u32>,
    pub style: InputStyle,
    pub menu: Option<MenuFace>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComboIcon {
    pub code_point: char,
    pub font: Option<NamedFont>,
    pub size: Option<f32>,
    pub spacing: f32,
    pub right: bool,
}
impl ComboOptions {
    pub(super) fn sanitize(&mut self, text: &mut usize) {
        if let Some(Length::Fixed(value)) = &mut self.menu_height {
            *value = bounded(*value);
        }
        bound_optional(&mut self.padding);
        pick::text_size(&mut self.text_size);
        pick::line_height(&mut self.line_height);
        if let Some(font) = &mut self.font {
            font.sanitize(text);
        }
        if let Some(icon) = &mut self.icon {
            pick::text_size(&mut icon.size);
            icon.spacing = bounded(icon.spacing);
            if let Some(font) = &mut icon.font {
                font.sanitize(text);
            }
        }
        self.style.sanitize();
        if let Some(menu) = &mut self.menu {
            menu.sanitize();
        }
    }
}
impl MenuFace {
    pub(super) fn sanitize(&mut self) {
        self.shadow.sanitize();
        bound_color(&mut self.background);
        bound_color(&mut self.text);
        bound_border(&mut self.border);
        bound_color(&mut self.selected_text);
        bound_color(&mut self.selected_background);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn combo_hostile_options_indices_and_styles_are_bounded() {
        let node = Node::ComboBox {
            key: "combo".into(),
            state_key: "state".into(),
            options: (0..MAX_OPTIONS + 2).map(|i| i.to_string()).collect(),
            selected: Some(MAX_OPTIONS as u32),
            reset: 0,
            placeholder: "é".repeat(MAX_STRING_BYTES),
            on_select: 0,
            width: Some(Length::Fixed(f32::INFINITY)),
            settings: Box::new(ComboOptions {
                padding: Some(f32::NAN),
                text_size: Some(f32::MAX),
                line_height: Some(-1.0),
                style: InputStyle {
                    active: InputFace {
                        icon: Some(Rgba([f32::NAN, 2.0, -1.0, f32::INFINITY])),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            }),
        };
        let mut frame = Frame {
            root: Some(node),
            ..Default::default()
        };
        sanitize(&mut frame).unwrap();
        let Node::ComboBox {
            options,
            selected,
            placeholder,
            width,
            settings,
            ..
        } = frame.root.unwrap()
        else {
            panic!()
        };
        assert_eq!(options.len(), MAX_OPTIONS);
        assert_eq!(selected, None);
        assert!(placeholder.len() <= MAX_STRING_BYTES);
        assert!(matches!(width, Some(Length::Fixed(value)) if value.is_finite()));
        assert_eq!(settings.padding, Some(0.0));
        assert_eq!(settings.text_size, Some(MAX_TEXT_PIXELS));
        assert_eq!(settings.line_height, Some(f32::EPSILON));
        assert!(
            settings
                .style
                .active
                .icon
                .unwrap()
                .0
                .iter()
                .all(|v| v.is_finite() && (0.0..=1.0).contains(v))
        );
    }
}
