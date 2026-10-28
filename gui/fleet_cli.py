"""Running the nikon-fleet CLI for the GUI: finding the binary, one call with a
timeout, and a single-flight job runner that keeps the call off the Tk thread.
No tkinter, so it can be tested without a display."""
import os
import queue
import signal
import subprocess
import threading
from dataclasses import dataclass
from pathlib import Path

# Overrides the binary lookup: a path to the executable to run.
ENV_CLI = "NIKON_FLEET_CLI"

# Seconds. A camera call takes 4-20 s; a quiet CLI that has not answered in
# these is hung, and is killed. `firmware browse` waits for the person to
# accept a licence in its own window, so it gets an hour.
TIMEOUT_QUICK_S = 30
TIMEOUT_CAMERA_S = 180
TIMEOUT_FIRMWARE_ADD_S = 600
TIMEOUT_BROWSE_S = 3600
# How long to wait for a killed child to be reaped before giving up on its output.
_REAP_S = 5


class CliNotFound(RuntimeError):
    pass


def find_cli(project: Path, env=None) -> Path:
    """The CLI binary: $NIKON_FLEET_CLI if set (an error if it names nothing),
    else the newer of target/release and target/debug under *project* (a stale
    release build would otherwise hide the build just made)."""
    env = os.environ if env is None else env
    override = env.get(ENV_CLI)
    if override:
        p = Path(override)
        if not p.is_file():
            raise CliNotFound(f"{ENV_CLI}={override} is not a file")
        return p
    found = [p for p in (project / "target" / "release" / "nikon-fleet",
                         project / "target" / "debug" / "nikon-fleet") if p.is_file()]
    if not found:
        raise CliNotFound(
            "the nikon-fleet binary was not found.\n"
            f"Run `cargo build --release` in {project} first, or set {ENV_CLI}."
        )
    return max(found, key=lambda p: p.stat().st_mtime)


def binary_stale_warning(binary: Path, project: Path) -> "str | None":
    """A warning when *binary* is older than the newest `*.rs` under
    project/src (a stale build runs fine and silently lacks the source's
    newer behaviour), else None. The binary is still run."""
    try:
        built = binary.stat().st_mtime
        newest = max((p.stat().st_mtime for p in (project / "src").rglob("*.rs")), default=None)
    except OSError:
        return None     # cannot tell: no warning is better than a wrong one
    if newest is not None and newest > built:
        return (f"the nikon-fleet binary ({binary}) is older than the source: "
                "rebuild with cargo build")
    return None


@dataclass(frozen=True)
class CliResult:
    """What one CLI call did. `returncode` is None when there is no exit
    status: the process could not start or was killed on the timeout (see
    `problem`)."""
    returncode: "int | None"
    stdout: str = ""
    stderr: str = ""
    problem: "str | None" = None
    timed_out: bool = False      # killed on the timeout (it may have done part of its work)

    @property
    def ok(self) -> bool:
        return self.returncode == 0

    def failure_text(self) -> str:
        """Why the call failed, in the CLI's own words where it gave any."""
        if self.problem:
            return self.problem
        return self.stderr.strip() or self.stdout.strip() or f"nikon-fleet exited {self.returncode}"


def _kill_group(proc: subprocess.Popen) -> None:
    """Kill the child and anything it started (the process group)."""
    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        # Already gone, or not ours to signal as a group: fall back to the child alone.
        try:
            proc.kill()
        except ProcessLookupError:
            pass


def run_cli(cmd: list, stdin_data: "str | None" = None, timeout: float = TIMEOUT_QUICK_S) -> CliResult:
    """Run *cmd* to completion or the timeout; never raises.

    The child gets its own process group so a timeout kills what it spawned
    too. Output is decoded leniently (a file name that is not UTF-8 must not
    crash the GUI). A process that cannot start, or is killed on the timeout,
    comes back with returncode None and a `problem` saying so.
    """
    try:
        proc = subprocess.Popen(
            cmd, stdin=subprocess.PIPE if stdin_data is not None else subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, errors="replace", start_new_session=True,
        )
    except (OSError, ValueError) as e:
        # ValueError: an argument with an embedded NUL.
        return CliResult(None, problem=f"could not run {cmd[0]}: {e}")
    try:
        out, err = proc.communicate(stdin_data, timeout=timeout)
    except subprocess.TimeoutExpired:
        _kill_group(proc)
        try:
            out, err = proc.communicate(timeout=_REAP_S)
        except subprocess.TimeoutExpired:
            out, err = "", ""
        return CliResult(None, out or "", err or "",
                         problem=f"timed out after {timeout:g} s and was stopped", timed_out=True)
    except OSError as e:
        _kill_group(proc)
        proc.wait()
        return CliResult(None, problem=f"talking to {cmd[0]} failed: {e}")
    return CliResult(proc.returncode, out, err)


class JobRunner:
    """One background call at a time, finished on the caller's (Tk) thread.

    `submit` starts *fn* on a worker thread and refuses (returns False) while
    one is in flight, so a double click cannot start a second camera session.
    The GUI calls `poll()` from `after`; it runs the finished job's callback on
    that thread. An exception in *fn* becomes a failed CliResult, and one in
    the callback goes to `on_callback_error`, so neither leaves the job
    "in flight" forever.
    """

    def __init__(self, on_callback_error=None):
        self._done: queue.Queue = queue.Queue()
        self._busy = False
        # Kept on this (the caller's) thread, not in the worker's closure: the
        # callback reaches GUI objects, and the last reference to one must not
        # be dropped on the worker thread (Tk objects are freed on the main thread).
        self._on_done = None
        self._on_callback_error = on_callback_error

    @property
    def busy(self) -> bool:
        return self._busy

    def submit(self, fn, on_done) -> bool:
        """Run `fn()` (returning a CliResult or any value) in the background;
        `on_done(result)` is called from `poll`. False if a job is running."""
        if self._busy:
            return False
        self._busy = True
        self._on_done = on_done

        def work() -> None:
            try:
                result = fn()
            except BaseException as e:   # the job must always end: busy is cleared by what is put here
                result = CliResult(None, problem=f"{type(e).__name__}: {e}")
                self._done.put(result)
                if not isinstance(e, Exception):
                    raise                # KeyboardInterrupt / SystemExit still end the thread
                return
            self._done.put(result)

        threading.Thread(target=work, daemon=True).start()
        return True

    def poll(self) -> bool:
        """Finish the job if it is done. Returns whether one is still running."""
        try:
            result = self._done.get_nowait()
        except queue.Empty:
            return self._busy
        on_done, self._on_done = self._on_done, None
        self._busy = False
        try:
            on_done(result)
        except Exception as e:  # reported, not swallowed
            if self._on_callback_error is not None:
                self._on_callback_error(e)
            else:
                raise
        return self._busy
