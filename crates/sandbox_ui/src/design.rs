//! Building blocks shared by the Sands panels, so every screen uses the same type scale,
//! spacing and status colors.

use gpui::{
    AnyElement, App, Bounds, ClickEvent, ElementId, Entity, FontWeight, Hsla, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, SharedString, Window, canvas, px,
    relative,
};
use ui::{Tooltip, prelude::*};

/// Status of a project, container, agent or check, shared by badges, dots and meters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone {
    Ok,
    Warn,
    Info,
    Muted,
}

impl Tone {
    pub(crate) fn color(self) -> Color {
        match self {
            Tone::Ok => Color::Success,
            Tone::Warn => Color::Warning,
            Tone::Info => Color::Accent,
            Tone::Muted => Color::Muted,
        }
    }

    pub(crate) fn hsla(self, cx: &App) -> Hsla {
        let status = cx.theme().status();
        match self {
            Tone::Ok => status.success,
            Tone::Warn => status.warning,
            Tone::Info => status.info,
            Tone::Muted => cx.theme().colors().text_muted,
        }
    }

    fn background(self, cx: &App) -> (Hsla, Hsla) {
        let status = cx.theme().status();
        match self {
            Tone::Ok => (status.success_background, status.success_border),
            Tone::Warn => (status.warning_background, status.warning_border),
            Tone::Info => (status.info_background, status.info_border),
            Tone::Muted => (
                cx.theme().colors().element_background,
                cx.theme().colors().border_variant,
            ),
        }
    }
}

/// An uppercase caption above a group of rows, with an optional control on the right.
pub(crate) fn section_header(
    title: impl Into<SharedString>,
    trailing: Option<AnyElement>,
) -> impl IntoElement {
    h_flex()
        .h_6()
        .justify_between()
        .child(
            Label::new(title.into().to_uppercase())
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .weight(FontWeight::SEMIBOLD),
        )
        .children(trailing)
}

pub(crate) fn status_dot(tone: Tone, cx: &App) -> impl IntoElement {
    div()
        .flex_none()
        .size(px(7.))
        .rounded_full()
        .bg(tone.hsla(cx))
}

pub(crate) fn badge(text: impl Into<SharedString>, tone: Tone, cx: &App) -> impl IntoElement {
    let (background, border) = tone.background(cx);
    div()
        .flex_none()
        .px_1p5()
        .rounded_sm()
        .border_1()
        .border_color(border)
        .bg(background)
        .child(
            Label::new(text)
                .size(LabelSize::XSmall)
                .color(tone.color())
                .single_line(),
        )
}

/// A row of muted secondary text.
pub(crate) fn caption(text: impl Into<SharedString>) -> Label {
    Label::new(text).size(LabelSize::Small).color(Color::Muted)
}

/// A selectable card: an icon, a title and a description. Used where a choice needs explaining,
/// such as the network mode or the agent of a project.
#[derive(IntoElement)]
pub(crate) struct OptionCard {
    id: ElementId,
    icon: IconName,
    title: SharedString,
    description: Option<SharedString>,
    selected: bool,
    on_click: Option<Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>>,
}

impl OptionCard {
    pub(crate) fn new(
        id: impl Into<ElementId>,
        icon: IconName,
        title: impl Into<SharedString>,
    ) -> Self {
        Self {
            id: id.into(),
            icon,
            title: title.into(),
            description: None,
            selected: false,
            on_click: None,
        }
    }

    pub(crate) fn description(mut self, description: impl Into<SharedString>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub(crate) fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub(crate) fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for OptionCard {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let colors = cx.theme().colors();
        let border = if self.selected {
            colors.border_focused
        } else {
            colors.border
        };
        let background = if self.selected {
            colors.element_selected
        } else {
            colors.surface_background
        };
        let hover = colors.element_hover;
        v_flex()
            .id(self.id)
            .flex_1()
            .min_w_0()
            .p_2p5()
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(border)
            .bg(background)
            .cursor_pointer()
            .when(!self.selected, |this| {
                this.hover(move |style| style.bg(hover))
            })
            .when_some(self.on_click, |this, handler| this.on_click(handler))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(self.icon)
                            .size(IconSize::Small)
                            .color(if self.selected {
                                Color::Accent
                            } else {
                                Color::Muted
                            }),
                    )
                    .child(
                        Label::new(self.title)
                            .size(LabelSize::Small)
                            .weight(FontWeight::MEDIUM),
                    ),
            )
            .children(self.description.map(|description| {
                Label::new(description)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
            }))
    }
}

