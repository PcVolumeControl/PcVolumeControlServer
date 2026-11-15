//! Spacing, sizes and widget styles in one place. Colours come from the
//! built-in light and dark palettes, so swapping the theme restyles
//! everything.

use std::time::Duration;

use iced::widget::scrollable::{Direction, Scrollbar};
use iced::widget::{
    Container, Scrollable, button as btn, container, scrollable, slider, svg, text,
};
use iced::{Border, Color, Element, Fill, Padding, Theme};

pub const SPACE_XS: f32 = 4.0;
pub const SPACE_S: f32 = 8.0;
pub const SPACE_M: f32 = 12.0;
pub const SPACE_L: f32 = 20.0;
pub const SPACE_XL: f32 = 28.0;

pub const TEXT_SMALL: f32 = 13.0;
/// iced's default text size, named so labels can be measured at it.
pub const TEXT_DEFAULT: f32 = 16.0;
pub const TEXT_BODY: f32 = 15.0;
pub const TEXT_SECTION: f32 = 18.0;
pub const TEXT_TITLE: f32 = 24.0;
pub const TEXT_HEADLINE: f32 = 32.0;

pub const RADIUS: f32 = 8.0;

pub const NAV_WIDTH: f32 = 150.0;
/// Labels of the Settings rows.
pub const LABEL_WIDTH: f32 = 140.0;
pub const STATUS_ICON: f32 = 64.0;

/// Sizes of the Mixer rows, which are built by the same code at every
/// density. Application icons keep one size (`APP_ICON`) at all densities,
/// since each application's icon is looked up once.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Density {
    /// Labels, names, percentages and mute buttons.
    pub text: f32,
    /// Session details, beneath the name.
    pub detail_text: f32,
    /// The "Master" label.
    pub master_text: f32,
    /// Between the columns of a row.
    pub spacing: f32,
    /// Between application rows.
    pub row_spacing: f32,
    /// Padding on each side of the rows (a card's), which the columns
    /// cannot use.
    pub inset: f32,
    pub mute_width: f32,
    pub mute_padding: Padding,
    pub percent_width: f32,
    pub picker_padding: Padding,
    /// The label column takes this share of the width the fixed columns
    /// leave, within `label_min` and `label_max`.
    pub label_share: f32,
    pub label_min: f32,
    pub label_max: f32,
    /// The slider keeps at least this much before the label shrinks below
    /// its minimum.
    pub slider_min: f32,
    /// The height of the device picker's and the master's rows and of
    /// notes, when fixed.
    pub row_height: Option<f32>,
    /// An application's name and the line for its detail beneath, kept
    /// whether or not it has one so rows never change height. Whole
    /// pixels, at least both lines' height.
    pub name_height: f32,
    /// The height of application rows, when fixed.
    pub app_row_height: Option<f32>,
}

/// The window's Mixer page.
pub const REGULAR: Density = Density {
    text: TEXT_DEFAULT,
    detail_text: TEXT_SMALL,
    master_text: TEXT_SECTION,
    spacing: SPACE_M,
    row_spacing: SPACE_S,
    inset: SPACE_L,
    mute_width: 90.0,
    mute_padding: btn::DEFAULT_PADDING,
    percent_width: 48.0,
    picker_padding: Padding {
        top: SPACE_S,
        right: SPACE_M,
        bottom: SPACE_S,
        left: SPACE_M,
    },
    label_share: 0.5,
    label_min: 120.0,
    label_max: 260.0,
    slider_min: 80.0,
    row_height: None,
    name_height: 38.0,
    app_row_height: None,
};

/// The mini mixer. Rows have a fixed height, so the window can be sized
/// for its rows before it is drawn.
pub const COMPACT: Density = Density {
    text: TEXT_COMPACT,
    detail_text: TEXT_COMPACT_DETAIL,
    master_text: TEXT_COMPACT,
    spacing: SPACE_S,
    row_spacing: SPACE_XS,
    inset: 0.0,
    mute_width: 70.0,
    mute_padding: Padding {
        top: 3.0,
        right: SPACE_S,
        bottom: 3.0,
        left: SPACE_S,
    },
    percent_width: 38.0,
    picker_padding: Padding {
        top: SPACE_XS,
        right: SPACE_S,
        bottom: SPACE_XS,
        left: SPACE_S,
    },
    label_share: 0.5,
    label_min: 100.0,
    label_max: 160.0,
    slider_min: 80.0,
    row_height: Some(MINI_ROW),
    name_height: MINI_APP_ROW,
    app_row_height: Some(MINI_APP_ROW),
};

