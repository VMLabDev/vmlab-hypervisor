use vmlab_hv_display::{Capabilities, Message, Rect, SurfaceKind};

fn hex(s: &str) -> Vec<u8> {
    s.split_whitespace()
        .map(|b| u8::from_str_radix(b, 16).unwrap())
        .collect()
}

#[test]
fn every_message_has_a_pinned_wire_layout() {
    let fixtures = [
        (
            Message::Hello {
                version: 1,
                capabilities: Capabilities(31),
            },
            "10 00 00 00 01 00 00 00 01 00 00 00 1f 00 00 00",
        ),
        (
            Message::SurfaceConfigure {
                surface_id: 1,
                width: 2,
                height: 3,
                drm_fourcc: 4,
                stride: 5,
                modifier: 6,
                kind: SurfaceKind::Dmabuf,
                buffers: 7,
            },
            "30 00 00 00 02 00 00 00 01 00 00 00 00 00 00 00 02 00 00 00 03 00 00 00 04 00 00 00 05 00 00 00 06 00 00 00 00 00 00 00 01 00 00 00 07 00 00 00",
        ),
        (
            Message::FrameReady {
                surface_id: 1,
                buffer: 2,
                seq: 3,
                timestamp_ns: 4,
                damage: vec![Rect {
                    x: 5,
                    y: 6,
                    width: 7,
                    height: 8,
                }],
            },
            "38 00 00 00 03 00 00 00 01 00 00 00 00 00 00 00 02 00 00 00 03 00 00 00 00 00 00 00 04 00 00 00 00 00 00 00 01 00 00 00 05 00 00 00 06 00 00 00 07 00 00 00 08 00 00 00",
        ),
        (
            Message::Release {
                surface_id: 1,
                buffer: 2,
                seq: 3,
            },
            "1c 00 00 00 04 00 00 00 01 00 00 00 00 00 00 00 02 00 00 00 03 00 00 00 00 00 00 00",
        ),
        (
            Message::CursorShape {
                width: 1,
                height: 1,
                hot_x: 0,
                hot_y: 0,
                argb: vec![1, 2, 3, 4],
            },
            "1c 00 00 00 05 00 00 00 01 00 00 00 01 00 00 00 00 00 00 00 00 00 00 00 01 02 03 04",
        ),
        (
            Message::CursorState {
                x: -1,
                y: 2,
                visible: true,
            },
            "14 00 00 00 06 00 00 00 ff ff ff ff 02 00 00 00 01 00 00 00",
        ),
        (
            Message::CpuCopy { surface_id: 1 },
            "10 00 00 00 07 00 00 00 01 00 00 00 00 00 00 00",
        ),
        (
            Message::Key {
                qcode: "a".into(),
                down: true,
            },
            "11 00 00 00 08 00 00 00 01 00 00 00 01 00 00 00 61",
        ),
        (
            Message::PointerAbs {
                x: 1,
                y: 2,
                width: 3,
                height: 4,
            },
            "18 00 00 00 09 00 00 00 01 00 00 00 02 00 00 00 03 00 00 00 04 00 00 00",
        ),
        (
            Message::Buttons { mask: 5 },
            "0c 00 00 00 0a 00 00 00 05 00 00 00",
        ),
        (
            Message::Scroll { dx: -1, dy: 2 },
            "10 00 00 00 0b 00 00 00 ff ff ff ff 02 00 00 00",
        ),
        (
            Message::LedState { mask: 3 },
            "0c 00 00 00 0c 00 00 00 03 00 00 00",
        ),
        (
            Message::RequestSize {
                width: 1,
                height: 2,
            },
            "10 00 00 00 0d 00 00 00 01 00 00 00 02 00 00 00",
        ),
    ];
    for (message, fixture) in fixtures {
        let bytes = hex(fixture);
        assert_eq!(message.encode().unwrap(), bytes, "{message:?}");
        assert_eq!(Message::decode(&bytes).unwrap(), message);
        for i in 0..bytes.len() {
            assert!(Message::decode(&bytes[..i]).is_err());
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(Message::decode(&trailing).is_err());
        let mut flags = bytes;
        flags[6] = 1;
        assert!(Message::decode(&flags).is_err());
    }
}

#[test]
fn malformed_counts_booleans_and_cursors_are_rejected() {
    for bytes in [
        "14 00 00 00 06 00 00 00 00 00 00 00 00 00 00 00 02 00 00 00",
        "18 00 00 00 05 00 00 00 ff ff ff ff ff ff ff ff 00 00 00 00 00 00 00 00",
        "28 00 00 00 03 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 ff ff ff ff",
    ] {
        assert!(Message::decode(&hex(bytes)).is_err());
    }
}