/// Numbered steps of a wizard, the current one highlighted.
pub(crate) fn stepper(steps: &[&'static str], current: usize, cx: &App) -> impl IntoElement {
    let colors = cx.theme().colors();
    h_flex()
        .gap_2()
        .children(steps.iter().enumerate().map(|(index, title)| {
            let done = index < current;
            let active = index == current;
            let (circle_background, number_color) = if active {
                (colors.border_focused, Color::Default)
            } else if done {
                (colors.element_selected, Color::Muted)
            } else {
                (colors.element_background, Color::Muted)
            };
            h_flex()
                .gap_1p5()
                .when(index > 0, |this| {
                    this.child(div().w_4().h_px().bg(colors.border))
                })
                .child(
                    div()
                        .size_5()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(circle_background)
                        .child(if done {
                            Icon::new(IconName::Check)
                                .size(IconSize::XSmall)
                                .color(Color::Muted)
                                .into_any_element()
                        } else {
                            Label::new((index + 1).to_string())
                                .size(LabelSize::XSmall)
                                .color(number_color)
                                .into_any_element()
                        }),
                )
                .child(Label::new(*title).size(LabelSize::Small).color(if active {
                    Color::Default
                } else {
                    Color::Muted
                }))
        }))
}

/// The one emphasized action of a screen: a filled accent button.
#[derive(IntoElement)]
pub(crate) struct PrimaryButton {
    id: ElementId,
    label: SharedString,
    secondary: bool,
    disabled: bool,
    on_click: Option<Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>>,
}

pub(crate) fn primary_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> PrimaryButton {
    PrimaryButton {
        id: id.into(),
        label: label.into(),
        secondary: false,
        disabled: false,
        on_click: None,
    }
}

/// A bordered button for actions next to a field, such as Browse.
pub(crate) fn secondary_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> PrimaryButton {
    PrimaryButton {
        secondary: true,
        ..primary_button(id, label)
    }
}

impl PrimaryButton {
    pub(crate) fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub(crate) fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for PrimaryButton {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let colors = cx.theme().colors();
        let (background, hover, border, text) = if self.secondary {
            (
                colors.element_background,
                colors.element_hover,
                colors.border,
                Color::Default,
            )
        } else {
            let accent = colors.border_focused;
            (
                accent,
                accent.opacity(0.85),
                accent,
                Color::Custom(gpui::white()),
            )
        };
        h_flex()
            .id(self.id)
            .flex_none()
            .h_7()
            .px_3()
            .justify_center()
            .rounded_md()
            .border_1()
            .border_color(border)
            .bg(background)
            .child(
                Label::new(self.label)
                    .size(LabelSize::Small)
                    .weight(FontWeight::MEDIUM)
                    .color(text),
            )
            .when(self.disabled, |this| this.opacity(0.5))
            .when(!self.disabled, |this| {
                this.cursor_pointer()
                    .hover(move |style| style.bg(hover))
                    .when_some(self.on_click, |this, handler| this.on_click(handler))
            })
    }
}

pub(crate) fn icon_button(
    id: impl Into<ElementId>,
    icon: IconName,
    tooltip: &'static str,
) -> IconButton {
    IconButton::new(id, icon)
        .icon_size(IconSize::Small)
        .tooltip(Tooltip::text(tooltip))
}

/// A range control like the resource sliders of Docker Desktop: drag the thumb or click the
/// track. Values snap to `step`. The scale can run past the selectable `limit`, to show the
/// whole of a resource and the part that cannot be given away.
pub(crate) struct Slider {
    value: f64,
    min: f64,
    max: f64,
    limit: f64,
    step: f64,
    dragging: bool,
}

impl Slider {
    pub(crate) fn new(value: f64, min: f64, max: f64, step: f64) -> Self {
        let max = max.max(min);
        let mut slider = Self {
            value,
            min,
            max,
            limit: max,
            step: step.max(f64::EPSILON),
            dragging: false,
        };
        slider.value = slider.snap(value);
        slider
    }

    /// Keeps values at or below `limit` while the scale still ends at `max`.
    pub(crate) fn with_limit(mut self, limit: f64) -> Self {
        self.limit = limit.clamp(self.min, self.max);
        self.value = self.snap(self.value);
        self
    }

    pub(crate) fn value(&self) -> f64 {
        self.value
    }

    pub(crate) fn set_value(&mut self, value: f64, cx: &mut Context<Self>) {
        let value = self.snap(value);
        if value != self.value {
            self.value = value;
            cx.notify();
        }
    }

    fn snap(&self, value: f64) -> f64 {
        let steps = ((value - self.min) / self.step).round();
        (self.min + steps * self.step).clamp(self.min, self.limit)
    }

    fn fraction_of(&self, value: f64) -> f32 {
        if self.max <= self.min {
            0.
        } else {
            ((value - self.min) / (self.max - self.min)) as f32
        }
    }

    fn fraction(&self) -> f32 {
        self.fraction_of(self.value)
    }

    fn set_from_position(&mut self, x: Pixels, bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        let width = f32::from(bounds.size.width).max(1.);
        let fraction = (f32::from(x - bounds.origin.x) / width).clamp(0., 1.) as f64;
        self.set_value(self.min + fraction * (self.max - self.min), cx);
    }
}

impl Render for Slider {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let fraction = self.fraction();
        let limit_fraction = self.fraction_of(self.limit);
        let entity = cx.entity();
        let dragging = self.dragging;
        div()
            .id("slider")
            .relative()
            .h(px(20.))
            .w_full()
            .cursor_pointer()
            .child(
                div()
                    .absolute()
                    .top(px(8.))
                    .left_0()
                    .right_0()
                    .h(px(4.))
                    .rounded_full()
                    .bg(colors.element_active)
                    .child(
                        div()
                            .h_full()
                            .rounded_full()
                            .bg(colors.border_focused)
                            .w(relative(fraction)),
                    ),
            )
            .when(limit_fraction < 1., |this| {
                let unavailable = cx.theme().status().warning_background;
                let marker = cx.theme().status().warning;
                this.child(
                    div()
                        .absolute()
                        .top(px(8.))
                        .left(relative(limit_fraction))
                        .right_0()
                        .h(px(4.))
                        .rounded_r_full()
                        .bg(unavailable),
                )
                .child(
                    div()
                        .absolute()
                        .top(px(5.))
                        .left(relative(limit_fraction))
                        .w(px(2.))
                        .h(px(10.))
                        .rounded_full()
                        .bg(marker),
                )
            })
            .child(
                div()
                    .absolute()
                    .top(px(3.))
                    .left(relative(fraction))
                    .ml(px(-7.))
                    .size(px(14.))
                    .rounded_full()
                    .border_2()
                    .border_color(colors.border_focused)
                    .bg(colors.text),
            )
            .child(
                canvas(
                    |bounds, _, _| bounds,
                    move |bounds, _, window, _| {
                        let down_entity = entity.clone();
                        window.on_mouse_event(move |event: &MouseDownEvent, phase, _, cx| {
                            if phase.bubble()
                                && event.button == MouseButton::Left
                                && bounds.contains(&event.position)
                            {
                                down_entity.update(cx, |slider, cx| {
                                    slider.dragging = true;
                                    slider.set_from_position(event.position.x, bounds, cx);
                                });
                            }
                        });
                        if dragging {
                            let move_entity = entity.clone();
                            window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                                if phase.bubble() && event.dragging() {
                                    move_entity.update(cx, |slider, cx| {
                                        slider.set_from_position(event.position.x, bounds, cx);
                                    });
                                }
                            });
                            window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                                if phase.bubble() {
                                    entity.update(cx, |slider, cx| {
                                        slider.dragging = false;
                                        cx.notify();
                                    });
                                }
                            });
                        }
                    },
                )
                .absolute()
                .size_full(),
            )
    }
}

