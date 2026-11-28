//! The entry's own texts in the UI language. The server picks the entry's translation
//! (`apps/<id>/i18n/<lang>.toml`, `docs/phase2/docker-apps.md` § 6.14) for the language asked
//! in `?lang=` and sends it as [`AppTexts`] beside the English fields: what the translation
//! has is shown in its language, anything else in English (`lang="en"`).

use leptos::prelude::*;
use rynas_api_types::apps::AppTexts;
use serde::de::DeserializeOwned;

use crate::i18n::{self, Lang};
use crate::ui::Remote;

/// `path` asking for the entry texts in `lang`.
fn with_lang(path: &str, lang: Lang) -> String {
    let sep = if path.contains('?') { '&' } else { '?' };
    format!("{path}{sep}lang={}", lang.code())
}

/// A catalogue address (`/api/v1/apps/catalog…`) in the UI language.
pub(crate) fn url(path: &str) -> String {
    with_lang(path, i18n::lang())
}

/// Reads `r` again at `path` in the new language when the UI language changes.
pub(crate) fn follow_lang<T: DeserializeOwned + Send + Sync + 'static>(r: Remote<T>, path: String) {
    Effect::new(move |prev: Option<Lang>| {
        let l = i18n::lang();
        if prev.is_some_and(|p| p != l) {
            r.load(with_lang(&path, l));
        }
        l
    });
}

/// The text `pick` finds in `texts` with their language, else `english` with `en`.
fn pick_text(texts: Option<&AppTexts>, pick: impl FnOnce(&AppTexts) -> Option<&String>, english: &str) -> (String, String) {
    texts
        .and_then(|t| pick(t).map(|s| (s.clone(), t.lang.clone())))
        .unwrap_or_else(|| (english.to_owned(), "en".to_owned()))
}

/// The texts of one entry in the UI language, when the server sent some.
#[derive(Clone, Copy)]
pub(crate) struct CatalogText(StoredValue<Option<AppTexts>>);

impl CatalogText {
    pub(crate) fn new(texts: Option<AppTexts>) -> Self {
        Self(StoredValue::new(texts))
    }

    /// The text `pick` finds in the translation and its language, else `english` and `en`.
    pub(crate) fn text(self, pick: impl FnOnce(&AppTexts) -> Option<&String>, english: &str) -> (String, String) {
        self.0.with_value(|t| pick_text(t.as_ref(), pick, english))
    }

    /// Reads the texts (`None`: the entry has none in the UI language).
    pub(crate) fn with<R>(self, f: impl FnOnce(Option<&AppTexts>) -> R) -> R {
        self.0.with_value(|t| f(t.as_ref()))
    }

    /// Just the text of [`CatalogText::text`] (inside a text that has no element of its own).
    pub(crate) fn plain(self, pick: impl FnOnce(&AppTexts) -> Option<&String>, english: &str) -> String {
        self.text(pick, english).0
    }

    /// [`CatalogText::text`] in a `span` marked with its language.
    pub(crate) fn span(self, pick: impl FnOnce(&AppTexts) -> Option<&String>, english: &str) -> AnyView {
        let (text, lang) = self.text(pick, english);
        view! { <span lang=lang>{text}</span> }.into_any()
    }

    /// The long description as Markdown, marked with its language.
    pub(super) fn markdown(self, english: &str) -> AnyView {
        let (text, lang) = self.text(|t| t.description.as_ref(), english);
        crate::markdown::render(&text, &lang)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_text_falls_back_to_english_on_its_own() {
        let mut t = AppTexts { lang: "zh-CN".into(), summary: Some("文档归档".into()), ..AppTexts::default() };
        t.ports.insert("web".into(), "网页界面".into());
        assert_eq!(pick_text(Some(&t), |t| t.summary.as_ref(), "Archive"), ("文档归档".into(), "zh-CN".into()));
        assert_eq!(pick_text(Some(&t), |t| t.ports.get("web"), "Web"), ("网页界面".into(), "zh-CN".into()));
        // a text the translation leaves out, and no translation at all
        assert_eq!(pick_text(Some(&t), |t| t.ports.get("sync"), "Sync"), ("Sync".into(), "en".into()));
        assert_eq!(pick_text(None, |t| t.summary.as_ref(), "Archive"), ("Archive".into(), "en".into()));
    }

    #[test]
    fn addresses_carry_the_language() {
        assert_eq!(with_lang("/api/v1/apps/catalog", Lang::EN), "/api/v1/apps/catalog?lang=en");
        assert_eq!(with_lang("/x?a=1", Lang::EN), "/x?a=1&lang=en");
    }
}
