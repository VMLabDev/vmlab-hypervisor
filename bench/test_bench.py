"""Unit tests for the bench suite's pure logic (stdlib unittest).

Run from the repo: `just bench-test` (or `cd bench && python3 -m unittest`).
The VM-booting path is verified by the real runs the recipes drive; these
tests pin the pieces that must not drift silently: statistics, the history
format and its regression decision, the QEMU argv (vmlab's, per notes/03),
the vmlab-hv machine description against the proto's field set, and the
noise-control policy.
"""

import argparse
import json
import os
import socket
import tempfile
import threading
import time
import unittest
from unittest import mock

import lib
from backends import hv_machine_description, qemu_vm_argv


class QmpTests(unittest.TestCase):
    def release(self, error=None, running=True):
        client, peer = socket.socketpair()
        client.settimeout(0.5)
        peer.settimeout(2)
        requests = []
        failures = []

        def serve():
            pending = ""
            decoder = json.JSONDecoder()
            try:
                peer.sendall(b'{"QMP":{"version":{},"capabilities":[]}}\r\n')
                for command in ["qmp_capabilities", "cont", "query-status"]:
                    while True:
                        try:
                            request, end = decoder.raw_decode(pending.lstrip())
                            pending = pending.lstrip()[end:]
                            break
                        except json.JSONDecodeError:
                            data = peer.recv(4096)
                            if not data:
                                return
                            pending += data.decode()
                    requests.append(request)
                    self.assertEqual(request["execute"], command)
                    reply = {"id": request.get("id")}
                    if error == command:
                        reply["error"] = {"class": "GenericError", "desc": "resume refused"}
                    else:
                        reply["return"] = {"running": running} if command == "query-status" else {}
                    event = {"event": "RESUME"}
                    peer.sendall((json.dumps(event) + "\r\n" + json.dumps(reply) + "\r\n").encode())
                    if error == command:
                        return
            except Exception as exc:
                failures.append(exc)
            finally:
                peer.close()

        thread = threading.Thread(target=serve)
        thread.start()
        connected = mock.Mock(wraps=client)
        connected.connect = mock.Mock()
        try:
            with mock.patch.object(lib.socket, "socket", return_value=connected):
                if error:
                    with self.assertRaisesRegex(lib.BenchError, "resume refused"):
                        lib.qmp_cont("unused", timeout=0.5)
                elif not running:
                    with self.assertRaisesRegex(lib.BenchError, "not running"):
                        lib.qmp_cont("unused", timeout=0.5)
                else:
                    lib.qmp_cont("unused", timeout=0.5)
        finally:
            client.close()
            thread.join(timeout=3)
        self.assertFalse(thread.is_alive())
        self.assertEqual(failures, [])
        return requests

    def test_release_handles_coalesced_events_and_matches_command_replies(self):
        requests = self.release()
        self.assertEqual([r["id"] for r in requests], [1, 2, 3])

    def test_release_reports_qmp_errors(self):
        self.release(error="cont")

    def test_release_checks_running_state(self):
        self.release(running=False)


class ResetMonitorTests(unittest.TestCase):
    def monitor(self, backend):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        path = os.path.join(directory.name, "control.sock")
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(path)
        listener.listen()
        self.addCleanup(listener.close)
        peers = []
        failures = []

        def serve():
            try:
                peer, _ = listener.accept()
                peers.append(peer)
                if backend == "qemu":
                    peer.sendall(b'{"QMP":{}}\n')
                request = b""
                while b"\n" not in request:
                    request += peer.recv(4096)
                message = json.loads(request)
                if backend == "qemu":
                    self.assertEqual(message["execute"], "qmp_capabilities")
                    peer.sendall(b'{"return":{},"id":1}\n')
                else:
                    self.assertEqual(message["type"], "hello")
                    peer.sendall((json.dumps({"type": "hello", "proto_version": lib.HV_PROTO_VERSION}) + "\n").encode())
            except Exception as error:
                failures.append(error)

        thread = threading.Thread(target=serve)
        thread.start()
        monitor = lib.ResetMonitor(path, backend, timeout=1)
        self.addCleanup(monitor.close)
        thread.join(timeout=2)
        self.assertFalse(thread.is_alive())
        self.assertEqual(failures, [])
        self.addCleanup(peers[0].close)
        return monitor, peers[0]

    def agent(self, monitor):
        agent, guest = socket.socketpair()
        self.addCleanup(guest.close)
        with mock.patch.object(lib.socket, "socket", return_value=agent):
            with mock.patch.object(agent.__class__, "connect"):
                client = lib.AgentClient("unused", reset_monitor=monitor)
        self.addCleanup(client.close)
        return client, guest

    def test_reset_interrupts_an_exec_whose_agent_socket_stays_open(self):
        for backend, event in (("qemu", {"event": "RESET"}),
                               ("vmlab-hv", {"type": "event", "event": "reset"})):
            with self.subTest(backend=backend):
                monitor, control = self.monitor(backend)
                client, _ = self.agent(monitor)
                control.sendall((json.dumps(event) + "\n").encode())
                with self.assertRaisesRegex(ConnectionError, "guest reset"):
                    client.exec(["powershell.exe", "-Command", "OOBE probe"], .2)

    def test_fragmented_and_coalesced_events_keep_the_reset_generation(self):
        monitor, control = self.monitor("vmlab-hv")
        control.sendall(b'{"type":"event","event":"resu')
        monitor.poll()
        self.assertEqual(monitor.generation, 0)
        control.sendall(b'med"}\n{"type":"event","event":"reset"}\n')
        monitor.poll()
        self.assertEqual(monitor.generation, 1)

    def reply(self, guest):
        pending = b""
        while len(pending) < 13:
            pending += guest.recv(4096)
        length = int.from_bytes(pending[4:8], "little")
        while len(pending) < 13 + length:
            pending += guest.recv(4096)
        cid = json.loads(pending[13:13 + length])["id"]
        guest.sendall(b"".join(lib.agent_frame(0, 0, json.dumps(message).encode()) for message in (
            {"event": "eof", "id": cid}, {"event": "exited", "id": cid, "code": 0})))

    def test_other_events_do_not_cancel_an_exec(self):
        monitor, control = self.monitor("vmlab-hv")
        client, guest = self.agent(monitor)
        control.sendall(b'{"type":"event","event":"resumed"}\n')
        thread = threading.Thread(target=self.reply, args=(guest,))
        thread.start()
        try:
            self.assertEqual(client.exec(["true"], 1)["code"], 0)
        finally:
            thread.join(timeout=2)
        self.assertFalse(thread.is_alive())

    def test_new_agent_session_uses_the_generation_after_reset(self):
        monitor, control = self.monitor("qemu")
        control.sendall(b'{"event":"RESET"}\n')
        client, guest = self.agent(monitor)
        self.assertEqual(client.reset_generation, 1)
        thread = threading.Thread(target=self.reply, args=(guest,))
        thread.start()
        try:
            self.assertEqual(client.exec(["true"], 1)["code"], 0)
        finally:
            thread.join(timeout=2)
        self.assertFalse(thread.is_alive())

    def test_control_disconnect_cancels_the_old_agent_session(self):
        monitor, control = self.monitor("vmlab-hv")
        client, _ = self.agent(monitor)
        control.close()
        with self.assertRaisesRegex(ConnectionError, "reset monitor socket closed"):
            client.exec(["true"], .2)

    def test_marker_observation_survives_reset_during_its_deletion(self):
        import run
        first, second = mock.Mock(), mock.Mock()
        ready = run.WindowsReady(mock.Mock(), argparse.Namespace(timeout_scale=1), "/unused", time.monotonic())
        calls = []

        def execute(agent, argv, timeout):
            calls.append(argv[-1])
            if len(calls) == 1:
                return {"code": 0}
            if len(calls) == 2:
                raise ConnectionError("guest reset")
            self.assertTrue(argv[-1].startswith("del /f /q"), argv)
            return {"code": 0}

        with mock.patch.object(ready, "connect", side_effect=[(first, {}), (second, {})]), \
                mock.patch.object(run, "win_exec", side_effect=execute), \
                mock.patch.object(run.time, "sleep"):
            agent, _ = ready.wait_marker()
        self.assertIs(agent, second)
        self.assertEqual(len(calls), 3)


