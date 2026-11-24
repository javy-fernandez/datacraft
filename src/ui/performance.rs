//! Low-power rendering for machines without a usable GPU.
//!
//! On Windows Server and in Remote Desktop sessions DirectX 12 usually runs on WARP (the
//! "Microsoft Basic Render Driver"), which rasterizes every frame on the CPU, and RDP then
//! encodes every frame again. Both costs scale with the number of frames, so low-power mode
//! draws fewer of them: no animations or scroll smoothing, no blurred shadows and at most
//! [`LOW_POWER_FPS`] frames per second while the user interacts.
use crate::i18n::tr;
use crate::ui::theme::pal;
use egui::{Align2, Color32, FontId, Id, LayerId, Order, RichText, Shadow, Ui};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Frame cap in low-power mode. Over RDP more frames are not visible anyway.
const LOW_POWER_FPS: f64 = 30.0;

/// egui only skips its wheel smoothing for deltas shorter than 8 points.
const WHEEL_STEP: f32 = 7.5;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RenderMode {
    /// Low-power on a software adapter or in a Remote Desktop session.
    #[default]
    Auto,
    LowPower,
    Full,
}

/// The adapter wgpu picked, as reported at startup.
pub struct Adapter {
    pub name: String,
    pub backend: String,
    /// Rendering on the CPU (WARP, llvmpipe...).
    pub software: bool,
}

pub struct Performance {
    pub mode: RenderMode,
    /// The Settings checkbox (persisted).
    pub show_stats: bool,
    /// `DATACRAFT_FPS=1`: statistics for this run only, never persisted.
    env_stats: bool,
    pub adapter: Option<Adapter>,
    /// Running in a Remote Desktop session.
    pub remote: bool,
    /// Low-power state last applied to the style.
    applied: Option<bool>,
    last_frame: Option<Instant>,
    /// Frame-cap waits of this frame and the previous one. eframe reports the previous frame's
    /// time, wait included, so the statistics take it out.
    slept: Duration,
    prev_slept: Duration,
    /// Start of the frames of the last second, for the frame statistics.
    frames: VecDeque<Instant>,
}

impl Performance {
    pub fn new(
        render_state: Option<&egui_wgpu::RenderState>,
        mode: RenderMode,
        show_stats: bool,
    ) -> Self {
        let adapter = render_state.map(|rs| {
            let info = rs.adapter.get_info();
            Adapter {
                software: info.device_type == wgpu::DeviceType::Cpu
                    || info.name.contains("Basic Render"),
                backend: format!("{:?}", info.backend),
                name: info.name,
            }
        });
        Self {
            mode,
            show_stats,
            env_stats: std::env::var("DATACRAFT_FPS").is_ok_and(|v| v == "1"),
            adapter,
            remote: remote_session(),
            applied: None,
            last_frame: None,
            slept: Duration::ZERO,
            prev_slept: Duration::ZERO,
            frames: VecDeque::new(),
        }
    }

    /// What `Auto` resolves to on this machine.
    fn auto_low_power(&self) -> bool {
        self.remote || self.adapter.as_ref().is_some_and(|a| a.software)
    }

    pub fn low_power(&self) -> bool {
        match self.mode {
            RenderMode::Auto => self.auto_low_power(),
            RenderMode::LowPower => true,
            RenderMode::Full => false,
        }
    }

    /// The low-power state changed since [`Self::apply_style`]: the theme must be reapplied
    /// (it restores the shadows) and this style on top of it.
    pub fn style_outdated(&self) -> bool {
        self.applied != Some(self.low_power())
    }

    /// Applies the low-power style on top of the theme (call it right after `theme::apply`).
    pub fn apply_style(&mut self, ctx: &egui::Context) {
        let low = self.low_power();
        let defaults = egui::Style::default();
        ctx.all_styles_mut(|s| {
            if low {
                s.animation_time = 0.0;
                s.scroll_animation = egui::style::ScrollAnimation::none();
                s.visuals.window_shadow = Shadow::NONE;
                s.visuals.popup_shadow = Shadow::NONE;
            } else {
                s.animation_time = defaults.animation_time;
                s.scroll_animation = defaults.scroll_animation;
            }
        });
        self.applied = Some(low);
    }

