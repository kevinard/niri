use crate::utils::MergeWith;
use crate::{Color, FloatOrInt};

/// Compositor decorations for windows that negotiated server-side decorations.
#[derive(Debug, Clone, PartialEq)]
pub struct Titlebar {
    pub off: bool,
    pub height: f64,
    pub font: String,
    pub active_color: Color,
    pub inactive_color: Color,
    pub text_color: Color,
}

impl Default for Titlebar {
    fn default() -> Self {
        Self {
            off: true,
            height: 32.,
            font: "sans 10".into(),
            active_color: Color::from_rgba8_unpremul(48, 48, 48, 255),
            inactive_color: Color::from_rgba8_unpremul(36, 36, 36, 255),
            text_color: Color::from_rgba8_unpremul(238, 238, 238, 255),
        }
    }
}

#[derive(knuffel::Decode, Debug, Default, Clone, PartialEq)]
pub struct TitlebarPart {
    #[knuffel(child)]
    pub on: bool,
    #[knuffel(child)]
    pub off: bool,
    #[knuffel(child, unwrap(argument))]
    pub height: Option<FloatOrInt<24, 64>>,
    #[knuffel(child, unwrap(argument))]
    pub font: Option<String>,
    #[knuffel(child)]
    pub active_color: Option<Color>,
    #[knuffel(child)]
    pub inactive_color: Option<Color>,
    #[knuffel(child)]
    pub text_color: Option<Color>,
}

impl MergeWith<TitlebarPart> for Titlebar {
    fn merge_with(&mut self, part: &TitlebarPart) {
        if part.off {
            self.off = true;
        }
        if part.on {
            self.off = false;
        }
        if let Some(height) = part.height {
            self.height = height.0;
        }
        if let Some(font) = &part.font {
            self.font.clone_from(font);
        }
        if let Some(color) = part.active_color {
            self.active_color = color;
        }
        if let Some(color) = part.inactive_color {
            self.inactive_color = color;
        }
        if let Some(color) = part.text_color {
            self.text_color = color;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    #[test]
    fn titlebars_are_opt_in_and_settings_are_bounded() {
        assert!(Config::default().layout.titlebar.off);
        let config = Config::parse_mem(
            "layout { titlebar { on; height 40; font \"sans 11\"; active-color \"#182428\"; }; }",
        )
        .unwrap();
        assert!(!config.layout.titlebar.off);
        assert_eq!(config.layout.titlebar.height, 40.);
        assert_eq!(config.layout.titlebar.font, "sans 11");
        for height in [0, 23, 65, 100000] {
            assert!(
                Config::parse_mem(&format!("layout {{ titlebar {{ height {height}; }}; }}"))
                    .is_err()
            );
        }
    }

    #[test]
    fn an_override_can_disable_titlebars_without_losing_their_style() {
        let mut titlebar = Titlebar::default();
        titlebar.merge_with(&TitlebarPart {
            on: true,
            height: Some(FloatOrInt(40.)),
            ..Default::default()
        });
        titlebar.merge_with(&TitlebarPart {
            off: true,
            ..Default::default()
        });
        assert!(titlebar.off);
        assert_eq!(titlebar.height, 40.);
    }
}