/// A slider with its title, current value and the range limits underneath.
pub(crate) fn slider_field(
    title: impl Into<SharedString>,
    value_text: impl Into<SharedString>,
    slider: &Entity<Slider>,
    min_text: impl Into<SharedString>,
    max_text: impl Into<SharedString>,
) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(
            h_flex()
                .justify_between()
                .child(Label::new(title).size(LabelSize::Small))
                .child(
                    Label::new(value_text)
                        .size(LabelSize::Small)
                        .weight(FontWeight::MEDIUM),
                ),
        )
        .child(slider.clone())
        .child(
            h_flex()
                .justify_between()
                .child(
                    Label::new(min_text)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(max_text)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sliders_snap_and_clamp() {
        let slider = Slider::new(3.3, 1.0, 8.0, 1.0);
        assert_eq!(slider.value(), 3.0);
        assert_eq!(Slider::new(99.0, 1.0, 8.0, 1.0).value(), 8.0);
        assert_eq!(Slider::new(0.6, 0.5, 24.0, 0.5).value(), 0.5);
        assert_eq!(Slider::new(5.0, 1.0, 9.0, 1.0).fraction(), 0.5);
        let limited = Slider::new(20.0, 0.5, 24.0, 0.5).with_limit(18.0);
        assert_eq!(limited.value(), 18.0);
        assert_eq!(limited.fraction_of(18.0), 17.5 / 23.5);
    }
}
