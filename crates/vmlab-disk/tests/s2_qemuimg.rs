//! S2: `vmlab-disk` against `qemu-img` (the spec's acceptance seam).
//!
//! Images written by the crate pass `qemu-img check` and their content
//! matches; images and snapshots written by qemu-img read back identically
//! through the crate, including backing chains, compressed clusters and
//! internal snapshots with vmstate. Skipped with a reason when qemu-img is
//! missing.

use std::io::Write as _;
use std::path::Path;
use std::process::Command;

use vmlab_disk::ops;
use vmlab_disk::{Format, SnapshotInfo};

const QEMU_IMG: &str = "/usr/bin/qemu-img";
const QEMU_IO: &str = "qemu-io";

fn qemu_img_available() -> bool {
    Path::new(QEMU_IMG).exists() || which(QEMU_IMG.split('/').next_back().unwrap())
}

fn which(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A scratch directory per test under the workspace target dir.
fn tmpdir(tag: &str) -> tempfile::TempDir {
    let base = env!("CARGO_TARGET_TMPDIR");
    std::fs::create_dir_all(base).unwrap();
    tempfile::Builder::new()
        .prefix(&format!("s2-{tag}-"))
        .tempdir_in(base)
        .unwrap()
}

#[test]
fn scratch_directories_are_isolated_across_processes() {
    let dir = tmpdir("isolation");
    let marker = dir.path().join("marker");
    if std::env::var_os("VMLAB_S2_TEMP_CHILD").is_some() {
        std::fs::write(&marker, b"child").unwrap();
        return;
    }
    std::fs::write(&marker, b"parent").unwrap();
    let second = tmpdir("isolation");
    assert_ne!(dir.path(), second.path());
    let children: Vec<_> = (0..4)
        .map(|_| {
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "scratch_directories_are_isolated_across_processes",
                ])
                .env("VMLAB_S2_TEMP_CHILD", "1")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(std::fs::read(&marker).unwrap(), b"parent");
    let second_path = second.path().to_path_buf();
    drop(second);
    assert!(!second_path.exists());
    assert_eq!(std::fs::read(&marker).unwrap(), b"parent");
}

fn run(bin: &str, args: &[&str]) -> std::process::Output {
    let out = Command::new(bin).args(args).output().expect(bin);
    assert!(
        out.status.success(),
        "{bin} {} failed: {}\n---\n{}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    out
}

fn qemu_img(args: &[&str]) -> std::process::Output {
    let bin = if Path::new(QEMU_IMG).exists() {
        QEMU_IMG
    } else {
        "qemu-img"
    };
    run(bin, args)
}

fn qemu_io(args: &[&str]) -> std::process::Output {
    run(QEMU_IO, args)
}

fn stdout_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn qemu_check(path: &Path) {
    let out = qemu_img(&["check", &path.to_string_lossy()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("No errors were found"),
        "qemu-img check did not pass for {}: {}",
        path.display(),
        stdout
    );
}

/// Read the whole guest-visible content of an image through the crate.
fn read_all(path: &Path, format: Format) -> Vec<u8> {
    let mut img = vmlab_disk::open(path, format, true).unwrap();
    let mut buf = vec![0u8; img.size() as usize];
    img.read_at(0, &mut buf).unwrap();
    buf
}

fn write_pattern(path: &Path, format: Format, offset: u64, data: &[u8]) {
    let mut img = vmlab_disk::open(path, format, false).unwrap();
    img.write_at(offset, data).unwrap();
    img.flush().unwrap();
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut x = seed;
    for _ in 0..len {
        x = x.wrapping_mul(31).wrapping_add(17);
        v.push(x);
    }
    v
}

/// Dump an image's guest content to a raw file (via the crate).
fn to_raw(path: &Path, format: Format, dest: &Path) {
    let content = read_all(path, format);
    std::fs::write(dest, content).unwrap();
}

fn assert_same_file(a: &Path, b: &Path, what: &str) {
    let (ca, cb) = (std::fs::read(a).unwrap(), std::fs::read(b).unwrap());
    assert_eq!(ca.len(), cb.len(), "{what}: length mismatch");
    let mut diff = None;
    for (i, (x, y)) in ca.iter().zip(cb.iter()).enumerate() {
        if x != y {
            diff = Some(i);
            break;
        }
    }
    assert!(
        diff.is_none(),
        "{what}: first difference at byte {:?}",
        diff
    );
}

macro_rules! require_qemu_img {
    () => {
        if !qemu_img_available() {
            eprintln!("SKIP: qemu-img not found (looked at {QEMU_IMG} and PATH)");
            return;
        }
    };
}

// --------------------------------------------------------------------------

/// Crate-created images pass `qemu-img check`; content matches a raw file
/// with the same writes, verified by `qemu-img compare`.
#[test]
fn s2_crate_written_images_pass_check_and_compare() {
    require_qemu_img!();
    let temp = tmpdir("check-compare");
    let dir = temp.path();
    let img_path = dir.join("made.qcow2");
    ops::create_qcow2(&img_path, 32 * 1024 * 1024, None, None).unwrap();

    let a = pattern(300_000, 1);
    let b = pattern(7, 2);
    write_pattern(&img_path, Format::Qcow2, 1_000_003, &a);
    write_pattern(&img_path, Format::Qcow2, 0, &b);
    // Overwrite part of `a` to exercise in-place and COW-free paths.
    write_pattern(&img_path, Format::Qcow2, 1_500_000, &[9u8; 10]);

    qemu_check(&img_path);

    // Expected raw content.
    let expected = dir.join("expected.raw");
    {
        let mut buf = vec![0u8; 32 * 1024 * 1024];
        buf[0..7].copy_from_slice(&b);
        buf[1_000_003..1_000_003 + a.len()].copy_from_slice(&a);
        buf[1_500_000..1_500_010].fill(9);
        std::fs::write(&expected, buf).unwrap();
    }
    let got = dir.join("got.raw");
    to_raw(&img_path, Format::Qcow2, &got);
    assert_same_file(&expected, &got, "crate-written content");
    qemu_img(&[
        "compare",
        &img_path.to_string_lossy(),
        &expected.to_string_lossy(),
    ]);
}

/// Backing chains: relative and absolute backing paths, raw and qcow2
/// backing formats, recursive reads through the crate.
#[test]
fn s2_backing_reads_preserve_offsets_within_clusters() {
    require_qemu_img!();
    let temp = tmpdir("backing-offsets");
    let base = temp.path().join("base.raw");
    let content: Vec<u8> = (0..262144)
        .map(|i| ((i / 512 + i % 251) % 256) as u8)
        .collect();
    std::fs::write(&base, &content).unwrap();
    let overlay = temp.path().join("overlay.qcow2");
    qemu_img(&[
        "create",
        "-f",
        "qcow2",
        "-b",
        &base.to_string_lossy(),
        "-F",
        "raw",
        &overlay.to_string_lossy(),
    ]);
    for allocated_l2 in [false, true] {
        if allocated_l2 {
            write_pattern(&overlay, Format::Qcow2, 196608, &[0x33; 512]);
        }
        let oracle = temp.path().join("oracle.raw");
        qemu_img(&[
            "convert",
            "-O",
            "raw",
            &overlay.to_string_lossy(),
            &oracle.to_string_lossy(),
        ]);
        let expected = std::fs::read(&oracle).unwrap();
        let mut disk = vmlab_disk::open(&overlay, Format::Qcow2, true).unwrap();
        for (offset, len) in [(512, 512), (65535, 1026), (66048, 4096)] {
            let mut got = vec![0; len];
            disk.read_at(offset as u64, &mut got).unwrap();
            assert_eq!(
                got,
                expected[offset..offset + len],
                "offset {offset}, allocated L2 {allocated_l2}"
            );
        }
    }
}

#[test]
fn s2_backing_chains() {
    require_qemu_img!();
    let temp = tmpdir("backing");
    let dir = temp.path();
    let base = dir.join("base.qcow2");
    qemu_img(&["create", "-f", "qcow2", &base.to_string_lossy(), "16M"]);
    qemu_io(&["-c", "write -P 0x5a 0 1M", &base.to_string_lossy()]);

    // Linked clone via the crate records an absolute backing path.
    let clone = dir.join("clone.qcow2");
    ops::create_linked_clone(&base, &clone).unwrap();
    let d = pattern(1000, 3);
    write_pattern(&clone, Format::Qcow2, 5 * 1024 * 1024, &d);
    qemu_check(&clone);

    // Content: base pattern everywhere plus the delta.
    let mut expect = vec![0x5au8; 1024 * 1024];
    expect.extend_from_slice(&vec![0u8; 16 * 1024 * 1024 - 1024 * 1024]);
    expect[5 * 1024 * 1024..5 * 1024 * 1024 + d.len()].copy_from_slice(&d);
    let got = read_all(&clone, Format::Qcow2);
    assert_eq!(got.len(), 16 * 1024 * 1024);
    assert_eq!(&got[..16], &[0x5au8; 16]);
    assert_eq!(&got[5 * 1024 * 1024..5 * 1024 * 1024 + 16], &d[..16]);
    assert!(
        got[2 * 1024 * 1024..2 * 1024 * 1024 + 16]
            .iter()
            .all(|&b| b == 0)
    );

    // A raw backing file referenced by a relative path.
    let raw_base = dir.join("rawbase.raw");
    std::fs::write(&raw_base, pattern(4 * 1024 * 1024, 9)).unwrap();
    let overlay = dir.join("overlay.qcow2");
    qemu_img(&[
        "create",
        "-f",
        "qcow2",
        "-b",
        "rawbase.raw",
        "-F",
        "raw",
        &overlay.to_string_lossy(),
    ]);
    // Opened through the crate from a different cwd-less relative resolution:
    // the relative name resolves against the image's directory.
    let content = read_all(&overlay, Format::Qcow2);
    assert_eq!(content, std::fs::read(&raw_base).unwrap());

    // Three-deep chain: base <- mid (qcow2) <- top, all via qemu-img, read
    // through the crate recursively.
    let mid = dir.join("mid.qcow2");
    qemu_img(&[
        "create",
        "-f",
        "qcow2",
        "-b",
        &base.to_string_lossy(),
        "-F",
        "qcow2",
        &mid.to_string_lossy(),
    ]);
    qemu_io(&["-c", "write -P 0x77 4M 512k", &mid.to_string_lossy()]);
    let top = dir.join("top.qcow2");
    ops::create_linked_clone(&mid, &top).unwrap();
    write_pattern(&top, Format::Qcow2, 6 * 1024 * 1024, &[0x33u8; 512]);
    let got = read_all(&top, Format::Qcow2);
    assert_eq!(got.len(), 16 * 1024 * 1024);
    assert!(got[..1024 * 1024].iter().all(|&b| b == 0x5a)); // base
    assert!(
        got[4 * 1024 * 1024..4 * 1024 * 1024 + 512 * 1024]
            .iter()
            .all(|&b| b == 0x77)
    ); // mid
    assert!(
        got[6 * 1024 * 1024..6 * 1024 * 1024 + 512]
            .iter()
            .all(|&b| b == 0x33)
    ); // top
    qemu_check(&top);
}

/// Images and snapshots created by qemu-img read back identically through
/// the crate: compressed clusters (deflate), version 2, refcount widths.
#[test]
fn s2_reads_qemu_created_images() {
    require_qemu_img!();
    let temp = tmpdir("qemu-made");
    let dir = temp.path();
    let size = 8 * 1024 * 1024;
    let expected = pattern(size as usize, 42);

    let raw = dir.join("src.raw");
    std::fs::write(&raw, &expected).unwrap();

    // Compressed clusters.
    let compressed = dir.join("compressed.qcow2");
    qemu_img(&[
        "convert",
        "-c",
        "-O",
        "qcow2",
        &raw.to_string_lossy(),
        &compressed.to_string_lossy(),
    ]);
    assert_eq!(read_all(&compressed, Format::Qcow2), expected);

    // Version 2 (compat 0.10): reads and writes.
    let v2 = dir.join("v2.qcow2");
    qemu_img(&[
        "convert",
        "-o",
        "compat=0.10",
        "-O",
        "qcow2",
        &raw.to_string_lossy(),
        &v2.to_string_lossy(),
    ]);
    assert_eq!(read_all(&v2, Format::Qcow2), expected);
    {
        let patch = pattern(250_000, 5);
        let mut img = vmlab_disk::open(&v2, Format::Qcow2, false).unwrap();
        img.write_at(3_000_007, &patch).unwrap();
        img.snapshot_create("v2snap", 0, 0).unwrap();
        img.flush().unwrap();
        drop(img);
        qemu_check(&v2);
        let mut view = read_all(&v2, Format::Qcow2);
        assert_eq!(&view[3_000_007..3_000_007 + 64], &patch[..64]);
        ops::snapshot_apply(&v2, "v2snap").unwrap();
        qemu_check(&v2);
        view = read_all(&v2, Format::Qcow2);
        assert_eq!(&view[3_000_007..3_000_007 + 64], &patch[..64]);
        ops::snapshot_delete(&v2, "v2snap").unwrap();
        qemu_check(&v2);
    }

    // Unusual refcount widths: qemu creates, crate writes, check passes.
    for bits in ["1", "64"] {
        let img = dir.join(format!("rc{bits}.qcow2"));
        qemu_img(&[
            "create",
            "-f",
            "qcow2",
            "-o",
            &format!("refcount_bits={bits}"),
            &img.to_string_lossy(),
            "8M",
        ]);
        write_pattern(&img, Format::Qcow2, 123_457, &pattern(200_000, 7));
        let mut back = vec![0u8; 200_000];
        {
            let mut r = vmlab_disk::open(&img, Format::Qcow2, true).unwrap();
            r.read_at(123_457, &mut back).unwrap();
        }
        assert_eq!(back, pattern(200_000, 7), "refcount_bits={bits}");
        qemu_check(&img);
    }

    // Cluster sizes other than 64 KiB.
    let small = dir.join("cluster4k.qcow2");
    qemu_img(&[
        "create",
        "-f",
        "qcow2",
        "-o",
        "cluster_size=4096",
        &small.to_string_lossy(),
        "8M",
    ]);
    write_pattern(&small, Format::Qcow2, 999, &pattern(100_000, 11));
    qemu_check(&small);
    let mut back = vec![0u8; 100_000];
    vmlab_disk::open(&small, Format::Qcow2, true)
        .unwrap()
        .read_at(999, &mut back)
        .unwrap();
    assert_eq!(back, pattern(100_000, 11));
}

/// Snapshot interop in both directions, disk-only: qemu-img snapshots read
/// and apply through the crate; crate snapshots list, apply and delete
/// through qemu-img. `qemu-img check` passes after every step.
#[test]
fn s2_snapshot_interop_disk_only() {
    require_qemu_img!();
    let temp = tmpdir("snap-interop");
    let dir = temp.path();
    let path = dir.join("s.qcow2");
    ops::create_qcow2(&path, 16 * 1024 * 1024, None, None).unwrap();
    let v1 = pattern(4096, 1);
    write_pattern(&path, Format::Qcow2, 0, &v1);

    // qemu-img creates a snapshot; crate lists it.
    qemu_img(&["snapshot", "-c", "qsnap", &path.to_string_lossy()]);
    let v2 = pattern(4096, 2);
    write_pattern(&path, Format::Qcow2, 0, &v2);
    qemu_check(&path);

    {
        let img = vmlab_disk::open(&path, Format::Qcow2, true).unwrap();
        let snaps = img.snapshots().unwrap();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].name, "qsnap");
        assert_eq!(snaps[0].vm_state_size, 0);
    }

    // Crate applies qemu's snapshot: content reverts to v1.
    ops::snapshot_apply(&path, "qsnap").unwrap();
    qemu_check(&path);
    assert_eq!(&read_all(&path, Format::Qcow2)[..64], &v1[..64]);

    // Crate creates a snapshot; qemu-img lists and applies it.
    ops::snapshot_create(&path, "csnap").unwrap();
    let v3 = pattern(4096, 3);
    write_pattern(&path, Format::Qcow2, 8 * 1024 * 1024, &v3);
    qemu_check(&path);
    let listing = stdout_of(&qemu_img(&["snapshot", "-l", &path.to_string_lossy()]));
    assert!(listing.contains("qsnap"), "listing: {listing}");
    assert!(listing.contains("csnap"), "listing: {listing}");

    qemu_img(&["snapshot", "-a", "csnap", &path.to_string_lossy()]);
    assert_eq!(&read_all(&path, Format::Qcow2)[..64], &v1[..64]);
    qemu_check(&path);

    // Deletes in both directions.
    qemu_img(&["snapshot", "-d", "qsnap", &path.to_string_lossy()]);
    ops::snapshot_delete(&path, "csnap").unwrap();
    qemu_check(&path);
    assert!(
        vmlab_disk::open(&path, Format::Qcow2, true)
            .unwrap()
            .snapshots()
            .unwrap()
            .is_empty()
    );
}

/// A snapshot with vmstate written by the crate is listed by
/// `qemu-img snapshot -l` with the right VM SIZE, reads back identically
/// after apply, and every step passes `qemu-img check`.
#[test]
fn s2_vmstate_round_trip() {
    require_qemu_img!();
    let temp = tmpdir("vmstate");
    let dir = temp.path();
    let path = dir.join("vm.qcow2");
    ops::create_qcow2(&path, 10 * 1024 * 1024, None, None).unwrap();
    let disk_data = pattern(4096, 5);
    write_pattern(&path, Format::Qcow2, 1_000_000, &disk_data);

    let state = pattern(3 * 1024 * 1024 + 12345, 0xee);
    {
        let mut img = vmlab_disk::open(&path, Format::Qcow2, false).unwrap();
        // Stream the state into the vmstate area in chunks, as the VMM will.
        for (i, chunk) in state.chunks(256 * 1024).enumerate() {
            img.write_vmstate((i * 256 * 1024) as u64, chunk).unwrap();
        }
        img.snapshot_create("online", state.len() as u64, 13_700_000_000_000)
            .unwrap();
        img.flush().unwrap();
    }
    qemu_check(&path);

    // qemu-img lists it with the right VM SIZE.
    let listing = stdout_of(&qemu_img(&["snapshot", "-l", &path.to_string_lossy()]));
    assert!(listing.contains("online"), "listing: {listing}");
    // qemu-img prints human sizes; ask it for exact bytes via info json.
    let info = stdout_of(&qemu_img(&[
        "info",
        "--output=json",
        &path.to_string_lossy(),
    ]));
    let json: serde_json::Value = serde_json::from_str(&info).unwrap();
    let snaps = json["snapshots"].as_array().unwrap();
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0]["name"].as_str().unwrap(), "online");
    assert_eq!(
        snaps[0]["vm-state-size"].as_u64().unwrap(),
        state.len() as u64
    );

    // The vmstate area is punched out of the active layer after create, so
    // disk reads still work and a second snapshot does not carry stale state.
    assert_eq!(
        &read_all(&path, Format::Qcow2)[1_000_000..1_000_064],
        &disk_data[..64]
    );

    // Apply restores disk content and vmstate.
    let info: SnapshotInfo = ops::snapshot_apply(&path, "online").unwrap();
    assert_eq!(info.vm_state_size, state.len() as u64);
    qemu_check(&path);
    {
        let mut img = vmlab_disk::open(&path, Format::Qcow2, true).unwrap();
        let mut got = vec![0u8; state.len()];
        img.read_vmstate(0, &mut got).unwrap();
        assert_eq!(got, state);
    }

    // qemu-img can apply our vmstate snapshot too.
    qemu_img(&["snapshot", "-a", "online", &path.to_string_lossy()]);
    qemu_check(&path);
    {
        let mut img = vmlab_disk::open(&path, Format::Qcow2, true).unwrap();
        let mut got = vec![0u8; state.len()];
        img.read_vmstate(0, &mut got).unwrap();
        assert_eq!(got, state);
    }

    ops::snapshot_delete(&path, "online").unwrap();
    qemu_check(&path);
}

