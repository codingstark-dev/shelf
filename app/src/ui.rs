//! Shelf's look: tokens and small builders on top of Ely.
#![allow(dead_code)] // the screens adopt these piece by piece

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use ely_gpui_component::{
    files::FileIcon,
    layout::Collapsible,
    motion::{self, ProgressBar, Spinner, SpinnerStyle},
    primitives::{Disclosure, Icon, IconName},
    theme::{ActiveTheme, IconSize, Mix, Mode, Palette, Radius, Theme},
};
use gpui::{
    Animation, AnimationExt, AnyElement, App, Div, ElementId, FontWeight, Hsla, Image, ImageFormat, InteractiveElement,
    IntoElement, ParentElement, SharedString, Stateful, StatefulInteractiveElement, Styled, black, div, img,
    prelude::FluentBuilder, px, rgba,
};
use jiff::Timestamp;

pub const SETUP_SIZE: (f32, f32) = (480.0, 500.0);
pub const LIBRARY_SIZE: (f32, f32) = (760.0, 520.0);
pub const LIBRARY_MIN: (f32, f32) = (600.0, 400.0);
pub const TOOLBAR_H: f32 = 52.0;
pub const STRIP_H: f32 = 32.0;

// ---- tokens ----

/// 0xRRGGBBAA.
fn c(rgba_hex: u32) -> Hsla {
    rgba(rgba_hex).into()
}

/// Section 1 colors on top of Ely's own palette for `mode`.
fn palette(mode: Mode) -> Palette {
    let dark = mode == Mode::Dark;
    let pick = |light: u32, dark_: u32| c(if dark { dark_ } else { light });
    let mut p = if dark { Palette::dark(false) } else { Palette::light(false) };
    p.glass = pick(0xFCFAF700, 0x13111000);
    p.border = pick(0x00000014, 0xFFFFFF1A);
    p.hover = pick(0x0000000D, 0xFFFFFF0F);
    p.selection = pick(0x007AFF33, 0x0A84FF4D);
    p.fg = pick(0x1D1D1FFF, 0xF5F5F7FF);
    p.fg_muted = pick(0x3C3C4399, 0xEBEBF599);
    p.fg_subtle = pick(0x3C3C4366, 0xEBEBF566);
    p.accent = pick(0x007AFFFF, 0x0A84FFFF);
    p.focus = p.accent;
    p.link = p.accent;
    p.accent_hover = pick(0x0A6FE8FF, 0x3B9BFFFF);
    p.on_accent = c(0xFFFFFFFF);
    p.success = pick(0x34C759FF, 0x30D158FF);
    p.danger = pick(0xFF3B30FF, 0xFF453AFF);
    p.danger_subtle = pick(0xFF3B301F, 0xFF453A33);
    p
}

/// Font and both palettes. Call once after `ely_gpui_component::init`.
pub fn install(cx: &mut App) {
    Theme::update(cx, |t| {
        t.font_family = ".SystemUIFont".into();
        t.mono_family = "Menlo".into();
    });
    for mode in [Mode::Light, Mode::Dark] {
        Theme::set_palette(mode, Some(palette(mode)), cx);
    }
    let mode = cx.theme().mode();
    Theme::set_mode_now(mode, cx); // no fade at launch
}

pub fn card_fill(cx: &App) -> Hsla {
    if cx.theme().is_dark() { c(0xFFFFFF0F) } else { c(0xFFFFFF8C) }
}

pub fn hairline(cx: &App) -> Hsla {
    cx.theme().colors.border
}

pub fn track(cx: &App) -> Hsla {
    if cx.theme().is_dark() { c(0xFFFFFF1A) } else { c(0x00000014) }
}

pub fn accent_alpha(a: f32, cx: &App) -> Hsla {
    cx.theme().colors.accent.opacity(a)
}

fn dur(base: Duration, cx: &App) -> Duration {
    motion::duration(base, cx)
}

