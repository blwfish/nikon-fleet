import os
import sys
import threading
import time

import pytest

from fleet_cli import (
    ENV_CLI, TIMEOUT_QUICK_S, CliNotFound, CliResult, JobRunner, binary_stale_warning, find_cli, run_cli,
)

PY = sys.executable


def _py(code: str) -> list:
    return [PY, "-c", code]


class TestRunCli:
    def test_success_captures_both_streams_and_the_status(self):
        r = run_cli(_py("import sys; print('out'); print('warn', file=sys.stderr)"))
        assert (r.returncode, r.stdout, r.stderr, r.problem) == (0, "out\n", "warn\n", None)
        assert r.ok

    def test_a_failure_keeps_its_status_and_stderr(self):
        r = run_cli(_py("import sys; print('why', file=sys.stderr); sys.exit(3)"))
        assert r.returncode == 3 and not r.ok
        assert r.failure_text() == "why"

    def test_stdin_reaches_the_child(self):
        r = run_cli(_py("import sys; print(sys.stdin.read().upper())"), stdin_data="secret")
        assert r.stdout == "SECRET\n"

    def test_no_stdin_data_means_the_child_sees_eof_not_a_hang(self):
        r = run_cli(_py("import sys; print(repr(sys.stdin.read()))"), timeout=10)
        assert r.stdout == "''\n"

    def test_a_binary_that_cannot_start_is_a_result_not_an_exception(self, tmp_path):
        r = run_cli([str(tmp_path / "nope")])
        assert r.returncode is None and "could not run" in r.problem
        assert r.failure_text() == r.problem

    def test_an_argument_with_a_nul_is_a_result_not_an_exception(self):
        r = run_cli([PY, "-c", "pass", "a\0b"])
        assert r.returncode is None and r.problem

    def test_output_that_is_not_utf8_does_not_crash(self):
        r = run_cli(_py("import sys; sys.stdout.buffer.write(b'caf\\xe9\\n')"))
        assert r.ok and r.stdout.startswith("caf")

    def test_a_hung_child_is_killed_on_the_timeout(self, tmp_path):
        pidfile = tmp_path / "pid"
        code = f"import os, time; open({str(pidfile)!r}, 'w').write(str(os.getpid())); time.sleep(60)"
        t0 = time.monotonic()
        r = run_cli(_py(code), timeout=1.0)
        assert time.monotonic() - t0 < 20
        assert r.returncode is None and "timed out" in r.problem and "1 s" in r.problem
        assert r.timed_out
        assert not run_cli([str(tmp_path / "nope")]).timed_out
        pid = int(pidfile.read_text())
        with pytest.raises(ProcessLookupError):
            os.kill(pid, 0)

    def test_the_grandchildren_of_a_hung_child_are_killed_too(self, tmp_path):
        pidfile = tmp_path / "gpid"
        child = f"import os, time; open({str(pidfile)!r}, 'w').write(str(os.getpid())); time.sleep(60)"
        parent = f"import subprocess, sys, time; subprocess.Popen([sys.executable, '-c', {child!r}]); time.sleep(60)"
        r = run_cli(_py(parent), timeout=1.5)
        assert r.problem
        pid = int(pidfile.read_text())
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                return
            time.sleep(0.05)
        raise AssertionError("the grandchild survived the timeout")

    # The timeout boundary, with a tenfold margin on each side so load cannot
    # push a run across it: a child ten times shorter than the timeout finishes,
    # one ten times longer is stopped.
    TIMEOUT = 3.0

    def test_a_child_well_inside_the_timeout_is_a_success(self):
        r = run_cli(_py(f"import time; time.sleep({self.TIMEOUT / 10}); print('ok')"), timeout=self.TIMEOUT)
        assert r.ok and r.stdout == "ok\n" and not r.timed_out

    def test_a_child_well_over_the_timeout_is_stopped_and_flagged(self):
        t0 = time.monotonic()
        r = run_cli(_py(f"import time; time.sleep({self.TIMEOUT * 10})"), timeout=self.TIMEOUT)
        assert r.timed_out and r.returncode is None
        assert time.monotonic() - t0 < self.TIMEOUT * 5      # stopped at the timeout, not run to completion

    def test_the_default_timeout_is_finite(self):
        assert 0 < TIMEOUT_QUICK_S < 600


class TestCliResult:
    def test_failure_text_prefers_the_problem_then_stderr_then_stdout_then_the_status(self):
        assert CliResult(None, "o", "e", "timed out").failure_text() == "timed out"
        assert CliResult(1, "o", " e \n").failure_text() == "e"
        assert CliResult(1, " o \n", "").failure_text() == "o"
        assert CliResult(7, "", "").failure_text() == "nikon-fleet exited 7"

    def test_ok_is_only_status_zero(self):
        assert CliResult(0).ok and not CliResult(1).ok and not CliResult(None).ok


