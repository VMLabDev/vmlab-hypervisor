"""Focused quiet-window orchestration and workload contracts, without VMs."""

import argparse
import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import lib
import phase2_window as window


class WindowTests(unittest.TestCase):
    def test_unrecognizable_binary_name_is_rejected_before_launch(self):
        with self.assertRaisesRegex(lib.BenchError, "so host VM counting sees it"):
            window.preflight("/tmp/not-a-countable-hypervisor")

    def test_plan_is_three_interleaved_repeats_of_only_the_four_target_bars(self):
        plan = window.plan()
        self.assertEqual(len(plan), 12)
        for suite in ("single", "g2g"):
            for repeat in range(1, 4):
                backends = [item["backend"] for item in plan if item["suite"] == suite and item["repeat"] == repeat]
                self.assertEqual(backends, ["qemu", "vmlab-hv"] if repeat % 2 else ["vmlab-hv", "qemu"])
        self.assertEqual(set(window.WORKLOADS), {
            "blk_seq_write_mbps", "net_tx_udp_mbps", "net_g2g_a2b_tcp_mbps", "net_g2g_b2a_tcp_mbps"})

    def test_dry_run_does_not_acquire_slots_or_launch_anything(self):
        output = io.StringIO()
        with mock.patch.object(window, "preflight", side_effect=AssertionError("unexpected preflight")), \
             mock.patch.object(window, "admission", side_effect=AssertionError("unexpected admission")), \
             contextlib.redirect_stdout(output):
            self.assertEqual(window.main(["--dry-run"]), 0)
        result = json.loads(output.getvalue())
        self.assertEqual(len(result["plan"]), 12)
        self.assertEqual(result["workloads"], list(window.WORKLOADS))

    def test_busy_admission_fails_without_booting(self):
        with tempfile.TemporaryDirectory() as root, \
             mock.patch.object(window.fcntl, "flock", side_effect=BlockingIOError), \
             mock.patch.object(lib, "project_vm_pids") as pids:
            with self.assertRaisesRegex(lib.BenchError, "slots are busy"):
                with window.admission(Path(root)):
                    self.fail("busy admission succeeded")
            pids.assert_not_called()

    def test_existing_project_vm_rejects_window_before_boot(self):
        with tempfile.TemporaryDirectory() as root, mock.patch.object(lib, "project_vm_pids", return_value=[123]):
            with self.assertRaisesRegex(lib.BenchError, "already running"):
                with window.admission(Path(root)):
                    self.fail("foreign VM was admitted")

    def test_monitor_failure_invalidates_results(self):
        monitor = window.Monitor("/tmp/owned-window")
        with mock.patch.object(lib, "project_vm_pids", side_effect=OSError("proc unavailable")):
            monitor.watch()
        with self.assertRaisesRegex(lib.BenchError, "monitor failed: proc unavailable"):
            monitor.check()

    def test_release_failure_reaps_the_partially_started_vm(self):
        args = argparse.Namespace(backend="vmlab-hv", hv_bin="/built/vmlab-hv")
        backend = mock.Mock()
        backend.release_cpus.side_effect = lib.BenchError("resume failed")
        backend.session.exit_code.return_value = 0
        with tempfile.TemporaryDirectory() as root, \
             mock.patch.object(lib, "prepare_run_dir", return_value=("disk", "bios", None)), \
             mock.patch.object(lib, "require_vm_slots") as slots, \
             mock.patch.object(window.backends, "X86HvBackend", return_value=backend):
            with self.assertRaisesRegex(lib.BenchError, "resume failed"):
                with window.vm(args, Path(root) / "guest", "16-23", "switch", "mac", "version"):
                    self.fail("release failure was ignored")
        slots.assert_called_once_with()
        backend.session.kill.assert_called_once_with()

    def test_cap_kill_is_a_measurement_failure(self):
        args = argparse.Namespace(backend="qemu", hv_bin="unused")
        backend = mock.Mock()
        backend.session.exit_code.return_value = -9
        backend.session.stderr_text.return_value = "memory cap killed the VM"
        with tempfile.TemporaryDirectory() as root, \
             mock.patch.object(lib, "prepare_run_dir", return_value=("disk", "bios", None)), \
             mock.patch.object(lib, "require_vm_slots"), \
             mock.patch.object(window.backends, "X86QemuBackend", return_value=backend):
            with self.assertRaisesRegex(lib.BenchError, "unexpected VM exit -9"):
                with window.vm(args, Path(root) / "guest", "16-23", "switch", "mac", "version"):
                    pass
        backend.session.kill.assert_called_once_with()

    def single(self, received=6500000):
        backend, agent, peer = mock.Mock(), mock.Mock(), mock.Mock()
        backend.backend_version.return_value = "version"
        peer.report.side_effect = [{"rx_bytes": 0}, {"rx_bytes": received}]
        commands = []

        def command(_agent, text, *_args, **_kwargs):
            commands.append(text)
            if "/tmp/randblke seqwrite" in text:
                return json.dumps({"bytes": 512 * 1048576, "secs": 0.25})
            if "udp-blast" in text:
                return json.dumps({"bytes": 6500000})
            return ""

        with tempfile.TemporaryDirectory() as root, \
             mock.patch.object(window, "vm", return_value=contextlib.nullcontext(backend)), \
             mock.patch.object(window, "ready", return_value=contextlib.nullcontext(agent)), \
             mock.patch.object(window.netpeer, "Peer", return_value=peer), \
             mock.patch.object(window.run, "guest_nic", return_value="eth0"), \
             mock.patch.object(window.run, "copy_in_tools"), \
             mock.patch.object(window.run, "must_sh", side_effect=command), \
             mock.patch.object(window.time, "sleep"):
            try:
                result = window.single(mock.Mock(), Path(root) / "single", "version")
            finally:
                peer.kill.assert_called_once_with()
        return result, commands

    def test_single_uses_unchanged_write_and_udp_workloads_only(self):
        result, commands = self.single()
        self.assertEqual(result["samples"], {"blk_seq_write_mbps": 2048.0,
            "net_tx_udp_mbps": round(6500000 * 8 / 12 / 1e6, 2)})
        self.assertIn("/tmp/randblke seqwrite /var/tmp/bench-seq.img 536870912", commands)
        self.assertTrue(any("65000 12.0" in text and "udp-blast" in text for text in commands))
        self.assertFalse(any(mode in text for text in commands for mode in ("seqread", "randread", "randwrite", "udp-sink")))

    def test_udp_delivery_failure_is_not_recorded_as_low_throughput(self):
        with self.assertRaisesRegex(lib.BenchError, "UDP delivery below 90%"):
            self.single(received=1)

    def test_pair_measures_both_directions_and_checks_byte_counts(self):
        hub = mock.Mock(switch_sock="switch")
        backend = mock.Mock()
        backend.backend_version.return_value = "version"
        agents = [mock.Mock(), mock.Mock()]
        with tempfile.TemporaryDirectory() as root, \
             mock.patch.object(window.netpeer, "Hub", return_value=hub), \
             mock.patch.object(window, "vm", side_effect=lambda *_args: contextlib.nullcontext(backend)), \
             mock.patch.object(window, "ready", side_effect=[contextlib.nullcontext(agent) for agent in agents]), \
             mock.patch.object(window.run, "g2g_setup_guest"), \
             mock.patch.object(window.run, "g2g_direction", side_effect=[
                 (21, {"bytes": 100}, {"bytes": 100}), (22, {"bytes": 200}, {"bytes": 200})]) as direction:
            result = window.pair(mock.Mock(), Path(root) / "pair", "version")
        self.assertEqual(result["samples"], {"net_g2g_b2a_tcp_mbps": 21, "net_g2g_a2b_tcp_mbps": 22})
        self.assertIs(direction.call_args_list[0].args[0], agents[1])
        self.assertIs(direction.call_args_list[1].args[0], agents[0])
        hub.kill.assert_called_once_with()

    def results(self):
        return [dict(item, samples={key: 100 + item["repeat"] for key in (
            window.WORKLOADS[:2] if item["suite"] == "single" else window.WORKLOADS[2:])},
            load_start=[0, 0, 0], load_end=[1, 1, 1], backend_version="version") for item in window.plan()]

    def test_history_contains_only_eight_target_records_and_three_samples_each(self):
        with tempfile.TemporaryDirectory() as root, mock.patch.object(lib, "HISTORY_DIR", root):
            records = window.aggregate(self.results(), {}, "commit", False)
            history = lib.read_history("x86_64")
        self.assertEqual(len(records), 8)
        self.assertEqual(len(history), 8)
        self.assertEqual({record["workload"] for record in records}, set(window.WORKLOADS))
        for record in records:
            self.assertEqual(record["samples"], [101, 102, 103])
            self.assertEqual(record["repeats"], 3)
            if record["backend"] == "vmlab-hv":
                self.assertIsNotNone(record["qemu_baseline"])

    def test_incomplete_cohort_does_not_append_partial_history(self):
        results = self.results()
        del results[-1]["samples"]["net_g2g_a2b_tcp_mbps"]
        with mock.patch.object(window.run, "finish_config") as finish:
            with self.assertRaisesRegex(lib.BenchError, "incomplete target"):
                window.aggregate(results, {}, "commit", False)
        finish.assert_not_called()

    def test_main_supplies_readiness_arguments_and_writes_only_target_results(self):
        environment = {"template": "version", "assets": {"hv": {"path": "/built/vmlab-hv"}}}
        monitor = mock.Mock(samples=[])

        def measured(args, _directory, version):
            self.assertEqual(args.timeout_scale, 1.0)
            self.assertEqual(version, "version")
            return {"samples": {key: 100 for key in window.WORKLOADS}, "backend_version": "version"}

        history = [{"backend": backend, "workload": key, "median": 100}
                   for backend in ("qemu", "vmlab-hv") for key in window.WORKLOADS]
        with tempfile.TemporaryDirectory() as root, \
             mock.patch.object(window, "preflight", return_value=environment), \
             mock.patch.object(window, "admission", return_value=contextlib.nullcontext()), \
             mock.patch.object(window, "Monitor", return_value=monitor), \
             mock.patch.object(window.run, "git_commit", return_value=("commit", False)), \
             mock.patch.object(window.run, "log"), \
             mock.patch.object(window.lib, "RUN_DIR", root), \
             mock.patch.object(window, "single", side_effect=measured) as single, \
             mock.patch.object(window, "pair", side_effect=measured) as pair, \
             mock.patch.object(window, "aggregate", return_value=history), \
             contextlib.redirect_stdout(io.StringIO()):
            output = Path(root) / "result.json"
            self.assertEqual(window.main(["--output", str(output)]), 0)
            result = json.loads(output.read_text())
        self.assertTrue(result["complete"])
        self.assertEqual(len(result["results"]), 12)
        self.assertEqual(len(result["bars"]), 4)
        self.assertEqual(single.call_count, 6)
        self.assertEqual(pair.call_count, 6)
        monitor.close.assert_called_once_with()


if __name__ == "__main__":
    unittest.main()
