//! Headless scene goldens. Rune paints with `--snap` (the same path as
//! `cargo run -p hello -- --snap`), at 1×, and this compares the PNG.
//!
//! Rune's text sheets treat a channel delta above 8 as a real miss (`over8` in
//! `text::pixel_compare`). `ELSECALL_UI_BLESS=1` rewrites the committed PNGs.
//! With no Metal device the test fails, unless `ELSECALL_UI_SKIP_GOLDENS=1`.

use std::path::{Path, PathBuf};

use crate::app;
use crate::theme;

const SCENES: &[&str] = &[
    "loading",
    "unavailable-first",
    "unavailable",
    "solo",
    "members",
];
const BLESS: &str =
    "ELSECALL_UI_BLESS=1 cargo test -p elsecall-ui --bin elsecall-ui fleet_scenes_match_goldens";
const APPEARANCES: &[&str] = &["light", "dark"];
/// Channel delta rune's pixel comparison counts as more than antialiasing noise.
const CHANNEL_TOLERANCE: u8 = 8;

#[test]
fn fleet_scenes_match_goldens() {
    if metal_device_count() == 0 {
        assert!(
            std::env::var_os("ELSECALL_UI_BLESS").is_none(),
            "Metal returned no devices, so the fleet goldens cannot be blessed"
        );
        if std::env::var_os("ELSECALL_UI_SKIP_GOLDENS").is_some() {
            eprintln!(
                "skipping fleet goldens: ELSECALL_UI_SKIP_GOLDENS=1 (Metal enumerated no devices)"
            );
            return;
        }
        panic!(
            "Fleet goldens need a Metal device. The probe found none (Metal failed to load, or this agent, SSH, or background session cannot see the GPU). Run the test in Terminal.app, or set ELSECALL_UI_SKIP_GOLDENS=1 to skip."
        );
    }
    let _guard = theme::serial_render();
    for scene in SCENES {
        for appearance in APPEARANCES {
            compare(scene, appearance);
        }
    }
}

fn metal_device_count() -> usize {
    unsafe {
        let metal = libc::dlopen(
            c"/System/Library/Frameworks/Metal.framework/Metal".as_ptr(),
            libc::RTLD_LAZY,
        );
        if metal.is_null() {
            return 0;
        }
        let copy_all = libc::dlsym(metal, c"MTLCopyAllDevices".as_ptr());
        let objc = libc::dlopen(c"/usr/lib/libobjc.A.dylib".as_ptr(), libc::RTLD_LAZY);
        let sel_register = libc::dlsym(objc, c"sel_registerName".as_ptr());
        let msg_send = libc::dlsym(objc, c"objc_msgSend".as_ptr());
        if copy_all.is_null() || sel_register.is_null() || msg_send.is_null() {
            return 0;
        }
        let copy_all: unsafe extern "C" fn() -> *mut std::ffi::c_void =
            std::mem::transmute(copy_all);
        let devices = copy_all();
        if devices.is_null() {
            return 0;
        }
        let sel_register: unsafe extern "C" fn(*const std::ffi::c_char) -> *mut std::ffi::c_void =
            std::mem::transmute(sel_register);
        let count_sel = sel_register(c"count".as_ptr());
        let send: unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void) -> usize =
            std::mem::transmute(msg_send);
        let count = send(devices, count_sel);
        let release = sel_register(c"release".as_ptr());
        let send_void: unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void) =
            std::mem::transmute(msg_send);
        send_void(devices, release);
        count
    }
}

fn compare(scene: &str, appearance: &str) {
    let golden = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{scene}-{appearance}.png"));
    let bless = std::env::var_os("ELSECALL_UI_BLESS").is_some();
    if !bless && !golden.is_file() {
        panic!("missing golden {}\nbless with: {BLESS}", golden.display());
    }
    let fresh = snap(scene, appearance);
    if bless {
        std::fs::create_dir_all(golden.parent().unwrap()).unwrap();
        std::fs::copy(&fresh, &golden).unwrap();
        std::fs::remove_file(&fresh).ok();
        return;
    }
    let (fw, fh, fresh_px) = read_png(&fresh);
    let (gw, gh, golden_px) = read_png(&golden);
    assert_eq!(
        (fw, fh),
        (gw, gh),
        "{scene} {appearance}: snapshot is {fw}x{fh}, golden is {gw}x{gh}"
    );
    assert_eq!(fresh_px.len(), golden_px.len());
    let delta = max_delta(&fresh_px, &golden_px);
    assert!(
        delta <= CHANNEL_TOLERANCE,
        "{scene} {appearance}: max channel delta {delta} exceeds {CHANNEL_TOLERANCE} (golden {})\nbless with: {BLESS}",
        golden.display()
    );
    std::fs::remove_file(&fresh).ok();
}

fn snap(scene: &str, appearance: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "elsecall-ui-{scene}-{appearance}-{}.png",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let path_arg = path.to_str().expect("temp path is utf-8").to_string();
    app::render_scene(vec![
        "--snap".into(),
        path_arg,
        "--scene".into(),
        scene.into(),
        "--theme".into(),
        appearance.into(),
        "--scale".into(),
        "1".into(),
        "--at".into(),
        "0".into(),
    ])
    .unwrap_or_else(|error| panic!("render {scene} {appearance}: {error}"));
    assert!(path.is_file(), "snap did not write {}", path.display());
    path
}

fn read_png(path: &Path) -> (u32, u32, Vec<u8>) {
    let file = std::fs::File::open(path)
        .unwrap_or_else(|error| panic!("open {} ({error}). Bless with: {BLESS}", path.display()));
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder.read_info().unwrap();
    let size = reader.output_buffer_size().expect("png output size");
    let mut buffer = vec![0; size];
    let info = reader.next_frame(&mut buffer).unwrap();
    buffer.truncate(info.buffer_size());
    assert_eq!(info.color_type, png::ColorType::Rgba);
    assert_eq!(info.bit_depth, png::BitDepth::Eight);
    (info.width, info.height, buffer)
}

fn max_delta(left: &[u8], right: &[u8]) -> u8 {
    left.iter()
        .zip(right)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0)
}