/// ops parity with qemu-img: info fields, linked clone content, rebase
/// (safe and unsafe), resize, convert (full, from a snapshot with a backing
/// file), commit.
#[test]
fn s2_ops_info_matches_qemu_img() {
    require_qemu_img!();
    let temp = tmpdir("ops-info");
    let dir = temp.path();
    let base = dir.join("base.qcow2");
    qemu_img(&["create", "-f", "qcow2", &base.to_string_lossy(), "16M"]);
    qemu_io(&["-c", "write -P 0x21 0 512k", &base.to_string_lossy()]);
    let top = dir.join("top.qcow2");
    ops::create_linked_clone(&base, &top).unwrap();
    write_pattern(&top, Format::Qcow2, 0, &[0x99u8; 4096]);
    ops::snapshot_create(&top, "snap").unwrap();

    let mine = ops::info(&top).unwrap();
    let theirs: serde_json::Value = serde_json::from_str(&stdout_of(&qemu_img(&[
        "info",
        "--output=json",
        &top.to_string_lossy(),
    ])))
    .unwrap();
    assert_eq!(mine.format, "qcow2");
    assert_eq!(mine.virtual_size, theirs["virtual-size"].as_u64().unwrap());
    assert_eq!(
        mine.backing_file.as_deref(),
        theirs["backing-filename"].as_str()
    );
    let q_full_backing = theirs["full-backing-filename"].as_str().map(str::to_string);
    assert_eq!(
        mine.full_backing_file
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        q_full_backing
    );
    assert_eq!(mine.chain.len(), 2, "chain includes base");
    assert_eq!(mine.chain[1].filename, base.canonicalize().unwrap());
    assert_eq!(mine.snapshots.len(), 1);
    assert_eq!(mine.snapshots[0].name, "snap");
    assert_eq!(
        mine.snapshots[0].vm_state_size,
        theirs["snapshots"][0]["vm-state-size"].as_u64().unwrap()
    );
}

