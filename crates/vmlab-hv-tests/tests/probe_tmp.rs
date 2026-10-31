use std::time::Duration;
use vmlab_hv_tests::agent::AgentClient;

#[test]
fn probe_play_tone() {
    let mut agent = AgentClient::connect(
        std::path::Path::new("/tmp/hv-audio-probe.sFil/agent.sock"),
        Duration::from_secs(60),
    )
    .expect("agent");
    let script = include_str!("audio_alsa.py").replace('\'', "'\\''");
    let script = format!(
        "printf '%s' '{script}' > /tmp/alsa.py
python3 - <<'EOF'
import math, struct
frames = bytearray()
for t in range(48000 * 2):
    s = int(6000 * math.sin(2 * math.pi * 440 * t / 48000))
    frames += struct.pack('<hh', s, s)
open('/tmp/tone.raw', 'wb').write(frames)
EOF
rm -f /tmp/done-marker
python3 /tmp/alsa.py unmute && echo UNMUTE-OK
python3 /tmp/alsa.py play 48000 2 < /tmp/tone.raw && echo PLAY-OK || echo PLAY-FAIL"
    );
    let out = agent
        .exec(&["sh", "-ec", &script], Duration::from_secs(120))
        .expect("exec");
    println!("exit={}", out.exit_code);
    println!("stdout:\n{}", String::from_utf8_lossy(&out.stdout));
    println!("stderr:\n{}", String::from_utf8_lossy(&out.stderr));
}
