// SPDX-License-Identifier: MPL-2.0

mod app;
mod config;
mod i18n;
mod idle_hook;

use config::Config;
use cosmic::cosmic_config::{self, CosmicConfigEntry};

const APP_ID: &str = "com.github.bgub.CosmicExtAppletVigil";

/// `--remove-suspend-override`: undo Vigil's change to cosmic-settings'
/// `Suspend` action. Run by `just uninstall` before the binary is deleted, and
/// usable by hand. Needs no GUI.
fn remove_suspend_override() -> std::process::ExitCode {
    let handler = match cosmic_config::Config::new(APP_ID, Config::VERSION) {
        Ok(handler) => handler,
        Err(e) => {
            eprintln!("cannot open Vigil's config: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let mut config = Config::get_entry(&handler).unwrap_or_else(|(_, config)| config);

    match idle_hook::remove(config.suspend_hook_installed_at) {
        Ok(removed) => {
            if removed {
                println!("Removed Vigil's Suspend command; it takes effect at your next login.");
            } else {
                println!("No Suspend command of Vigil's to remove.");
            }
            if config.suspend_hook_installed_at != 0 {
                config.suspend_hook_installed_at = 0;
                let _ = config.write_entry(&handler);
            }
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("cannot update the Suspend command: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn main() -> std::process::ExitCode {
    if std::env::args().nth(1).as_deref() == Some("--remove-suspend-override") {
        return remove_suspend_override();
    }

    let requested_languages = i18n_embed::DesktopLanguageRequester::requested_languages();
    i18n::init(&requested_languages);
    match cosmic::applet::run::<app::AppModel>(()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            std::process::ExitCode::FAILURE
        }
    }
}