#[test]
fn s2_ops_rebase() {
    require_qemu_img!();
    let temp = tmpdir("ops-rebase");
    let dir = temp.path();

    // Unsafe rebase rewrites the reference as given, relative stays relative.
    let base = dir.join("base.qcow2");
    qemu_img(&["create", "-f", "qcow2", &base.to_string_lossy(), "8M"]);
    qemu_io(&["-c", "write -P 0x41 0 256k", &base.to_string_lossy()]);
    let overlay = dir.join("overlay.qcow2");
    ops::create_linked_clone(&base, &overlay).unwrap();
    ops::rebase_unsafe(&overlay, "base.qcow2", Some("qcow2")).unwrap();
    let info = ops::info(&overlay).unwrap();
    assert_eq!(info.backing_file.as_deref(), Some("base.qcow2"));
    let theirs: serde_json::Value = serde_json::from_str(&stdout_of(&qemu_img(&[
        "info",
        "--output=json",
        &overlay.to_string_lossy(),
    ])))
    .unwrap();
    assert_eq!(theirs["backing-filename"].as_str().unwrap(), "base.qcow2");
    // Content still reads through the relative reference.
    assert!(
        read_all(&overlay, Format::Qcow2)[..4096]
            .iter()
            .all(|&b| b == 0x41)
    );
    qemu_check(&overlay);

    // Safe rebase onto a different base keeps the visible content.
    let base2 = dir.join("base2.qcow2");
    std::fs::copy(&base, &base2).unwrap();
    qemu_io(&["-c", "write -P 0x42 128k 64k", &base2.to_string_lossy()]);
    let before = read_all(&overlay, Format::Qcow2);
    ops::rebase_safe(&overlay, &base2, Some("qcow2")).unwrap();
    let after = read_all(&overlay, Format::Qcow2);
    assert_eq!(before, after, "safe rebase changed visible content");
    // Only the differing cluster was written into the overlay: 128k..192k.
    let info = ops::info(&overlay).unwrap();
    assert_eq!(
        info.full_backing_file.as_deref(),
        Some(base2.canonicalize().unwrap().as_path())
    );
    qemu_check(&overlay);
}