class TestFindCli:
    def _bins(self, tmp_path):
        release = tmp_path / "target" / "release"
        debug = tmp_path / "target" / "debug"
        release.mkdir(parents=True)
        debug.mkdir(parents=True)
        return release / "nikon-fleet", debug / "nikon-fleet"

    def test_the_environment_override_wins(self, tmp_path):
        mine = tmp_path / "mine"
        mine.write_text("x")
        rel, _ = self._bins(tmp_path)
        rel.write_text("x")
        assert find_cli(tmp_path, {ENV_CLI: str(mine)}) == mine

    def test_an_override_that_names_nothing_is_an_error_not_a_fallback(self, tmp_path):
        rel, _ = self._bins(tmp_path)
        rel.write_text("x")
        with pytest.raises(CliNotFound) as e:
            find_cli(tmp_path, {ENV_CLI: str(tmp_path / "gone")})
        assert ENV_CLI in str(e.value)

    def test_an_empty_override_is_ignored(self, tmp_path):
        rel, _ = self._bins(tmp_path)
        rel.write_text("x")
        assert find_cli(tmp_path, {ENV_CLI: ""}) == rel

    def test_the_newer_build_wins_not_always_release(self, tmp_path):
        rel, dbg = self._bins(tmp_path)
        rel.write_text("x")
        dbg.write_text("x")
        os.utime(rel, (1000, 1000))
        os.utime(dbg, (2000, 2000))
        assert find_cli(tmp_path, {}) == dbg
        os.utime(rel, (3000, 3000))
        assert find_cli(tmp_path, {}) == rel

    def test_one_build_is_enough(self, tmp_path):
        _, dbg = self._bins(tmp_path)
        dbg.write_text("x")
        assert find_cli(tmp_path, {}) == dbg

    def test_no_binary_says_how_to_build_one(self, tmp_path):
        with pytest.raises(CliNotFound) as e:
            find_cli(tmp_path, {})
        assert "cargo build" in str(e.value) and ENV_CLI in str(e.value)


class TestJobRunner:
    def _drain(self, runner, timeout=5.0):
        deadline = time.monotonic() + timeout
        while runner.busy and time.monotonic() < deadline:
            runner.poll()
            time.sleep(0.005)
        assert not runner.busy

    def test_a_job_runs_and_its_callback_runs_on_the_polling_thread(self):
        runner = JobRunner()
        seen = []
        assert runner.submit(lambda: CliResult(0, "x"), lambda r: seen.append((r.stdout, threading.current_thread())))
        self._drain(runner)
        assert seen == [("x", threading.current_thread())]

    def test_a_second_job_is_refused_while_one_is_in_flight(self):
        runner = JobRunner()
        gate = threading.Event()
        started = []
        assert runner.submit(lambda: (gate.wait(5), CliResult(0))[1], lambda r: started.append("first"))
        assert runner.busy
        assert runner.submit(lambda: CliResult(0), lambda r: started.append("second")) is False
        gate.set()
        self._drain(runner)
        assert started == ["first"]          # the refused job never ran

    def test_a_new_job_is_accepted_once_the_first_has_finished(self):
        runner = JobRunner()
        done = []
        runner.submit(lambda: CliResult(0), done.append)
        self._drain(runner)
        assert runner.submit(lambda: CliResult(0, "again"), done.append)
        self._drain(runner)
        assert [r.stdout for r in done] == ["", "again"]

    def test_poll_before_the_job_is_done_reports_busy_and_runs_nothing(self):
        runner = JobRunner()
        gate = threading.Event()
        done = []
        runner.submit(lambda: (gate.wait(5), CliResult(0))[1], done.append)
        assert runner.poll() is True and done == []
        gate.set()
        self._drain(runner)
        assert len(done) == 1

    def test_poll_with_nothing_submitted_is_idle(self):
        assert JobRunner().poll() is False

    def test_an_exception_in_the_job_becomes_a_failed_result_and_clears_busy(self):
        runner = JobRunner()
        got = []

        def boom():
            raise ValueError("bad zip")
        runner.submit(boom, got.append)
        self._drain(runner)
        assert got[0].returncode is None and "ValueError: bad zip" in got[0].problem

    def test_an_exception_in_the_callback_is_reported_and_clears_busy(self):
        errors = []
        runner = JobRunner(on_callback_error=errors.append)

        def bad_callback(_r):
            raise RuntimeError("callback broke")
        runner.submit(lambda: CliResult(0), bad_callback)
        self._drain(runner)
        assert [str(e) for e in errors] == ["callback broke"]
        # and the runner is usable again
        assert runner.submit(lambda: CliResult(0), lambda r: None)
        self._drain(runner)

    def test_without_a_handler_a_callback_exception_propagates_but_busy_is_cleared(self):
        runner = JobRunner()
        runner.submit(lambda: CliResult(0), lambda r: 1 / 0)
        deadline = time.monotonic() + 5
        with pytest.raises(ZeroDivisionError):
            while time.monotonic() < deadline:
                runner.poll()
                time.sleep(0.005)
        assert not runner.busy

    def test_a_callback_may_submit_the_next_job(self):
        runner = JobRunner()
        order = []

        def first_done(_r):
            order.append("first")
            assert runner.submit(lambda: CliResult(0), lambda r: order.append("second"))
        runner.submit(lambda: CliResult(0), first_done)
        self._drain(runner)
        assert order == ["first", "second"]


