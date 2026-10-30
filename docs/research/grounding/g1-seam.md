# G1: the backend seam

All paths relative to the vmlab repo root. Line counts are `wc -l`. The full
QEMU flag/QMP-command surface is already catalogued in
`/home/wil/dev/vmlab-hypervisor/docs/research/qemu-viability/00-vmlab-qemu-inventory.md`
(not restated below); this slice covers the seam itself and what lives outside it.

## 1. The seam: src/labd/hypervisor/ (1257 lines)

Files: `mod.rs` (319), `qemu.rs` (273), `fake.rs` (665, `#[cfg(test)]` only —
`mod.rs:40-41`). Purpose: "everything that touches the host sits below this
seam; everything that decides what the machine's state *means* sits above it"
(`mod.rs:10-12`). Doc states argv building is deliberately NOT here
(`mod.rs:23-25`).

Three traits plus two value types:

- **`Process`** (`mod.rs:50-65`): `is_running`, `exit_status() -> Option<String>`
  (free text, because that is all `classify_exit` reads), `wait_exit(timeout)`,
  `kill`. Covers emulator, swtpm and virtiofsd alike.
- **`Control`** (`mod.rs:72-119`): `resume`, `powerdown` (ACPI request, not a
  kill), `quit`, `guest_shutdown() -> bool`, `sleep_watch() -> Option<watch::Receiver<bool>>`
  (default `None`), `wakeup` (default bails), and `qmp() -> Option<QmpClient>`
  (default `None`; the QMP client sits *behind* the trait, but the concrete
  `crate::qmp::QmpClient` still crosses the seam — `mod.rs:35,116`).
- **`Hypervisor`** (`mod.rs:169-212`): `start_tpm(machine, state_dir, ctrl_sock, log)`,
  `virtiofsd() -> Option<crate::qemu::virtiofsd::Virtiofsd>` (capability probe;
  the concrete type crosses the seam, `mod.rs:188`), `start_virtiofsd(...)`,
  `start_emulator(LaunchSpec) -> Running`, `guest_asset(arch)` (container
  kernel+initramfs location, a host property).
- **`LaunchSpec`** (`mod.rs:137-155`): `label`, `binary`, `args: Vec<String>`,
  `env: ChildEnv`, `log`, `qmp_sock`, `fds: Vec<ChildFd>`, `channels:
  GuestChannels{agent, ctl}`. `ChildEnv`/`ChildFd` are `qemu::process` types
  (`mod.rs:34`). **`Running`** (`mod.rs:158-161`): `{Arc<dyn Process>,
  Arc<dyn Control>}`.

Free functions above the seam: `sleep_transition` maps QMP event names
SUSPEND/WAKEUP/RESET/RESUME to asleep/awake (`mod.rs:222-228`); `classify_exit`
(`mod.rs:236-255`) — note it string-matches `"exit status: 0"` on the exit
status (`mod.rs:242`), so any backend must report `std::process::ExitStatus`'s
printed shape.

**Qemu adapter** (`qemu.rs`): unit struct. `start_tpm` spawns swtpm then polls
the ctrl socket 50×20 ms (`qemu.rs:31-50`). `start_emulator` writes a launch
header, `Proc::spawn_with_fds`, then `connect_qmp_retry` (100×100 ms, quoting
the log if the process dies) (`qemu.rs:73-99, 214-235`). `QemuControl::new`
subscribes the QMP event stream *before* the caller gets the handle: SHUTDOWN
with reason `guest-shutdown`/`guest-reset` sets the flag (`qemu.rs:161-165`);
sleep transitions feed the watch (`qemu.rs:167-169`). Control methods map to
`cont`/`system_powerdown`/`quit`/`system_wakeup` (`qemu.rs:181-208`).