#[test]
fn s2_ops_resize_and_convert() {
    require_qemu_img!();
    let temp = tmpdir("ops-resize-convert");
    let dir = temp.path();
    let img = dir.join("r.qcow2");
    ops::create_qcow2(&img, 4 * 1024 * 1024, None, None).unwrap();
    write_pattern(&img, Format::Qcow2, 0, &[1, 2, 3, 4]);

    ops::resize(&img, 12 * 1024 * 1024).unwrap();
    let info = ops::info(&img).unwrap();
    assert_eq!(info.virtual_size, 12 * 1024 * 1024);
    qemu_check(&img);
    // The grown region is writable and zero.
    write_pattern(&img, Format::Qcow2, 8 * 1024 * 1024, &[7u8; 512]);
    let mut tail = [0u8; 512];
    vmlab_disk::open(&img, Format::Qcow2, true)
        .unwrap()
        .read_at(8 * 1024 * 1024, &mut tail)
        .unwrap();
    assert_eq!(tail, [7u8; 512]);
    qemu_check(&img);

    // Full convert from raw matches the source exactly.
    let raw = dir.join("src.raw");
    std::fs::write(&raw, pattern(6 * 1024 * 1024, 3)).unwrap();
    let conv = dir.join("conv.qcow2");
    ops::convert(&raw, &conv, None, None, None).unwrap();
    qemu_img(&["compare", &conv.to_string_lossy(), &raw.to_string_lossy()]);
    qemu_check(&conv);
}

