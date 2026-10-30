use super::*;

#[test]
fn edid_has_valid_checksum_and_preferred_timing() {
    for (w, h) in [(1024, 768), (1920, 1080), (3840, 2160)] {
        let e = edid(w, h);
        for block in e.as_chunks::<128>().0 {
            assert_eq!(block.iter().fold(0u8, |a, b| a.wrapping_add(*b)), 0);
        }
        assert_eq!(u32::from(e[56]) | (u32::from(e[58] >> 4) << 8), w);
        assert_eq!(u32::from(e[59]) | (u32::from(e[61] >> 4) << 8), h);
        assert_eq!(&e[..8], &[0, 255, 255, 255, 255, 255, 255, 0]);
    }
}

#[test]
fn edid_external_decoder_matches_qemu_reference_when_available() {
    use std::process::Command;
    if Command::new("edid-decode").arg("--help").output().is_err() {
        eprintln!("SKIP EDID conformance: edid-decode missing");
        return;
    }
    let decode = |bytes: &[u8]| {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(bytes).unwrap();
        Command::new("edid-decode")
            .arg("--check")
            .arg(file.path())
            .output()
            .unwrap()
    };
    let reference = decode(&edid(1280, 800));
    let reference_text = String::from_utf8(reference.stdout).unwrap();
    // The captured QEMU CTA block lacks a VCDB. D30 preserves that byte-for-byte;
    // resized timings must not add decoder failures to the reference's failures.
    eprintln!("QEMU reference EDID decoder status: {}", reference.status);
    let failures = |text: &str| {
        text.split_once("\nFailures:\n")
            .map(|(_, errors)| errors.to_owned())
    };
    for (w, h) in [(1024, 768), (1920, 1080), (3840, 2160)] {
        let result = decode(&edid(w, h));
        let output = String::from_utf8(result.stdout).unwrap();
        assert_eq!(result.status, reference.status, "{output}");
        assert_eq!(failures(&output), failures(&reference_text), "{output}");
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