class StatsTests(unittest.TestCase):
    def test_median_odd_and_even(self):
        self.assertEqual(lib.median_and_spread([3.0, 1.0, 2.0]), (2.0, 2.0))
        self.assertEqual(lib.median_and_spread([4.0, 1.0, 3.0, 2.0]), (2.5, 3.0))

    def test_median_single_sample(self):
        self.assertEqual(lib.median_and_spread([7.3]), (7.3, 0.0))

    def test_verdict_ok_within_spread(self):
        v, allowed = lib.regression_verdict(
            {"median": 28.0, "spread": 1.5}, {"median": 29.0, "spread": 0.5}
        )
        self.assertEqual(v, "ok")
        self.assertEqual(allowed, 1.5)

    def test_verdict_worse_beyond_spread(self):
        v, _ = lib.regression_verdict(
            {"median": 28.0, "spread": 1.5}, {"median": 35.0, "spread": 0.5}
        )
        self.assertEqual(v, "worse")

    def test_verdict_floor_for_tiny_spreads(self):
        # Zero measured spread must not make every move a regression.
        v, allowed = lib.regression_verdict(
            {"median": 2.0, "spread": 0.0}, {"median": 2.08, "spread": 0.0}
        )
        self.assertEqual(v, "ok")
        self.assertEqual(allowed, 0.1)

    def test_verdict_better(self):
        v, _ = lib.regression_verdict(
            {"median": 28.0, "spread": 0.5}, {"median": 20.0, "spread": 0.5}
        )
        self.assertEqual(v, "better")


class HistoryTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.patch = mock.patch.object(lib, "HISTORY_DIR", self.tmp.name)
        self.patch.start()
        self.addCleanup(self.patch.stop)

    def record(self, **kw):
        rec = {
            "recorded": "2026-10-31T00:00:00",
            "commit": "f" * 40,
            "dirty": False,
            "host": "testhost",
            "backend": "qemu",
            "arch": "aarch64",
            "guest": "debian-13",
            "vcpus": 1,
            "workload": "boot_to_agent_ready",
            "unit": "s",
            "repeats": 3,
            "samples": [1.0, 2.0, 3.0],
            "median": 2.0,
            "spread": 2.0,
            "qemu_baseline": None,
            "env": {},
        }
        rec.update(kw)
        return rec

    def test_append_and_read_round_trip(self):
        lib.append_history("aarch64", self.record())
        lib.append_history("aarch64", self.record(vcpus=4))
        records = lib.read_history("aarch64")
        self.assertEqual([r["vcpus"] for r in records], [1, 4])
        self.assertEqual(records[0]["workload"], "boot_to_agent_ready")

    def test_read_missing_file_is_empty(self):
        self.assertEqual(lib.read_history("riscv64"), [])

    def test_latest_baseline_picks_newest_qemu_match(self):
        lib.append_history("aarch64", self.record(median=30.0, recorded="t1"))
        lib.append_history(
            "aarch64", self.record(backend="vmlab-hv", median=60.0, recorded="t2")
        )
        lib.append_history(
            "aarch64", self.record(median=28.0, recorded="t3", qemu_baseline=None)
        )
        lib.append_history("aarch64", self.record(median=26.0, recorded="t4"))
        base = lib.latest_baseline(
            lib.read_history("aarch64"), "aarch64", "debian-13", 1, "boot_to_agent_ready"
        )
        self.assertEqual(base["median"], 26.0)
        self.assertEqual(base["recorded"], "t4")

    def test_latest_baseline_none_when_only_hv(self):
        lib.append_history(
            "aarch64", self.record(backend="vmlab-hv", median=60.0)
        )
        self.assertIsNone(
            lib.latest_baseline(
                lib.read_history("aarch64"), "aarch64", "debian-13", 1,
                "boot_to_agent_ready",
            )
        )


class CheckTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.patch = mock.patch.object(lib, "HISTORY_DIR", self.tmp.name)
        self.patch.start()
        self.addCleanup(self.patch.stop)
        self.args = argparse.Namespace(arch="aarch64", backend="all", guest=None)

    def write(self, medians, backend="qemu", vcpus=1, workload="xz_9_32M"):
        for m, s in medians:
            lib.append_history(
                "aarch64",
                {
                    "recorded": "t", "commit": "f" * 40, "dirty": False,
                    "host": "h", "backend": backend, "arch": "aarch64",
                    "guest": "debian-13", "vcpus": vcpus, "workload": workload,
                    "unit": "s", "repeats": 3, "samples": [], "median": m,
                    "spread": s, "qemu_baseline": None, "env": {},
                },
            )

    def test_single_entry_is_not_a_failure(self):
        import check

        self.write([(34.0, 1.0)])
        regressions, notes = check.compare("aarch64", self.args)
        self.assertEqual(regressions, [])
        self.assertTrue(any("nothing to compare" in n for n in notes))

    def test_regression_within_noise_passes(self):
        import check

        self.write([(34.0, 2.0), (35.0, 1.0)])
        regressions, _ = check.compare("aarch64", self.args)
        self.assertEqual(regressions, [])

    def test_regression_beyond_noise_fails(self):
        import check

        self.write([(34.0, 0.5), (48.0, 1.0)])
        regressions, _ = check.compare("aarch64", self.args)
        self.assertEqual(len(regressions), 1)
        self.assertIn("worse", regressions[0])

    def test_improvement_is_noted_not_failed(self):
        import check

        self.write([(34.0, 0.5), (20.0, 1.0)])
        regressions, notes = check.compare("aarch64", self.args)
        self.assertEqual(regressions, [])
        self.assertTrue(any("better" in n for n in notes))

    def test_configs_do_not_cross_compare(self):
        import check

        # A big qemu regression must not taint the vcpus=4 config.
        self.write([(34.0, 0.5), (48.0, 0.5)])
        self.write([(20.0, 0.5), (20.2, 0.5)], vcpus=4)
        regressions, _ = check.compare("aarch64", self.args)
        self.assertEqual(len(regressions), 1)

    def test_main_exit_code(self):
        import check

        self.write([(34.0, 0.5), (48.0, 0.5)])
        self.assertEqual(check.main(["--arch", "aarch64"]), 1)
        self.write([(47.0, 0.5)])
        # 48 -> 47 is an improvement beyond the 5 % floor: clean run.
        self.assertEqual(check.main(["--arch", "aarch64"]), 0)