/// `convert -l snapshot.name=… -B backing`: the dest holds only the delta
/// and matches what qemu-img's own export produces.
#[test]
fn s2_ops_convert_snapshot_with_backing() {
    require_qemu_img!();
    let temp = tmpdir("ops-export");
    let dir = temp.path();
    let base = dir.join("base.qcow2");
    qemu_img(&["create", "-f", "qcow2", &base.to_string_lossy(), "16M"]);
    qemu_io(&["-c", "write -P 0x50 0 1M", &base.to_string_lossy()]);

    let src = dir.join("src.qcow2");
    ops::create_linked_clone(&base, &src).unwrap();
    write_pattern(&src, Format::Qcow2, 2 * 1024 * 1024, &[0xau8; 300_000]);
    qemu_check(&src);
    ops::snapshot_create(&src, "point").unwrap();
    // Post-snapshot changes must not appear in the export.
    write_pattern(&src, Format::Qcow2, 2 * 1024 * 1024, &[0xbu8; 300_000]);
    write_pattern(&src, Format::Qcow2, 3 * 1024 * 1024, &[0xcu8; 100_000]);

    // Export via the crate; export via qemu-img; both must read the same.
    let mine = dir.join("mine.qcow2");
    ops::convert(&src, &mine, Some("point"), Some(&base), Some("qcow2")).unwrap();
    let theirs = dir.join("theirs.qcow2");
    qemu_img(&[
        "convert",
        "-O",
        "qcow2",
        "-l",
        "snapshot.name=point",
        "-B",
        &base.to_string_lossy(),
        "-F",
        "qcow2",
        &src.to_string_lossy(),
        &theirs.to_string_lossy(),
    ]);
    qemu_img(&[
        "compare",
        &mine.to_string_lossy(),
        &theirs.to_string_lossy(),
    ]);
    qemu_check(&mine);

    // And the exported view has the snapshot's content, not the later writes.
    let content = read_all(&mine, Format::Qcow2);
    assert!(content[..1024 * 1024].iter().all(|&b| b == 0x50)); // base through
    assert!(
        content[2 * 1024 * 1024..2 * 1024 * 1024 + 300_000]
            .iter()
            .all(|&b| b == 0x0a)
    ); // snapshot-time delta
    assert!(
        content[3 * 1024 * 1024..3 * 1024 * 1024 + 100_000]
            .iter()
            .all(|&b| b == 0)
    ); // post-snapshot write absent
    let info = ops::info(&mine).unwrap();
    assert_eq!(
        info.full_backing_file.as_deref(),
        Some(base.canonicalize().unwrap().as_path())
    );
}

