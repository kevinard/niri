use crate::utils::MergeWith;
use crate::{Color, FloatOrInt};

/// Compositor decorations for windows that negotiated server-side decorations.
#[derive(Debug, Clone, PartialEq)]
pub struct Titlebar {
    pub off: bool,
    pub height: f64,
    pub font: String,
    pub title_align: TitleAlign,
    /// Buttons in their visual order. Duplicates are ignored; an empty list hides all buttons.
    pub buttons: Vec<TitlebarButton>,
    pub buttons_side: ButtonsSide,
    pub double_click: DoubleClickAction,
    pub active_color: Color,
    pub inactive_color: Color,
    pub text_color: Color,
    /// Text and icon color of inactive windows; `None` uses `text_color`.
    pub inactive_text_color: Option<Color>,
    pub button_hover_color: Color,
    pub close_hover_color: Color,
}

impl Default for Titlebar {
    fn default() -> Self {
        Self {
            off: true,
            height: 32.,
            font: "sans 10".into(),
            title_align: TitleAlign::Left,
            buttons: vec![TitlebarButton::Maximize, TitlebarButton::Close],
            buttons_side: ButtonsSide::Right,
            double_click: DoubleClickAction::Maximize,
            active_color: Color::from_rgba8_unpremul(48, 48, 48, 255),
            inactive_color: Color::from_rgba8_unpremul(36, 36, 36, 255),
            text_color: Color::from_rgba8_unpremul(238, 238, 238, 255),
            inactive_text_color: None,
            button_hover_color: Color::from_rgba8_unpremul(255, 255, 255, 38),
            close_hover_color: Color::from_rgba8_unpremul(192, 28, 40, 255),
        }
    }
}

impl Titlebar {
    pub fn text_color(&self, active: bool) -> Color {
        if active {
            self.text_color
        } else {
            self.inactive_text_color.unwrap_or(self.text_color)
        }
    }

    pub fn background_color(&self, active: bool) -> Color {
        if active {
            self.active_color
        } else {
            self.inactive_color
        }
    }
}

#[derive(knuffel::DecodeScalar, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum TitleAlign {
    #[default]
    Left,
    Center,
}

#[derive(knuffel::DecodeScalar, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitlebarButton {
    /// Opens a menu to set the column width, pin the column, float the window or show the
    /// column as tabs.
    Layout,
    Maximize,
    Close,
}

#[derive(knuffel::DecodeScalar, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ButtonsSide {
    Left,
    #[default]
    Right,
}

#[derive(knuffel::DecodeScalar, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum DoubleClickAction {
    #[default]
    Maximize,
    None,
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
    #[knuffel(child, unwrap(argument))]
    pub title_align: Option<TitleAlign>,
    #[knuffel(child, unwrap(arguments))]
    pub buttons: Option<Vec<TitlebarButton>>,
    #[knuffel(child, unwrap(argument))]
    pub buttons_side: Option<ButtonsSide>,
    #[knuffel(child, unwrap(argument))]
    pub double_click: Option<DoubleClickAction>,
    #[knuffel(child)]
    pub active_color: Option<Color>,
    #[knuffel(child)]
    pub inactive_color: Option<Color>,
    #[knuffel(child)]
    pub text_color: Option<Color>,
    #[knuffel(child)]
    pub inactive_text_color: Option<Color>,
    #[knuffel(child)]
    pub button_hover_color: Option<Color>,
    #[knuffel(child)]
    pub close_hover_color: Option<Color>,
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
        if let Some(align) = part.title_align {
            self.title_align = align;
        }
        if let Some(buttons) = &part.buttons {
            self.buttons.clear();
            for button in buttons {
                if !self.buttons.contains(button) {
                    self.buttons.push(*button);
                }
            }
        }
        if let Some(side) = part.buttons_side {
            self.buttons_side = side;
        }
        if let Some(action) = part.double_click {
            self.double_click = action;
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
        if let Some(color) = part.inactive_text_color {
            self.inactive_text_color = Some(color);
        }
        if let Some(color) = part.button_hover_color {
            self.button_hover_color = color;
        }
        if let Some(color) = part.close_hover_color {
            self.close_hover_color = color;
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

    #[test]
    fn layout_and_behavior_options_parse() {
        let config = Config::parse_mem(
            r##"layout {
                titlebar {
                    title-align "center"
                    buttons "close" "maximize" "close"
                    buttons-side "left"
                    double-click "none"
                    inactive-text-color "#808080"
                    button-hover-color "#00000020"
                    close-hover-color "#e01b24"
                }
            }"##,
        )
        .unwrap();
        let titlebar = &config.layout.titlebar;
        assert_eq!(titlebar.title_align, TitleAlign::Center);
        assert_eq!(
            titlebar.buttons,
            [TitlebarButton::Close, TitlebarButton::Maximize]
        );
        assert_eq!(titlebar.buttons_side, ButtonsSide::Left);
        assert_eq!(titlebar.double_click, DoubleClickAction::None);
        assert_eq!(
            titlebar.text_color(false),
            Color::from_rgba8_unpremul(128, 128, 128, 255)
        );
        assert_eq!(titlebar.text_color(true), Titlebar::default().text_color);

        let hidden = Config::parse_mem("layout { titlebar { buttons; }; }").unwrap();
        assert!(hidden.layout.titlebar.buttons.is_empty());
        assert!(Config::parse_mem("layout { titlebar { buttons \"minimize\"; }; }").is_err());
        assert!(Config::parse_mem("layout { titlebar { title-align \"right\"; }; }").is_err());
    }
}
