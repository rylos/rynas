//! rynas web interface: Leptos CSR shell, built with trunk (see `Trunk.toml`).

mod alerts;
mod api;
mod auth;
mod chart;
mod chassis;
mod config;
#[cfg(feature = "demo")]
mod demo;
mod form;
mod i18n;
mod icons;
mod layout;
mod live;
mod notify;
mod pages;
mod palette;
mod prefs;
mod qr;
mod shell;
#[cfg(test)]
mod source_check;
#[cfg(test)]
mod style_check;
mod tasks;
mod toast;
mod ui;

fn main() {
    console_error_panic_hook::set_once();
    i18n::init();
    api::init();
    config::init();
    toast::init();
    leptos::mount::mount_to_body(shell::App);
    // the static boot screen in index.html stays until the first real paint
    if let Some(boot) = leptos::prelude::document().get_element_by_id("boot") {
        boot.remove();
    }
}
