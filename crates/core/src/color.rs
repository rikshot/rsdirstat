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
    let ratio = ((mtime - min_time) as f64) / ((max_time - min_time) as f64);
    (ratio * 120.0) as u16
}

pub const COLOR_MODE_AGE: u8 = 1;