/// `commit`: the top's active layer lands in its backing and the top is
/// left exactly as it was (`qemu-img commit -d` semantics).
#[test]
fn s2_ops_commit() {
    require_qemu_img!();
    let temp = tmpdir("ops-commit");
    let dir = temp.path();
    let base = dir.join("base.qcow2");
    qemu_img(&["create", "-f", "qcow2", &base.to_string_lossy(), "16M"]);
    qemu_io(&["-c", "write -P 0x60 0 512k", &base.to_string_lossy()]);
    let before_base = read_all(&base, Format::Qcow2);

    let top = dir.join("top.qcow2");
    ops::create_linked_clone(&base, &top).unwrap();
    write_pattern(&top, Format::Qcow2, 2 * 1024 * 1024, &[0xdu8; 200_000]);
    let before_top = read_all(&top, Format::Qcow2);

    ops::commit(&top).unwrap();

    let after_base = read_all(&base, Format::Qcow2);
    assert_eq!(&after_base[..512 * 1024], &before_base[..512 * 1024]);
    assert!(
        after_base[2 * 1024 * 1024..2 * 1024 * 1024 + 200_000]
            .iter()
            .all(|&b| b == 0x0d)
    );
    // The top is untouched.
    assert_eq!(read_all(&top, Format::Qcow2), before_top);
    qemu_check(&base);
    qemu_check(&top);

    // And the whole chain still reads consistently.
    qemu_img(&["compare", &top.to_string_lossy(), &base.to_string_lossy()]);
}

/// Read-only opens never modify the image.
#[test]
fn s2_read_only_never_modifies() {
    require_qemu_img!();
    let temp = tmpdir("ro");
    let dir = temp.path();
    let path = dir.join("ro.qcow2");
    ops::create_qcow2(&path, 8 * 1024 * 1024, None, None).unwrap();
    write_pattern(&path, Format::Qcow2, 0, &[3u8; 4096]);
    ops::snapshot_create(&path, "s").unwrap();
    let before = std::fs::read(&path).unwrap();

    {
        let mut img = vmlab_disk::open(&path, Format::Qcow2, true).unwrap();
        assert!(img.read_only());
        let mut buf = vec![0u8; 8 * 1024 * 1024];
        img.read_at(0, &mut buf).unwrap();
        assert!(matches!(
            img.write_at(0, &[1]),
            Err(vmlab_disk::Error::ReadOnly)
        ));
        assert!(!img.snapshots().unwrap().is_empty());
        let mut out = vec![0u8; 128];
        img.read_vmstate(0, &mut out).unwrap(); // empty area reads zeros
        assert!(out.iter().all(|&b| b == 0));
    }
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "read-only open wrote"
    );
}

/// Discard frees clusters, reads as zeros (or backing), and keeps the image
/// checkable.
#[test]
fn s2_subcluster_discard_preserves_neighboring_metadata() {
    require_qemu_img!();
    let temp = tmpdir("partial-discard");
    let path = temp.path().join("active.qcow2");
    let reference = temp.path().join("qemu.qcow2");
    ops::create_qcow2(&path, 1 << 20, None, None).unwrap();
    write_pattern(&path, Format::Qcow2, 0, &vec![0x77; 1 << 20]);
    std::fs::copy(&path, &reference).unwrap();
    run(
        QEMU_IO,
        &[
            "-f",
            "qcow2",
            "-c",
            "discard 512 512",
            reference.to_str().unwrap(),
        ],
    );
    {
        let mut image = vmlab_disk::open(&path, Format::Qcow2, false).unwrap();
        image.discard(512, 512).unwrap();
        image.flush().unwrap();
    }
    let actual = read_all(&path, Format::Qcow2);
    assert!(actual[..512].iter().all(|b| *b == 0x77));
    assert!(actual[1024..].iter().all(|b| *b == 0x77));
    assert_eq!(actual, read_all(&reference, Format::Qcow2));
    qemu_check(&path);
}

#[test]
fn s2_discard_and_zero_semantics() {
    require_qemu_img!();
    let temp = tmpdir("discard");
    let dir = temp.path();
    let path = dir.join("d.qcow2");
    ops::create_qcow2(&path, 8 * 1024 * 1024, None, None).unwrap();
    write_pattern(&path, Format::Qcow2, 0, &[0x77u8; 1024 * 1024]);
    {
        let mut img = vmlab_disk::open(&path, Format::Qcow2, false).unwrap();
        img.discard(128 * 1024, 256 * 1024).unwrap();
        img.flush().unwrap();
    }
    qemu_check(&path);
    let content = read_all(&path, Format::Qcow2);
    assert!(content[..128 * 1024].iter().all(|&b| b == 0x77));
    assert!(content[128 * 1024..384 * 1024].iter().all(|&b| b == 0));
    assert!(content[384 * 1024..1024 * 1024].iter().all(|&b| b == 0x77));
}

/// Snapshot lifecycle churn: create/apply/delete interleaved, with vmstate
/// of varying sizes, leaves a checkable image every time (mirrors what the
/// VMM does across a machine's life).
#[test]
fn s2_snapshot_churn() {
    require_qemu_img!();
    let temp = tmpdir("churn");
    let dir = temp.path();
    let path = dir.join("c.qcow2");
    ops::create_qcow2(&path, 64 * 1024 * 1024, None, None).unwrap();
    write_pattern(&path, Format::Qcow2, 7 * 1024 * 1024, &pattern(100_000, 4));

    let mut states = Vec::new();
    for round in 0..4u8 {
        let state = pattern(400_000 + round as usize * 100_000, round);
        {
            let mut img = vmlab_disk::open(&path, Format::Qcow2, false).unwrap();
            img.write_vmstate(0, &state).unwrap();
            img.snapshot_create(&format!("s{round}"), state.len() as u64, 0)
                .unwrap();
            // disk keeps moving between snapshots
            img.write_at(1024 * 1024 * (round as u64 + 1), &[round; 4096])
                .unwrap();
            img.flush().unwrap();
        }
        qemu_check(&path);
        states.push(state);
    }

    // Apply each snapshot in reverse order; vmstate and disk revert.
    for round in (0..4u8).rev() {
        let info = ops::snapshot_apply(&path, &format!("s{round}")).unwrap();
        assert_eq!(info.vm_state_size, states[round as usize].len() as u64);
        let mut img = vmlab_disk::open(&path, Format::Qcow2, true).unwrap();
        let mut got = vec![0u8; states[round as usize].len()];
        img.read_vmstate(0, &mut got).unwrap();
        assert_eq!(got, states[round as usize]);
        let disk = read_all(&path, Format::Qcow2);
        assert_eq!(
            &disk[7 * 1024 * 1024..7 * 1024 * 1024 + 16],
            &pattern(100_000, 4)[..16]
        );
        if round > 0 {
            // writes from later rounds are gone
            assert!(
                disk[1024 * 1024 * (round as usize + 1)..][..4096]
                    .iter()
                    .all(|&b| b == 0)
            );
        }
    }
    qemu_check(&path);

    // Delete all, one by one; each step stays checkable.
    for round in 0..4u8 {
        ops::snapshot_delete(&path, &format!("s{round}")).unwrap();
        qemu_check(&path);
    }
}

