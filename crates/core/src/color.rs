pub fn hash_name(name: &str) -> u16 {
    let mut hash: i32 = 0;
    for code_unit in name.encode_utf16() {
        hash = hash.wrapping_shl(5).wrapping_sub(hash).wrapping_add(code_unit as i32);
    }
    hash.rem_euclid(360) as u16
}

pub fn hash_id_to_hue(id: u64) -> u16 {
    ((id.wrapping_mul(2654435761) >> 16) % 360) as u16
}

pub fn hue_for_extension(extension: &str) -> u16 {
    let mime = mime_guess::from_ext(extension).first_or(mime::APPLICATION_OCTET_STREAM);
    match mime.type_().as_str() {
        "video" => 220,
        "audio" => 280,
        "image" => 130,
        "text" => 55,
        "font" => 310,
        "application" => 5,
        _ => hash_name(extension),
    }
}

pub fn age_hue(mtime: i64, min_time: i64, max_time: i64) -> u16 {
    if max_time <= min_time || mtime <= 0 {
        return 60;
    }
    // Clamp: during a streaming scan the range can still be growing, so an mtime above the
    // current max would otherwise push the hue past the intended 0..=120 band.
    let ratio = (((mtime - min_time) as f64) / ((max_time - min_time) as f64)).clamp(0.0, 1.0);
    (ratio * 120.0) as u16
}

pub const COLOR_MODE_AGE: u8 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_name_known_values() {
        assert_eq!(hash_name("hello"), 322);
        assert_eq!(hash_name("world"), 162);
        assert_eq!(hash_name(""), 0);
    }

    #[test]
    fn hash_name_in_range() {
        for name in ["hello", "world", "", "a", "long_file_name.tar.gz", "日本語"] {
            let h = hash_name(name);
            assert!(h < 360, "hash_name({name:?}) = {h}, expected < 360");
        }
    }

    #[test]
    fn hash_name_different_names_differ() {
        assert_ne!(hash_name("hello"), hash_name("world"));
        assert_ne!(hash_name("foo"), hash_name("bar"));
    }

    #[test]
    fn hash_id_to_hue_known_values() {
        assert_eq!(hash_id_to_hue(0), 0);
        assert_eq!(hash_id_to_hue(42), 145);
    }

    #[test]
    fn hash_id_to_hue_in_range() {
        for id in [0, 1, 100, u64::MAX, u64::MAX / 2] {
            let h = hash_id_to_hue(id);
            assert!(h < 360, "hash_id_to_hue({id}) = {h}, expected < 360");
        }
    }

    #[test]
    fn hue_for_extension_video() {
        assert_eq!(hue_for_extension("mp4"), 220);
        assert_eq!(hue_for_extension("mkv"), 220);
    }

    #[test]
    fn hue_for_extension_audio() {
        assert_eq!(hue_for_extension("mp3"), 280);
        assert_eq!(hue_for_extension("flac"), 280);
    }

    #[test]
    fn hue_for_extension_image() {
        assert_eq!(hue_for_extension("png"), 130);
        assert_eq!(hue_for_extension("jpg"), 130);
    }

    #[test]
    fn hue_for_extension_text() {
        assert_eq!(hue_for_extension("txt"), 55);
        assert_eq!(hue_for_extension("csv"), 55);
    }

    #[test]
    fn hue_for_extension_font() {
        assert_eq!(hue_for_extension("ttf"), 310);
    }

    #[test]
    fn hue_for_extension_woff_as_application() {
        // mime_guess classifies woff as application/font-woff, not font/*
        assert_eq!(hue_for_extension("woff"), 5);
    }

    #[test]
    fn hue_for_extension_application() {
        assert_eq!(hue_for_extension("exe"), 5);
        assert_eq!(hue_for_extension("pdf"), 5);
    }

    #[test]
    fn hue_for_extension_unknown_gets_application() {
        assert_eq!(hue_for_extension("xyzzy_unknown"), 5);
    }

    #[test]
    fn age_hue_midpoint() {
        assert_eq!(age_hue(50, 0, 100), 60);
    }

    #[test]
    fn age_hue_at_min() {
        assert_eq!(age_hue(1, 1, 101), 0);
    }

    #[test]
    fn age_hue_at_max() {
        assert_eq!(age_hue(100, 0, 100), 120);
    }

    #[test]
    fn age_hue_degenerate_equal_bounds() {
        assert_eq!(age_hue(50, 50, 50), 60);
    }

    #[test]
    fn age_hue_degenerate_inverted_bounds() {
        assert_eq!(age_hue(50, 100, 0), 60);
    }

    #[test]
    fn age_hue_degenerate_zero_mtime() {
        assert_eq!(age_hue(0, 0, 100), 60);
        assert_eq!(age_hue(-1, 0, 100), 60);
    }
}
