# G2 — product constraints and vocabulary (vmlab)

Grounding slice for the vmlab-hypervisor planning map. Sources: `docs/vmlab-prd.md` (PRD, binding contract per CLAUDE.md:289-290), `CONTEXT.md`, `docs/adr/*.md`, `CLAUDE.md`, git history, and spot-reads of `src/`. Paths relative to the vmlab repo root. Facts taken from the background inventory (`/home/wil/dev/vmlab-hypervisor/docs/research/qemu-viability/00-vmlab-qemu-inventory.md`) are marked "(inv.)" — its file:line cites were not all re-verified here.

## 1. Constraints a replacement hypervisor must honour

**Scope.** vmlab targets QEMU/KVM exclusively, direct QMP, no libvirt (PRD §1, docs/vmlab-prd.md:15, 31). A second backend is *not* an existing product requirement — §1.2 lists "hypervisors other than QEMU/KVM" as a non-goal (docs/vmlab-prd.md:31). The insertion point that exists today is the Hypervisor seam (§3 of this report).

### Snapshots (PRD §7.3)
- Online (disk + RAM + device state, beyond the unavoidable pause) and offline (disk only) both required (docs/vmlab-prd.md:470-473).
- Every snapshot records power state at capture; an online restore resumes running, an offline restore leaves the VM off (docs/vmlab-prd.md:475).
- Mechanism: **qcow2-internal snapshots wherever possible**; external only where internal can't meet the contract; must coexist with the linked-clone backing chain — "the contract, not the mechanism, is what binds" (docs/vmlab-prd.md:477; §16 #4, docs/vmlab-prd.md:934).
- Lab-wide snapshot verbs cover all machines; cross-VM consistency is best-effort, explicitly not coordinated (docs/vmlab-prd.md:475).
- Refusals (a backend must keep refusing these): online capture/restore of a machine with virtiofs devices when virtiofsd predates state transfer (docs/vmlab-prd.md:477); online capture of a suspended VM (docs/vmlab-prd.md:464); online capture while USB devices are attached (docs/vmlab-prd.md:302); dev-machine capture with unsynced guest work refuses with **no** escape flag, restore refuses while a sync halt stands unless `--discard-guest-changes` (§19.6, docs/vmlab-prd.md:1995-2003).
- Post-restore clock: `clock = "host"` (default) sends the agent `set_time` within 60 s of resume; `"snapshot"` keeps capture time; an RFC 3339 value pins the RTC start (`-rtc base=`) (docs/vmlab-prd.md:479-487). RTC holds UTC (docs/vmlab-prd.md:485).
- Containers snapshot with full parity: qcow2-internal on the per-container scratch qcow2; each snapshot records the pinned image digest and restore under a different pin fails (docs/vmlab-prd.md:1023-1040).
- vmlab performs the restore, so it brackets the workspace syncer: restore = host-only digest-based re-seed completing before the watch reopens (docs/vmlab-prd.md:1972-1993).

### Worktree freeze (PRD §21, ADR-0019)
- Freeze needs the lab stopped and an **offline** snapshot on every machine; an online snapshot is refused — a cold boot from its disk alone is a power cut (docs/vmlab-prd.md:2316-2319).
- Mechanism per disk: temporary internal snapshot → `qemu-img snapshot -a <snap>` → move file to `.vmlab/freeze/<n>/<machine>/` read-only → main lab's new disk is an overlay written by `qemu-img convert -l snapshot.name=<tmp> -B <frozen> -F qcow2` → delete temp snapshot (docs/vmlab-prd.md:2323-2328). Firmware vars (`OVMF_VARS.fd`), `tpm-state`, volumes and state records are copied beside (docs/vmlab-prd.md:2331-2333).
- Generations chain (`n+1` backed by `n`); worktree labs keep their generation (docs/vmlab-prd.md:2337-2340).
- A worktree `up` clones overlays from the freeze by absolute path, carrying MACs, firmware vars, TPM state, image pins; it refuses a missing generation and never falls back to the template (docs/vmlab-prd.md:2360-2375).
- Snapshots taken before the freeze are **locked** (they live in the frozen file); restoring the freeze point replaces the lab's overlays with empty ones; thaw commits overlays down the chain with `qemu-img commit` (docs/vmlab-prd.md:2379-2395).
- Nothing in vmlab may open a frozen file read-write (docs/vmlab-prd.md:2430-2431). Root constraint recorded in ADR-0019: a linked clone backs onto a file's *current* state and cannot name an internal snapshot; a running QEMU holds a write lock (docs/adr/0019-*.md:25-33).
- Worktree isolation: NAT refuses host addresses, forwards bind `127.0.0.1`, global segments renamed `<segment>@<worktree>` (docs/vmlab-prd.md:2399-2415); port slots shift fixed ports per worktree (docs/vmlab-prd.md:2445-2466).

