use std::ffi::c_char;
use std::ptr;

use cascadia_audio_win_mvp::{
    cascadia_audio_debug_counters, cascadia_audio_is_playing, cascadia_audio_start,
    cascadia_audio_stop,
};

#[test]
fn start_rejects_null_url_pointer() {
    cascadia_audio_stop();
    assert_eq!(cascadia_audio_start(ptr::null::<c_char>()), 0);
    assert_eq!(cascadia_audio_is_playing(), 0);
}

#[test]
fn start_rejects_invalid_utf8_url() {
    cascadia_audio_stop();
    let bytes = [0xffu8, 0x00];
    assert_eq!(cascadia_audio_start(bytes.as_ptr().cast::<c_char>()), 0);
    assert_eq!(cascadia_audio_is_playing(), 0);
}

#[test]
fn debug_counters_require_non_null_pointers() {
    let mut decoded = 0u64;
    let mut errors = 0u64;
    assert_eq!(
        cascadia_audio_debug_counters(&mut decoded, &mut errors, ptr::null_mut()),
        0
    );
}
