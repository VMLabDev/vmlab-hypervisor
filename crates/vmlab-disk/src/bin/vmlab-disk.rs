//! `vmlab-disk`: a small CLI over the image operations, for debugging.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use vmlab_disk::Format;
use vmlab_disk::ops;
use vmlab_disk::qcow2::QCOW_MAGIC;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("vmlab-disk: {e}");
            ExitCode::FAILURE
        }
    }
}

const USAGE: &str = "\
usage: vmlab-disk <command> [args]

commands:
  create <path> --size <bytes> [--backing <path>] [--backing-format <fmt>]
  clone <backing> <dest>
  rebase <path> --backing <path> [--format <fmt>] [--unsafe]
  info <path>
  resize <path> <new-size>
  convert <src> <dest> [--snapshot <name>] [--backing <path>] [--backing-format <fmt>]
  snapshot <path> --list | --create <name> | --apply <name> | --delete <name>
  commit <path>
  read <path> <offset> <len>
  write <path> <offset> <hex-bytes>
  bench <path> <write|read> <total-bytes> [--chunk <bytes>] [--seed <n>]";

fn run(args: &[String]) -> Result<(), String> {
    let cmd = args.first().ok_or_else(|| USAGE.to_string())?;
    let rest = &args[1..];
    let flag = |name: &str| -> Option<String> {
        rest.iter()
            .position(|a| a == name)
            .and_then(|i| rest.get(i + 1))
            .cloned()
    };
    let value_flags = [
        "--size",
        "--backing",
        "--backing-format",
        "--format",
        "--snapshot",
        "--create",
        "--apply",
        "--chunk",
        "--seed",
        "--delete",
    ];
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        if value_flags.contains(&rest[i].as_str()) {
            i += 2;
        } else if rest[i].starts_with("--") {
            i += 1;
        } else {
            positional.push(rest[i].clone());
            i += 1;
        }
    }
    let path = positional.first().map(PathBuf::from);

    match cmd.as_str() {
        "create" => {
            let path = path.ok_or("create needs a path")?;
            let size = parse_u64(&flag("--size").ok_or("create needs --size")?)?;
            let backing = flag("--backing").map(PathBuf::from);
            let backing_format = flag("--backing-format");
            ops::create_qcow2(&path, size, backing.as_deref(), backing_format.as_deref())
                .map_err(str_err)
        }
        "clone" => {
            let (backing, dest) = two_positional(&positional, "clone <backing> <dest>")?;
            ops::create_linked_clone(Path::new(&backing), Path::new(&dest)).map_err(str_err)
        }
        "rebase" => {
            let path = path.ok_or("rebase needs a path")?;
            let backing = flag("--backing").ok_or("rebase needs --backing")?;
            let format = flag("--format");
            if rest.contains(&"--unsafe".to_string()) {
                ops::rebase_unsafe(&path, &backing, format.as_deref()).map_err(str_err)
            } else {
                ops::rebase_safe(&path, Path::new(&backing), format.as_deref()).map_err(str_err)
            }
        }
        "info" => {
            let path = path.ok_or("info needs a path")?;
            let info = ops::info(&path).map_err(str_err)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&info).map_err(|e| e.to_string())?
            );
            Ok(())
        }
        "resize" => {
            let path = path.ok_or("resize needs a path")?;
            let size = positional.get(1).ok_or("resize needs a new size")?;
            ops::resize(&path, parse_u64(size)?).map_err(str_err)
        }
        "convert" => {
            let (src, dest) = two_positional(&positional, "convert <src> <dest>")?;
            let snapshot = flag("--snapshot");
            let backing = flag("--backing").map(PathBuf::from);
            let backing_format = flag("--backing-format");
            ops::convert(
                Path::new(&src),
                Path::new(&dest),
                snapshot.as_deref(),
                backing.as_deref(),
                backing_format.as_deref(),
            )
            .map_err(str_err)
        }
        "snapshot" => {
            let path = path.ok_or("snapshot needs a path")?;
            if let Some(name) = flag("--create") {
                ops::snapshot_create(&path, &name).map_err(str_err)
            } else if let Some(name) = flag("--apply") {
                ops::snapshot_apply(&path, &name)
                    .map(|info| println!("applied {} ({})", info.name, info.id))
                    .map_err(str_err)
            } else if let Some(name) = flag("--delete") {
                ops::snapshot_delete(&path, &name).map_err(str_err)
            } else if rest.contains(&"--list".to_string()) {
                let snaps = ops::snapshot_list(&path).map_err(str_err)?;
                for s in snaps {
                    println!(
                        "{:>3} {:<24} vm-state {:>12}  date {}.{:09}",
                        s.id, s.name, s.vm_state_size, s.date_sec, s.date_nsec
                    );
                }
                Ok(())
            } else {
                Err("snapshot needs one of --list/--create/--apply/--delete".into())
            }
        }
        "commit" => {
            let path = path.ok_or("commit needs a path")?;
            ops::commit(&path).map_err(str_err)
        }
        "read" => {
            let path = path.ok_or("read needs a path")?;
            let offset = parse_u64(positional.get(1).ok_or("read needs an offset")?)?;
            let len = parse_u64(positional.get(2).ok_or("read needs a length")?)?;
            read_cmd(&path, offset, len)
        }
        "write" => {
            let path = path.ok_or("write needs a path")?;
            let offset = parse_u64(positional.get(1).ok_or("write needs an offset")?)?;
            let hex = positional.get(2).ok_or("write needs hex bytes")?;
            write_cmd(&path, offset, hex)
        }
        "bench" => {
            let path = path.ok_or("bench needs a path")?;
            let mode = positional.get(1).ok_or("bench needs write|read")?;
            let total = parse_u64(positional.get(2).ok_or("bench needs a total")?)?;
            let chunk = match flag("--chunk") {
                Some(c) => parse_u64(&c)?,
                None => 1024 * 1024,
            };
            let seed = match flag("--seed") {
                Some(s) => parse_u64(&s)?,
                None => 1,
            } as u8;
            bench_cmd(&path, mode, total, chunk, seed)
        }
        _ => Err(USAGE.to_string()),
    }
}