### Templates — on-disk and OCI (PRD §6)
- Store key arch+name+version; references `<arch>/<name>[@<version>][#<variant>]`, arch always explicit, never inferred from host (docs/vmlab-prd.md:363).
- Store layout: `<root>/<arch>/<name>/<version>/disk.qcow2` + `template.wcl` + `variants/<name>/disk.qcow2` (overlay, relative backing `../../disk.qcow2`) (docs/vmlab-prd.md:416-423). A variant-bearing base is unsealed and never cloned by a lab (docs/vmlab-prd.md:410-414).
- Sealed metadata records hardware, provenance, baked `agent_version`, wscript-surface version; a band of surfaces is enforced (docs/vmlab-prd.md:364).
- A VM's disk is a **linked clone** (qcow2 overlay) backed by the resolved disk's absolute path; the recorded `clone-source` refuses a changed template reference (docs/vmlab-prd.md:437, 449, 427).
- `scratch` = blank qcow2, no template layer (docs/vmlab-prd.md:392-396). Builds run as a synthetic one-VM scratch lab; effective build hardware is block-over-source merged once and the **profile is never frozen** into the sealed image (docs/adr/0009-*.md:45-69).
- OCI distribution: templates are OCI **artifacts, not container images** — `application/vnd.vmlab.template.v1` artifactType, config blob = metadata, layers = qcow2 chunked (default 512 MiB, zstd), multi-arch via image index; variants are manifests of their own tagged `<version>--<variant>`, with the base manifest naming its variants (docs/vmlab-prd.md:375-387; §16 #8/#9, docs/vmlab-prd.md:938-939). Export/import archive = qcow2 + metadata + variants, digest-verified (docs/vmlab-prd.md:366).

### Container micro-VMs (PRD §18)
- Every OCI container runs in a **micro-VM**: pinned Alpine `linux-virt` kernel + purpose-built initramfs (`vmlab-cinit` as PID 1 spawning `vmlab-agent`), booted directly `-kernel/-initrd/-append` (docs/vmlab-prd.md:973-975).
- `cpus`/`memory` resolve through the same chain (block > profile; no template layer) with no built-in default — omission is a validation error; shipped `container` profile floor 1 vCPU / 256 MiB (docs/vmlab-prd.md:976-981).
- Image layers flattened (whiteout-aware) into a read-only squashfs; per-container scratch qcow2 as the writable overlayfs layer (docs/vmlab-prd.md:982-984).
- Config reaches the guest as a `ContainerSpec` over the ctl channel (a second virtio-serial port, `vmlab.ctl.0`); volumes attach as **vhost-user-fs** devices (one virtiofsd each, shared `memory-backend-memfd` RAM), SMB fallback where virtiofsd is unusable; **no 9p device is ever attached** (migration blocker) (docs/vmlab-prd.md:984-997).
- Rootless: no `--privileged`, no added capabilities — `/dev/kvm` only, TCG fallback without it (docs/vmlab-prd.md:998-1001).
- Stop ladder mirrors VMs; container stdout/stderr is the serial console log (docs/vmlab-prd.md:1019-1021). Cross-arch containers out of scope; a crun/native runtime backend is recorded as a possible fast path "if micro-VM start latency ever matters" (docs/vmlab-prd.md:1044-1046).

### Agent channel (PRD §7.4)
- `vmlab-agent` rides one multiplexed **virtio-serial** port, `vmlab.agent.0`: readiness, terminals, streaming exec, digest-verified file transfer (now the `fileops` RPC session), tail, metrics, clipboard, `set_time`, OS info, per-NIC IPs, graceful shutdown/reboot (docs/vmlab-prd.md:489-498).
- `agent_transport` per profile: `"virtio-serial"` (default), `"isa-serial"` (16550 UART on COM1, serial log moves to COM2, C89 `vmlab-agent-legacy` advertising only `exec` + `set_time`), `"none"` (screen-driven, never ready) (docs/vmlab-prd.md:493). `windows-xp`, `windows-9x`, `templeos` profiles use isa-serial (docs/vmlab-prd.md:315-317).
- Channel topology invariant: **the host opens channels, the guest only ever answers** — no guest-initiated channel opens exist or may be added (docs/adr/0013-*.md:27-37).
- QEMU's own guest agent (QGA) is not used; commit `1ed73b3` "vmlab-agent replaces the QEMU guest agent everywhere".

### Networking (PRD §9)
- Each segment is a virtual L2 switch inside its owning daemon; every NIC attaches via a QEMU **stream-socket netdev** over a unix socket (docs/vmlab-prd.md:595).
- Frame format on that socket: **4-byte big-endian length prefix + one raw Ethernet frame per send**; cap 64 KiB + 14 B header (src/net/framing.rs:1-14; commit `05e43ee`).
- No privileges required — no tap, bridge, macvlan or CAP_NET_ADMIN — which is what makes WSL2 first-class (docs/vmlab-prd.md:597).
- The netdev attachment is deliberately a substitution seam: "a vhost-user or tap backend can be substituted per segment later without changing lab semantics" (docs/vmlab-prd.md:600).
- Fast paths (§9.1.1): **afxdp** tier = NICs as `-netdev tap,fd=` with pre-opened fds + per-segment XDP forwarding in-kernel, punting broadcast/gateway/unknown/isolated to userspace; **sockmap** tier measured *slower* than userspace, never auto-selected; **userspace** always the fallback. Both kernel tiers need CAP_BPF + CAP_NET_ADMIN, are proved per-host before use, and degrade silently (docs/vmlab-prd.md:602-610).
- DHCP on by default (reservations keyed on persisted MAC, option 121 static routes); DNS on by default, suffix `vmlab.internal`; NAT is a slirp/passt-style userspace port on the switch; port forwards are host listeners; filtering/redirection mutable at runtime (docs/vmlab-prd.md:624-628, 633-635, 656, 665-677).
- Cross-host segments: same trunk protocol over TCP between supervisors, PSK auth (docs/vmlab-prd.md:90, 616; §16 #5, docs/vmlab-prd.md:935).

### Display and recording (PRD §11, §11.1)
- Every VM gets a **VNC display on a unix socket, always present**; TCP optional and off; SPICE deferred; `gui = true` is never QEMU's own window (docs/vmlab-prd.md:836-838).
- Recording holds its own long-lived RFB connection to the same socket, receives incremental updates + cursor shape, composites pointer and click ripples, pipes constant-rate frames to host `ffmpeg`, reconnects across `vm restart` and snapshot restore (docs/vmlab-prd.md:844, 858). Recording from first frame requires the VM to start **paused** (`-S`) and be resumed over control once the recorder attaches (docs/vmlab-prd.md:846).
- Screenshots/OCR read the framebuffer via QMP `screendump`, falling back to one full non-incremental RFB update (32 bpp Raw) on GL displays where `screendump` answers `no surface` (docs/vmlab-prd.md:796).
- Input injection: `send_keys` (QMP sendkey naming), `type_text`, mouse move/click/drag (docs/vmlab-prd.md:775-777).

### USB passthrough (PRD §5.2.1)
- Opt-in `usb = true` boots a hot-plug xHCI (`usbx`); devices hot-plug at runtime as `usb-host` pinned by `hostbus` + `hostport` **and** `hostaddr` (docs/vmlab-prd.md:279, 294).
- Ordered attach refusals (container; not running / no `usb = true`; selector matches nothing/several; held by another machine; HID keyboard/mouse without `--force`; unopenable `/dev/bus/usb` node with the setfacl/udev fixes named) (docs/vmlab-prd.md:285-292).
- Tracking is runtime-only, in daemon memory; sysfs polled every 2 s while anything is attached to catch unplugs/replugs (docs/vmlab-prd.md:298-300). Online snapshots refused while devices are attached (docs/vmlab-prd.md:302).

### GPU (PRD §5.2)
- Three modes: `passthrough` (`vfio-pci,host=<addr>`, exclusive), `virgl` (virtio-gpu-gl; `virtio-vga-gl` on x86), `vulkan` (Venus: `venus=true,blob=true,hostmem=4G`) (docs/vmlab-prd.md:215-222).
- A paravirtual GPU is the guest's **only** display adapter; host-side rendering with the framebuffer scraped for VNC/screenshots (`egl-headless` display path); screenshots must keep working in all GPU modes (docs/vmlab-prd.md:220-222).
- Host GPUs are declared in host config by name + PCI address (a lab file never carries an address); a GPU may name its own `qemu_binary` — needed because **QEMU 11.1's egl-headless mishandles NVIDIA blob dma-buf scanout** — plus `qemu_env` (docs/vmlab-prd.md:224-254).
- Vulkan loader is filtered in the emulator's environment (`VK_LOADER_DRIVERS_SELECT` etc.) plus vmlab's own implicit `VK_LAYER_VMLAB_pci_filter` layer so a Venus guest sees exactly one card (docs/vmlab-prd.md:248-259).
- ADR-0018: the supervisor runs one `vmlab-gpu-keepawake` helper per held GPU (EGL context, 16×16 clear every 100 ms) so NVIDIA runtime-suspend never loses the GPU between labs (docs/adr/0018-*.md:32-54; docs/vmlab-prd.md:261-264).

### Shared folders (PRD §7.5)
- One `share {}` surface, two transports: **virtiofs** (one virtiofsd per share as vhost-user-fs, run with `--migration-mode=find-paths --migration-verify-handles` so session state rides the snapshot's migration stream; requires QEMU ≥ 8.2 / virtiofsd ≥ 1.11, else online snapshots of such machines are refused) and **SMB at the segment gateway** (universal fallback, zero device state in snapshots; SMB1/NTLMv1 opt-in for XP-era; per-lab generated credentials, NTLMv2 + signing baseline) (docs/vmlab-prd.md:512-526).
- virtiofs into Windows guests is not trusted with large files (virtio-win VioFS read bug), so shipped Windows profiles leave the capability off (docs/vmlab-prd.md:516).
- Share *contents* are host state, outside snapshot scope; a VM with no NICs cannot have shares (docs/vmlab-prd.md:560-561).

### Host platforms, arches, profiles
- Hosts are **Linux, with WSL2 explicitly first-class** (docs/vmlab-prd.md:15, 896-898); macOS guests and hosts are a non-goal (docs/vmlab-prd.md:2299). WSL2 constrains networking to no tap/bridge and needs nested virtualisation for KVM (docs/vmlab-prd.md:898).
- Accelerators: `kvm` (only when `/dev/kvm` opens and guest arch matches host, else warning + TCG fallback; `kvm` forced = refusal instead of fallback) and `tcg`; `hvf`/`whpx` rejected (docs/vmlab-prd.md:214; inv. src/qemu/profiles/mod.rs:387).
- Accepted arches: x86_64, x86 (label running on the x86_64 emulator), aarch64, riscv64, loongarch64, s390x, ppc64 (src/config/model.rs:1156-1165). Only x86_64/aarch64/riscv64 have firmware tables and container support (inv. src/qemu/firmware.rs:83-91, src/qemu/resolve.rs:633-637).
- Profiles are WCL data, user-overridable; the shipped set spans q35/SeaBIOS Windows 11→DOS eras with e1000/pcnet NICs and isa-serial agent tiers (docs/vmlab-prd.md:308-322). Hardware precedence: **VM block > template > profile**, one resolver for both machine kinds (docs/vmlab-prd.md:273; ADR-0008).
- Firmware ships with vmlab: Debian edk2 (OVMF 4M plain + secboot with `.ms` VARS, AAVMF, RISCV pairs), pinned by version and sha256; secure boot refuses a host without an enrolled-key store rather than boot in setup mode (docs/vmlab-prd.md:267-269).

### Performance and startup targets
- **Performance is a stated non-goal** — the userspace fabric "will not approach tap/bridge throughput" (docs/vmlab-prd.md:27). No numeric startup-time or throughput target exists anywhere in the PRD (checked §1–§21; none found). Container micro-VM start latency is named once, as the condition under which a crun backend might matter (docs/vmlab-prd.md:1044-1046).
- Nested-virtualisation timing is documented qualitatively (Hyper-V reference TSC ~20 ns native vs 51 µs nested under WSL2) as motivation for enlightenments, not a target (docs/vmlab-prd.md:213).

### Security posture
- **Not a security boundary**: "a lab tool for a single trusted user… makes no hardening claims"; no multi-tenancy (docs/vmlab-prd.md:29).
- Rootless baseline: control plane needs no privileges (docs/vmlab-prd.md:597); containers need `/dev/kvm` only (docs/vmlab-prd.md:998-1001); the optional eBPF fast path's CAP_BPF + CAP_NET_ADMIN are granted as file capabilities by `install.sh`, opt-in, default no (docs/vmlab-prd.md:610; install.sh:36-40).
- **No seccomp or sandboxing of the emulator exists** — repo-wide grep for seccomp/sandbox finds none applied to QEMU (verified; only the rootless *guest asset build* uses "rootless" in a build sense, CLAUDE.md:155). Any sandboxing would be new behaviour, not a preserved one.

## 2. Domain vocabulary the new repo should reuse (CONTEXT.md)

- **Machine** — anything a lab boots, attaches to a segment, and drives through the agent; VM or container (CONTEXT.md:47).
- **VM** — machine booted from a template's linked clone (CONTEXT.md:53); **Container** — machine from an OCI image, inside a micro-VM (CONTEXT.md:58); **Micro-VM** — the tiny VM (Alpine kernel + vmlab's init) each container runs in (CONTEXT.md:64).
- **Lab** — machines + networks in one `vmlab.wcl` (CONTEXT.md:15); **Lab instance** — same file under `<declared>-<instance>` (CONTEXT.md:22); **Segment** — virtual L2 switch with daemon-supplied services (CONTEXT.md:72).
- **Guest** — the OS inside a machine (CONTEXT.md:68); **Scratch VM** — blank-disk VM, no template (CONTEXT.md:77).
- **Hypervisor** — "the seam between deciding a machine should run and the host actually running it", stated as what running means, handing back its own Process/Control handles; avoid *driver, backend, runtime* (CONTEXT.md:363-372).
- **Spawner** — the guest-side counterpart of that seam inside vmlab-agent; identity is its parameter (CONTEXT.md:374-382).
- **Template** — sealed read-only qcow2 in the store, `<arch>/<name>[@version][#variant]` (CONTEXT.md:223); **Base** — the one disk every variant is built on, unsealed when variants exist (CONTEXT.md:230); **Variant** — sealed overlay state of a version (CONTEXT.md:239); **Linked clone** — CoW overlay a machine boots (CONTEXT.md:249); **Store** — local template store, all writes via the supervisor (CONTEXT.md:253); **OCI artifact** — template in a registry, chunked zstd layers (CONTEXT.md:261).
- **Profile** — named hardware defaults, one precedence chain for both kinds (CONTEXT.md:284); **Effective build hardware** — block-over-source merge driving the build; profile never frozen (CONTEXT.md:276).
- **Capability** — something one machine can do that another might not; probed and reported, never inferred from kind (CONTEXT.md:149); **Display** — framebuffer + input/OCR/matching over it (CONTEXT.md:169).
- **vmlab-agent** — first-party in-guest agent over virtio-serial; avoid *QGA, guest tools* (CONTEXT.md:317).
- **Fast path** — optional eBPF acceleration above the userspace switch (CONTEXT.md:776); **virtiofs** — snapshot-safe shared-filesystem transport, SMB is the fallback (CONTEXT.md:781).
- **Freeze / Worktree lab / Worktree isolation** — read-only snapshot generations under `.vmlab/freeze/<n>/`, labs cloned from them, and the isolation rules a freeze turns on (CONTEXT.md:29-45).
- **Host GPU** — GPU declared by name in host config; labs name it, never an address (CONTEXT.md:685); **PCI filter** — vmlab's implicit Vulkan layer hiding all but one physical device (CONTEXT.md:700); **Keep-awake helper** — per-GPU EGL context holder run by the supervisor (CONTEXT.md:710).
- **Snapshot bracket / Re-seed** — what vmlab does around snapshots of workspace machines; the host-only digest reconcile a restore owes (CONTEXT.md:641-663).
- **USB Attachment / Selector** — one host USB device held by one running VM; how a person names a device (CONTEXT.md:206-219).
- **Plan (LabPlan / Share plan / Mount steps / Forward plan / Pull ledger)** — decisions computed as values before execution (CONTEXT.md:394-421).

## 3. ADRs by number (★ = binds a hypervisor backend)

| # | Decision (docs/adr/README.md:9-29) | Binds? |
|---|---|---|
| 0001 | The Hypervisor seam substitutes the host, and Machine is the test surface | ★ defines the seam a backend implements |
| 0002 | The Machine interface is the only route to a machine | ★ a second backend arrives as one more Machine implementation; capability model governs kind differences |
| 0003 | Decisions are computed as values before execution | ★ emulator-adjacent decisions (share plan, mount steps, forward plan, USB attach args) are values a backend only executes |
| 0004 | Lab status is a typed projection | no |
| 0005 | The schema is reflected, not restated | no |
| 0006 | One WCL block extractor | no |
| 0007 | The wire protocol carries a typed vocabulary | no |
| 0008 | Hardware resolution happens once, for both machine kinds | ★ one resolver (VM > template > profile) feeds whatever argv the backend consumes; argv builder must stay pure |
| 0009 | Build hardware resolves block over source; profile stays live | ★ what a build boots and what is sealed into metadata |
| 0010 | The supervisor owns the template store | no |
| 0011 | A lab name is its host-global runtime identity | no |
| 0012 | vmlab terminates SSH on the host | no (superseded; surface removed) |
| 0013 | The host opens channels, the guest answers | ★ channel topology on the agent transport the backend provides |
| 0014 | The workspace is a guest-local copy of a canonical host tree | no (rides the agent) |
| 0015 | One guest-side Spawner seam carries identity | no (guest-side; the deliberate mirror of 0001 inside the agent) |
| 0016 | A shared app rides the guest family's own window protocol | no (rides the agent) |
| 0017 | A template version is a base plus sealed variants, stored as overlays | ★ fixes qcow2 overlays + the impossibility of cloning an internal snapshot as the disk model |
| 0018 | The supervisor holds a host GPU awake | ★ host-GPU lifecycle around emulator starts |
| 0019 | A worktree lab is a linked clone of a frozen main lab | ★ qcow2 overlay chains, read-only frozen disks, locked snapshots |

All are Accepted except 0012 (Superseded); 0011 is amended (docs/adr/README.md:21-22).

## 4. Prior attempts at replacing or wrapping QEMU

- **Graveyard repos** (CLAUDE.md:11-14): `vmlab_qemu` (QMP/QGA driver crate), `vmlab_oci` (OCI registry client for VM disk images), `vmlab_floppy` (pure-Rust FAT for floppy images), all archived under github.com/wiltaylor/.graveyard-private, buried 2026-06-12 — "consult them for prior art only; the PRD overrides anything they did."
- **In-repo seam history**: commit `cc37a1d` "refactor(labd): put a Hypervisor seam under machine boot" introduced `trait Hypervisor`; it stalled unused until `f07ca36` "Finish the Hypervisor seam and test machine lifecycle through Machine (#31)". ADR-0001 records why the first seam failed: placed at "which binary do I exec", returning concrete host types, so the fake had to spawn real processes (docs/adr/0001-*.md:15-36). The redesigned seam lives at src/labd/hypervisor/ (mod.rs trait, qemu.rs production adapter, fake.rs test adapter).
- **QGA era**: commit `eab4fa6` built exec/cp/osinfo through the QEMU guest agent (qga file-append mode); `1ed73b3` "feat!: vmlab-agent replaces the QEMU guest agent everywhere".
- **Branches**: no hypervisor/backend branches exist (`git branch -a`: only feat/*, fix/*, integrate/win9x, worktree-agent-*, main). No ADR proposes a second backend; ADR-0001's "two adapters" are QEMU and the test fake (docs/adr/0001-*.md:53-55).
- **External prior art**: ADR-0018 cites `labs/zeus/keepawake/` in vmlab-win3d as measured prior art for the keep-awake helper (docs/adr/0018-*.md:24-28).

### The seam contract today (src/labd/hypervisor/mod.rs)
A second backend implements: `start_tpm` (swtpm socket), `virtiofsd()` capability probe (decides virtiofs vs SMB *and* whether online snapshots are possible), `start_virtiofsd`, `start_emulator(LaunchSpec) -> Running{Process, Control}`, `guest_asset(arch)` (src/labd/hypervisor/mod.rs:169-212). `Control` covers resume-from-paused-start, ACPI powerdown, quit, guest-shutdown flag, S3 sleep watch + wakeup, and an **optional live `qmp()` escape hatch** — snapshot save/load, framebuffer capture and scripted input "genuinely need a hypervisor" and bypass the seam today (src/labd/hypervisor/mod.rs:73-118). Argv composition is deliberately *not* in the seam — that is `crate::qemu`'s pure, tested job (src/labd/hypervisor/mod.rs:23-25).

## Open questions this raises

- Q1: The seam's `qmp()` escape hatch means snapshots, input and framebuffer capture are QEMU-shaped above the seam. A second backend forces those paths to be re-modelled as seam operations first — how large is that refactor?
- Q2: qcow2 is load-bearing beyond disks: internal snapshots (§7.3), variant overlays and `base_sha256` (§6.6, ADR-0017), clone chains (§7.1), freeze generations (§21, ADR-0019) and OCI chunking (§6.4) all assume it. Does the new hypervisor keep qcow2, or does each of those features need a new mechanism?
- Q3: §1.2 declares non-QEMU hypervisors a non-goal; adopting a second backend is a product-level PRD change, not just an engineering one. Who owns that amendment?
- Q4: The `qemu_args` escape hatch is appended verbatim today (inv. src/qemu/cmdline.rs:722) and e2e labs use it — any backend switch breaks labs that lean on QEMU-specific flags. What compatibility story (per-backend args? translation? deprecation) is intended?
- Q5: "Not a security boundary" (§1.2) means QEMU runs unsandboxed with no seccomp. Does the clean-room hypervisor keep that posture, or adopt sandboxing the current system never had — and is that a scope change?
- Q6: GPU parity: Venus needs `egl-headless`-style host GL scanout, a per-GPU alternate emulator binary on NVIDIA (QEMU 11.1 dma-buf patch), a Vulkan loader filter layer, and a supervisor keep-awake helper. Which of these are requirements for the new backend?
- Q7: The legacy tier (isa-serial 16550 on COM1 for NT4/9x/DOS/TempleOS guests, PRD §7.4/§5.3) implies exact legacy-device emulation. Is that in scope for a rust-vmm-style stack, which largely lacks those devices?
- Q8: S3 suspend (a distinct `suspended` power state, `SUSPEND`/`WAKEUP` events, `system_wakeup`, §7.2) is user-visible. Must the new backend advertise ACPI S3 identically?

Not checked: `docs/manual/` beyond greps, `e2e/labs/` breadth, `guest/` agent sources beyond CLAUDE.md's summary, and the graveyard repos themselves (referenced via CLAUDE.md only).