    /// Start of a frame: caps the frame rate in low-power mode. Input that arrives while
    /// waiting is handled by the next frame, so nothing is lost.
    pub fn begin_frame(&mut self) {
        let now = Instant::now();
        self.prev_slept = std::mem::take(&mut self.slept);
        if self.low_power()
            && let Some(last) = self.last_frame
        {
            let min = Duration::from_secs_f64(1.0 / LOW_POWER_FPS);
            let elapsed = now - last;
            if elapsed < min {
                self.slept = min - elapsed;
                std::thread::sleep(self.slept);
            }
        }
        let now = Instant::now();
        self.last_frame = Some(now);
        self.frames.push_back(now);
        while self
            .frames
            .front()
            .is_some_and(|t| now - *t > Duration::from_secs(1))
        {
            self.frames.pop_front();
        }
    }

    /// egui spreads each mouse wheel notch over ~10 frames to animate it. In low-power mode the
    /// notch is split into steps short enough for egui to apply them all in this frame.
    pub fn instant_wheel(&self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        if !self.low_power()
            || !raw
                .events
                .iter()
                .any(|e| matches!(e, egui::Event::MouseWheel { .. }))
        {
            return;
        }
        let line = ctx.options(|o| o.input_options.line_scroll_speed);
        let page = raw.screen_rect.map_or(600.0, |r| r.height());
        let events = std::mem::take(&mut raw.events);
        for event in events {
            match event {
                egui::Event::MouseWheel {
                    unit,
                    delta,
                    phase: egui::TouchPhase::Move,
                    modifiers,
                } => {
                    let delta = match unit {
                        egui::MouseWheelUnit::Point => delta,
                        egui::MouseWheelUnit::Line => delta * line,
                        egui::MouseWheelUnit::Page => delta * page,
                    };
                    let steps = (delta.length() / WHEEL_STEP).ceil().max(1.0);
                    for _ in 0..steps as usize {
                        raw.events.push(egui::Event::MouseWheel {
                            unit: egui::MouseWheelUnit::Point,
                            delta: delta / steps,
                            phase: egui::TouchPhase::Move,
                            modifiers,
                        });
                    }
                }
                other => raw.events.push(other),
            }
        }
    }

    /// Frame statistics in the bottom-right corner (Settings, or `DATACRAFT_FPS=1`).
    /// `frame_time` is eframe's `cpu_usage`: the previous frame, painting included.
    pub fn stats_overlay(&self, ctx: &egui::Context, frame_time: Option<f32>) {
        if !self.show_stats && !self.env_stats {
            return;
        }
        let adapter = self
            .adapter
            .as_ref()
            .map_or("?".to_string(), |a| format!("{} ({})", a.name, a.backend));
        let mode = if self.low_power() {
            "low-power"
        } else {
            "full"
        };
        let ms = frame_time.map_or("-".to_string(), |s| {
            format!(
                "{:.1} ms",
                (s - self.prev_slept.as_secs_f32()).max(0.0) * 1000.0
            )
        });
        let text = format!(
            "{} fps · frame {ms} · {mode} · {adapter}",
            self.frames.len()
        );
        let painter = ctx.layer_painter(LayerId::new(Order::Debug, Id::new("frame_stats")));
        let pos = ctx.content_rect().right_bottom() - egui::vec2(8.0, 36.0);
        let galley = painter.layout_no_wrap(text, FontId::monospace(11.0), Color32::WHITE);
        let rect = Align2::RIGHT_BOTTOM
            .anchor_size(pos, galley.size())
            .expand(4.0);
        painter.rect_filled(rect, 4.0, Color32::from_black_alpha(190));
        painter.galley(rect.min + egui::vec2(4.0, 4.0), galley, Color32::WHITE);
    }

    /// Rendering section of the Settings dialog.
    pub fn settings_ui(&mut self, ui: &mut Ui) {
        let muted = pal(ui).muted;
        ui.label(RichText::new(tr("Rendering")).small().color(muted));
        if let Some(a) = &self.adapter {
            let kind = if a.software {
                format!(" · {}", tr("software (CPU)"))
            } else {
                String::new()
            };
            ui.label(RichText::new(format!("{} ({}){kind}", a.name, a.backend)).small());
        }
        if self.remote {
            ui.label(RichText::new(tr("Remote Desktop session")).small());
        }
        let auto = if self.auto_low_power() {
            tr("Automatic (low-power)")
        } else {
            tr("Automatic (full quality)")
        };
        ui.radio_value(&mut self.mode, RenderMode::Auto, auto);
        ui.radio_value(&mut self.mode, RenderMode::LowPower, tr("Low-power"));
        ui.radio_value(&mut self.mode, RenderMode::Full, tr("Full quality"));
        ui.checkbox(&mut self.show_stats, tr("Show frame statistics"));
    }
}

#[cfg(windows)]
fn remote_session() -> bool {
    const SM_REMOTESESSION: i32 = 0x1000;
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetSystemMetrics(index: i32) -> i32;
    }
    // SAFETY: takes an integer and returns one; no pointers or preconditions.
    unsafe { GetSystemMetrics(SM_REMOTESESSION) != 0 }
}

