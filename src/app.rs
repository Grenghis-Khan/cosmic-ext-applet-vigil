// SPDX-License-Identifier: MPL-2.0

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::fl;
use cosmic::Theme;
use cosmic::cosmic_config::{self, CosmicConfigEntry};
use cosmic::iced::futures::SinkExt;
use cosmic::iced::{Alignment, Length, Limits, Subscription, window::Id};
use cosmic::iced_winit::commands::popup::{destroy_popup, get_popup};
use cosmic::prelude::*;
use cosmic::widget::{self, container};

const BOLT_SVG: &[u8] = include_bytes!("../resources/bolt.svg");
const INFINITY_SVG: &[u8] = include_bytes!("../resources/infinity.svg");

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[derive(Default)]
pub struct AppModel {
    core: cosmic::Core,
    popup: Option<Id>,
    config: Config,
    config_handler: Option<cosmic_config::Config>,
    /// Session-bus inhibit that keeps the screen on (`org.freedesktop.ScreenSaver`).
    screensaver_conn: Option<zbus::blocking::Connection>,
    screensaver_cookie: Option<u32>,
    /// System-bus inhibit that keeps the machine up (`org.freedesktop.login1`),
    /// held open as a file descriptor, alongside the `what` string it was
    /// taken with so a changed setting can re-acquire.
    logind_fd: Option<zbus::zvariant::OwnedFd>,
    logind_what: Option<String>,
}

impl Drop for AppModel {
    fn drop(&mut self) {
        // Release only this instance's inhibits. Deliberately does not touch
        // the shared config: one panel instance exiting must not turn Vigil off
        // on the others.
        self.release_all();
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    TogglePopup,
    ToggleVigil,
    Activate(u32),
    Deactivate,
    Tick,
    PopupClosed(Id),
    UpdateConfig(Config),
    SetAllowScreenOff(bool),
    SetAwakeWithLidClosed(bool),
}

impl AppModel {
    /// Whether Vigil is on. Shared by every panel instance via config.
    fn active(&self) -> bool {
        self.config.active
    }

    /// Seconds left before expiry; zero when indefinite or inactive.
    ///
    /// Derived from the shared deadline rather than counted down locally, so
    /// each monitor's instance shows the same value.
    fn remaining_secs(&self) -> u32 {
        if !self.config.active || self.config.expiry_ts == 0 {
            return 0;
        }
        u32::try_from(self.config.expiry_ts.saturating_sub(now_secs())).unwrap_or(u32::MAX)
    }

    /// Take a screensaver inhibit for this instance. Does not touch config.
    ///
    /// Every instance holds its own inhibit. `cosmic-idle` keeps inhibitors in
    /// a list and only lets the screen sleep once all of them are gone, so the
    /// duplication is harmless and avoids electing a single owner process.
    fn acquire_screensaver(&mut self) -> bool {
        if self.screensaver_cookie.is_some() {
            return true;
        }

        let conn = match zbus::blocking::Connection::session() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("failed to connect to session bus: {e}");
                return false;
            }
        };

        let reply = conn.call_method(
            Some("org.freedesktop.ScreenSaver"),
            "/org/freedesktop/ScreenSaver",
            Some("org.freedesktop.ScreenSaver"),
            "Inhibit",
            &("Vigil", "User requested screen stay awake"),
        );

