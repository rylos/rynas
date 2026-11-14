//! "About rynas": opened from the version at the bottom of the sidebar. Names and the
//! copyright line stay literal in every language.

use leptos::prelude::*;

use crate::i18n::tr;
use crate::icons::{self, Icon};
use crate::ui::Modal;

const REPO: &str = env!("CARGO_PKG_REPOSITORY");

/// (name, role, GitHub profile)
const AUTHORS: &[(&str, &str, &str)] = &[("Marco Ziliani", "about.role.author", "https://github.com/rylos")];

#[component]
pub fn AboutDialog(on_close: Callback<()>) -> impl IntoView {
    let title = Signal::derive(move || tr("about.title")().to_string());
    let link = |href: String, label: &'static str, icon: &'static str| {
        view! {
            <a class="about-row-link" href=href target="_blank" rel="noopener noreferrer">
                <Icon icon=icon size=16 />
                <span>{tr(label)}</span>
                <Icon icon=icons::EXTERNAL_LINK size=14 />
            </a>
        }
    };
    view! {
        <Modal
            title=title
            on_close=on_close
            foot=move || view! { <button type="button" class="btn" on:click=move |_| on_close.run(())>{tr("close")}</button> }
        >
            <div class="about">
                <div class="about-head">
                    <span class="about-logo">{crate::auth::logo()}</span>
                    <div>
                        <div class="about-name">"rynas"</div>
                        <div class="about-version">{crate::i18n::tf("about.version", &[("version", env!("CARGO_PKG_VERSION"))])}</div>
                    </div>
                </div>
                <p class="about-text-free">{tr("about.free")}</p>
                <p class="about-copy">"© 2026 Marco Ziliani"</p>
                <div class="about-section">
                    <div class="about-subtitle">{tr("about.authors")}</div>
                    {AUTHORS
                        .iter()
                        .map(|(name, role, profile)| view! {
                            <div class="about-person">
                                <div>
                                    <div class="about-person-name">{*name}</div>
                                    <div class="about-person-role">{tr(role)}</div>
                                </div>
                                <a class="icon-btn sm" href=*profile target="_blank" rel="noopener noreferrer" aria-label=*profile title=*profile>
                                    <Icon icon=icons::GITHUB size=16 />
                                </a>
                            </div>
                        })
                        .collect_view()}
                    <div class="about-person">
                        <div class="about-person-role">{tr("about.contributors_text")}</div>
                    </div>
                </div>
                <div class="about-links">
                    {link(REPO.to_string(), "nav.project", icons::GITHUB)}
                    {link(format!("{REPO}/graphs/contributors"), "about.contributors", icons::USERS)}
                    {link(format!("{REPO}/issues"), "about.issues", icons::INFO)}
                    {link(format!("{REPO}/blob/main/LICENSE"), "about.license", icons::INFO)}
                </div>
            </div>
        </Modal>
    }
}
