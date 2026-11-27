//! Localized descriptions and form labels shipped with the application catalogue.
//!
//! Match the entry ID and its current source text so updated and third-party
//! descriptions can still display their own wording.

use std::collections::HashMap;
use std::sync::OnceLock;

use leptos::prelude::*;

const ZH_CN: &str = "zh-CN";

// JSON strings with escapes cannot borrow from the input.
type Dictionary = HashMap<String, HashMap<String, String>>;

fn dictionary() -> &'static Dictionary {
    static DICTIONARY: OnceLock<Dictionary> = OnceLock::new();
    DICTIONARY.get_or_init(|| {
        serde_json::from_str(include_str!("../../../assets/app-catalog/zh-CN.json"))
            .expect("the bundled Chinese application catalogue should be valid JSON")
    })
}

pub(super) fn lookup<'a>(id: &str, source: &'a str) -> (&'a str, &'static str) {
    lookup_in(crate::i18n::lang().code(), id, source)
}

fn lookup_in<'a>(language: &str, id: &str, source: &'a str) -> (&'a str, &'static str) {
    let translated = (language == ZH_CN).then(|| {
        let entry = dictionary().get(id)?;
        let key = source.trim();
        // Catalog Markdown can be checked out with Windows line endings.
        if key.contains("\r\n") {
            entry.get(&key.replace("\r\n", "\n"))
        } else {
            entry.get(key)
        }
    }).flatten();
    translated.map_or((source, "en"), |text| (text.as_str(), ZH_CN))
}

#[derive(Clone, Copy)]
pub(crate) struct CatalogText(pub StoredValue<String>);

impl CatalogText {
    pub(super) fn new(id: &str) -> Self {
        Self(StoredValue::new(id.to_owned()))
    }

    pub(crate) fn signal(self, source: String) -> Memo<(String, &'static str)> {
        Memo::new(move |_| {
            let (text, lang) = self.0.with_value(|id| lookup(id, &source));
            (text.to_owned(), lang)
        })
    }

    pub(crate) fn span(self, source: String) -> AnyView {
        let translated = self.signal(source);
        view! {
            <span lang=move || translated.with(|(_, lang)| *lang)>
                {move || translated.with(|(text, _)| text.clone())}
            </span>
        }
        .into_any()
    }

    pub(super) fn markdown(self, source: String) -> AnyView {
        let translated = self.signal(source);
        (move || translated.with(|(text, lang)| crate::markdown::render(text, lang))).into_any()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_uses_the_current_entry_and_language() {
        assert_eq!(lookup_in(ZH_CN, "nextcloud", "Admin user name"), ("管理员用户名", ZH_CN));
        assert_eq!(lookup_in("it", "nextcloud", "Admin user name"), ("Admin user name", "en"));
        assert_eq!(lookup_in(ZH_CN, "custom", "Admin user name"), ("Admin user name", "en"));
        assert_eq!(lookup_in(ZH_CN, "nextcloud", "Primary account"), ("Primary account", "en"));
    }

    #[test]
    fn descriptions_can_use_windows_line_endings() {
        let (src, zh) = dictionary()["nextcloud"].iter().find(|(src, _)| src.contains('\n')).unwrap();
        assert_eq!(lookup_in(ZH_CN, "nextcloud", &src.replace('\n', "\r\n")), (zh.as_str(), ZH_CN));
    }
}