**Who calls it**: `VmInstance.hv` (`vm.rs:320`) and `ContainerInstance.hv`
(`container.rs:363`), both hard-wired to `Arc::new(Qemu)` at construction
(`vm.rs:404`, `container.rs:405`). Call sites: `start_tpm` (`vm.rs:983`),
`start_virtiofsd` (`vm.rs:866`, `container.rs:637`), `virtiofsd()` probe
(`vm.rs:429, 835, 1495`, `container.rs:592, 611`, and `lab.rs:1074-1078` asks
any machine since all share one host), `start_emulator` (`vm.rs:1029`,
`container.rs:752`), `guest_asset` (`container.rs:732`). Stop ladders and the
exit monitor use only `Process`/`Control` (`vm.rs:1274-1336, 1078-1093`).

**The fake** (`fake.rs`, test-only): a machine is a `watch` channel plus flags;
`Script` models host facts (tpm/virtiofsd failures, old virtiofsd, missing
guest asset, live agent) and `Run` models per-boot behaviour (start failure, ctl
timeline, self-exit with/without guest initiation, `ignores_powerdown`,
`ignores_quit`, `sleeps_after`, agent restarts) (`fake.rs:51-155`). It stands up
real unix sockets speaking the actual vmlab-agent and cinit ctl protocols
(`fake.rs:485-606`), mirroring QEMU's `server=on,wait=off` chardevs. Injected
via the test-only `set_hypervisor` (`vm.rs:414-417`, `container.rs:414-417`);
all 38 `#[tokio::test]` lifecycle tests in `lifecycle_tests.rs` drive it through
the `vm()`/`container()` helpers (`lifecycle_tests.rs:241-287, 306-345`).

## 2. QEMU-specific behaviour outside the seam

| Area | Interface | Lines | What it does |
|---|---|---|---|
| `src/qemu/cmdline.rs` | argv | 1571 | Pure argv builder. `emulator_binary` = `qemu-system-{arch}` (`:280-281`); `qemu_args` appended verbatim, last (`:721-722`). |
| `src/qemu/resolve.rs` | — (pure) | 1301 | Config+profiles → `ResolvedVm`/`ResolvedContainer`; passes `qemu_args` through (`:82`, `:602-603`). |
| `src/qemu/process.rs` | OS process | 627 | `Proc::spawn_with_fds`, log header/tail, `spawn_swtpm` (`:277-295`), `binary_on_path` (`:268`), `kill_lab_orphans` matches QEMU/swtpm/virtiofsd/smbd by cmdline markers (`:308-346`). |
| `src/qemu/firmware.rs` | argv (pflash) | 997 | OVMF/AAVMF/RISCV lookup, bundled edk2 + distro fallback. Per-VM VARS copy at first start (`vm.rs:969-978`). |
| `src/qemu/container.rs` | argv | 533 | Micro-VM argv (`-kernel/-initrd/-append`, virtio-blk, no VNC); called at `container.rs:741-745`. |
| `src/qemu/virtiofsd.rs` | vhost-user + CLI | 575 | Capability probe and spawn; `--migration-mode` gating for online snapshots. |
| `src/qemu/hostgpu.rs` + `vk_layer.rs` + `keepawake.rs` | env/argv/C helper | 1654+173+92 | Host-GPU pick incl. per-GPU `qemu_binary`; Vulkan loader env folded into `LaunchSpec.env` (`vm.rs:1011-1033`); Vulkan layer install (`vm.rs:1014-1018`). |
| `src/qmp/` (client 559) | QMP JSON | 1171 | Client + event broadcast. Consumed above the seam via `Control::qmp()`: online snapshots (`vm.rs:1361-1447`, `container.rs:1057-1136`), USB `device_add`/`device_del` (`vm.rs:1837-1862`), `screendump`/`send-key`/`input-send-event` (`display/mod.rs:120-274`), `quit` (`machine.rs:834-838`). |
| `src/vnc.rs` | RFB 3.8 | 531 (+280 fake, +301 tests) | Auth-less RFB client; the only capture path on GL displays (`vnc.rs:16-20`). Users: display input/capture fallback (`display/mod.rs:98-134`), recorder streams frames to ffmpeg (`record/executor.rs:68-75`). |
| `src/usb/mod.rs` | QMP `device_add` args | 568 | `plan_attach` builds `usb-host` JSON with hostbus/hostaddr/hostport (`:521-534`); online-snapshot refusal rationale (`:547-565`). |
| `src/template/qimg.rs` | qemu-img CLI | 300 | All qemu-img wrappers: create/linked-clone/`rebase -u`/safe rebase/`info -U --output=json`/resize/convert/`snapshot -c/-a/-d`/`commit -d`/`export_overlay` (`:93-207`). |
| qemu-img callers | qcow2 on-disk | — | VM first-start clone/blank (`vm.rs:637-641`); container scratch (`container.rs:723-726`); template build + variant rebase + sealed flatten (`template/build.rs:526, 583, 1226-1228`); store in-use check walks backing chains (`template/store.rs:688-746`); freeze plan executes Export/Overlay/Commit/Rebase steps (`freeze/steps.rs:174-181`); freeze inspection reads chains off headers (`freeze/inspect.rs:5-7`); supervisor in-use scan (`supervisor/backing.rs:1-13`); worktree clones (`labd/freeze_clone.rs:497`). OCI uses qemu-img/qemu-io **only in tests** (`oci/client.rs:2476-2558`, tests mod at `:1647`); OCI rootfs is squashfs (`oci/image/flatten.rs:1, 45`). |
| Snapshot orchestration | QMP + qemu-img | ~100 | `vm.rs:1349-1451`: online via `snapshot-save/load/delete` with vmstate on `disk0` and devices `disk0..N`; offline via qimg on each disk. Containers use node `scratch` (`container.rs:1058, 1083, 1136`). |
| `qemu_args` handling | config → argv | — | Parsed at `config/extract.rs:596, 978, 1063` → `config/model.rs:502, 1012, 1089` → `resolve.rs` → appended last (`cmdline.rs:721-722`). Template/variant build VMs carry their own, rendered into the generated lab file (`template/build.rs:1953-1956`, variant override `:1835`). |