class TestBinaryStaleWarning:
    def _project(self, tmp_path, binary_mtime, *source_mtimes):
        src = tmp_path / "src" / "ops"
        src.mkdir(parents=True)
        binary = tmp_path / "nikon-fleet"
        binary.write_text("x")
        os.utime(binary, (binary_mtime, binary_mtime))
        for i, m in enumerate(source_mtimes):
            f = src / f"f{i}.rs"
            f.write_text("//")
            os.utime(f, (m, m))
        return binary

    def test_a_binary_older_than_a_source_file_is_warned_about(self, tmp_path):
        binary = self._project(tmp_path, 1000, 500, 1001)
        warning = binary_stale_warning(binary, tmp_path)
        assert "older than the source" in warning and "cargo build" in warning and str(binary) in warning

    def test_at_below_above_the_newest_source(self, tmp_path):
        assert binary_stale_warning(self._project(tmp_path, 1000, 999), tmp_path) is None     # source older
        # the same mtime is not stale (a build and the checkout in the same second)
        other = tmp_path / "b"
        other.mkdir()
        assert binary_stale_warning(self._project(other, 1000, 1000), other) is None
        third = tmp_path / "c"
        third.mkdir()
        assert binary_stale_warning(self._project(third, 1000, 1001), third)               # one second newer

    def test_only_rs_files_under_src_count(self, tmp_path):
        binary = self._project(tmp_path, 1000, 500)
        (tmp_path / "src" / "notes.txt").write_text("x")
        os.utime(tmp_path / "src" / "notes.txt", (9999, 9999))
        (tmp_path / "other.rs").write_text("x")
        os.utime(tmp_path / "other.rs", (9999, 9999))
        assert binary_stale_warning(binary, tmp_path) is None

    def test_no_source_tree_is_no_warning(self, tmp_path):
        binary = tmp_path / "nikon-fleet"
        binary.write_text("x")
        assert binary_stale_warning(binary, tmp_path) is None
        (tmp_path / "src").mkdir()
        assert binary_stale_warning(binary, tmp_path) is None

    def test_a_missing_binary_is_no_warning_not_a_crash(self, tmp_path):
        assert binary_stale_warning(tmp_path / "gone", tmp_path) is None


class TestJobRunnerFailures:
    def _drain(self, runner, timeout=5.0):
        deadline = time.monotonic() + timeout
        while runner.busy and time.monotonic() < deadline:
            try:
                runner.poll()
            except Exception:
                pass
            time.sleep(0.005)
        assert not runner.busy

    def test_a_keyboard_interrupt_in_the_job_ends_the_job_and_clears_busy(self, monkeypatch):
        seen = []
        monkeypatch.setattr(threading, "excepthook", lambda args: seen.append(args.exc_type))
        runner = JobRunner()
        got = []

        def interrupted():
            raise KeyboardInterrupt("stop")
        runner.submit(interrupted, got.append)
        self._drain(runner)
        assert got[0].returncode is None and "KeyboardInterrupt" in got[0].problem
        assert seen == [KeyboardInterrupt]                      # not swallowed: the thread still ends with it

    def test_a_system_exit_in_the_job_is_a_failed_result_too(self, monkeypatch):
        monkeypatch.setattr(threading, "excepthook", lambda args: None)
        runner = JobRunner()
        got = []

        def exits():
            raise SystemExit(3)
        runner.submit(exits, got.append)
        self._drain(runner)
        assert "SystemExit" in got[0].problem

    def test_a_job_failure_does_not_stop_the_next_job(self):
        runner = JobRunner()
        runner.submit(lambda: 1 / 0, lambda r: None)
        self._drain(runner)
        got = []
        assert runner.submit(lambda: CliResult(0, "fine"), got.append)
        self._drain(runner)
        assert got[0].stdout == "fine"

    def test_an_error_handler_that_raises_still_clears_busy(self):
        def bad_handler(_e):
            raise RuntimeError("the handler broke too")
        runner = JobRunner(on_callback_error=bad_handler)
        runner.submit(lambda: CliResult(0), lambda r: 1 / 0)
        deadline = time.monotonic() + 5
        with pytest.raises(RuntimeError, match="the handler broke too"):
            while time.monotonic() < deadline:
                runner.poll()
                time.sleep(0.005)
        assert not runner.busy
        assert runner.submit(lambda: CliResult(0), lambda r: None)      # and it is usable again
