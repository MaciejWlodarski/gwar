//! The web app's noise suppression: `gwar-denoise` behind a plain C ABI. The browser runs it on
//! its audio thread (an AudioWorklet), whose scope lacks what wasm-bindgen's glue needs, so the
//! module exports these few functions and imports nothing.
//!
//! The page writes the model's archive into [`gwar_denoise_archive`] and loads it with
//! [`gwar_denoise_new`]; then, for each 10 ms frame, it writes the samples into
//! [`gwar_denoise_frame`] and calls [`gwar_denoise_process`], which cleans them in place.

use std::sync::Mutex;

use gwar_denoise::{FRAME, Model, NoiseSuppression};

pub struct Denoiser {
    model: Model,
    frame: [f32; FRAME],
}

/// The archive being handed over, until [`gwar_denoise_new`] takes it.
static ARCHIVE: Mutex<Vec<u8>> = Mutex::new(Vec::new());
/// Why the last call failed.
static ERROR: Mutex<String> = Mutex::new(String::new());

fn fail(error: anyhow::Error) {
    *ERROR.lock().unwrap_or_else(|e| e.into_inner()) = format!("{error:#}");
}

/// Room for an archive of `len` bytes, for the page to write it there.
#[unsafe(no_mangle)]
pub extern "C" fn gwar_denoise_archive(len: usize) -> *mut u8 {
    let mut archive = ARCHIVE.lock().unwrap_or_else(|e| e.into_inner());
    *archive = vec![0; len];
    archive.as_mut_ptr()
}

/// Loads the model from the archive handed over; null if that fails (see [`gwar_denoise_error`]).
#[unsafe(no_mangle)]
pub extern "C" fn gwar_denoise_new() -> *mut Denoiser {
    let archive = std::mem::take(&mut *ARCHIVE.lock().unwrap_or_else(|e| e.into_inner()));
    match Model::from_archive(&archive) {
        Ok(model) => Box::into_raw(Box::new(Denoiser { model, frame: [0.0; FRAME] })),
        Err(error) => {
            fail(error);
            std::ptr::null_mut()
        }
    }
}

/// Samples per frame (480: 10 ms at 48 kHz).
#[unsafe(no_mangle)]
pub extern "C" fn gwar_denoise_frame_length() -> usize {
    FRAME
}

/// The frame [`gwar_denoise_process`] cleans.
///
/// # Safety
/// `denoiser` comes from [`gwar_denoise_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gwar_denoise_frame(denoiser: *mut Denoiser) -> *mut f32 {
    unsafe { (*denoiser).frame.as_mut_ptr() }
}

/// Cleans the frame in place, `strength` in percent (1 to 100); 0 if that fails.
///
/// # Safety
/// `denoiser` comes from [`gwar_denoise_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gwar_denoise_process(denoiser: *mut Denoiser, strength: u32) -> u32 {
    let denoiser = unsafe { &mut *denoiser };
    let strength = NoiseSuppression::percent(strength.clamp(1, 100) as u8);
    match denoiser.model.process(&mut denoiser.frame, strength) {
        Ok(()) => 1,
        Err(error) => {
            fail(error);
            0
        }
    }
}

/// Starts over from the untouched model (a gap in the audio).
///
/// # Safety
/// `denoiser` comes from [`gwar_denoise_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gwar_denoise_reset(denoiser: *mut Denoiser) {
    unsafe { (*denoiser).model.reset() }
}

/// The last failure's message, UTF-8: where it starts; [`gwar_denoise_error_len`] bytes long.
#[unsafe(no_mangle)]
pub extern "C" fn gwar_denoise_error() -> *const u8 {
    ERROR.lock().unwrap_or_else(|e| e.into_inner()).as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn gwar_denoise_error_len() -> usize {
    ERROR.lock().unwrap_or_else(|e| e.into_inner()).len()
}

/// Nothing here asks for randomness (see Cargo.toml); if something ever does, it gets an error.
#[cfg(target_family = "wasm")]
#[unsafe(no_mangle)]
unsafe extern "Rust" fn __getrandom_v03_custom(_dest: *mut u8, _len: usize) -> Result<(), getrandom::Error> {
    Err(getrandom::Error::UNSUPPORTED)
}