class QemuArgvTests(unittest.TestCase):
    def test_argv_is_vmlab_shape(self):
        argv = qemu_vm_argv(
            "riscv64", 4,
            overlay="/run/ov.qcow2", code_fd="/fw/CODE.fd", vars_fd="/run/vars.fd",
            serial="/run/serial.log", agent_sock="/run/agent.sock",
            qmp_sock="/run/qmp.sock", vnc_sock="/run/vnc.sock",
        )
        self.assertEqual(
            argv,
            [
                "qemu-system-riscv64",
                "-name", "vmlab:bench/riscv64",
                "-machine", "virt,acpi=off",
                "-accel", "tcg",
                "-cpu", "max",
                "-smp", "4",
                "-m", "2048M",
                "-display", "none",
                "-vnc", "unix:/run/vnc.sock,share=ignore",
                "-qmp", "unix:/run/qmp.sock,server=on,wait=off",
                "-monitor", "none",
                "-serial", "file:/run/serial.log",
                "-drive", "if=pflash,format=raw,readonly=on,file=/fw/CODE.fd",
                "-drive", "if=pflash,format=raw,file=/run/vars.fd",
                "-chardev", "socket,id=vagent0,path=/run/agent.sock,server=on,wait=off",
                "-device", "virtio-serial-pci",
                "-device", "virtserialport,chardev=vagent0,name=vmlab.agent.0",
                "-blockdev",
                "driver=qcow2,node-name=disk0,file.driver=file,"
                "file.filename=/run/ov.qcow2",
                "-device", "virtio-blk-pci,drive=disk0",
                "-nic", "none",
                "-device", "qemu-xhci",
                "-device", "usb-tablet",
                "-S",
            ],
        )

    def test_aarch64_machine_is_acpi_virt(self):
        argv = qemu_vm_argv(
            "aarch64", 1,
            overlay="/o", code_fd="/c", vars_fd="/v", serial="/s",
            agent_sock="/a", qmp_sock="/q", vnc_sock="/n",
        )
        self.assertIn("virt", argv[argv.index("-machine") + 1])
        self.assertNotIn("virt,acpi=off", argv)


# The MachineDescription field set of vmlab-hv-proto as of this crate. The
# bench emits a subset; if the proto renames or removes a field the bench
MACHINE_FIELDS = {
    "machine", "cpu", "hyperv", "display", "tablet", "usb", "tpm", "console_pci",
    "ahci_pci", "name", "arch", "accel", "vcpus", "memory_mib", "boot",
    "disks", "nics", "serial_log", "ports", "shares", "audio", "rtc_base",
}
BOOT_FIRMWARE_FIELDS = {"kind", "firmware", "code", "vars", "secure_boot", "bootorder"}
DISK_FIELDS = {"pci", "bus", "port", "cdrom", "id", "path", "format",
               "read_only", "vmstate"}
PORT_FIELDS = {"name", "socket"}


class HvMachineTests(unittest.TestCase):
    def setUp(self):
        self.desc = hv_machine_description(
            "/run", "riscv64", 4, "/run/ov.qcow2", "/fw/CODE.fd", "/run/vars.fd"
        )

    def test_fields_fit_proto_schema(self):
        self.assertTrue(set(self.desc) <= MACHINE_FIELDS, set(self.desc) - MACHINE_FIELDS)
        self.assertTrue(
            set(self.desc["boot"]) <= BOOT_FIRMWARE_FIELDS,
            set(self.desc["boot"]) - BOOT_FIRMWARE_FIELDS,
        )
        for disk in self.desc["disks"]:
            self.assertTrue(set(disk) <= DISK_FIELDS, set(disk) - DISK_FIELDS)
        for port in self.desc["ports"]:
            self.assertTrue(set(port) <= PORT_FIELDS, set(port) - PORT_FIELDS)

    def test_matches_qemu_backend_guest(self):
        self.assertEqual(self.desc["arch"], "riscv64")
        self.assertEqual(self.desc["accel"], "tcg")
        self.assertEqual(self.desc["vcpus"], 4)
        self.assertEqual(self.desc["memory_mib"], 2048)
        self.assertEqual(self.desc["boot"]["firmware"], "ovmf")
        self.assertEqual(self.desc["ports"][0]["name"], "vmlab.agent.0")
        self.assertEqual(self.desc["disks"][0]["format"], "qcow2")
        self.assertFalse(self.desc["disks"][0]["read_only"])