fn str_err(e: vmlab_disk::Error) -> String {
    e.to_string()
}

fn parse_u64(s: &str) -> Result<u64, String> {
    s.parse::<u64>().map_err(|_| format!("not a number: {s}"))
}

fn two_positional(pos: &[String], usage: &str) -> Result<(String, String), String> {
    if pos.len() < 2 {
        return Err(format!("expected {usage}"));
    }
    Ok((pos[0].clone(), pos[1].clone()))
}

fn open_any(path: &Path, read_only: bool) -> Result<Box<dyn vmlab_disk::DiskImage>, String> {
    let mut magic = [0u8; 4];
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    use std::io::Read;
    f.read_exact(&mut magic).map_err(|e| e.to_string())?;
    let format = if u32::from_be_bytes(magic) == QCOW_MAGIC {
        Format::Qcow2
    } else {
        Format::Raw
    };
    vmlab_disk::open(path, format, read_only).map_err(str_err)
}

fn read_cmd(path: &Path, offset: u64, len: u64) -> Result<(), String> {
    let mut img = open_any(path, true)?;
    let mut buf = vec![0u8; len as usize];
    img.read_at(offset, &mut buf).map_err(str_err)?;
    for (i, chunk) in buf.chunks(16).enumerate() {
        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
        let ascii: String = chunk
            .iter()
            .map(|&b| {
                if (0x20..0x7f).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        println!(
            "{:08x}  {:<47}  {}",
            offset + (i * 16) as u64,
            hex.join(" "),
            ascii
        );
    }
    Ok(())
}

/// Sequential write or read of `total` bytes through `DiskImage`,
/// printing throughput. `read` verifies the pattern `write` laid down.
/// The pattern is deterministic so tests can check the bytes (see the
/// `bench_fill` copy in tests/s2_qemuimg.rs).
fn bench_cmd(path: &Path, mode: &str, total: u64, chunk: u64, seed: u8) -> Result<(), String> {
    let write = match mode {
        "write" => true,
        "read" => false,
        _ => return Err("bench mode must be write or read".into()),
    };
    let chunk = chunk.clamp(512, 8 * 1024 * 1024) as usize;
    let mut buf = vec![0u8; chunk];
    let mut expect = vec![0u8; chunk];
    let t0 = std::time::Instant::now();
    let mut img = open_any(path, !write)?;
    let mut done = 0u64;
    while done < total {
        let n = chunk.min((total - done) as usize);
        bench_fill(&mut expect[..n], done / chunk as u64, seed);
        if write {
            img.write_at(done, &expect[..n]).map_err(str_err)?;
        } else {
            img.read_at(done, &mut buf[..n]).map_err(str_err)?;
            if buf[..n] != expect[..n] {
                return Err(format!("bench read mismatch at offset {done}"));
            }
        }
        done += n as u64;
    }
    if write {
        img.flush().map_err(str_err)?;
    }
    let secs = t0.elapsed().as_secs_f64();
    println!(
        "bench {mode} {total} bytes in {secs:.3}s: {:.1} MB/s",
        total as f64 / 1e6 / secs.max(f64::MIN_POSITIVE),
    );
    Ok(())
}

/// Deterministic per-chunk pattern for `bench`.
fn bench_fill(buf: &mut [u8], chunk_index: u64, seed: u8) {
    let mut x = seed.wrapping_add((chunk_index % 251) as u8);
    for b in buf.iter_mut() {
        x = x.wrapping_mul(31).wrapping_add(17);
        *b = x;
    }
}

fn write_cmd(path: &Path, offset: u64, hex: &str) -> Result<(), String> {
    let bytes = decode_hex(hex)?;
    let mut img = open_any(path, false)?;
    img.write_at(offset, &bytes).map_err(str_err)?;
    img.flush().map_err(str_err)
}

fn decode_hex(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return Err("hex string must have an even length".into());
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|e| e.to_string()))
        .collect()
}