pub const TEXT_COMPACT: f32 = 14.0;
pub const TEXT_COMPACT_DETAIL: f32 = 12.0;

/// The mini mixer: a fixed width, with room for a few words of each
/// session's detail, and a height that follows its rows up to
/// `MINI_MAX_HEIGHT` (see `mini::height`).
pub const MINI_WIDTH: f32 = 440.0;
pub const MINI_MAX_HEIGHT: f32 = 460.0;
pub const MINI_ROW: f32 = 32.0;
pub const MINI_APP_ROW: f32 = 34.0;
/// Inside the frame. On the right the scroll gutter, which every row
/// keeps, does the padding; one pixel clears the frame's border.
pub const MINI_PADDING: Padding = Padding {
    top: SPACE_M,
    right: 1.0,
    bottom: SPACE_M,
    left: SCROLL_GUTTER,
};
/// Between the header, the device picker, the master row, the divider and
/// the application list.
pub const MINI_GAP: f32 = SPACE_S;
pub const MINI_HEADER: f32 = 24.0;
pub const MINI_HEADER_ICON: f32 = 14.0;
pub const MINI_DIVIDER: f32 = 1.0;
/// Between the mini mixer and the taskbar or the screen's edges.
pub const MINI_SCREEN_MARGIN: f32 = SPACE_S;

/// Application icons in the Mixer.
pub const APP_ICON: f32 = 24.0;
pub const APP_ICON_RADIUS: f32 = 6.0;
pub const APP_ICON_GAP: f32 = SPACE_S;
/// The letter on an application's fallback tile.
pub const APP_ICON_LETTER: f32 = 13.0;
/// Opacity of a muted application's icon.
pub const MUTED_OPACITY: f32 = 0.45;
/// Opacity of a muted application's detail, already a dim colour.
pub const MUTED_DETAIL_OPACITY: f32 = 0.6;

pub const TOOLTIP_DELAY: Duration = Duration::from_millis(400);

/// Scroll areas keep their scrollbar in a gutter at the page's right edge,
/// empty whether or not the content scrolls, so the scrollbar never covers
/// content; content outside them is inset by the same gutter so right edges
/// line up.
pub const SCROLLBAR_WIDTH: f32 = 8.0;
pub const SCROLL_GUTTER: f32 = SCROLLBAR_WIDTH + SPACE_S;

/// Space around a page; the right side holds the scroll gutter.
pub const PAGE_PADDING: Padding = Padding {
    top: SPACE_M,
    right: SPACE_XL - SCROLL_GUTTER,
    bottom: SPACE_M,
    left: SPACE_XL,
};

#[derive(Debug, Clone, Copy)]
pub enum Button {
    Primary,
    Secondary,
    Success,
    Danger,
    /// A navigation entry; `true` when selected.
    Nav(bool),
    /// A bare icon that shows a background when hovered or pressed.
    Icon,
}

/// A built-in button style with rounded corners.
pub fn button(kind: Button) -> impl Fn(&Theme, btn::Status) -> btn::Style {
    move |theme, status| {
        let base = match kind {
            Button::Primary | Button::Nav(true) => btn::primary(theme, status),
            Button::Secondary => btn::secondary(theme, status),
            Button::Success => btn::success(theme, status),
            Button::Danger => btn::danger(theme, status),
            Button::Nav(false) => btn::text(theme, status),
            Button::Icon => {
                let palette = theme.extended_palette();
                let background = match status {
                    btn::Status::Hovered => Some(palette.background.weak.color.into()),
                    btn::Status::Pressed => Some(palette.background.strong.color.into()),
                    btn::Status::Active | btn::Status::Disabled => None,
                };
                btn::Style {
                    background,
                    text_color: palette.background.base.text,
                    ..btn::Style::default()
                }
            }
        };
        btn::Style {
            border: Border {
                radius: RADIUS.into(),
                ..base.border
            },
            ..base
        }
    }
}

/// A rounded, bordered surface grouping related controls.
pub fn card(theme: &Theme) -> container::Style {
    let base = container::bordered_box(theme);
    container::Style {
        border: Border {
            radius: RADIUS.into(),
            ..base.border
        },
        ..base
    }
}

