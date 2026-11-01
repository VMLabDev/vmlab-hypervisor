//! Check staged content and named artifacts without printing secret fragments.

use std::{io::Read, path::Path, process::Command};

fn contains_secret(bytes: &[u8], fragments: &[&[u8]]) -> bool {
    fragments.iter().any(|fragment| {
        bytes
            .windows(fragment.len())
            .any(|bytes| bytes == *fragment)
    })
}

fn scan(path: &Path, fragments: &[&[u8]]) -> Result<(), String> {
    if path.is_dir() {
        for child in std::fs::read_dir(path).map_err(|_| "cannot enumerate artifact")? {
            scan(
                &child.map_err(|_| "cannot enumerate artifact")?.path(),
                fragments,
            )?;
        }
    } else if path.is_file() {
        let mut file = std::fs::File::open(path).map_err(|_| "cannot read artifact")?;
        let mut buffer = vec![0; 1024 * 1024 + 64];
        let mut retained = 0;
        loop {
            let count = file
                .read(&mut buffer[retained..])
                .map_err(|_| "cannot scan artifact")?;
            if count == 0 {
                break;
            }
            let length = retained + count;
            if contains_secret(&buffer[..length], fragments) {
                return Err("secret fragment found in artifact; path suppressed".into());
            }
            retained = length.min(63);
            buffer.copy_within(length - retained..length, 0);
        }
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let key = vmlab_hv_tests::host_key::load_host_osk()?
        .ok_or("no configured key available for fragment scan")?;
    let key = key.as_bytes();
    let fragments = [key, &key[..8], &key[..32], &key[32..]];
    let staged = Command::new("git")
        .args(["diff", "--cached", "--binary"])
        .output()
        .map_err(|_| "cannot read staged diff")?;
    if !staged.status.success() {
        return Err("cannot read staged diff".into());
    }
    if contains_secret(&staged.stdout, &fragments) {
        return Err("secret fragment found in staged diff".into());
    }
    for path in std::env::args_os().skip(1) {
        scan(Path::new(&path), &fragments)?;
    }
    println!("Secret fragment scan passed");
    Ok(())
}
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
