//! Linux evdev keycodes (linux/input-event-codes.h) to QEMU QKeyCode names.

use std::collections::BTreeSet;
use std::io;
use vmlab_hv_display::Message;

// Physical positions, independent of the host's layout and active modifiers.
pub const KEYS: &[(u32, &str)] = &[
    (1, "esc"),
    (2, "1"),
    (3, "2"),
    (4, "3"),
    (5, "4"),
    (6, "5"),
    (7, "6"),
    (8, "7"),
    (9, "8"),
    (10, "9"),
    (11, "0"),
    (12, "minus"),
    (13, "equal"),
    (14, "backspace"),
    (15, "tab"),
    (16, "q"),
    (17, "w"),
    (18, "e"),
    (19, "r"),
    (20, "t"),
    (21, "y"),
    (22, "u"),
    (23, "i"),
    (24, "o"),
    (25, "p"),
    (26, "bracket_left"),
    (27, "bracket_right"),
    (28, "ret"),
    (29, "ctrl"),
    (30, "a"),
    (31, "s"),
    (32, "d"),
    (33, "f"),
    (34, "g"),
    (35, "h"),
    (36, "j"),
    (37, "k"),
    (38, "l"),
    (39, "semicolon"),
    (40, "apostrophe"),
    (41, "grave_accent"),
    (42, "shift"),
    (43, "backslash"),
    (44, "z"),
    (45, "x"),
    (46, "c"),
    (47, "v"),
    (48, "b"),
    (49, "n"),
    (50, "m"),
    (51, "comma"),
    (52, "dot"),
    (53, "slash"),
    (54, "shift_r"),
    (55, "kp_multiply"),
    (56, "alt"),
    (57, "spc"),
    (58, "caps_lock"),
    (59, "f1"),
    (60, "f2"),
    (61, "f3"),
    (62, "f4"),
    (63, "f5"),
    (64, "f6"),
    (65, "f7"),
    (66, "f8"),
    (67, "f9"),
    (68, "f10"),
    (69, "num_lock"),
    (70, "scroll_lock"),
    (71, "kp_7"),
    (72, "kp_8"),
    (73, "kp_9"),
    (74, "kp_subtract"),
    (75, "kp_4"),
    (76, "kp_5"),
    (77, "kp_6"),
    (78, "kp_add"),
    (79, "kp_1"),
    (80, "kp_2"),
    (81, "kp_3"),
    (82, "kp_0"),
    (83, "kp_decimal"),
    (86, "less"),
    (87, "f11"),
    (88, "f12"),
    (89, "ro"),
    (91, "hiragana"),
    (92, "henkan"),
    (93, "katakanahiragana"),
    (94, "muhenkan"),
    (96, "kp_enter"),
    (97, "ctrl_r"),
    (98, "kp_divide"),
    (99, "print"),
    (100, "alt_r"),
    (101, "lf"),
    (102, "home"),
    (103, "up"),
    (104, "pgup"),
    (105, "left"),
    (106, "right"),
    (107, "end"),
    (108, "down"),
    (109, "pgdn"),
    (110, "insert"),
    (111, "delete"),
    (113, "audiomute"),
    (114, "volumedown"),
    (115, "volumeup"),
    (116, "power"),
    (117, "kp_equals"),
    (119, "pause"),
    (121, "kp_comma"),
    (122, "lang1"),
    (123, "lang2"),
    (124, "yen"),
    (125, "meta_l"),
    (126, "meta_r"),
    (127, "compose"),
    (128, "stop"),
    (129, "again"),
    (130, "props"),
    (131, "undo"),
    (132, "front"),
    (133, "copy"),
    (134, "open"),
    (135, "paste"),
    (136, "find"),
    (137, "cut"),
    (138, "help"),
    (139, "menu"),
    (140, "calculator"),
    (142, "sleep"),
    (143, "wake"),
    (155, "mail"),
    (156, "ac_bookmarks"),
    (157, "computer"),
    (158, "ac_back"),
    (159, "ac_forward"),
    (163, "audionext"),
    (164, "audioplay"),
    (165, "audioprev"),
    (166, "audiostop"),
    (172, "ac_home"),
    (173, "ac_refresh"),
    (183, "f13"),
    (184, "f14"),
    (185, "f15"),
    (186, "f16"),
    (187, "f17"),
    (188, "f18"),
    (189, "f19"),
    (190, "f20"),
    (191, "f21"),
    (192, "f22"),
    (193, "f23"),
    (194, "f24"),
    (226, "mediaselect"),
];

