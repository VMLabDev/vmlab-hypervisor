//! A private sound server with a null sink and no hardware discovery.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use vmlab_hv_tests::process::ManagedChild;

pub struct SilentPipeWire {
    recorder: Option<ManagedChild>,
    recorder_log: std::path::PathBuf,
    _policy: ManagedChild,
    _server: ManagedChild,
}

fn command(program: &str, directory: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .env("XDG_RUNTIME_DIR", directory)
        .env("PIPEWIRE_RUNTIME_DIR", directory)
        .env("XDG_STATE_HOME", directory)
        .env("PIPEWIRE_REMOTE", "pipewire-0")
        .env_remove("PIPEWIRE_CONFIG_NAME")
        .env_remove("PIPEWIRE_CONFIG_DIR")
        .env_remove("WIREPLUMBER_CONFIG_DIR");
    let log = std::fs::File::create(directory.join(format!("{program}.log"))).unwrap();
    command
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(log);
    command
}

impl SilentPipeWire {
    pub fn start(directory: &Path) -> Self {
        let config = directory.join("pipewire.conf");
        std::fs::write(&config, include_str!("audio_pipewire.conf")).unwrap();
        let mut server =
            ManagedChild::helper(command("pipewire", directory).arg("-c").arg(config)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::os::unix::net::UnixStream::connect(directory.join("pipewire-0")).is_err() {
            assert!(
                server.try_wait().unwrap().is_none(),
                "private PipeWire exited"
            );
            assert!(
                Instant::now() < deadline,
                "private PipeWire startup timed out"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        // The policy profile has no ALSA, Bluetooth, or video-device monitors.
        let policy =
            ManagedChild::helper(command("wireplumber", directory).args(["-p", "policy"])).unwrap();
        loop {
            let output = command("pw-dump", directory)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .unwrap();
            assert!(output.status.success(), "pw-dump: {output:?}");
            let objects: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
            if objects
                .iter()
                .any(|o| o["info"]["props"]["port.name"] == "monitor_FL")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "private null sink ports unavailable"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut recorder = ManagedChild::helper(
            command("pw-record", directory)
                .args([
                    "--target",
                    "quality-null",
                    "--rate",
                    "48000",
                    "--channels",
                    "2",
                    "--format",
                    "s16",
                    "--latency",
                    "256",
                    "--raw",
                    "--properties",
                    "stream.capture.sink=true",
                ])
                .arg(directory.join("playback.rendered")),
        )
        .unwrap();
        while std::fs::metadata(directory.join("playback.rendered")).map_or(0, |m| m.len()) == 0 {
            assert!(
                recorder.try_wait().unwrap().is_none(),
                "pw-record startup: {}",
                std::fs::read_to_string(directory.join("pw-record.log")).unwrap()
            );
            assert!(
                Instant::now() < deadline,
                "private sink monitor produced no PCM"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        Self {
            recorder: Some(recorder),
            recorder_log: directory.join("pw-record.log"),
            _policy: policy,
            _server: server,
        }
    }

    pub fn stop_recording(&mut self) {
        if let Some(mut child) = self.recorder.take() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "pw-record exited before capture finished"
            );
            // SAFETY: this PID is our live recorder, and SIGINT flushes its output.
            assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
            let status = child.wait().unwrap();
            let errors = std::fs::read_to_string(&self.recorder_log).unwrap();
            // pw-cat 1.6 returns 1 on signal-driven recording shutdown. Its
            // success code is reserved for a drained playback stream.
            assert!(
                status.success() || (status.code() == Some(1) && errors.is_empty()),
                "pw-record failed: {status}: {errors}"
            );
        }
    }
}

#[test]
#[ignore = "requires PipeWire, WirePlumber, and pw-record"]
fn private_sink_records_silence() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let mut server = SilentPipeWire::start(directory.path());
    server.stop_recording();
    let pcm = std::fs::read(directory.path().join("playback.rendered")).unwrap();
    assert!(!pcm.is_empty());
    assert!(pcm.iter().all(|byte| *byte == 0));
}
