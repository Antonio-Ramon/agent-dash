//! Idioma da interface: português se o sistema estiver em pt-*, inglês nos
//! demais. `AGENT_DASH_LANG=en` (ou `pt`) força um idioma.

use std::sync::OnceLock;

pub fn en() -> bool {
    static EN: OnceLock<bool> = OnceLock::new();
    *EN.get_or_init(|| !is_pt(&std::env::var("AGENT_DASH_LANG").ok().filter(|s| !s.is_empty()).unwrap_or_else(os_lang)))
}

/// Texto no idioma da interface.
pub fn t(pt: &'static str, en_: &'static str) -> &'static str {
    if en() { en_ } else { pt }
}

/// `tr!("pt {x}", "en {x}")`: `format!` no idioma da interface.
macro_rules! tr {
    ($pt:literal, $en:literal $(, $a:expr)* $(,)?) => {
        if $crate::i18n::en() { format!($en $(, $a)*) } else { format!($pt $(, $a)*) }
    };
}
pub(crate) use tr;

fn is_pt(lang: &str) -> bool {
    lang.get(..2).is_some_and(|p| p.eq_ignore_ascii_case("pt"))
}

/// Idioma de exibição do Windows (o que `Get-UICulture` mostra).
#[cfg(windows)]
fn os_lang() -> String {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
    }
    const LANG_PORTUGUESE: u16 = 0x16;
    let primary = unsafe { GetUserDefaultUILanguage() } & 0x3ff;
    if primary == LANG_PORTUGUESE { "pt".into() } else { "en".into() }
}

#[cfg(not(windows))]
fn os_lang() -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"].iter().filter_map(|v| std::env::var(v).ok()).find(|s| !s.is_empty()).unwrap_or_default()
}

#[test]
fn detecta_portugues() {
    assert!(is_pt("pt") && is_pt("pt-BR") && is_pt("pt_PT.UTF-8") && is_pt("PT"));
    assert!(!is_pt("en-US") && !is_pt("es") && !is_pt("C") && !is_pt("") && !is_pt("p"));
}
