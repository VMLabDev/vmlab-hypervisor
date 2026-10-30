//! Confirm guest boot and the selected scanout driver during a memory probe.

use std::path::Path;
use std::time::Duration;
use vmlab_hv_tests::agent::AgentClient;

fn main() {
    let socket = std::env::args().nth(1).expect("agent socket path");
    let mut agent = AgentClient::connect(Path::new(&socket), Duration::from_secs(120)).unwrap();
    let script = if std::env::args().nth(2).as_deref() == Some("helpers") {
        "uname -r; test -r /sys/bus/pci/devices/0000:00:05.0/vendor; lspci -k -s 00:05.0"
    } else {
        "uname -r; lspci -k -s 00:04.0; dmesg | grep -iE 'virtio.*gpu|drm|framebuffer'"
    };
    let result = agent
        .exec(&["sh", "-ec", script], Duration::from_secs(30))
        .unwrap();
    print!("{}", String::from_utf8_lossy(&result.stdout));
    eprint!("{}", String::from_utf8_lossy(&result.stderr));
    assert_eq!(result.exit_code, 0);
}