class NoiseControlTests(unittest.TestCase):
    def test_g2g_samples_are_named_for_the_sender_and_receiver(self):
        import run
        agent_a, agent_b = object(), object()
        backend_a = argparse.Namespace(agent_socket=lambda: "a", session=argparse.Namespace(exit_code=None))
        backend_b = argparse.Namespace(agent_socket=lambda: "b", session=argparse.Namespace(exit_code=None))

        def direction(sender, _receiver, _ip, _port, _window, flows=1):
            return (100 if sender is agent_a else 200), {"bytes": 1}, {"bytes": 1}

        with tempfile.TemporaryDirectory() as tmp, \
                mock.patch.object(lib, "RUN_DIR", tmp), \
                mock.patch.object(lib, "require_vm_slots"), \
                mock.patch.object(run.netpeer, "Hub"), \
                mock.patch.object(run, "g2g_launch", side_effect=[backend_a, backend_b]), \
                mock.patch.object(lib, "wait_for_agent", side_effect=[(agent_a, {}), (agent_b, {})]), \
                mock.patch.object(run, "g2g_setup_guest"), \
                mock.patch.object(run, "g2g_direction", side_effect=direction), \
                mock.patch.object(run, "g2g_teardown"):
            result, _ = run.g2g_one_repeat(run.parse_args([]), "ubuntu-26.04", 2,
                "16-19", "20-23", "test", 1)
            self.assertEqual(result["samples"]["net_g2g_a2b_tcp_mbps"], 100)
            self.assertEqual(result["samples"]["net_g2g_b2a_tcp_mbps"], 200)

    def test_capacity_failure_does_not_start_a_peer(self):
        import run
        with mock.patch.object(lib, "require_vm_slots", side_effect=lib.BenchError("full")), \
                mock.patch.object(run.netpeer, "Peer") as peer:
            with self.assertRaisesRegex(lib.BenchError, "full"):
                run.x86_one_repeat(None, "ubuntu-26.04", 2, "16-23", "test", 1)
            peer.assert_not_called()

    def test_launch_failure_preserves_error_and_reaps_peer(self):
        import run
        backend = argparse.Namespace(
            launch=mock.Mock(side_effect=lib.BenchError("launch blocked")),
            quit=mock.Mock())
        with tempfile.TemporaryDirectory() as tmp, \
                mock.patch.object(lib, "RUN_DIR", tmp), \
                mock.patch.object(lib, "require_vm_slots"), \
                mock.patch.object(lib, "prepare_run_dir", return_value=("overlay", "code", "vars")), \
                mock.patch.object(run.netpeer, "Peer") as peer, \
                mock.patch.object(run, "X86HvBackend", return_value=backend):
            args = run.parse_args(["--backend", "vmlab-hv"])
            with self.assertRaisesRegex(lib.BenchError, "launch blocked"):
                run.x86_one_repeat(args, "ubuntu-26.04", 2, "16-23", "test", 1)
            peer.return_value.kill.assert_called_once()

    def test_g2g_teardown_handles_a_partial_pair(self):
        import run
        backend = argparse.Namespace(name="vmlab-hv", quit=mock.Mock(),
            session=argparse.Namespace(kill=mock.Mock()))
        run.g2g_teardown([backend, None], [None, None])
        backend.session.kill.assert_called_once()

    def test_exec_does_not_reuse_a_channel_before_guest_cleanup(self):
        from collections import deque
        seen = set()
        replies = deque()

        def send(frame):
            message = json.loads(frame[13:])
            cid = message["id"]
            if cid in seen:
                replies.append((0, 0, json.dumps({"event": "error", "id": cid,
                    "msg": "channel id already in use"}).encode()))
            else:
                seen.add(cid)
                replies.extend((0, 0, json.dumps(message).encode()) for message in [
                    {"event": "eof", "id": cid},
                    {"event": "exited", "id": cid, "code": 0}])

        with mock.patch.object(socket, "socket") as factory:
            factory.return_value.sendall.side_effect = send
            for _ in range(2):
                client = lib.AgentClient("unused")
                client._recv_frame = lambda _timeout: replies.popleft()
                for _ in range(3):
                    self.assertEqual(client.exec(["true"], 1)["code"], 0)
                client.close()
        self.assertEqual(len(seen), 6)

    def test_sampler_resolves_elf_virtual_addresses_for_pie_and_exec(self):
        import subprocess
        with tempfile.TemporaryDirectory() as tmp:
            binary = os.path.join(tmp, "ipsample")
            source = os.path.join(lib.BENCH_DIR, "ipsample.c")
            for flags in (["-fPIE", "-pie"], ["-fno-pie", "-no-pie"]):
                subprocess.run(["cc", "-O2", *flags, "-o", binary, source], check=True)
                subprocess.run([binary, "--self-test"], check=True, capture_output=True)

    def test_host_vm_count_includes_other_worktrees_and_excludes_android(self):
        with tempfile.TemporaryDirectory() as proc:
            for pid, argv in {
                1: ["/other/target/release/vmlab-hv", "--machine", "machine.json"],
                2: ["qemu-system-x86_64", "-name", "other-project"],
                3: ["/opt/android/qemu-system-x86_64", "-avd", "thevault"],
                4: ["python3", "run.py", "--backend", "vmlab-hv"],
                5: [],
                6: ["/worktree/target/debug/vmlab-hv-test-plain", "--machine", "test.json"],
            }.items():
                os.mkdir(os.path.join(proc, str(pid)))
                with open(os.path.join(proc, str(pid), "cmdline"), "wb") as f:
                    f.write(b"\0".join(os.fsencode(arg) for arg in argv))
            self.assertEqual(lib.project_vm_pids(proc), [1, 2, 6])

    def test_vm_cap_checked_before_launch(self):
        with mock.patch.dict(os.environ, {"VMLAB_HV_TEST_VM_LIMIT": "4"}):
            with mock.patch.object(lib, "project_vm_pids", return_value=[1, 2, 3]):
                lib.require_vm_slots()
                with self.assertRaisesRegex(lib.BenchError, "need 2 VM slots"):
                    lib.require_vm_slots(2)
            with mock.patch.object(lib, "project_vm_pids", return_value=[1, 2, 3, 4]):
                with self.assertRaisesRegex(lib.BenchError, "running project VM PIDs"):
                    lib.require_vm_slots()

    def test_pin_windows(self):
        with mock.patch.object(os, "cpu_count", return_value=32):
            self.assertEqual(lib.pin_cpus(1), "16-19")
            self.assertEqual(lib.pin_cpus(4), "16-23")

    def test_pin_override_wins(self):
        self.assertEqual(lib.pin_cpus(4, override="2,6,8"), "2,6,8")

    def test_version_key_prefers_newer_template(self):
        with tempfile.TemporaryDirectory() as tmp:
            with mock.patch.object(lib, "TEMPLATE_ROOT", tmp):
                root = os.path.join(tmp, "riscv64", "debian-13")
                for v in ("13.20260601.9", "13.20261001.0", "13.20260601.10"):
                    os.makedirs(os.path.join(root, v))
                    open(os.path.join(root, v, "disk.qcow2"), "w").close()
                self.assertEqual(
                    lib.latest_template("riscv64", "debian-13"), "13.20261001.0"
                )

    def test_missing_template_is_an_error(self):
        with mock.patch.object(
            lib, "TEMPLATE_ROOT", "/nonexistent-vmlab-templates"
        ):
            with self.assertRaises(lib.BenchError):
                lib.latest_template("aarch64", "debian-13")


class ScriptSmokeTests(unittest.TestCase):
    def test_run_help(self):
        import run
        import contextlib
        import io

        with contextlib.redirect_stdout(io.StringIO()):
            args = run.parse_args(["--backend", "vmlab-hv", "--arch", "riscv64"])
        self.assertEqual(args.backend, "vmlab-hv")
        self.assertEqual(run.resolve_arches("all"), ["aarch64", "riscv64"])
        self.assertEqual(run.resolve_arches("riscv64,aarch64"), ["riscv64", "aarch64"])
        with self.assertRaises(lib.BenchError):
            run.resolve_arches("i686")

    def test_agent_frame_layout(self):
        f = lib.agent_frame(0, 7, b"xyz")
        self.assertEqual(f[:4], b"VMLB")
        length, kind, channel = lib.struct.unpack("<IBI", f[4:13])
        self.assertEqual((length, kind, channel), (3, 0, 7))



# ---------------------------------------------------------------------------
# x86_64 KVM suite (phase 2, HV-D28)
# ---------------------------------------------------------------------------

NIC_FIELDS = {"rom", "pci", "mac", "mtu", "attachment"}
DISPLAY_FIELDS = {"device", "gpu", "render_node", "pci", "rom", "socket", "vnc_socket"}