// ---- hero and cards ----

/// The app logo, decoded once from the embedded PNG (works from `cargo run` and a bundled .app).
fn logo() -> Arc<Image> {
    static LOGO: OnceLock<Arc<Image>> = OnceLock::new();
    LOGO.get_or_init(|| Arc::new(Image::from_bytes(ImageFormat::Png, include_bytes!("../assets/icons/app-icon-256.png").to_vec()))).clone()
}

/// Hero block: 64pt logo, 20/600 title, 13 subtitle; centered column.
pub fn hero(title: impl Into<SharedString>, subtitle: impl Into<SharedString>, cx: &App) -> AnyElement {
    let colors = &cx.theme().colors;
    div()
        .flex()
        .flex_col()
        .items_center()
        .child(img(logo()).size(px(64.0)))
        .child(
            div()
                .mt(px(14.0))
                .text_size(px(20.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors.fg)
                .child(title.into()),
        )
        .child(div().mt(px(4.0)).text_size(px(13.0)).text_color(colors.fg_muted).child(subtitle.into()))
        .into_any_element()
}

/// Rounded 12 card, card_fill, 1pt hairline, 4pt inner inset; dividers inset `divider_inset` from the left.
pub fn group_card(id: impl Into<ElementId>, rows: Vec<AnyElement>, divider_inset: f32, cx: &App) -> AnyElement {
    let line = hairline(cx).opacity(0.8);
    let mut card = div()
        .id(id)
        .w_full()
        .flex()
        .flex_col()
        .p(px(4.0))
        .rounded(px(12.0))
        .bg(card_fill(cx))
        .border_1()
        .border_color(hairline(cx));
    for (i, row) in rows.into_iter().enumerate() {
        if i > 0 {
            card = card.child(div().h(px(1.0)).ml(px(divider_inset)).bg(line));
        }
        card = card.child(row);
    }
    card.into_any_element()
}

// ---- setup steps ----

#[derive(Clone, Copy, PartialEq)]
pub enum StepState {
    Pending(u8),
    Active(u8),
    Busy,
    Done,
    Failed,
}

/// Which glyph a state draws.
fn glyph(state: StepState) -> &'static str {
    match state {
        StepState::Pending(_) | StepState::Active(_) => "number",
        StepState::Busy => "spinner",
        StepState::Done => "check",
        StepState::Failed => "x",
    }
}

/// 24pt circle (fixed box, so a state change never shifts layout).
pub fn status_circle(id: impl Into<ElementId>, state: StepState, cx: &App) -> AnyElement {
    let id: ElementId = id.into();
    let colors = &cx.theme().colors;
    let circle = |fill: Hsla| div().size(px(24.0)).flex_none().rounded_full().bg(fill).flex().items_center().justify_center();
    let number = |n: u8, color: Hsla| {
        div().text_size(px(12.0)).font_weight(FontWeight::SEMIBOLD).text_color(color).child(n.to_string())
    };
    let pop = |icon: IconName, id: ElementId| {
        Icon::new(icon).size(IconSize::Sm).color(colors.on_accent).with_animation(
            (id, "pop"),
            Animation::new(dur(motion::SLOW, cx)).with_easing(motion::spring),
            |icon, t| icon.scale(0.6 + 0.4 * t),
        )
    };
    match state {
        StepState::Pending(n) => circle(track(cx)).child(number(n, colors.fg_muted)).into_any_element(),
        StepState::Active(n) => circle(accent_alpha(0.15, cx)).child(number(n, colors.accent)).into_any_element(),
        StepState::Busy => circle(accent_alpha(0.15, cx))
            .child(Spinner::new((id, "spin")).style(SpinnerStyle::Ring).size(IconSize::Sm).color(colors.accent))
            .into_any_element(),
        StepState::Done => circle(colors.success).child(pop(IconName::Check, id)).into_any_element(),
        StepState::Failed => circle(colors.danger)
            .child(pop(IconName::X, id.clone()))
            .with_animation(
                (id, "shake"),
                Animation::new(dur(Duration::from_millis(240), cx)),
                |el, t| el.relative().left(px(2.0 * (t * std::f32::consts::TAU * 3.0).sin() * (1.0 - t))),
            )
            .into_any_element(),
    }
}