pub fn qcode(evdev: u32) -> Option<&'static str> {
    KEYS.iter()
        .find(|(code, _)| *code == evdev)
        .map(|(_, name)| *name)
}

pub struct Input {
    pub grabbed: bool,
    pub buttons: u32,
    chord: BTreeSet<&'static str>,
    pressed: BTreeSet<&'static str>,
    forwarded: BTreeSet<&'static str>,
}

impl Input {
    pub fn new(chord: &str) -> io::Result<Self> {
        let mut keys = BTreeSet::new();
        for name in chord.split('+') {
            let Some((_, key)) = KEYS.iter().find(|(_, key)| *key == name) else {
                return Err(crate::invalid(
                    "release chord contains an unknown QEMU key name",
                ));
            };
            if !keys.insert(*key) {
                return Err(crate::invalid("release chord contains a duplicate key"));
            }
        }
        Ok(Self {
            grabbed: false,
            buttons: 0,
            chord: keys,
            pressed: BTreeSet::new(),
            forwarded: BTreeSet::new(),
        })
    }

    pub fn grab(&mut self) {
        self.grabbed = self.can_grab();
    }

    pub fn can_grab(&self) -> bool {
        !self.chord.is_subset(&self.pressed)
    }

    pub fn release(&mut self) -> Vec<Message> {
        self.grabbed = false;
        let mut messages: Vec<_> = std::mem::take(&mut self.forwarded)
            .into_iter()
            .map(|key| Message::Key {
                qcode: key.into(),
                down: false,
            })
            .collect();
        if self.buttons != 0 {
            messages.push(Message::Buttons { mask: 0 });
        }
        self.buttons = 0;
        messages
    }

    pub fn focus_lost(&mut self) -> Vec<Message> {
        self.pressed.clear();
        self.release()
    }

    pub fn key(&mut self, evdev: u32, down: bool) -> Vec<Message> {
        let Some(key) = qcode(evdev) else {
            return Vec::new();
        };
        if down {
            self.pressed.insert(key);
            if self.grabbed && self.chord.is_subset(&self.pressed) {
                return self.release();
            }
        } else {
            self.pressed.remove(key);
        }
        if !self.grabbed {
            return Vec::new();
        }
        let changed = if down {
            self.forwarded.insert(key)
        } else {
            self.forwarded.remove(key)
        };
        if !changed {
            return Vec::new();
        }
        vec![Message::Key {
            qcode: key.into(),
            down,
        }]
    }

    pub fn button(&mut self, bit: u32, down: bool) -> Vec<Message> {
        if down {
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }
        vec![Message::Buttons { mask: self.buttons }]
    }
}

/// Physical letterboxed viewport, used by both rendering and pointer translation.
pub fn viewport(window: (u32, u32), guest: (u32, u32)) -> (f64, f64, f64, f64) {
    if window.0 == 0 || window.1 == 0 || guest.0 == 0 || guest.1 == 0 {
        return (0., 0., 0., 0.);
    }
    let scale = (window.0 as f64 / guest.0 as f64).min(window.1 as f64 / guest.1 as f64);
    let (w, h) = (guest.0 as f64 * scale, guest.1 as f64 * scale);
    ((window.0 as f64 - w) / 2., (window.1 as f64 - h) / 2., w, h)
}

pub fn pointer(position: (f64, f64), window: (u32, u32), guest: (u32, u32)) -> Option<Message> {
    let (left, top, width, height) = viewport(window, guest);
    if width == 0. || height == 0. || !position.0.is_finite() || !position.1.is_finite() {
        return None;
    }
    Some(Message::PointerAbs {
        x: ((position.0 - left) * guest.0 as f64 / width)
            .floor()
            .clamp(0., guest.0.saturating_sub(1) as f64) as u32,
        y: ((position.1 - top) * guest.1 as f64 / height)
            .floor()
            .clamp(0., guest.1.saturating_sub(1) as f64) as u32,
        width: guest.0,
        height: guest.1,
    })
}