#[cfg(not(windows))]
fn remote_session() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perf(mode: RenderMode, software: bool, remote: bool) -> Performance {
        Performance {
            mode,
            show_stats: false,
            env_stats: false,
            adapter: Some(Adapter {
                name: "Microsoft Basic Render Driver".into(),
                backend: "Dx12".into(),
                software,
            }),
            remote,
            applied: None,
            last_frame: None,
            slept: Duration::ZERO,
            prev_slept: Duration::ZERO,
            frames: VecDeque::new(),
        }
    }

    #[test]
    fn auto_is_low_power_on_software_adapters_and_remote_sessions() {
        assert!(perf(RenderMode::Auto, true, false).low_power());
        assert!(perf(RenderMode::Auto, false, true).low_power());
        assert!(!perf(RenderMode::Auto, false, false).low_power());
        assert!(perf(RenderMode::LowPower, false, false).low_power());
        assert!(!perf(RenderMode::Full, true, true).low_power());
    }

    #[test]
    fn leaving_low_power_restores_the_theme_style() {
        let ctx = egui::Context::default();
        let mut perf = perf(RenderMode::LowPower, true, false);
        crate::ui::theme::apply(&ctx, true);
        let themed = ctx.global_style();
        perf.apply_style(&ctx);
        assert_eq!(ctx.global_style().visuals.window_shadow, Shadow::NONE);
        assert_eq!(ctx.global_style().animation_time, 0.0);
        assert!(!perf.style_outdated());

        perf.mode = RenderMode::Full;
        assert!(perf.style_outdated());
        crate::ui::theme::apply(&ctx, true);
        perf.apply_style(&ctx);
        assert_eq!(
            ctx.global_style().visuals.window_shadow,
            themed.visuals.window_shadow
        );
        assert_eq!(ctx.global_style().animation_time, themed.animation_time);
        assert!(!perf.style_outdated());
    }

    #[test]
    fn wheel_notches_become_steps_egui_does_not_smooth() {
        let ctx = egui::Context::default();
        let mut raw = egui::RawInput {
            events: vec![
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Line,
                    delta: egui::vec2(0.0, -1.0),
                    phase: egui::TouchPhase::Move,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::Text("x".into()),
            ],
            ..Default::default()
        };
        perf(RenderMode::LowPower, true, false).instant_wheel(&ctx, &mut raw);
        let line = ctx.options(|o| o.input_options.line_scroll_speed);
        let mut total = 0.0;
        for e in &raw.events[..raw.events.len() - 1] {
            let egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta,
                ..
            } = e
            else {
                panic!("{e:?}")
            };
            assert!(delta.length() < 8.0);
            total += delta.y;
        }
        assert!((total + line).abs() < 0.01);
        assert!(matches!(raw.events.last(), Some(egui::Event::Text(_))));

        let mut untouched = raw.clone();
        perf(RenderMode::Full, true, false).instant_wheel(&ctx, &mut untouched);
        assert_eq!(untouched.events.len(), raw.events.len());
    }

    /// Scroll offset after one frame that receives a single wheel notch.
    fn offset_after_one_notch(low_power: bool) -> f32 {
        let ctx = egui::Context::default();
        let perf = perf(
            if low_power {
                RenderMode::LowPower
            } else {
                RenderMode::Full
            },
            true,
            false,
        );
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 300.0));
        let frame = |events: Vec<egui::Event>| {
            let mut raw = egui::RawInput {
                screen_rect: Some(screen),
                events,
                ..Default::default()
            };
            perf.instant_wheel(&ctx, &mut raw);
            let mut offset = 0.0;
            let mut output = ctx.run_ui(raw, |ui| {
                offset = egui::ScrollArea::vertical()
                    .show(ui, |ui| ui.allocate_space(egui::vec2(100.0, 5000.0)))
                    .state
                    .offset
                    .y;
            });
            output.textures_delta.clear();
            offset
        };
        frame(vec![egui::Event::PointerMoved(egui::pos2(50.0, 50.0))]);
        frame(vec![egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, -1.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::NONE,
        }]);
        frame(Vec::new())
    }

    #[test]
    fn a_wheel_notch_scrolls_at_once_in_low_power_mode() {
        let line = egui::Context::default().options(|o| o.input_options.line_scroll_speed);
        assert!((offset_after_one_notch(true) - line).abs() < 0.5);
        assert!(offset_after_one_notch(false) < line - 1.0);
    }
}