class X86WorkloadTests(unittest.TestCase):
    def test_workload_dispatch(self):
        self.assertEqual(lib.workloads("riscv64"), lib.TCG_WORKLOADS)
        self.assertEqual(
            lib.workloads("x86_64", "ubuntu-26.04"), lib.X86_UBUNTU_WORKLOADS
        )
        self.assertEqual(lib.workloads("x86_64", "windows-10"), ("boot_to_agent_ready",))
        self.assertEqual(lib.workloads("x86_64", "windows-server-2025"), ("boot_to_agent_ready",))
        self.assertEqual(lib.workloads("x86_64", suite="g2g"), lib.X86_G2G_WORKLOADS)
        self.assertEqual(lib.workloads("x86_64", suite="g2g1500"), lib.X86_G2G1500_WORKLOADS)
        self.assertEqual(lib.workloads("x86_64", suite="g2g-multiflow"), lib.X86_G2G_MULTIFLOW_WORKLOADS)

    def test_new_network_suites_preserve_jumbo_samples_and_bars(self):
        self.assertEqual(lib.G2G_SUITES["g2g"]["mtu"], 65520)
        self.assertEqual(lib.G2G_SUITES["g2g"]["flows"], 1)
        self.assertFalse(lib.G2G_SUITES["g2g"]["vnet"])
        for suite, flows in (("g2g1500", 1), ("g2g-multiflow", 4)):
            config = lib.G2G_SUITES[suite]
            self.assertEqual(config["mtu"], 1500)
            self.assertEqual(config["flows"], flows)
            self.assertTrue(config["vnet"])
            for name in config["workloads"]:
                self.assertNotIn(name, lib.X86_G2G_WORKLOADS)
                self.assertNotIn(name, lib.D28_BAR)
                self.assertEqual(lib.UNIT[name], "Mbit/s")
                self.assertEqual(lib.DIRECTION[name], "higher")

    def test_pair_suites_use_distinct_short_run_names(self):
        names = [lib.run_dir_name("vmlab-hv", "x86_64", "ubuntu-26.04", 2, 1, suite)
                 for suite in ("full", *lib.G2G_SUITES)]
        self.assertEqual(len(set(names)), 4)
        self.assertTrue(all(len(name) < 30 for name in names))

    def test_pair_launch_selects_mtu_and_hypervisor_framing(self):
        import run
        for backend in ("qemu", "vmlab-hv"):
            for suite, config in lib.G2G_SUITES.items():
                with self.subTest(backend=backend, suite=suite), \
                        mock.patch.object(lib, "prepare_run_dir", return_value=("ov", "code", "vars")), \
                        mock.patch.object(lib, "latest_template", return_value="version"), \
                        mock.patch.object(run, "X86QemuBackend") as qemu, \
                        mock.patch.object(run, "X86HvBackend") as hv:
                    args = run.parse_args(["--backend", backend, "--suite", suite])
                    run.g2g_launch(args, "a", "ubuntu-26.04", 2, "16-19", "hub", "mac", "run")
                    factory = qemu if backend == "qemu" else hv
                    self.assertEqual(factory.call_args.args[9], config["mtu"])
                    if backend == "vmlab-hv":
                        self.assertEqual(factory.call_args.kwargs, {"vnet": config["vnet"]})
                    else:
                        self.assertEqual(factory.call_args.kwargs, {})

    def test_pair_guest_setup_uses_selected_mtu(self):
        import run
        with mock.patch.object(run, "guest_nic", return_value="eth0"), \
                mock.patch.object(run, "must_sh") as execute, \
                mock.patch.object(run, "copy_in_tools"):
            run.g2g_setup_guest(object(), "192.168.60.2", mtu=1500)
            self.assertIn("mtu 1500", execute.call_args.args[1])

    def test_multiflow_direction_checks_each_flow(self):
        import run
        sent = {"bytes": 16, "flows": 4, "per_flow_bytes": [4, 4, 4, 4]}
        received = {"bytes": 16, "flows": 4, "per_flow_bytes": [4, 4, 4, 4]}
        for counts in ([4, 4, 4, 4], [8, 4, 4, 0]):
            received["per_flow_bytes"] = counts
            with mock.patch.object(run, "must_sh", side_effect=["", json.dumps(sent), json.dumps(received)]) as execute, \
                    mock.patch.object(run.time, "sleep"):
                if counts[-1]:
                    run.g2g_direction(object(), object(), "192.168.60.2", 5903, 12, flows=4)
                    self.assertIn("tcp-multisink 5903 4", execute.call_args_list[0].args[1])
                    self.assertIn("tcp-multiblast 192.168.60.2 5903 12 4", execute.call_args_list[1].args[1])
                else:
                    with self.assertRaisesRegex(lib.BenchError, "missing concurrent flows"):
                        run.g2g_direction(object(), object(), "192.168.60.2", 5903, 12, flows=4)

    def test_every_workload_has_unit_and_direction(self):
        for name in set(lib.UNIT) | set(lib.DIRECTION):
            self.assertIn(name, lib.UNIT)
            self.assertIn(name, lib.DIRECTION)
        for name, direction in lib.DIRECTION.items():
            self.assertIn(direction, ("lower", "higher"))
        # Rates regress downward; seconds and CPU percent regress upward.
        self.assertEqual(lib.DIRECTION["net_tx_udp_mbps"], "higher")
        self.assertEqual(lib.DIRECTION["blk_rand_read_4k_iops"], "higher")
        self.assertEqual(lib.DIRECTION["boot_to_agent_ready"], "lower")
        self.assertEqual(lib.DIRECTION["idle_cpu_pct"], "lower")

    def test_d28_bars_cover_the_x86_suite(self):
        every = set(lib.X86_UBUNTU_WORKLOADS) | set(lib.X86_WINDOWS_WORKLOADS) | set(lib.X86_G2G_WORKLOADS)
        self.assertEqual(set(lib.D28_BAR), every)
        self.assertEqual(lib.D28_BAR["boot_to_agent_ready"], ("<=", 1.00))
        for name in lib.X86_UBUNTU_WORKLOADS + lib.X86_G2G_WORKLOADS:
            if name.startswith(("blk_", "net_")):
                self.assertEqual(lib.D28_BAR[name], (">=", 0.80), name)
        self.assertIsNone(lib.D28_BAR["idle_cpu_pct"])

    def test_rate_verdict_regresses_downward(self):
        v, _ = lib.regression_verdict(
            {"median": 900.0, "spread": 10.0, "direction": "higher"},
            {"median": 700.0, "spread": 10.0, "direction": "higher"},
        )
        self.assertEqual(v, "worse")
        v, _ = lib.regression_verdict(
            {"median": 900.0, "spread": 10.0, "direction": "higher"},
            {"median": 1200.0, "spread": 10.0, "direction": "higher"},
        )
        self.assertEqual(v, "better")
        v, _ = lib.regression_verdict(
            {"median": 900.0, "spread": 10.0, "direction": "higher"},
            {"median": 880.0, "spread": 10.0, "direction": "higher"},
        )
        self.assertEqual(v, "ok")

    def test_rate_median_rounding(self):
        self.assertEqual(lib.median_and_spread([1000.0, 1002.0, 1004.0], "iops"), (1002.0, 4.0))
        self.assertEqual(lib.median_and_spread([12.345, 12.347], "Mbit/s"), (12.346, 0.002))


