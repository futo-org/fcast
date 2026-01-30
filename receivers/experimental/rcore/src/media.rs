use gst_play::prelude::*;

pub fn play_stream_title(stream: &gst_play::PlayStreamInfo) -> String {
    let mut res = String::new();
    if let Some(tags) = stream.tags() {
        if let Some(language) = tags.get::<gst::tags::LanguageName>() {
            res += language.get();
        } else if let Some(language) = tags.get::<gst::tags::LanguageCode>() {
            let code = language.get();
            if let Some(lang) = gst_tag::language_codes::language_name(code) {
                res += lang;
            } else {
                res += code;
            }
        }
        if let Some(title) = tags.get::<gst::tags::Title>() {
            if !res.is_empty() {
                res += " - ";
            }
            let title = title.get();
            if !title.is_empty() {
                res += &title[0..title.len().min(16)];
                if title.len() >= 16 {
                    res += "...";
                }
            }
        }
    }

    if res.is_empty() {
        res += "Unknown";
    }

    res
}
