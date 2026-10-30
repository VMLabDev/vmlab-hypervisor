use super::*;

#[test]
fn edid_has_valid_checksum_and_preferred_timing() {
    for (w, h) in [(1024, 768), (1920, 1080), (3840, 2160)] {
        let e = edid(w, h);
        assert_eq!(e.iter().fold(0u8, |a, b| a.wrapping_add(*b)), 0);
        assert_eq!(u32::from(e[56]) | (u32::from(e[58] >> 4) << 8), w);
        assert_eq!(u32::from(e[59]) | (u32::from(e[61] >> 4) << 8), h);
        assert_eq!(&e[..8], &[0, 255, 255, 255, 255, 255, 255, 0]);
    }
}

#[test]
fn edid_passes_external_decoder_when_available() {
    use std::process::Command;
    if Command::new("edid-decode").arg("--help").output().is_err() {
        eprintln!("SKIP EDID conformance: edid-decode missing");
        return;
    }
    for (w, h) in [(1024, 768), (1920, 1080), (3840, 2160)] {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&edid(w, h)).unwrap();
        let result = Command::new("edid-decode")
            .arg("--check")
            .arg(file.path())
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
    }
}

#[test]
fn all_formats_convert_to_bgra() {
    for (format, pixels) in [
        (1, [1, 2, 3, 4]),
        (2, [1, 2, 3, 0]),
        (3, [4, 3, 2, 1]),
        (4, [0, 3, 2, 1]),
        (67, [3, 2, 1, 4]),
        (68, [0, 1, 2, 3]),
        (121, [4, 1, 2, 3]),
        (134, [3, 2, 1, 0]),
    ] {
        let r = Resource {
            width: 1,
            height: 1,
            format,
            pixels: pixels.to_vec(),
            backing: vec![],
        };
        let alpha = if matches!(format, 2 | 4 | 68 | 134) {
            255
        } else {
            4
        };
        assert_eq!(r.pixel(0), [1, 2, 3, alpha]);
    }
}

#[test]
fn invalid_snapshots_do_not_change_gpu() {
    let mut gpu = Gpu::new("gpu");
    gpu.request_size(1920, 1080).unwrap();
    let state = gpu.save_state().unwrap();
    for n in 0..state.len() {
        assert!(gpu.restore_state(&state[..n]).is_err());
        assert_eq!(gpu.save_state().unwrap(), state);
    }
    let mut state = state;
    state[0] = 0;
    assert!(gpu.restore_state(&state).is_err());
}