class X86TemplateTests(unittest.TestCase):
    def test_windows_picks_the_standard_variant(self):
        with tempfile.TemporaryDirectory() as tmp:
            with mock.patch.object(lib, "TEMPLATE_ROOT", tmp):
                base = os.path.join(tmp, "x86_64", "windows-10", "19045.1")
                os.makedirs(os.path.join(base, "variants", "standard"))
                open(os.path.join(base, "disk.qcow2"), "w").close()
                open(os.path.join(base, "variants", "standard", "disk.qcow2"), "w").close()
                self.assertTrue(
                    lib.template_disk("x86_64", "windows-10", "19045.1").endswith(
                        os.path.join("variants", "standard", "disk.qcow2")
                    )
                )

    def test_ubuntu_uses_the_plain_disk(self):
        with tempfile.TemporaryDirectory() as tmp:
            with mock.patch.object(lib, "TEMPLATE_ROOT", tmp):
                base = os.path.join(tmp, "x86_64", "ubuntu-26.04", "26.04.1")
                os.makedirs(base)
                open(os.path.join(base, "disk.qcow2"), "w").close()
                self.assertEqual(
                    lib.template_disk("x86_64", "ubuntu-26.04", "26.04.1"),
                    os.path.join(base, "disk.qcow2"),
                )

    def test_missing_disk_is_an_error(self):
        with mock.patch.object(lib, "TEMPLATE_ROOT", "/nonexistent"):
            with self.assertRaises(lib.BenchError):
                lib.template_disk("x86_64", "ubuntu-26.04", "26.04.1")


class X86QemuArgvTests(unittest.TestCase):
    def base_argv(self, guest="ubuntu-26.04", mtu=65520):
        from backends import qemu_x86_vm_argv

        return qemu_x86_vm_argv(
            guest, 2, "/run/ov.qcow2", "/fw/CODE.fd", "/run/vars.fd",
            "/run/serial.log", "/run/agent.sock", "/run/qmp.sock", "/run/vnc.sock",
            "/run/switch.sock", "52:54:00:12:34:56", mtu,
        )

    def pairs(self, argv):
        return {argv[i]: argv[i + 1] for i in range(1, len(argv) - 1, 2)}

    def devices(self, argv):
        return [argv[i + 1] for i, a in enumerate(argv) if a == "-device"]

    def test_ubuntu_is_vmlab_shape(self):
        argv = self.base_argv()
        self.assertEqual(argv[0], "qemu-system-x86_64")
        p = self.pairs(argv)
        self.assertEqual(p["-name"], "vmlab:bench/ubuntu-26.04")
        self.assertEqual(p["-machine"], "q35")
        self.assertEqual(p["-accel"], "kvm")
        self.assertEqual(p["-cpu"], "host,vmx=off,svm=off")
        self.assertEqual(p["-smp"], "2")
        self.assertEqual(p["-m"], "4096M")
        self.assertEqual(p["-display"], "none")
        self.assertIn("share=ignore", p["-vnc"])
        self.assertIn("server=on,wait=off", p["-qmp"])
        self.assertEqual(p["-monitor"], "none")
        self.assertEqual(p["-serial"], "file:/run/serial.log")
        self.assertNotIn("-drive", p)  # SeaBIOS: no pflash
        self.assertIn("name=vmlab.agent.0", " ".join(argv))
        self.assertEqual(
            p["-blockdev"],
            "driver=qcow2,node-name=disk0,file.driver=file,file.filename=/run/ov.qcow2",
        )
        devices = self.devices(argv)
        # vmlab's device order: console port, disk, NIC, display, tablet.
        self.assertEqual(devices[0], "virtio-serial-pci")
        self.assertIn("virtserialport,chardev=vagent0,name=vmlab.agent.0", devices)
        self.assertIn("virtio-blk-pci,drive=disk0,bootindex=50", devices)
        self.assertIn("virtio-vga", devices)
        self.assertIn("usb-tablet", devices)
        self.assertIn("-netdev", argv)
        self.assertEqual(
            p["-netdev"],
            "stream,id=net0,server=off,addr.type=unix,addr.path=/run/switch.sock",
        )
        self.assertIn("virtio-net-pci,netdev=net0,mac=52:54:00:12:34:56,host_mtu=65520", devices)
        self.assertIn("-usb", argv)
        self.assertEqual(argv[-1], "-S")

    def test_mtu_deviation_is_optional_and_symmetric(self):
        argv = self.base_argv(mtu=None)
        self.assertNotIn("host_mtu", " ".join(argv))

    def test_windows_is_vmlab_shape(self):
        from backends import qemu_x86_vm_argv

        argv = qemu_x86_vm_argv(
            "windows-10", 4, "/run/ov.qcow2", "/fw/OVMF_CODE_4M.fd", "/run/vars.fd",
            "/run/serial.log", "/run/agent.sock", "/run/qmp.sock", "/run/vnc.sock",
            "/run/switch.sock", "52:54:00:12:34:56", None,
        )
        p = self.pairs(argv)
        self.assertEqual(p["-m"], "8192M")
        self.assertEqual(p["-smp"], "4")
        self.assertIn("hv-relaxed", p["-cpu"])
        self.assertIn("hv-tlbflush", p["-cpu"])
        self.assertNotIn("bootindex", " ".join(self.devices(argv)))  # OVMF boots alone
        drives = [argv[i + 1] for i, a in enumerate(argv) if a == "-drive"]
        self.assertEqual(len(drives), 2)
        self.assertIn("readonly=on,file=/fw/OVMF_CODE_4M.fd", drives[0])
        self.assertIn("file=/run/vars.fd", drives[1])
        with mock.patch.object(lib, "host_apic_virtualization", return_value=True):
            argv2 = qemu_x86_vm_argv(
                "windows-10", 4, "/run/ov.qcow2", "/fw/C", "/run/v", "/s", "/a",
                "/q", "/v", "/sw", "52:54:00:12:34:56", None,
            )
            p2 = self.pairs(argv2)
            self.assertIn("hv-avic", p2["-cpu"])
            self.assertIn("kvm-pit.lost_tick_policy=discard", argv2)

    def test_windows_bootindex_absent(self):
        from backends import qemu_x86_vm_argv

        argv = qemu_x86_vm_argv(
            "windows-10", 4, "/run/ov.qcow2", "/c", "/v", "/s", "/a", "/q", "/vc",
            "/sw", "52:54:00:12:34:56", None,
        )
        self.assertNotIn("bootindex", " ".join(argv))

    def test_no_network_is_vmlab_shape(self):
        from backends import hv_x86_machine_description, qemu_x86_vm_argv

        argv = qemu_x86_vm_argv(
            "windows-10", 4, "/run/ov.qcow2", "/c", "/v", "/s", "/a", "/q", "/vc",
            None, "52:54:00:12:34:56", None,
        )
        self.assertIn("-nic", argv)
        self.assertEqual(argv[argv.index("-nic") + 1], "none")
        self.assertNotIn("-netdev", argv)
        desc = hv_x86_machine_description(
            "/run", "windows-10", 4, "/run/ov.qcow2", "/c", "/v", None,
            "52:54:00:12:34:56", None, None,
        )
        self.assertEqual(desc["nics"], [])