/// Larger buffers move through the cluster path without per-byte syscalls
/// and stay correct (a 16 MiB write/read at an unaligned offset).
#[test]
fn s2_large_unaligned_io() {
    require_qemu_img!();
    let temp = tmpdir("large");
    let dir = temp.path();
    let path = dir.join("l.qcow2");
    ops::create_qcow2(&path, 32 * 1024 * 1024, None, None).unwrap();
    let data = pattern(16 * 1024 * 1024, 21);
    let t0 = std::time::Instant::now();
    write_pattern(&path, Format::Qcow2, 123_457, &data);
    let write_secs = t0.elapsed().as_secs_f64();
    let mut got = vec![0u8; data.len()];
    let t1 = std::time::Instant::now();
    vmlab_disk::open(&path, Format::Qcow2, true)
        .unwrap()
        .read_at(123_457, &mut got)
        .unwrap();
    let read_secs = t1.elapsed().as_secs_f64();
    assert_eq!(got, data);
    assert!(
        write_secs < 10.0 && read_secs < 10.0,
        "pathologically slow: write {write_secs:.2}s read {read_secs:.2}s"
    );
    qemu_check(&path);
    let _ = std::io::stdout().flush();
}

/// Deterministic per-chunk pattern used by `vmlab-disk bench` (kept in
/// sync with `bench_fill` in src/bin/vmlab-disk.rs).
fn bench_fill(buf: &mut [u8], chunk_index: u64, seed: u8) {
    let mut x = seed.wrapping_add((chunk_index % 251) as u8);
    for b in buf.iter_mut() {
        x = x.wrapping_mul(31).wrapping_add(17);
        *b = x;
    }
}

#[test]
fn s2_snapshot_apply_then_write_same_handle() {
    require_qemu_img!();
    let temp = tmpdir("apply-write");
    let dir = temp.path();
    let path = dir.join("disk.qcow2");
    let amount = 4 * 1024 * 1024;
    ops::create_qcow2(&path, amount as u64, None, None).unwrap();
    let original = vec![0x31; amount];
    let changed = vec![0x72; amount];
    {
        let mut img = vmlab_disk::open(&path, Format::Qcow2, false).unwrap();
        img.write_at(0, &original).unwrap();
        img.write_vmstate(0, &vec![0x55; 128 * 1024]).unwrap();
        img.snapshot_create("saved", 128 * 1024, 0).unwrap();
        img.write_at(0, &changed).unwrap();
        img.snapshot_apply("saved").unwrap();
        qemu_check(&path);
        let mut vmstate = vec![0; 128 * 1024];
        img.read_vmstate(0, &mut vmstate).unwrap();
        assert_eq!(vmstate, vec![0x55; 128 * 1024]);
        let mut restored = vec![0; amount];
        img.read_at(0, &mut restored).unwrap();
        assert_eq!(restored, original);
        img.write_at(0, &changed).unwrap();
        img.flush().unwrap();
    }
    qemu_check(&path);
    assert_eq!(read_all(&path, Format::Qcow2), changed);
    let expected = dir.join("expected.raw");
    std::fs::write(&expected, &changed).unwrap();
    qemu_img(&[
        "compare",
        "-f",
        "qcow2",
        "-F",
        "raw",
        &path.to_string_lossy(),
        &expected.to_string_lossy(),
    ]);
    ops::snapshot_apply(&path, "saved").unwrap();
    qemu_check(&path);
    assert_eq!(read_all(&path, Format::Qcow2), original);
}

