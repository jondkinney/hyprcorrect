//! The review popup — opens on the configured `review` chord and
//! shows the smart provider's proposed correction for the focused
//! window's last sentence. The user accepts (Enter) or cancels
//! (Esc); the daemon does the actual emit so focus has time to
//! return to the source window after the popup closes.

use std::time::Duration;

use eframe::egui;
use hyprcorrect_core::runtime::{self, ReviewRequest};

const APP_ID: &str = "hyprcorrect-review";
const REFOCUS_DELAY_MS: u64 = 150;

/// Run the review popup. Reads the pending review request from the
/// runtime file; if there isn't one, returns immediately (the
/// daemon spawns this binary blindly on the review chord and might
/// race against an empty buffer).
pub(crate) fn run() {
    let request = match runtime::read_review_request() {
        Ok(Some(req)) => req,
        Ok(None) => return,
        Err(e) => {
            eprintln!("hyprcorrect: could not read review request: {e}");
            return;
        }
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id(APP_ID)
            .with_title("hyprcorrect — Review")
            .with_inner_size([560.0, 280.0])
            .with_resizable(false),
        vsync: false,
        ..Default::default()
    };
    let _ = eframe::run_native(
        "hyprcorrect — Review",
        options,
        Box::new(move |cc| {
            crate::prefs::install_glyph_fonts(&cc.egui_ctx);
            Ok(Box::new(ReviewApp::new(request)))
        }),
    );
}

struct ReviewApp {
    request: ReviewRequest,
    /// `"apply"` or `"cancel"` once the user decides. `None` until
    /// the window closes (X-button close → cancel).
    decision: Option<&'static str>,
}

impl ReviewApp {
    fn new(request: ReviewRequest) -> Self {
        Self {
            request,
            decision: None,
        }
    }
}

impl eframe::App for ReviewApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Hotkeys: Enter applies, Esc cancels. Check early so the
        // user can decide without ever touching the mouse.
        let (apply, cancel) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Escape),
            )
        });
        if apply {
            self.decision = Some("apply");
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if cancel {
            self.decision = Some("cancel");
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::central_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(20, 18)),
            )
            .show(ctx, |ui| {
                ui.heading("Review correction");
                ui.add_space(12.0);

                section_label(ui, "Original");
                show_block(ui, &self.request.original, egui::Color32::from_gray(170));

                ui.add_space(14.0);
                section_label(ui, "Proposed");
                show_block(ui, &self.request.corrected, egui::Color32::from_gray(230));

                ui.add_space(20.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel  (Esc)").clicked() {
                        self.decision = Some("cancel");
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    let apply_label = egui::RichText::new("Apply  (↵)")
                        .color(egui::Color32::from_rgb(90, 200, 120));
                    if ui.button(apply_label).clicked() {
                        self.decision = Some("apply");
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Default to cancel if the user closed via the X button or
        // window-close event without making a choice.
        let decision = self.decision.unwrap_or("cancel");
        // The daemon picks up the next step by reading the trigger-
        // action file and re-signaling. Writing the action *before*
        // signaling ensures the daemon sees the right routing.
        let action = match decision {
            "apply" => "review-apply",
            _ => "review-cancel",
        };
        if let Err(e) = std::fs::write(runtime::action_path(), action) {
            eprintln!("hyprcorrect: could not write review action: {e}");
            return;
        }
        // Give Hyprland a beat to refocus the window the user came
        // from before the daemon's emit lands. The popup's window is
        // closing right now; the kernel won't deliver our SIGUSR1
        // until after this thread sleeps anyway, but we want the
        // refocus to be complete before the daemon's emit fires.
        std::thread::sleep(Duration::from_millis(REFOCUS_DELAY_MS));
        notify_daemon();
    }
}

fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).strong().size(14.0));
    ui.add_space(4.0);
}

fn show_block(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    egui::Frame::new()
        .fill(egui::Color32::from_gray(40))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(egui::RichText::new(text).color(color).size(14.0));
        });
}

fn notify_daemon() {
    let Ok(Some(pid)) = runtime::read_daemon_pid() else {
        return;
    };
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args(["-USR1", &pid.to_string()])
            .output();
    }
    #[cfg(not(unix))]
    let _ = pid;
}