        match reply {
            Ok(msg) => match msg.body().deserialize::<u32>() {
                Ok(cookie) => {
                    self.screensaver_conn = Some(conn);
                    self.screensaver_cookie = Some(cookie);
                    true
                }
                Err(e) => {
                    eprintln!("failed to parse Inhibit reply: {e}");
                    false
                }
            },
            Err(e) => {
                eprintln!("failed to call ScreenSaver.Inhibit: {e}");
                false
            }
        }
    }

    /// Drop this instance's screensaver inhibit. Does not touch config.
    fn release_screensaver(&mut self) {
        if let (Some(conn), Some(cookie)) =
            (self.screensaver_conn.take(), self.screensaver_cookie.take())
        {
            let _ = conn.call_method(
                Some("org.freedesktop.ScreenSaver"),
                "/org/freedesktop/ScreenSaver",
                Some("org.freedesktop.ScreenSaver"),
                "UnInhibit",
                &(cookie,),
            );
        }
    }

    /// The logind inhibit this instance should be holding, if any.
    ///
    /// `None` means no logind inhibit is wanted, which is the case whenever
    /// Vigil is off or is only being asked to keep the screen on.
    fn wanted_logind_what(&self) -> Option<String> {
        if !self.config.active {
            return None;
        }
        let mut what = Vec::new();
        if self.config.allow_screen_off {
            what.push("sleep");
        }
        if self.config.awake_with_lid_closed {
            what.push("handle-lid-switch");
        }
        (!what.is_empty()).then(|| what.join(":"))
    }

    /// Take a logind block inhibit. Does not touch config.
    ///
    /// Unlike the ScreenSaver cookie this is a file descriptor: logind holds
    /// the inhibit for exactly as long as the fd is open, so dropping it — or
    /// the process dying for any reason — releases it without a round trip.
    fn acquire_logind(&mut self, what: &str) -> bool {
        let conn = match zbus::blocking::Connection::system() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("failed to connect to system bus: {e}");
                return false;
            }
        };

        let reply = conn.call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "Inhibit",
            &(
                what,
                "Vigil",
                "User requested the system stay awake",
                "block",
            ),
        );

        match reply {
            Ok(msg) => match msg.body().deserialize::<zbus::zvariant::OwnedFd>() {
                Ok(fd) => {
                    self.logind_fd = Some(fd);
                    self.logind_what = Some(what.to_owned());
                    true
                }
                Err(e) => {
                    eprintln!("failed to parse logind Inhibit reply: {e}");
                    false
                }
            },
            Err(e) => {
                eprintln!("failed to call login1.Inhibit({what}): {e}");
                false
            }
        }
    }

    /// Drop this instance's logind inhibit by closing the fd.
    fn release_logind(&mut self) {
        self.logind_fd = None;
        self.logind_what = None;
    }

    /// Is this instance currently holding anything at all?
    fn is_inhibiting(&self) -> bool {
        self.screensaver_cookie.is_some() || self.logind_fd.is_some()
    }

    /// Bring the inhibits this instance holds in line with the shared config.
    ///
    /// Used when another monitor's instance toggled Vigil, and after any local
    /// change. Never writes config, which is what stops two instances from
    /// echoing updates back and forth.
    fn apply_inhibits(&mut self) {
        // Keeping the screen on and keeping the system up are separate
        // mechanisms: cosmic-idle's ScreenSaver inhibit suppresses screen-off
        // and suspend together, so "screen may sleep" has to go through logind.
        if self.config.active && !self.config.allow_screen_off {
            self.acquire_screensaver();
        } else {
            self.release_screensaver();
        }

        let wanted = self.wanted_logind_what();
        if self.logind_what != wanted {
            // The inhibit is immutable once taken, so a changed `what` string
            // means dropping the old fd and asking for a new one.
            self.release_logind();
            if let Some(what) = wanted {
                self.acquire_logind(&what);
            }
        }
    }

    /// Release everything this instance holds, without touching config.
    fn release_all(&mut self) {
        self.release_screensaver();
        self.release_logind();
    }

    /// Turn Vigil on in response to a user action, and publish that to the
    /// other instances.
    fn activate(&mut self, duration_mins: u32) {
        let previous = self.config.clone();

        self.config.duration_mins = duration_mins;
        self.config.active = true;
        self.config.expiry_ts = if duration_mins == 0 {
            0
        } else {
            now_secs() + u64::from(duration_mins) * 60
        };
        self.apply_inhibits();

        if self.is_inhibiting() {
            self.save_config();
        } else {
            // Nothing was acquired, so do not advertise an active state the
            // other instances would try and fail to match.
            self.config = previous;
            self.apply_inhibits();
        }
    }

    /// Turn Vigil off in response to a user action or expiry, and publish that
    /// to the other instances.
    fn deactivate(&mut self) {
        self.release_all();
        self.config.active = false;
        self.config.expiry_ts = 0;
        self.save_config();
    }

    /// Change one of the behaviour toggles, re-deriving what is held.
    fn set_allow_screen_off(&mut self, value: bool) {
        self.config.allow_screen_off = value;
        self.apply_inhibits();
        self.save_config();
    }

    fn set_awake_with_lid_closed(&mut self, value: bool) {
        self.config.awake_with_lid_closed = value;
        self.apply_inhibits();
        self.save_config();
    }

    fn is_indefinite(&self) -> bool {
        self.active() && self.config.duration_mins == 0
    }

    fn format_remaining(&self) -> String {
        if self.is_indefinite() {
            "\u{221e}".to_string()
        } else {
            let mins = self.remaining_secs().div_ceil(60);
            format!("{mins}")
        }
    }

    fn format_remaining_full(&self) -> String {
        if self.is_indefinite() {
            fl!("indefinite")
        } else {
            let secs = self.remaining_secs();
            format!("{:02}:{:02}", secs / 60, secs % 60)
        }
    }

    fn save_config(&self) {
        if let Some(ref handler) = self.config_handler {
            let _ = self.config.write_entry(handler);
        }
    }

    fn active_color() -> cosmic::iced::Color {
        cosmic::iced::Color::from_rgb(0.96, 0.76, 0.07)
    }

    fn active_color_muted() -> cosmic::iced::Color {
        let c = Self::active_color();
        cosmic::iced::Color::from_rgba(c.r, c.g, c.b, 0.3)
    }
}