/// Holes freed by an earlier session are reused by a later process: fill,
/// snapshot, overwrite (the snapshot pins the old clusters), delete the
/// snapshot and discard the active range, then write the same amount from a separate `vmlab-disk`
/// process. The file must not grow beyond a little metadata, and
/// `qemu-img check` stays clean.
#[test]
fn s2_freed_clusters_reused_across_processes() {
    require_qemu_img!();
    let temp = tmpdir("reuse");
    let dir = temp.path();
    let path = dir.join("r.qcow2");
    ops::create_qcow2(&path, 64 * 1024 * 1024, None, None).unwrap();
    let amount = 4 * 1024 * 1024;
    write_pattern(&path, Format::Qcow2, 0, &pattern(amount, 1));
    ops::snapshot_create(&path, "s").unwrap();
    qemu_check(&path);
    write_pattern(&path, Format::Qcow2, amount as u64, &pattern(amount, 2));
    {
        let mut img = vmlab_disk::open(&path, Format::Qcow2, false).unwrap();
        img.discard(0, amount as u64).unwrap();
        img.flush().unwrap();
    }
    ops::snapshot_delete(&path, "s").unwrap();
    qemu_check(&path);
    let len_freed = std::fs::metadata(&path).unwrap().len();

    let out = Command::new(env!("CARGO_BIN_EXE_vmlab-disk"))
        .args([
            "bench",
            &path.to_string_lossy(),
            "write",
            &amount.to_string(),
            "--chunk",
            "1048576",
            "--seed",
            "90",
        ])
        .output()
        .expect("spawn vmlab-disk bench");
    assert!(
        out.status.success(),
        "vmlab-disk bench failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    println!("{}", String::from_utf8_lossy(&out.stdout).trim());

    let len_after = std::fs::metadata(&path).unwrap().len();
    assert!(
        len_after <= len_freed + 2 * 65536,
        "freed holes were not reused across processes: file grew \
         {len_freed} -> {len_after}"
    );
    qemu_check(&path);

    // The other process's data reads back through the crate.
    let content = read_all(&path, Format::Qcow2);
    let mut expect = vec![0u8; 1024 * 1024];
    for (i, chunk) in content[..amount].chunks(1024 * 1024).enumerate() {
        bench_fill(&mut expect, i as u64, 90);
        assert_eq!(chunk, &expect[..], "bench chunk {i} differs");
    }
    let _ = std::io::stdout().flush();
}

/// Long churn across reopen cycles: each cycle writes a stripe into the
/// filled area, records vmstate, takes a snapshot and deletes it, then
/// reopens — so every cycle's free map is rebuilt from disk. The file
/// stays bounded and every cycle remains checkable.
#[test]
fn s2_reuse_churn_across_reopens() {
    require_qemu_img!();
    let temp = tmpdir("churn-reuse");
    let dir = temp.path();
    let path = dir.join("c.qcow2");
    ops::create_qcow2(&path, 64 * 1024 * 1024, None, None).unwrap();
    write_pattern(&path, Format::Qcow2, 0, &pattern(8 * 1024 * 1024, 7));
    let len_start = std::fs::metadata(&path).unwrap().len();
    let mut prev_len = len_start;
    for round in 0..10u64 {
        {
            let mut img = vmlab_disk::open(&path, Format::Qcow2, false).unwrap();
            // A 1 MiB stripe rotating inside the filled area: in-place.
            let offset = (round % 8) * 1024 * 1024;
            img.write_at(offset, &pattern(1024 * 1024, round as u8))
                .unwrap();
            let vm = pattern(128 * 1024, round as u8 + 50);
            img.write_vmstate(0, &vm).unwrap();
            img.snapshot_create(&format!("t{round}"), vm.len() as u64, 0)
                .unwrap();
            img.snapshot_delete(&format!("t{round}")).unwrap();
            img.flush().unwrap();
        }
        let len = std::fs::metadata(&path).unwrap().len();
        // Round 0 legitimately builds the first vmstate structures (the
        // L2 past the guest area, two data clusters, a snapshot L1 and
        // table, the punch COW); every later round must reuse them.
        let slack = if round == 0 { 8 * 65536 } else { 65536 };
        assert!(
            len <= prev_len + slack,
            "round {round}: file grew {prev_len} -> {len}: freed holes \
             were not reused across the reopen"
        );
        prev_len = len;
        qemu_check(&path);
    }
    assert!(
        prev_len <= len_start + 8 * 65536,
        "churn grew the file {len_start} -> {prev_len}"
    );

    // The churn left the right bytes behind: stripe k was last written by
    // round k+8 for k in 0..2 (the wrap) and round k for k in 2..8.
    let content = read_all(&path, Format::Qcow2);
    for k in 0..8u64 {
        let last = if k < 2 { k + 8 } else { k };
        let stripe = &content[(k * 1024 * 1024) as usize..][..1024 * 1024];
        assert_eq!(stripe, &pattern(1024 * 1024, last as u8)[..], "stripe {k}");
    }
    let _ = std::io::stdout().flush();
}

/// Sequential throughput baseline: write then read 1 GiB through
/// `DiskImage` in 1 MiB chunks, printing MB/s so later virtio-blk work has
/// a number to compare against. Ignored by default: it needs 1 GiB of
/// scratch and tens of seconds.
#[test]
#[ignore = "slow: 1 GiB throughput baseline (run with cargo test -- --ignored)"]
fn s2_sequential_throughput_1gib() {
    require_qemu_img!();
    let temp = tmpdir("throughput");
    let dir = temp.path();
    let path = dir.join("t.qcow2");
    ops::create_qcow2(&path, 2 * 1024 * 1024 * 1024, None, None).unwrap();
    let chunk = 1024 * 1024;
    let chunks = 1024u64; // 1 GiB
    let mut buf = vec![0u8; chunk];
    let mut expect = vec![0u8; chunk];

    let t0 = std::time::Instant::now();
    {
        let mut img = vmlab_disk::open(&path, Format::Qcow2, false).unwrap();
        for i in 0..chunks {
            bench_fill(&mut buf, i, 1);
            img.write_at(i * chunk as u64, &buf).unwrap();
        }
        img.flush().unwrap();
    }
    let write_secs = t0.elapsed().as_secs_f64();

    let t1 = std::time::Instant::now();
    {
        let mut img = vmlab_disk::open(&path, Format::Qcow2, true).unwrap();
        for i in 0..chunks {
            bench_fill(&mut expect, i, 1);
            img.read_at(i * chunk as u64, &mut buf).unwrap();
            assert_eq!(buf, expect, "chunk {i} differs");
        }
    }
    let read_secs = t1.elapsed().as_secs_f64();

    let total = (chunks as usize * chunk) as f64;
    println!(
        "sequential 1 GiB through DiskImage: write {:.0} MB/s, read {:.0} MB/s \
         (write {write_secs:.2}s, read {read_secs:.2}s)",
        total / 1e6 / write_secs,
        total / 1e6 / read_secs,
    );
    assert!(
        write_secs < 120.0 && read_secs < 120.0,
        "pathologically slow: write {write_secs:.2}s read {read_secs:.2}s"
    );
    qemu_check(&path);
    let _ = std::io::stdout().flush();
}