/// Setup row: circle + title + detail (red when Failed) + optional trailing control; `body` opens under the row.
pub fn step_row(
    id: impl Into<ElementId>,
    state: StepState,
    title: impl Into<SharedString>,
    detail: Option<SharedString>,
    trailing: Option<AnyElement>,
    body: Option<AnyElement>,
    cx: &App,
) -> AnyElement {
    let id: ElementId = id.into();
    let colors = &cx.theme().colors;
    let detail_color = if state == StepState::Failed { colors.danger } else { colors.fg_muted };
    let row = div()
        .min_h(px(56.0))
        .py(px(8.0))
        .px(px(14.0))
        .flex()
        .items_center()
        .gap(px(12.0))
        .child(status_circle((id.clone(), "circle"), state, cx))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(div().text_size(px(13.0)).font_weight(FontWeight::MEDIUM).text_color(colors.fg).child(title.into()))
                .when_some(detail, |el, d| {
                    el.child(div().text_size(px(11.0)).line_clamp(3).text_color(detail_color).child(d))
                }),
        )
        .children(trailing);
    div()
        .id(id.clone())
        .flex()
        .flex_col()
        .child(row)
        .when_some(body, |el, body| el.child(Collapsible::new((id, "body"), true).child(body)))
        .into_any_element()
}

// ---- pill button ----

#[derive(Clone, Copy, PartialEq)]
pub enum PillKind {
    Accent,
    Neutral,
    Destructive,
}

