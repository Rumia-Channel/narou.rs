use super::super::settings::NovelSettings;

pub fn apply_setting_override(settings: &mut NovelSettings, key: &str, value: &serde_yaml::Value) {
    match key {
        "enable_convert_num_to_kanji" => {
            if let Some(b) = value.as_bool() {
                settings.enable_convert_num_to_kanji = b;
            }
        }
        "enable_ruby" => {
            if let Some(b) = value.as_bool() {
                settings.enable_ruby = b;
            }
        }
        "enable_auto_join_line" => {
            if let Some(b) = value.as_bool() {
                settings.enable_auto_join_line = b;
            }
        }
        "enable_pack_blank_line" => {
            if let Some(b) = value.as_bool() {
                settings.enable_pack_blank_line = b;
            }
        }
        "enable_yokogaki" => {
            if let Some(b) = value.as_bool() {
                settings.enable_yokogaki = b;
            }
        }
        "enable_auto_indent" => {
            if let Some(b) = value.as_bool() {
                settings.enable_auto_indent = b;
            }
        }
        "enable_auto_join_in_brackets" => {
            if let Some(b) = value.as_bool() {
                settings.enable_auto_join_in_brackets = b;
            }
        }
        "enable_convert_horizontal_ellipsis" => {
            if let Some(b) = value.as_bool() {
                settings.enable_convert_horizontal_ellipsis = b;
            }
        }
        "enable_dakuten_font" => {
            if let Some(b) = value.as_bool() {
                settings.enable_dakuten_font = b;
            }
        }
        "enable_display_end_of_book" => {
            if let Some(b) = value.as_bool() {
                settings.enable_display_end_of_book = b;
            }
        }
        "enable_erase_introduction" => {
            if let Some(b) = value.as_bool() {
                settings.enable_erase_introduction = b;
            }
        }
        "enable_erase_postscript" => {
            if let Some(b) = value.as_bool() {
                settings.enable_erase_postscript = b;
            }
        }
        "enable_enchant_midashi" => {
            if let Some(b) = value.as_bool() {
                settings.enable_enchant_midashi = b;
            }
        }
        "enable_author_comments" => {
            if let Some(b) = value.as_bool() {
                settings.enable_author_comments = b;
            }
        }
        "enable_illust" => {
            if let Some(b) = value.as_bool() {
                settings.enable_illust = b;
            }
        }
        "enable_transform_fraction" => {
            if let Some(b) = value.as_bool() {
                settings.enable_transform_fraction = b;
            }
        }
        "enable_transform_date" => {
            if let Some(b) = value.as_bool() {
                settings.enable_transform_date = b;
            }
        }
        "enable_convert_page_break" => {
            if let Some(b) = value.as_bool() {
                settings.enable_convert_page_break = b;
            }
        }
        "enable_add_date_to_title" => {
            if let Some(b) = value.as_bool() {
                settings.enable_add_date_to_title = b;
            }
        }
        "enable_ruby_youon_to_big" => {
            if let Some(b) = value.as_bool() {
                settings.enable_ruby_youon_to_big = b;
            }
        }
        "enable_kana_ni_to_kanji_ni" => {
            if let Some(b) = value.as_bool() {
                settings.enable_kana_ni_to_kanji_ni = b;
            }
        }
        "enable_insert_word_separator" => {
            if let Some(b) = value.as_bool() {
                settings.enable_insert_word_separator = b;
            }
        }
        "enable_insert_char_separator" => {
            if let Some(b) = value.as_bool() {
                settings.enable_insert_char_separator = b;
            }
        }
        "enable_strip_decoration_tag" => {
            if let Some(b) = value.as_bool() {
                settings.enable_strip_decoration_tag = b;
            }
        }
        "enable_add_end_to_title" => {
            if let Some(b) = value.as_bool() {
                settings.enable_add_end_to_title = b;
            }
        }
        "enable_prolonged_sound_mark_to_dash" => {
            if let Some(b) = value.as_bool() {
                settings.enable_prolonged_sound_mark_to_dash = b;
            }
        }
        _ => {}
    }
}