Freeze total: 3403 lines (plan 1384, ports 534, mod 358, steps 373, inspect 314,
qemu_tests 440). Supervisor `backing.rs` 512. `labd/vm.rs` 2282, `labd/container.rs` 1967.

## 3. How a second backend plugs in today

Hard-wired, not chosen: every `VmInstance`/`ContainerInstance` constructs
`Arc::new(Qemu)` (`vm.rs:404`, `container.rs:405`); there is no per-lab or
per-machine config, and the only replacement path is `#[cfg(test)]`
(`vm.rs:414`, `container.rs:414`). The field is per machine, so a per-machine
choice is one constructor parameter away — but nothing above the constructor
knows a backend exists.

What a non-QEMU backend must provide to satisfy each caller, beyond the trait:

1. **Consume or replace argv**: callers build `LaunchSpec.args` with
   `qemu::build_args` (`vm.rs:1019-1024`) / `build_container_args`
   (`container.rs:741-745`) and pick `binary` via `render.emulator` or
   `emulator_binary` (`vm.rs:1031`, `container.rs:754`). The seam takes a
   concrete argv; composition is outside it by stated design (`mod.rs:23-25`).
2. **Guest channels**: bind `vmlab.agent.0` / `vmlab.ctl.0` unix sockets itself
   — QEMU binds them from the argv; the fake stands them up manually
   (`mod.rs:121-134`, `fake.rs:485-505`).
3. **A QMP substitute or degraded features**: `Control::qmp()` returning `None`
   makes online snapshots, USB attach, QMP screendump/input, and container
   online snapshots fail with "not running" (`vm.rs:521-528`,
   `container.rs:476-483`, `display/mod.rs:46`). These paths take the concrete
   `QmpClient`, not a trait.