/// The mini mixer's window: the page background inside a hairline border,
/// which sets the borderless window apart from what is behind it.
pub fn mini_frame(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(palette.background.base.color.into()),
        border: Border {
            color: palette.background.strong.color,
            width: 1.0,
            radius: 0.0.into(),
        },
        ..container::Style::default()
    }
}

/// An icon in the mini mixer's header, in the text colour.
pub fn header_icon(theme: &Theme, _status: svg::Status) -> svg::Style {
    svg::Style {
        color: Some(theme.extended_palette().background.base.text),
    }
}

/// The fallback tile of an application without an icon.
pub fn icon_tile(background: Color) -> container::Style {
    container::Style {
        background: Some(background.into()),
        border: Border {
            radius: APP_ICON_RADIUS.into(),
            ..Border::default()
        },
        ..container::Style::default()
    }
}

/// Where an application icon appears once it has been looked up.
pub fn icon_placeholder(theme: &Theme) -> container::Style {
    icon_tile(theme.extended_palette().background.weak.color)
}

/// A vertically scrolling area filling the remaining height, with its
/// scrollbar in the gutter.
pub fn scroll_area<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
) -> Scrollable<'a, Message> {
    scrollable(beside_gutter(content))
        .direction(Direction::Vertical(
            Scrollbar::new()
                .width(SCROLLBAR_WIDTH)
                .scroller_width(SCROLLBAR_WIDTH),
        ))
        .height(Fill)
}

/// Content outside a scroll area, ending where scrolling content ends.
pub fn beside_gutter<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
) -> Container<'a, Message> {
    container(content).padding(Padding::ZERO.right(SCROLL_GUTTER))
}

/// The volume slider; a muted control's slider is greyed out.
pub fn volume_slider(muted: bool) -> impl Fn(&Theme, slider::Status) -> slider::Style {
    move |theme, status| {
        let base = slider::default(theme, status);
        if !muted {
            return base;
        }
        let palette = theme.extended_palette();
        let fill = palette.background.strong.color;
        slider::Style {
            rail: slider::Rail {
                backgrounds: (fill.into(), palette.background.weak.color.into()),
                ..base.rail
            },
            handle: slider::Handle {
                background: fill.into(),
                ..base.handle
            },
        }
    }
}

/// The bubble of a tooltip.
pub fn tooltip(theme: &Theme) -> container::Style {
    let base = container::bordered_box(theme);
    container::Style {
        border: Border {
            radius: (RADIUS / 2.0).into(),
            ..base.border
        },
        ..base
    }
}

/// De-emphasised text such as section labels and hints.
pub fn muted_text(theme: &Theme) -> text::Style {
    text::secondary(theme)
}

/// A session's detail beneath its name, de-emphasised like `muted_text`
/// and fainter still when the session is muted.
pub fn session_detail(muted: bool) -> impl Fn(&Theme) -> text::Style {
    move |theme| {
        let style = muted_text(theme);
        if !muted {
            return style;
        }
        text::Style {
            color: style.color.map(|c| c.scale_alpha(MUTED_DETAIL_OPACITY)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_details_fit_their_rows() {
        // iced's default line height.
        let line = |size: f32| size * 1.3;
        for d in [REGULAR, COMPACT] {
            assert!(d.name_height >= line(d.text) + line(d.detail_text), "{d:?}");
            assert!(d.detail_text < d.text);
            assert_eq!(d.name_height, d.name_height.round());
            if let Some(row) = d.app_row_height {
                assert!(row >= d.name_height);
            }
        }
    }

    #[test]
    fn muted_details_are_fainter() {
        for theme in [Theme::Light, Theme::Dark] {
            let normal = session_detail(false)(&theme).color.unwrap();
            let muted = session_detail(true)(&theme).color.unwrap();
            assert!(muted.a < normal.a);
            assert_ne!(normal, theme.extended_palette().background.base.text);
        }
    }

    #[test]
    fn muted_sliders_are_greyed() {
        for theme in [Theme::Light, Theme::Dark] {
            let status = slider::Status::Active;
            let normal = volume_slider(false)(&theme, status);
            let muted = volume_slider(true)(&theme, status);
            assert_eq!(normal, slider::default(&theme, status));
            assert_ne!(muted.rail.backgrounds, normal.rail.backgrounds);
            assert_ne!(muted.handle.background, normal.handle.background);
            let primary = theme.extended_palette().primary.base.color;
            assert_ne!(muted.handle.background, primary.into());
        }
    }
}