class X86HvMachineTests(unittest.TestCase):
    def make(self, guest, mtu=65520, *, vnet=False):
        from backends import hv_x86_machine_description

        return hv_x86_machine_description(
            "/run", guest, lib.X86_GUESTS[guest]["vcpus"], "/run/ov.qcow2",
            "/fw/code.fd", "/run/vars.fd", "/run/switch.sock",
            "52:54:00:12:34:56", mtu, "/run/ipxe.rom", vnet=vnet,
        )

    def test_fields_fit_proto_schema(self):
        for guest in ("ubuntu-26.04", "windows-10"):
            desc = self.make(guest)
            self.assertEqual(desc["machine"], "q35")
            self.assertEqual(desc["arch"], "x86_64")
            self.assertEqual(desc["accel"], "kvm")
            self.assertTrue(set(desc) <= MACHINE_FIELDS, set(desc) - MACHINE_FIELDS)
            self.assertTrue(set(desc["boot"]) <= BOOT_FIRMWARE_FIELDS)
            for disk in desc["disks"]:
                self.assertTrue(set(disk) <= DISK_FIELDS)
            for nic in desc["nics"]:
                self.assertTrue(set(nic) <= NIC_FIELDS, set(nic) - NIC_FIELDS)
            self.assertTrue(set(desc["display"]) <= DISPLAY_FIELDS)

    def test_pci_layout_matches_qemu_auto_assignment(self):
        # What QEMU assigns for the same argv (docs/hwref, query-pci probe).
        desc = self.make("ubuntu-26.04")
        self.assertEqual(desc["console_pci"], {"bus": 0, "device": 1, "function": 0})
        self.assertEqual(desc["disks"][0]["pci"], {"bus": 0, "device": 2, "function": 0})
        self.assertEqual(desc["nics"][0]["pci"], {"bus": 0, "device": 3, "function": 0})
        self.assertEqual(desc["display"]["pci"], {"bus": 0, "device": 4, "function": 0})
        self.assertEqual(desc["tablet"], {"bus": 0, "device": 5, "function": 0})
        self.assertEqual(desc["ahci_pci"], {"bus": 0, "device": 31, "function": 2})

    def test_seabios_bootorder_is_qemus(self):
        desc = self.make("ubuntu-26.04")
        self.assertEqual(desc["boot"]["firmware"], "seabios")
        self.assertEqual(desc["boot"]["bootorder"], ["/pci@i0cf8/scsi@2/disk@0,0"])
        self.assertNotIn("vars", desc["boot"])

    def test_ovmf_pair_and_hyperv(self):
        desc = self.make("windows-10")
        self.assertEqual(desc["boot"]["firmware"], "ovmf")
        self.assertEqual(desc["boot"]["code"], "/fw/code.fd")
        self.assertEqual(desc["boot"]["vars"], "/run/vars.fd")
        self.assertFalse(desc["boot"]["secure_boot"])
        self.assertEqual(desc["vcpus"], 4)
        self.assertEqual(desc["memory_mib"], 8192)
        self.assertIn("time", desc["hyperv"])
        self.assertIn("tlbflush", desc["hyperv"])
        self.assertNotIn("bootorder", desc["boot"])

    def test_nic_stream_attachment_and_mtu(self):
        desc = self.make("ubuntu-26.04")
        nic = desc["nics"][0]
        self.assertEqual(nic["attachment"], {"kind": "stream", "socket": "/run/switch.sock"})
        self.assertEqual(nic["mtu"], 65520)
        self.assertEqual(nic["rom"], "/run/ipxe.rom")
        self.assertEqual(nic["mac"], "52:54:00:12:34:56")
        bare = self.make("ubuntu-26.04", mtu=None)
        self.assertNotIn("mtu", bare["nics"][0])

    def test_disk_is_a_writable_qcow2_vmstate_disk(self):
        desc = self.make("ubuntu-26.04")
        disk = desc["disks"][0]
        self.assertEqual(disk["id"], "disk0")
        self.assertEqual(disk["format"], "qcow2")
        self.assertFalse(disk["read_only"])
        self.assertTrue(disk["vmstate"])

    def test_vnet_attachment_has_one_queue_pair_per_vcpu(self):
        desc = self.make("ubuntu-26.04", mtu=1500, vnet=True)
        self.assertEqual(desc["nics"][0]["attachment"], {
            "kind": "stream_vnet", "socket": "/run/switch.sock", "queue_pairs": desc["vcpus"],
        })
        self.assertEqual(desc["nics"][0]["mtu"], 1500)