fn colored_bg(color: cosmic::iced::Color, radius: f32) -> impl Fn(&Theme) -> container::Style {
    move |_theme: &Theme| container::Style {
        background: Some(color.into()),
        border: cosmic::iced::Border {
            radius: radius.into(),
            ..Default::default()
        },
        ..container::Style::default()
    }
}

/// Accent outline marking the selected duration while Vigil is idle.
///
/// Deliberately distinct from `Button::Suggested`'s filled accent, which is
/// reserved for the preset that is actually running: an outline reads as
/// "this is what will start" rather than "this is running now".
fn selected_outline() -> cosmic::theme::Button {
    fn base(theme: &Theme) -> widget::button::Style {
        let cosmic = theme.cosmic();
        widget::button::Style {
            border_radius: cosmic.corner_radii.radius_xl.into(),
            border_width: 2.0,
            border_color: cosmic.accent.base.into(),
            text_color: Some(cosmic.accent_text_color().into()),
            icon_color: Some(cosmic.accent_text_color().into()),
            ..widget::button::Style::new()
        }
    }

    fn filled(theme: &Theme, color: cosmic::iced::Color) -> widget::button::Style {
        widget::button::Style {
            background: Some(color.into()),
            ..base(theme)
        }
    }

    cosmic::theme::Button::Custom {
        active: Box::new(|_focused, theme| base(theme)),
        disabled: Box::new(base),
        hovered: Box::new(|_focused, theme| filled(theme, theme.cosmic().button.hover.into())),
        pressed: Box::new(|_focused, theme| filled(theme, theme.cosmic().button.pressed.into())),
    }
}

impl cosmic::Application for AppModel {
    type Executor = cosmic::executor::Default;
    type Flags = ();
    type Message = Message;

    const APP_ID: &'static str = "com.github.bgub.CosmicExtAppletVigil";

    fn core(&self) -> &cosmic::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    fn init(
        core: cosmic::Core,
        _flags: Self::Flags,
    ) -> (Self, Task<cosmic::Action<Self::Message>>) {
        let (config, config_handler) =
            match cosmic_config::Config::new(Self::APP_ID, Config::VERSION) {
                Ok(handler) => {
                    let config = match Config::get_entry(&handler) {
                        Ok(config) => config,
                        Err((_errors, config)) => config,
                    };
                    (config, Some(handler))
                }
                Err(_) => (Config::default(), None),
            };

        let mut app = AppModel {
            core,
            config,
            config_handler,
            popup: None,
            screensaver_conn: None,
            screensaver_cookie: None,
            logind_fd: None,
            logind_what: None,
        };

        // Adopt whatever state is already shared. This covers an instance
        // starting late (a monitor plugged in while Vigil is on) and
        // `cosmic-panel` respawning an applet it killed.
        if app.config.active {
            if app.config.expiry_ts != 0 && now_secs() >= app.config.expiry_ts {
                app.deactivate();
            } else {
                app.apply_inhibits();
            }
        }

        (app, Task::none())
    }

