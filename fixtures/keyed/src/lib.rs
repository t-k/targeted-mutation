//! Translations kept by language and page. Every language is tested with every page,
//! but never two languages in one store: a key without the language passes.

use std::collections::BTreeMap;

fn key(language: &str, slug: &str) -> String {
    format!("{language}/{slug}")
}

#[derive(Default)]
pub struct Labels {
    texts: BTreeMap<String, String>,
}

impl Labels {
    pub fn put(&mut self, language: &str, slug: &str, text: &str) {
        self.texts.insert(key(language, slug), text.to_owned());
    }

    pub fn get(&self, language: &str, slug: &str) -> Option<&str> {
        self.texts.get(&key(language, slug)).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn japanese_pages() {
        let mut labels = Labels::default();
        labels.put("ja", "home", "こんにちは");
        labels.put("ja", "about", "このサイトについて");
        assert_eq!(labels.get("ja", "home"), Some("こんにちは"));
        assert_eq!(labels.get("ja", "about"), Some("このサイトについて"));
    }

    #[test]
    fn english_pages() {
        let mut labels = Labels::default();
        labels.put("en", "home", "Hello");
        labels.put("en", "about", "About this site");
        assert_eq!(labels.get("en", "home"), Some("Hello"));
        assert_eq!(labels.get("en", "about"), Some("About this site"));
    }
}