class GuestToolTests(unittest.TestCase):
    """nettool runs on the host here: its modes are host-agnostic."""

    def test_udp_blast_and_sink_over_loopback(self):
        import run as run_mod  # noqa: F401  (compile check)
        import socket
        import subprocess
        import sys

        tool = os.path.join(os.path.dirname(os.path.abspath(__file__)), "nettool.py")
        sink = subprocess.Popen(
            [sys.executable, tool, "udp-sink", "59019", "2"],
            stdout=subprocess.PIPE, text=True,
        )
        time.sleep(0.5)
        blast = subprocess.run(
            [sys.executable, tool, "udp-blast", "127.0.0.1", "59019", "1400", "1"],
            capture_output=True, text=True, check=True,
        )
        out = json.loads(blast.stdout.strip().splitlines()[-1])
        self.assertGreater(out["bytes"], 0)
        sink_out = json.loads(sink.communicate(timeout=10)[0].strip().splitlines()[-1])
        self.assertGreater(sink_out["bytes"], 0)

    def test_tcp_blast_and_sink_over_loopback(self):
        import subprocess
        import sys

        tool = os.path.join(os.path.dirname(os.path.abspath(__file__)), "nettool.py")
        sink = subprocess.Popen(
            [sys.executable, tool, "tcp-sink", "59020"],
            stdout=subprocess.PIPE, text=True,
        )
        import time as _time

        _time.sleep(0.5)
        blast = subprocess.run(
            [sys.executable, tool, "tcp-blast", "127.0.0.1", "59020", "1"],
            capture_output=True, text=True, check=True,
        )
        out = json.loads(blast.stdout.strip().splitlines()[-1])
        self.assertGreater(out["bytes"], 0)
        sink_out = json.loads(sink.communicate(timeout=10)[0].strip().splitlines()[-1])
        self.assertGreater(sink_out["bytes"], 0)

    def test_four_tcp_flows_transfer_concurrently_over_loopback(self):
        import subprocess
        import sys

        tool = os.path.join(os.path.dirname(os.path.abspath(__file__)), "nettool.py")
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            port = str(reservation.getsockname()[1])
        sink = subprocess.Popen(
            [sys.executable, tool, "tcp-multisink", port, "4"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        try:
            time.sleep(0.5)
            blast = subprocess.run(
                [sys.executable, tool, "tcp-multiblast", "127.0.0.1", port, "0.3", "4"],
                capture_output=True, text=True, check=True, timeout=10,
            )
            sent = json.loads(blast.stdout.strip())
            stdout, stderr = sink.communicate(timeout=10)
            self.assertEqual(sink.returncode, 0, stderr)
            received = json.loads(stdout.strip())
            for result in (sent, received):
                self.assertEqual(result["flows"], 4)
                self.assertEqual(len(result["per_flow_bytes"]), 4)
                self.assertTrue(all(count > 0 for count in result["per_flow_bytes"]))
                self.assertEqual(sum(result["per_flow_bytes"]), result["bytes"])
            self.assertEqual(sent["bytes"], received["bytes"])
            self.assertEqual(sorted(sent["per_flow_bytes"]), sorted(received["per_flow_bytes"]))
        finally:
            if sink.poll() is None:
                sink.kill()
            sink.communicate(timeout=5)

    def test_multiflow_arguments_are_bounded(self):
        import nettool
        for flows in (0, -1, 257):
            with self.assertRaises(ValueError):
                nettool.tcp_multiblast("127.0.0.1", 1, 1, flows)
            with self.assertRaises(ValueError):
                nettool.tcp_multisink(1, flows)
        for seconds in (0, -1, float("inf"), float("nan")):
            with self.assertRaises(ValueError):
                nettool.tcp_multiblast("127.0.0.1", 1, seconds, 4)

    def test_multiflow_sender_supplies_every_connection_before_drain(self):
        import concurrent.futures
        import subprocess
        import sys

        tool = os.path.join(os.path.dirname(os.path.abspath(__file__)), "nettool.py")
        sockets = []
        sender = None

        def drain(conn):
            count = 1
            while data := conn.recv(1024 * 1024):
                count += len(data)
            return count

        try:
            with socket.socket() as listener:
                listener.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 65536)
                listener.bind(("127.0.0.1", 0))
                listener.listen(4)
                listener.settimeout(3)
                port = str(listener.getsockname()[1])
                sender = subprocess.Popen(
                    [sys.executable, tool, "tcp-multiblast", "127.0.0.1", port, "0.2", "4"],
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                )
                for _ in range(4):
                    conn, _ = listener.accept()
                    conn.settimeout(3)
                    sockets.append(conn)
                # A serial sender blocks filling the first connection while
                # the receiver waits for the other three to carry data.
                for conn in sockets:
                    self.assertEqual(conn.recv(1), b"t")
                with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
                    received = list(pool.map(drain, sockets))
                stdout, stderr = sender.communicate(timeout=5)
                self.assertEqual(sender.returncode, 0, stderr)
                report = json.loads(stdout)
                self.assertEqual(sum(received), report["bytes"])
                self.assertEqual(sorted(received), sorted(report["per_flow_bytes"]))
        finally:
            for conn in sockets:
                conn.close()
            if sender is not None:
                if sender.poll() is None:
                    sender.kill()
                sender.communicate(timeout=5)


class NetPeerTests(unittest.TestCase):
    """The framing peer, exercised without a VM."""

    @classmethod
    def setUpClass(cls):
        import netpeer

        netpeer.compile_tools()
        cls.netpeer = netpeer

    def test_peer_arps_counts_and_blasts(self):
        import struct

        netpeer = self.netpeer
        with tempfile.TemporaryDirectory() as tmp:
            peer = netpeer.Peer(tmp, "192.168.50.2", "52:54:00:12:34:56", 5901)
            self.addCleanup(peer.kill)
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.connect(peer.switch_sock)

            def send_frame(b):
                s.sendall(struct.pack(">I", len(b)) + b)

            def recv_frame():
                hdr = b""
                while len(hdr) < 4:
                    hdr += s.recv(4 - len(hdr))
                n = struct.unpack(">I", hdr)[0]
                b = b""
                while len(b) < n:
                    b += s.recv(n - len(b))
                return b

            arp = (
                b"\xff" * 6 + b"\x52\x54\x00\x12\x34\x56" + b"\x08\x06"
                + b"\x00\x01\x08\x00\x06\x04\x00\x01" + b"\x52\x54\x00\x12\x34\x56"
                + bytes([192, 168, 50, 2]) + b"\x00" * 6 + bytes([192, 168, 50, 1])
            )
            send_frame(arp)
            reply = recv_frame()
            self.assertEqual(reply[6:12], bytes.fromhex("525400aa0001"))

            ip = bytearray(20)
            ip[0] = 0x45
            ip[2:4] = struct.pack(">H", 28 + 100)
            ip[8] = 64
            ip[9] = 17
            ip[12:16] = bytes([192, 168, 50, 2])
            ip[16:20] = bytes([192, 168, 50, 1])
            frame = (
                bytes.fromhex("525400aa0001") + b"\x52\x54\x00\x12\x34\x56"
                + b"\x08\x00" + bytes(ip)
                + struct.pack(">HHHH", 5, 5901, 8 + 100, 0) + b"u" * 100
            )
            for _ in range(5):
                send_frame(frame)
            time.sleep(0.3)
            report = peer.report()
            self.assertEqual(report["rx_frames"], 5)
            # rx_bytes counts ethernet-level bytes: 5 × (14+20+8+100).
            self.assertEqual(report["rx_bytes"], 5 * 142)
            self.assertEqual(report["rx_arp"], 1)

            peer.blast(0.5, 1200)
            got = 0
            s.settimeout(2)
            deadline = time.monotonic() + 1.5
            try:
                while time.monotonic() < deadline:
                    recv_frame()
                    got += 1
            except (socket.timeout, OSError):
                pass
            self.assertGreater(got, 10)

    def test_hub_pumps_both_ways(self):
        import struct

        netpeer = self.netpeer
        with tempfile.TemporaryDirectory() as tmp:
            hub = netpeer.Hub(tmp)
            self.addCleanup(hub.kill)
            a = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            a.connect(hub.switch_sock)
            b = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            b.connect(hub.switch_sock)
            time.sleep(0.2)
            a.sendall(struct.pack(">I", 3) + b"aaa")
            self.assertEqual(b.recv(64), struct.pack(">I", 3) + b"aaa")
            b.sendall(struct.pack(">I", 2) + b"bb")
            self.assertEqual(a.recv(64), struct.pack(">I", 2) + b"bb")

    def test_hub_preserves_large_vnet_records_and_queue_ids(self):
        import struct

        with tempfile.TemporaryDirectory() as tmp:
            hub = self.netpeer.Hub(tmp)
            self.addCleanup(hub.kill)
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as a, \
                    socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as b:
                a.connect(hub.switch_sock)
                b.connect(hub.switch_sock)
                a.settimeout(2)
                b.settimeout(2)
                for source, target, queue in ((a, b, 3), (b, a, 1)):
                    metadata = struct.pack(">HH", 1, queue) + struct.pack("<BBHHHHH", 1, 1, 54, 1460, 34, 16, 0)
                    body = metadata + b"\x00" * 14 + b"t" * 65535
                    record = struct.pack(">I", len(body)) + body
                    source.sendall(record[:2])
                    source.sendall(record[2:])
                    received = bytearray()
                    while len(received) < len(record):
                        data = target.recv(len(record) - len(received))
                        self.assertTrue(data, "hub disconnected before the full vnet record")
                        received.extend(data)
                    self.assertEqual(received, record)


class CpuSamplingTests(unittest.TestCase):
    def test_self_cpu_seconds_grows(self):
        os.urandom(8 << 20)
        first = lib.process_cpu_seconds(os.getpid())
        os.urandom(16 << 20)
        second = lib.process_cpu_seconds(os.getpid())
        self.assertIsNotNone(first)
        self.assertGreaterEqual(second, first)

    def test_dead_pid_is_none(self):
        self.assertIsNone(lib.process_cpu_seconds(2**22 + 1))


if __name__ == "__main__":
    unittest.main()