    fn on_close_requested(&self, id: Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    fn view(&self) -> Element<'_, Self::Message> {
        let (_, panel_h) = self.core.applet.suggested_window_size();
        #[allow(clippy::cast_precision_loss)]
        let diameter = panel_h.get() as f32;
        let radius = diameter / 2.0;

        let icon = widget::icon(widget::icon::from_svg_bytes(BOLT_SVG).symbolic(true))
            .width(Length::Fixed(18.0))
            .height(Length::Fixed(18.0));

        let bg_color = if self.active() {
            Self::active_color_muted()
        } else {
            cosmic::iced::Color::TRANSPARENT
        };

        let mut row = widget::row().push(icon).align_y(Alignment::Center);

        if self.active() {
            if self.is_indefinite() {
                row = row.push(
                    widget::icon(widget::icon::from_svg_bytes(INFINITY_SVG).symbolic(true))
                        .width(Length::Fixed(14.0))
                        .height(Length::Fixed(7.0)),
                );
            } else {
                row = row.push(widget::text(self.format_remaining()).size(14.0));
            }
            row = row.spacing(4);
        }

        let active = self.active();
        let pill_height = diameter * 0.8;
        let pill_radius = pill_height / 2.0;
        let content = widget::container(row)
            .height(Length::Fixed(if active { pill_height } else { diameter }))
            .width(if active {
                Length::Shrink
            } else {
                Length::Fixed(diameter)
            })
            .align_x(Alignment::Center)
            .align_y(Alignment::Center)
            .padding(if active {
                [0.0, pill_radius / 2.0]
            } else {
                [0.0, 0.0]
            })
            .style(move |_theme: &Theme| {
                let r = if active { pill_radius } else { radius };
                container::Style {
                    background: Some(bg_color.into()),
                    border: cosmic::iced::Border {
                        radius: r.into(),
                        ..Default::default()
                    },
                    ..container::Style::default()
                }
            });

        let btn = widget::button::custom(self.core.applet.autosize_window(content))
            .on_press(Message::ToggleVigil)
            .class(cosmic::theme::Button::AppletIcon);

        widget::mouse_area(btn)
            .on_right_release(Message::TogglePopup)
            .into()
    }

    fn view_window(&self, _id: Id) -> Element<'_, Self::Message> {
        let status_text = if self.active() {
            fl!("active")
        } else {
            fl!("inactive")
        };

        let status_color = if self.active() {
            Self::active_color_muted()
        } else {
            cosmic::iced::Color::from_rgba(1.0, 1.0, 1.0, 0.08)
        };

        let status_row = widget::row()
            .push(widget::text::heading(status_text))
            .push(widget::space().width(Length::Fill))
            .push_maybe(if self.active() {
                Some(widget::text::heading(self.format_remaining_full()))
            } else {
                None
            })
            .align_y(Alignment::Center);

        let status_block = widget::container(status_row)
            .width(Length::Fill)
            .padding([10, 16])
            .style(colored_bg(status_color, 12.0));

        // Duration presets. The selected preset is always marked, so the popup
        // shows which duration will be used before Vigil is started rather than
        // only once it is running.
        let selected_mins = self.config.duration_mins;
        let running = self.active();

        let preset_class = |mins: u32| {
            if selected_mins != mins {
                cosmic::theme::Button::Standard
            } else if running {
                cosmic::theme::Button::Suggested
            } else {
                selected_outline()
            }
        };

        // Pressing the running preset stops Vigil. Every other press starts it
        // with that duration, which also covers switching duration mid-run.
        let preset_msg = |mins: u32| {
            if selected_mins == mins && running {
                Message::Deactivate
            } else {
                Message::Activate(mins)
            }
        };

