use gpui::{Hsla, WindowAppearance, rgb, rgba};

/// Minimalist design tokens that track the OS light/dark appearance.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub is_dark: bool,
    pub bg: Hsla,
    pub surface_subtle: Hsla,
    pub row_hover: Hsla,
    pub row_selected: Hsla,
    pub border: Hsla,
    pub border_focus: Hsla,
    pub text_primary: Hsla,
    pub text_secondary: Hsla,
    pub text_muted: Hsla,
    pub accent: Hsla,
    pub selection_bg: Hsla,
    pub favorite: Hsla,
    pub tab_active_bg: Hsla,
    pub badge_bg: Hsla,
    pub thumb_bg: Hsla,
}

impl Theme {
    pub fn from_appearance(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::dark(),
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::light(),
        }
    }

    pub fn dark() -> Self {
        Self {
            is_dark: true,
            bg: rgb(0x141517).into(),
            surface_subtle: rgb(0x1b1d21).into(),
            row_hover: rgb(0x1d2026).into(),
            row_selected: rgb(0x252932).into(),
            border: rgb(0x2a2d35).into(),
            border_focus: rgb(0x5b8af5).into(),
            text_primary: rgb(0xeceef2).into(),
            text_secondary: rgb(0x8d93a0).into(),
            text_muted: rgb(0x636874).into(),
            accent: rgb(0x5b8af5).into(),
            selection_bg: rgba(0x5b8af540).into(),
            favorite: rgb(0xe5a93b).into(),
            tab_active_bg: rgb(0x272a32).into(),
            badge_bg: rgb(0x22252c).into(),
            thumb_bg: rgb(0x1f2228).into(),
        }
    }

    pub fn light() -> Self {
        Self {
            is_dark: false,
            bg: rgb(0xfafafb).into(),
            surface_subtle: rgb(0xf1f2f5).into(),
            row_hover: rgb(0xefeff3).into(),
            row_selected: rgb(0xe6e9f0).into(),
            border: rgb(0xe2e4e9).into(),
            border_focus: rgb(0x2f6feb).into(),
            text_primary: rgb(0x16181d).into(),
            text_secondary: rgb(0x686e7a).into(),
            text_muted: rgb(0x9298a4).into(),
            accent: rgb(0x2f6feb).into(),
            selection_bg: rgba(0x2f6feb30).into(),
            favorite: rgb(0xc98216).into(),
            tab_active_bg: rgb(0xffffff).into(),
            badge_bg: rgb(0xe8eaef).into(),
            thumb_bg: rgb(0xeceef2).into(),
        }
    }
}