/// Capsule button, h24. Caller chains `.on_click(..)` and must skip the action itself while `!enabled || busy`
/// (gpui can't cancel a click listener added later).
pub fn pill_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    kind: PillKind,
    enabled: bool,
    busy: bool,
    cx: &App,
) -> Stateful<Div> {
    let colors = &cx.theme().colors;
    let (fill, hover, text) = match kind {
        PillKind::Accent => (colors.accent, colors.accent_hover, colors.on_accent),
        PillKind::Neutral => (track(cx), colors.active, colors.fg),
        PillKind::Destructive => (colors.danger_subtle, colors.danger.opacity(0.25), colors.danger),
    };
    let press = if kind == PillKind::Accent { colors.accent_hover.mix(&black(), 0.12) } else { colors.border_strong };
    let live = enabled && !busy;
    div()
        .id(id)
        .h(px(24.0))
        .px(px(12.0))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .gap(px(6.0))
        .rounded_full()
        .bg(fill)
        .text_size(px(12.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(text)
        .when(busy, |el| el.child(Spinner::new("pill-busy").size(IconSize::Sm).color(text)))
        .child(label.into())
        .when(live, |el| el.cursor_pointer().hover(move |s| s.bg(hover)).active(move |s| s.bg(press)))
        .when(!enabled, |el| el.opacity(0.4))
}

// ---- files ----

pub struct FileRowData {
    pub name: SharedString,
    pub folder: bool,
    pub size: Option<u64>,
    pub modified: Timestamp,
    pub encrypted: bool,
}

fn human_size(data: &FileRowData) -> String {
    let Some(bytes) = data.size.filter(|_| !data.folder) else { return "\u{2014}".into() };
    let mut value = bytes as f64;
    let mut unit = "B";
    for next in ["KB", "MB", "GB", "TB"] {
        if value < 1000.0 {
            break;
        }
        value /= 1000.0;
        unit = next;
    }
    if unit == "B" { format!("{bytes} B") } else { format!("{value:.1} {unit}") }
}

fn relative(ts: Timestamp) -> String {
    let secs = Timestamp::now().as_second() - ts.as_second();
    match secs {
        ..60 => "Just now".into(),
        ..3600 => format!("{} min ago", secs / 60),
        ..86_400 => format!("{} hr ago", secs / 3600),
        ..172_800 => "Yesterday".into(),
        ..604_800 => format!("{} days ago", secs / 86_400),
        _ => ts.to_zoned(jiff::tz::TimeZone::system()).strftime("%b %-d, %Y").to_string(),
    }
}

fn file_icon(data: &FileRowData, size: IconSize) -> FileIcon {
    if data.folder { FileIcon::folder(false).size(size) } else { FileIcon::file(&data.name).size(size) }
}

/// 40pt list row. Caller chains `.on_click` / `.on_double_click`. `actions` show on row hover (group = the row id).
pub fn file_row(
    id: impl Into<ElementId>,
    data: &FileRowData,
    selected: bool,
    actions: Vec<AnyElement>,
    cx: &App,
) -> Stateful<Div> {
    let id: ElementId = id.into();
    let group: SharedString = id.to_string().into();
    let colors = &cx.theme().colors;
    let (hover, selection, active) = (colors.hover, colors.selection, colors.active);
    let cell = |w: f32, text: String| {
        div().w(px(w)).flex_none().text_size(px(11.0)).text_color(colors.fg_muted).whitespace_nowrap().child(text)
    };
    div()
        .id(id)
        .group(group.clone())
        .h(px(40.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .gap(px(12.0))
        .rounded(cx.theme().radius(Radius::Lg))
        .cursor_pointer()
        .when(selected, |el| el.bg(selection))
        .when(!selected, |el| el.hover(move |s| s.bg(hover)))
        .active(move |s| s.bg(active))
        .child(div().flex_none().text_color(if data.folder { colors.accent } else { colors.fg_muted }).child(file_icon(data, IconSize::Xl)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .text_size(px(13.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(colors.fg)
                        .child(data.name.clone()),
                )
                .when(data.encrypted, |el| el.child(Icon::new(IconName::Lock).size(IconSize::Xs).color(colors.fg_subtle))),
        )
        .child(cell(80.0, human_size(data)).text_right())
        .child(cell(120.0, relative(data.modified)))
        .child(
            div()
                .w(px(52.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_end()
                .gap(px(4.0))
                .when(!selected, |el| el.opacity(0.0).group_hover(group, |s| s.opacity(1.0)))
                .children(actions),
        )
}

/// Tile for grid view: 112x112, icon (32, Ely's largest), name 12pt 2 lines.
pub fn file_tile(id: impl Into<ElementId>, data: &FileRowData, selected: bool, cx: &App) -> Stateful<Div> {
    let colors = &cx.theme().colors;
    let (hover, selection) = (colors.hover, colors.selection);
    div()
        .id(id)
        .size(px(112.0))
        .flex_none()
        .p(px(8.0))
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(6.0))
        .rounded(cx.theme().radius(Radius::Lg))
        .cursor_pointer()
        .when(selected, |el| el.bg(selection))
        .when(!selected, |el| el.hover(move |s| s.bg(hover)))
        .child(div().text_color(if data.folder { colors.accent } else { colors.fg_muted }).child(file_icon(data, IconSize::Xxl)))
        .child(
            div()
                .w_full()
                .text_center()
                .text_size(px(12.0))
                .line_clamp(2)
                .text_color(colors.fg)
                .child(data.name.clone()),
        )
}

// ---- drop ----

/// Dashed drop target that fills its parent. Caller chains `.on_click` to open the picker.
pub fn drop_target(
    id: impl Into<ElementId>,
    headline: impl Into<SharedString>,
    sub: impl Into<SharedString>,
    over: bool,
    cx: &App,
) -> Stateful<Div> {
    let id: ElementId = id.into();
    let dark = cx.theme().is_dark();
    let colors = &cx.theme().colors;
    let icon = Icon::new(IconName::CloudUpload).size(IconSize::Xxl).color(colors.accent);
    let icon = if over {
        icon.with_animation(
            (id.clone(), "scale"),
            Animation::new(dur(motion::BASE, cx)).with_easing(motion::spring),
            |icon, t| icon.scale(1.0 + 0.08 * t),
        )
        .into_any_element()
    } else {
        icon.into_any_element()
    };
    div()
        .id(id)
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(8.0))
        .rounded(px(16.0))
        .cursor_pointer()
        .map(|el| {
            if over {
                el.border_2().border_color(colors.accent).bg(accent_alpha(if dark { 0.16 } else { 0.10 }, cx))
            } else {
                el.border(px(1.5)).border_dashed().border_color(colors.fg.opacity(if dark { 0.30 } else { 0.25 }))
            }
        })
        .child(
            div()
                .size(px(72.0))
                .rounded_full()
                .bg(accent_alpha(0.12, cx))
                .flex()
                .items_center()
                .justify_center()
                .child(icon),
        )
        .child(
            div().text_size(px(15.0)).font_weight(FontWeight::SEMIBOLD).text_color(colors.fg).child(headline.into()),
        )
        .child(div().text_size(px(13.0)).text_color(colors.fg_muted).child(sub.into()))
}

/// Full-window overlay while dragging over a non-empty list. It does not occlude, so the drop still lands.
pub fn drop_overlay(label: impl Into<SharedString>, cx: &App) -> AnyElement {
    let colors = &cx.theme().colors;
    let dark = cx.theme().is_dark();
    div()
        .absolute()
        .top(px(8.0))
        .left(px(8.0))
        .right(px(8.0))
        .bottom(px(8.0))
        .rounded(px(16.0))
        .border_2()
        .border_color(colors.accent)
        .bg(accent_alpha(if dark { 0.16 } else { 0.10 }, cx))
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .h(px(32.0))
                .px(px(14.0))
                .flex()
                .items_center()
                .rounded_full()
                .bg(colors.surface)
                .text_size(px(13.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(colors.fg)
                .child(label.into()),
        )
        .with_animation(
            "drop-overlay",
            Animation::new(dur(motion::FAST, cx)).with_easing(motion::ease_out_cubic),
            |el, t| el.opacity(t),
        )
        .into_any_element()
}

// ---- strip and errors ----

/// Bottom strip: left = optional transfer summary + progress, right = `usage`.
pub fn status_strip(transfer: Option<(SharedString, f32)>, usage: SharedString, cx: &App) -> AnyElement {
    let colors = &cx.theme().colors;
    div()
        .h(px(STRIP_H))
        .flex_none()
        .px(px(14.0))
        .flex()
        .items_center()
        .justify_between()
        .gap(px(12.0))
        .border_t_1()
        .border_color(hairline(cx))
        .text_size(px(11.0))
        .child(
            div().flex().items_center().gap(px(8.0)).when_some(transfer, |el, (text, value)| {
                el.child(div().w(px(120.0)).h(px(4.0)).child(ProgressBar::new("strip-progress", value)))
                    .child(div().text_color(colors.fg_muted).child(text))
            }),
        )
        .child(div().text_color(colors.fg_subtle).child(usage))
        .into_any_element()
}

/// Friendly error card. `retry` is a finished button (`pill_button(.., Accent, ..).on_click(..)`);
/// `toggle` is an empty clickable `div().id(..).on_click(..)` that gets the "Details" label and chevron.
pub fn error_card(
    headline: impl Into<SharedString>,
    hint: impl Into<SharedString>,
    details: SharedString,
    open: bool,
    retry: Stateful<Div>,
    toggle: Stateful<Div>,
    cx: &App,
) -> AnyElement {
    let colors = &cx.theme().colors;
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(8.0))
        .child(Icon::new(IconName::TriangleAlert).size(IconSize::Xxl).color(colors.danger))
        .child(div().text_size(px(15.0)).font_weight(FontWeight::SEMIBOLD).text_color(colors.fg).child(headline.into()))
        .child(div().max_w(px(320.0)).text_center().text_size(px(13.0)).text_color(colors.fg_muted).child(hint.into()))
        .child(
            div().mt(px(4.0)).flex().items_center().gap(px(8.0)).child(retry).child(
                toggle
                    .h(px(24.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .rounded_full()
                    .cursor_pointer()
                    .text_size(px(12.0))
                    .text_color(colors.fg_muted)
                    .hover(|s| s.bg(cx.theme().colors.hover))
                    .child("Details")
                    .child(Disclosure::new("error-details", open)),
            ),
        )
        .when(open, |el| {
            el.child(
                div()
                    .id("error-details-text")
                    .mt(px(4.0))
                    .w(px(360.0))
                    .max_h(px(120.0))
                    .overflow_y_scroll()
                    .p(px(8.0))
                    .rounded(px(8.0))
                    .bg(colors.danger_subtle)
                    .font_family(cx.theme().mono_family.clone())
                    .text_size(px(11.0))
                    .text_color(colors.fg)
                    .child(details),
            )
        })
        .with_animation(
            "error-in",
            Animation::new(dur(motion::BASE, cx)).with_easing(motion::ease_out_cubic),
            |el, t| el.opacity(t).mt(motion::NUDGE * (1.0 - t)),
        )
        .into_any_element()
}

// ---- settings ----

/// 12/500 muted header above, optional 11 caption below, card of rows.
pub fn settings_group(
    id: impl Into<ElementId>,
    header: Option<&str>,
    footer: Option<&str>,
    rows: Vec<AnyElement>,
    cx: &App,
) -> AnyElement {
    let colors = &cx.theme().colors;
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .when_some(header, |el, h| {
            el.child(
                div().px(px(4.0)).text_size(px(12.0)).font_weight(FontWeight::MEDIUM).text_color(colors.fg_muted).child(h.to_string()),
            )
        })
        .child(group_card(id, rows, 10.0, cx))
        .when_some(footer, |el, f| el.child(div().px(px(4.0)).text_size(px(11.0)).text_color(colors.fg_subtle).child(f.to_string())))
        .into_any_element()
}

/// 44pt row: label left (13), trailing control or muted value right.
pub fn settings_row(label: impl Into<SharedString>, trailing: AnyElement, cx: &App) -> AnyElement {
    div()
        .h(px(44.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .justify_between()
        .gap(px(12.0))
        .text_size(px(13.0))
        .child(div().flex_none().text_color(cx.theme().colors.fg).child(label.into()))
        .child(div().min_w_0().flex().items_center().text_color(cx.theme().colors.fg_muted).child(trailing))
        .into_any_element()
}

// ---- tray ----

#[derive(Clone, Copy, PartialEq)]
pub enum TrayState {
    Idle,
    Transferring,
    Error,
}

/// SF Symbol for `TrayIcon::new`.
pub fn tray_symbol(state: TrayState) -> &'static str {
    match state {
        TrayState::Idle => "externaldrive",
        TrayState::Transferring => "externaldrive.badge.icloud",
        TrayState::Error => "externaldrive.badge.exclamationmark",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_symbols_are_distinct() {
        let names = [TrayState::Idle, TrayState::Transferring, TrayState::Error].map(tray_symbol);
        assert!(names.iter().all(|n| !n.is_empty()));
        assert!(names[0] != names[1] && names[1] != names[2] && names[0] != names[2]);
    }

    #[test]
    fn glyphs_follow_state() {
        assert_eq!(glyph(StepState::Pending(3)), "number");
        assert_eq!(glyph(StepState::Active(2)), "number");
        assert_eq!(glyph(StepState::Busy), "spinner");
        assert_eq!(glyph(StepState::Done), "check");
        assert_eq!(glyph(StepState::Failed), "x");
    }
}