        let dur_btn = |mins: u32, label: &'static str| {
            widget::button::standard(label)
                .on_press(preset_msg(mins))
                .width(Length::Fill)
                .class(preset_class(mins))
        };

        let infinity_btn = widget::button::custom(
            widget::text("\u{221e}").size(26.0).line_height(0.8).center(),
        )
        .padding([6, 12])
        .on_press(preset_msg(0))
        .width(Length::Shrink)
        .class(preset_class(0));

        let duration_row = widget::row()
            .push(dur_btn(15, "15m"))
            .push(dur_btn(30, "30m"))
            .push(dur_btn(60, "1h"))
            .push(dur_btn(120, "2h"))
            .push(infinity_btn)
            .spacing(6)
            .align_y(Alignment::Center);

        // Behaviour toggles. Both default off, so Vigil behaves exactly as
        // before until one is turned on.
        let allow_screen_off = widget::toggler(self.config.allow_screen_off)
            .label(fl!("allow-screen-off"))
            .on_toggle(Message::SetAllowScreenOff)
            .width(Length::Fill);

        let awake_with_lid_closed = widget::toggler(self.config.awake_with_lid_closed)
            .label(fl!("awake-with-lid-closed"))
            .on_toggle(Message::SetAwakeWithLidClosed)
            .width(Length::Fill);

        let content = widget::column()
            .push(status_block)
            .push(widget::text::heading(fl!("duration")))
            .push(duration_row)
            .push(widget::divider::horizontal::default())
            .push(allow_screen_off)
            .push(awake_with_lid_closed)
            .spacing(12)
            .align_x(Alignment::Center)
            .padding(12);

        self.core.applet.popup_container(content).into()
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        let mut subs = vec![
            self.core()
                .watch_config::<Config>(Self::APP_ID)
                .map(|update| Message::UpdateConfig(update.config)),
        ];

        if self.active() && !self.is_indefinite() {
            struct TimerTick;
            subs.push(Subscription::run_with(
                std::any::TypeId::of::<TimerTick>(),
                |_| {
                    cosmic::iced::stream::channel::<Message>(1, async |mut channel| {
                        loop {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            _ = channel.send(Message::Tick).await;
                        }
                    })
                },
            ));
        }

        Subscription::batch(subs)
    }

    fn update(&mut self, message: Self::Message) -> Task<cosmic::Action<Self::Message>> {
        match message {
            Message::ToggleVigil => {
                if self.active() {
                    self.deactivate();
                } else {
                    let duration = self.config.duration_mins;
                    self.activate(duration);
                }
            }
            Message::Activate(mins) => {
                self.activate(mins);
            }
            Message::Deactivate => {
                self.deactivate();
            }
            Message::Tick => {
                // Only drives the countdown redraw; the deadline itself lives
                // in config, so both instances expire at the same moment.
                if self.remaining_secs() == 0 {
                    self.deactivate();
                }
            }
            Message::UpdateConfig(config) => {
                self.config = config;
                self.apply_inhibits();
            }
            Message::SetAllowScreenOff(value) => {
                self.set_allow_screen_off(value);
            }
            Message::SetAwakeWithLidClosed(value) => {
                self.set_awake_with_lid_closed(value);
            }
            Message::TogglePopup => {
                return if let Some(p) = self.popup.take() {
                    destroy_popup(p)
                } else {
                    let new_id = Id::unique();
                    self.popup.replace(new_id);
                    let mut popup_settings = self.core.applet.get_popup_settings(
                        self.core.main_window_id().unwrap(),
                        new_id,
                        None,
                        None,
                        None,
                    );
                    popup_settings.positioner.size_limits = Limits::NONE
                        .max_width(372.0)
                        .min_width(300.0)
                        .min_height(100.0)
                        .max_height(400.0);
                    get_popup(popup_settings)
                };
            }
            Message::PopupClosed(id) => {
                if self.popup.as_ref() == Some(&id) {
                    self.popup = None;
                }
            }
        }
        Task::none()
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The custom style closures run inside the renderer; a panic or a deadlock
    /// on the global THEME lock would take the applet down on popup open.
    #[test]
    fn selected_outline_styles_resolve() {
        let theme = Theme::default();
        let cosmic::theme::Button::Custom {
            active,
            disabled,
            hovered,
            pressed,
        } = selected_outline()
        else {
            panic!("expected a Custom button style");
        };

        for style in [active(false, &theme), active(true, &theme), disabled(&theme)] {
            assert_eq!(style.border_width, 2.0);
        }
        for style in [
            hovered(false, &theme),
            pressed(false, &theme),
            hovered(true, &theme),
            pressed(true, &theme),
        ] {
            assert!(style.background.is_some(), "hover/press should fill");
            assert_eq!(style.border_width, 2.0, "outline persists on hover/press");
        }
    }
}