4. **RFB on `vnc.sock`** (`vm.rs:178-180`): `DisplayHost::vnc_sock`
   (`display/mod.rs:48`) and the recorder (`record/executor.rs:69`) connect
   over RFB; the GL capture path has no QMP alternative (`vnc.rs:16-20`).
5. **qcow2 compatibility**: disks, snapshots, templates and freeze chains are
   managed with qemu-img entirely above the seam (§2).
6. **Host probes**: accel plan / KVM usability (`vm.rs:964`,
   `container.rs:733`), GPU render/env/keepawake (`vm.rs:1011-1018`),
   `required_binaries` = `qemu-img` + `qemu-system-{arch}` + `swtpm`
   (`vm.rs:1579-1595`, `container.rs:1227-1232`), and `kill_lab_orphans`'s
   cmdline markers (`process.rs:339-346`).
7. **Semantics already encoded above the seam**: exit-status string shape
   (`mod.rs:242`), QMP event names for sleep (`mod.rs:222-228`), swtpm
   ctrl-socket readiness contract (`mod.rs:170-172`), virtiofsd
   migration-mode capability gating (`fake.rs:144-147` models the old-binary
   case; `virtiofsd.rs` probes it).

## 4. On-disk artefacts vmlab owns in QEMU formats

- **Per-VM disks**: `.vmlab/vms/<name>/disk0.qcow2` (linked clone of the
  template variant, or blank — `vm.rs:637-641`) plus `disk-<name>.qcow2`;
  snapshot addressing depends on this order (`machine.rs:248-251`). qcow2
  **internal** snapshots carry vmstate for online saves (`vm.rs:1364`).
- **OVMF_VARS.fd** per VM, copied once at first start, never replaced
  (`vm.rs:209-211, 969-978`) — edk2 variable-store (pflash) format.
- **tpm-state/** per VM (`vm.rs:212-214`) — swtpm state directory.
- **Container scratch**: `scratch.qcow2` (`container.rs:102-104`).
- **Template store**: `<root>/<arch>/<name>/<version>/{disk.qcow2, template.wcl}`
  plus `variants/<name>/disk.qcow2` overlays whose backing file is the relative
  `../../disk.qcow2` (`store.rs:3-6, 25, 688`); secure-boot templates carry a
  pre-enrolled VARS file (`store.rs:26-28`).
- **Freeze generations**: `.vmlab/freeze/<n>/<machine>/disk*.qcow2` read-only,
  chained by `qemu-img rebase` (overlay → gen N → … → variant → base); OVMF
  VARS, tpm-state, `clone-source`, `firstboot.done` carried beside them
  (`freeze/mod.rs:12-22`, `plan.rs:181-186`). Dependence is read off the qcow2
  headers themselves (`inspect.rs:5-7`, `backing.rs:4-9`).
- Runtime sockets (QEMU-created, not formats): `qmp.sock`, `agent.sock`,
  `vnc.sock` (`vm.rs:166-180`), `ctl.sock` (`container.rs:78-87`).

## Open questions this raises

1. The seam excludes composition while `LaunchSpec` is argv: does a second
   backend consume `ResolvedVm` directly (resolve.rs is pure), and where does
   that mapping live?
2. `Control::qmp()` hands the concrete `QmpClient` above the seam; snapshots,
   USB and display all take it. Widen `Control`, per-feature capability traits,
   or accept degraded modes?
3. qcow2 is the lingua franca of store, snapshots and freeze chains. Does the
   new hypervisor commit to qcow2 (and to internal-snapshot vmstate semantics),
   or does a disk-format seam come first?
4. RFB is load-bearing for recording and GL capture with no QMP fallback —
   what is backend #2's display surface?
5. S3 sleep/wake and the swtpm/virtiofsd helper contracts are encoded above
   and inside the seam respectively; are they in scope for the new backend?
