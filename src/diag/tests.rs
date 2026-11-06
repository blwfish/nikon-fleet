use super::*;
use tempfile::tempdir;

fn mk(dir: &Path, pid: u32, cap: usize) -> Diag {
    let mut c = Config::new(dir.to_path_buf(), Subsystem::Cli);
    c.pid = pid;
    c.ring_cap = cap;
    Diag::new(c).unwrap()
}

fn ring_lines(dir: &Path, pid: u32) -> Vec<String> {
    fs::read_to_string(ring_path(dir, pid)).unwrap().lines().map(str::to_string).collect()
}

fn leaf_ok(d: &Diag, op: &str) {
    d.span(Subsystem::MaidSdk, op, Class::Leaf, &[]).finish(Outcome::Ok);
}

// ── vocabulary ───────────────────────────────────────────────────────────

#[test]
fn the_hand_written_names_are_the_serde_names_and_all_lists_every_variant() {
    // Two spellings of one vocabulary (as_str and serde) must not drift.
    for s in Subsystem::ALL {
        assert_eq!(serde_json::to_string(&s).unwrap(), format!("\"{}\"", s.as_str()), "{s:?}");
    }
    for o in Outcome::ALL {
        assert_eq!(serde_json::to_string(&o).unwrap(), format!("\"{}\"", o.as_str()), "{o:?}");
    }
    // An exhaustive match: adding a variant breaks this until ALL and the count are updated too.
    fn sub_index(s: Subsystem) -> usize {
        match s {
            Subsystem::Cli => 0,
            Subsystem::Gui => 1,
            Subsystem::Mcp => 2,
            Subsystem::Usb => 3,
            Subsystem::PtpIp => 4,
            Subsystem::MaidSdk => 5,
            Subsystem::Snapshot => 6,
            Subsystem::Transplant => 7,
            Subsystem::Restore => 8,
            Subsystem::Firmware => 9,
            Subsystem::Settings => 10,
            Subsystem::Lock => 11,
            Subsystem::Diag => 12,
        }
    }
    let mut idx: Vec<usize> = Subsystem::ALL.iter().map(|s| sub_index(*s)).collect();
    idx.sort_unstable();
    assert_eq!(idx, (0..13).collect::<Vec<_>>());
    fn out_index(o: Outcome) -> usize {
        match o {
            Outcome::Ok => 0,
            Outcome::Failed => 1,
            Outcome::Timeout => 2,
            Outcome::Stalled => 3,
            Outcome::Indeterminate => 4,
            Outcome::Panicked => 5,
            Outcome::Abandoned => 6,
            Outcome::Cancelled => 7,
        }
    }
    let mut idx: Vec<usize> = Outcome::ALL.iter().map(|o| out_index(*o)).collect();
    idx.sort_unstable();
    assert_eq!(idx, (0..8).collect::<Vec<_>>());
    // An unknown name is refused, not defaulted.
    assert!(serde_json::from_str::<Subsystem>("\"usbb\"").is_err());
    assert!(serde_json::from_str::<Outcome>("\"success\"").is_err());
}

#[test]
fn val_text_is_cut_only_above_the_limit_and_marked_when_cut() {
    assert_eq!(Val::text(&"a".repeat(TEXT_MAX_CHARS)), Val::Text("a".repeat(TEXT_MAX_CHARS)));
    let Val::Text(cut) = Val::text(&"a".repeat(TEXT_MAX_CHARS + 1)) else { panic!("text") };
    assert!(cut.ends_with(TRUNCATED_MARK), "{cut}");
    assert_eq!(cut.chars().count(), TEXT_MAX_CHARS + TRUNCATED_MARK.chars().count());
    // The limit counts characters, not bytes: 256 two-byte characters are not cut.
    assert_eq!(Val::text(&"\u{e9}".repeat(TEXT_MAX_CHARS)), Val::Text("\u{e9}".repeat(TEXT_MAX_CHARS)));
    assert!(matches!(Val::text(&"\u{e9}".repeat(TEXT_MAX_CHARS + 1)), Val::Text(t) if t.ends_with(TRUNCATED_MARK)));
    assert_eq!(Val::text(""), Val::Text(String::new()));
}

#[test]
fn val_hash_and_len_identify_a_payload_without_holding_it() {
    assert_eq!(Val::hash(b"abc"), Val::hash(b"abc"));
    assert_ne!(Val::hash(b"abc"), Val::hash(b"abd"));
    let Val::Hash(h) = Val::hash(b"") else { panic!("hash") };
    assert_eq!(h, "e3b0c44298fc1c14", "SHA-256 of the empty input starts e3b0c44298fc1c14");
    assert_eq!(Val::len(0), Val::Len(0));
    assert_eq!(Val::len(323), Val::Len(323));
}

#[test]
fn a_payload_never_reaches_the_files_only_its_length_and_hash() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4001, 64);
    let secret = b"hunter2-ftp-password";
    d.span(Subsystem::Usb, "write", Class::Leaf, &[("data_len", Val::len(secret.len())), ("data_hash", Val::hash(secret))])
        .finish(Outcome::Ok);
    let ring = fs::read_to_string(ring_path(dir.path(), 4001)).unwrap();
    assert!(!ring.contains("hunter2"), "{ring}");
    assert!(ring.contains(&format!("{}", match Val::hash(secret) { Val::Hash(h) => h, _ => unreachable!() })));
}

// ── paths ────────────────────────────────────────────────────────────────

#[test]
fn the_directory_comes_from_the_override_then_the_os_data_dir_then_nothing() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| OsString::from(*v))
    };
    assert_eq!(dir_for(Os::MacOs, &env(&[("HOME", "/Users/me"), (DIR_ENV, "/x/diag")])), Some(PathBuf::from("/x/diag")));
    assert_eq!(
        dir_for(Os::MacOs, &env(&[("HOME", "/Users/me")])),
        Some(PathBuf::from("/Users/me/Library/Application Support/net.blw.fleet/diag"))
    );
    assert_eq!(dir_for(Os::Windows, &env(&[("APPDATA", r"C:\Users\me\AppData\Roaming")])), Some(PathBuf::from(r"C:\Users\me\AppData\Roaming").join("net.blw.fleet").join("diag")));
    // An empty override is unset, as for the other overrides here.
    assert_eq!(dir_for(Os::MacOs, &env(&[("HOME", "/Users/me"), (DIR_ENV, "")])).unwrap().ends_with("diag"), true);
    // No home and no override: no directory, and the caller must say so.
    assert_eq!(dir_for(Os::MacOs, &env(&[])), None);
    assert_eq!(dir_for(Os::Windows, &env(&[])), None);
}

#[test]
fn the_pid_in_a_file_name_round_trips_the_two_path_helpers_and_nothing_else_parses() {
    let d = Path::new("/x");
    for pid in [0u32, 1, 4242, u32::MAX] {
        for p in [inflight_path(d, pid), ring_path(d, pid)] {
            assert_eq!(pid_of_file_name(&p.file_name().unwrap().to_string_lossy()), Some(pid), "{p:?}");
        }
    }
    for name in ["inflight-.json", "ring-abc.jsonl", "ring-1.json", "inflight-1.jsonl", "ring-1.jsonl.tmp", ".ring-1.jsonl.9.0.tmp", "other", "ring--1.jsonl"] {
        assert_eq!(pid_of_file_name(name), None, "{name}");
    }
}

// ── in-flight file ───────────────────────────────────────────────────────

#[test]
fn a_span_is_in_the_inflight_file_while_open_and_the_file_is_gone_when_nothing_is_open() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4002, 64);
    let path = inflight_path(dir.path(), 4002);
    assert!(!path.exists(), "nothing open at the start");
    let s = d.span(Subsystem::Usb, "ptp_txn", Class::Leaf, &[("op", Val::Hex(0x1016)), ("param", Val::U64(0xD090))]);
    let file: InflightFile = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!((file.pid, file.role, file.spans.len()), (4002, Subsystem::Cli, 1));
    assert_eq!(file.spans[0].op, "ptp_txn");
    assert_eq!(file.spans[0].fields, vec![("op".to_string(), Val::Hex(0x1016)), ("param".to_string(), Val::U64(0xD090))]);
    s.finish(Outcome::Ok);
    assert!(!path.exists());
    assert_eq!(ring_lines(dir.path(), 4002).len(), 1, "the completed span is in the ring file");
}

#[test]
fn nested_spans_are_listed_outermost_first_and_leave_only_the_outer_after_the_inner_ends() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4003, 64);
    let outer = d.span(Subsystem::Transplant, "apply", Class::Top, &[]);
    let inner = d.span(Subsystem::Usb, "ptp_txn", Class::Leaf, &[]);
    let file: InflightFile = serde_json::from_slice(&fs::read(inflight_path(dir.path(), 4003)).unwrap()).unwrap();
    assert_eq!(file.spans.iter().map(|s| (s.op.as_str(), s.depth)).collect::<Vec<_>>(), vec![("apply", 0), ("ptp_txn", 1)]);
    inner.finish(Outcome::Ok);
    let file: InflightFile = serde_json::from_slice(&fs::read(inflight_path(dir.path(), 4003)).unwrap()).unwrap();
    assert_eq!(file.spans.len(), 1);
    assert_eq!(file.spans[0].op, "apply");
    outer.finish(Outcome::Ok);
    assert!(!inflight_path(dir.path(), 4003).exists());
}

#[test]
fn a_span_dropped_without_a_result_is_abandoned_and_one_dropped_while_unwinding_is_panicked() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4004, 64);
    {
        let _s = d.span(Subsystem::Settings, "load", Class::Top, &[]);
    }
    assert_eq!(d.snapshot().last().unwrap().outcome, Some(Outcome::Abandoned));
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _s = d.span(Subsystem::Settings, "save", Class::Top, &[]);
        panic!("boom");
    }));
    assert!(r.is_err());
    assert_eq!(d.snapshot().last().unwrap().outcome, Some(Outcome::Panicked));
    assert!(d.open_spans().is_empty(), "an unwound span is not left in flight");
}

#[test]
fn finish_with_adds_the_fields_known_only_at_the_end() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4005, 64);
    d.span(Subsystem::Usb, "ptp_txn", Class::Leaf, &[("op", Val::Hex(0x1015))]).finish_with(Outcome::Failed, &[("response", Val::Hex(0x200A))]);
    let e = d.snapshot().pop().unwrap();
    assert_eq!(e.outcome, Some(Outcome::Failed));
    assert_eq!(e.fields.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), vec!["op", "response"]);
}

#[test]
fn set_updates_an_open_span_in_the_inflight_file_replaces_rather_than_duplicates_and_reaches_the_entry() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4040, 64);
    let s = d.span(Subsystem::Usb, "ptp_txn", Class::Leaf, &[("op", Val::Hex(0x1016))]);
    let read = || -> InflightFile { serde_json::from_slice(&fs::read(inflight_path(dir.path(), 4040)).unwrap()).unwrap() };
    s.set("phase", Val::text("command_sent"));
    assert_eq!(read().spans[0].fields.last().unwrap(), &("phase".to_string(), Val::text("command_sent")));
    s.set("phase", Val::text("data_sent"));
    let fields = read().spans[0].fields.clone();
    assert_eq!(fields.iter().filter(|(k, _)| k == "phase").count(), 1, "replaced, not appended: {fields:?}");
    assert_eq!(fields.iter().find(|(k, _)| k == "phase").unwrap().1, Val::text("data_sent"));
    s.finish(Outcome::Failed);
    let e = d.snapshot().pop().unwrap();
    assert_eq!(e.fields.iter().find(|(k, _)| k == "phase").unwrap().1, Val::text("data_sent"), "the last stage is in the completed entry");
    // Setting on a span that has ended is a no-op, not a panic: take a no-op span for that.
    Span::none().set("phase", Val::text("x"));
}

#[test]
fn real_threads_each_see_their_own_depth_and_all_appear_in_flight() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4006, 64);
    let barrier = std::sync::Barrier::new(9);
    std::thread::scope(|sc| {
        for _ in 0..8 {
            sc.spawn(|| {
                let outer = d.span(Subsystem::Gui, "cmd", Class::Top, &[]);
                let inner = d.span(Subsystem::MaidSdk, "read", Class::Leaf, &[]);
                barrier.wait(); // all 8 threads have two spans open
                barrier.wait(); // main has looked
                inner.finish(Outcome::Ok);
                outer.finish(Outcome::Ok);
            });
        }
        barrier.wait();
        let open = d.open_spans();
        assert_eq!(open.len(), 16);
        assert_eq!(open.iter().filter(|s| s.depth == 0).count(), 8, "depth is per thread");
        assert_eq!(open.iter().filter(|s| s.depth == 1).count(), 8);
        barrier.wait();
    });
    assert!(d.open_spans().is_empty());
    assert!(!inflight_path(dir.path(), 4006).exists());
    assert_eq!(d.snapshot().len(), 16);
}

// ── ring and eviction ────────────────────────────────────────────────────

#[test]
fn eviction_ranks_routine_leaf_successes_first_and_failed_top_spans_last() {
    let mk_entry = |kind, class, outcome| Entry {
        seq: 0, ts_ms: 0, kind, class, subsystem: Subsystem::Cli, op: String::new(), fields: vec![], depth: 0, thread: 0, outcome, dur_us: 0,
    };
    let leaf_ok = mk_entry(Kind::Span, Class::Leaf, Some(Outcome::Ok));
    let top_ok = mk_entry(Kind::Span, Class::Top, Some(Outcome::Ok));
    let event = mk_entry(Kind::Event, Class::Top, None);
    let leaf_bad = mk_entry(Kind::Span, Class::Leaf, Some(Outcome::Timeout));
    let top_bad = mk_entry(Kind::Span, Class::Top, Some(Outcome::Failed));
    assert!(eviction_rank(&leaf_ok) < eviction_rank(&top_ok));
    assert!(eviction_rank(&top_ok) < eviction_rank(&event));
    assert_eq!(eviction_rank(&event), eviction_rank(&leaf_bad));
    assert!(eviction_rank(&leaf_bad) < eviction_rank(&top_bad));
    // Every non-ok outcome ranks as failed, including the ones that mean "unknown".
    for o in Outcome::ALL.into_iter().filter(|o| *o != Outcome::Ok) {
        assert!(eviction_rank(&mk_entry(Kind::Span, Class::Leaf, Some(o))) >= 2, "{o:?}");
    }
}

#[test]
fn a_burst_of_routine_reads_cannot_push_out_the_top_spans_or_the_failure() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4007, MIN_RING_CAP);
    for i in 0..3 {
        d.span(Subsystem::Transplant, &format!("step{i}"), Class::Top, &[]).finish(Outcome::Ok);
    }
    d.span(Subsystem::MaidSdk, "read_failed", Class::Leaf, &[]).finish(Outcome::Failed);
    for _ in 0..200 {
        leaf_ok(&d, "read");
    }
    let ring = d.snapshot();
    assert_eq!(ring.len(), MIN_RING_CAP);
    for i in 0..3 {
        assert!(ring.iter().any(|e| e.op == format!("step{i}")), "step{i} survived");
    }
    assert!(ring.iter().any(|e| e.op == "read_failed"), "the failure survived");
}

#[test]
fn the_ring_holds_exactly_its_capacity_and_evicts_one_on_the_next_entry() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4008, MIN_RING_CAP);
    for i in 0..MIN_RING_CAP {
        d.span(Subsystem::Cli, &format!("c{i}"), Class::Leaf, &[]).finish(Outcome::Ok);
    }
    assert_eq!(d.snapshot().len(), MIN_RING_CAP, "at capacity nothing is evicted yet");
    assert_eq!(d.snapshot()[0].op, "c0");
    d.span(Subsystem::Cli, "next", Class::Leaf, &[]).finish(Outcome::Ok);
    let ring = d.snapshot();
    assert_eq!(ring.len(), MIN_RING_CAP);
    assert_eq!(ring[0].op, "c1", "the oldest routine entry went");
    assert_eq!(ring.last().unwrap().op, "next");
}

#[test]
fn when_everything_is_a_successful_top_span_the_oldest_goes() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4009, MIN_RING_CAP);
    for i in 0..MIN_RING_CAP + 1 {
        d.span(Subsystem::Cli, &format!("t{i}"), Class::Top, &[]).finish(Outcome::Ok);
    }
    let ring = d.snapshot();
    assert_eq!((ring.len(), ring[0].op.as_str()), (MIN_RING_CAP, "t1"));
}

#[test]
fn a_capacity_below_the_minimum_is_raised_to_it() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4010, 0);
    for _ in 0..50 {
        leaf_ok(&d, "x");
    }
    assert_eq!(d.snapshot().len(), MIN_RING_CAP);
}

// ── disk ring: append-only with compaction ───────────────────────────────

#[test]
fn the_ring_file_never_exceeds_twice_the_capacity_and_always_ends_with_the_newest_entry() {
    let dir = tempdir().unwrap();
    let cap = MIN_RING_CAP;
    let d = mk(dir.path(), 4011, cap);
    for i in 0..10 * cap {
        d.span(Subsystem::Cli, &format!("c{i}"), Class::Leaf, &[]).finish(Outcome::Ok);
        let lines = ring_lines(dir.path(), 4011);
        assert!(lines.len() <= 2 * cap, "{} lines after {} entries", lines.len(), i + 1);
        let last: Entry = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last.op, format!("c{i}"));
    }
    // Every entry in memory is on disk.
    let on_disk: Vec<u64> = ring_lines(dir.path(), 4011).iter().map(|l| serde_json::from_str::<Entry>(l).unwrap().seq).collect();
    for e in d.snapshot() {
        assert!(on_disk.contains(&e.seq), "seq {} missing from the file", e.seq);
    }
}

#[test]
fn compaction_happens_at_exactly_twice_the_capacity() {
    let dir = tempdir().unwrap();
    let cap = MIN_RING_CAP;
    let d = mk(dir.path(), 4012, cap);
    for _ in 0..2 * cap - 1 {
        leaf_ok(&d, "x");
    }
    assert_eq!(ring_lines(dir.path(), 4012).len(), 2 * cap - 1, "just below the threshold: plain appends");
    leaf_ok(&d, "x");
    assert_eq!(ring_lines(dir.path(), 4012).len(), cap, "at the threshold: rewritten from the ring");
}

#[test]
fn a_trace_file_keeps_every_entry_uncompacted() {
    let dir = tempdir().unwrap();
    let trace = dir.path().join("full.jsonl");
    let mut c = Config::new(dir.path().to_path_buf(), Subsystem::Cli);
    c.pid = 4013;
    c.ring_cap = MIN_RING_CAP;
    c.trace_file = Some(trace.clone());
    let d = Diag::new(c).unwrap();
    for _ in 0..50 {
        leaf_ok(&d, "x");
    }
    assert_eq!(fs::read_to_string(&trace).unwrap().lines().count(), 50);
    assert_eq!(d.snapshot().len(), MIN_RING_CAP);
}

#[test]
fn old_processes_files_are_pruned_beyond_the_keep_count_but_never_this_ones() {
    let dir = tempdir().unwrap();
    let n = KEEP_PROCESSES + 5;
    for i in 0..n {
        let pid = 5000 + i as u32;
        let f = fs::File::create(ring_path(dir.path(), pid)).unwrap();
        f.set_modified(UNIX_EPOCH + std::time::Duration::from_secs(1_000_000 + i as u64)).unwrap();
    }
    let _d = mk(dir.path(), 9999, 64);
    let mut pids: Vec<u32> = fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .filter_map(|e| pid_of_file_name(&e.file_name().to_string_lossy()))
        .collect();
    pids.sort_unstable();
    assert_eq!(pids.len(), KEEP_PROCESSES, "this process plus the {} newest", KEEP_PROCESSES - 1);
    assert!(pids.contains(&9999));
    assert!(!pids.contains(&5000), "the oldest was removed");
    assert!(pids.contains(&(5000 + n as u32 - 1)), "the newest was kept");
}

// ── write failures never fail the operation ──────────────────────────────

#[test]
fn a_failed_diagnostics_write_is_counted_and_does_not_disturb_the_operation() {
    let dir = tempdir().unwrap();
    let diag_dir = dir.path().join("d");
    let d = mk(&diag_dir, 4014, 64);
    assert_eq!(d.write_failures(), 0);
    fs::remove_dir_all(&diag_dir).unwrap();
    // The in-flight write fails (its directory is gone); nothing else about the span changes.
    let s = d.span(Subsystem::Usb, "ptp_txn", Class::Leaf, &[]);
    assert!(d.write_failures() > 0);
    s.finish(Outcome::Ok);
    assert_eq!(d.snapshot().len(), 1, "the in-memory record is still kept");
}

// ── reading it back ──────────────────────────────────────────────────────

#[test]
fn the_verdicts_follow_from_what_is_open_whether_the_process_is_alive_and_whether_it_exited() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4020, 64);
    let open = d.span(Subsystem::MaidSdk, "read_capability", Class::Leaf, &[("cap", Val::Hex(0x8001))]);
    std::mem::forget(open); // the process "dies" with it open
    let verdict = |alive: Option<bool>| {
        let t = load(dir.path(), &|_| alive).unwrap();
        t.into_iter().find(|t| t.pid == 4020).unwrap().verdict()
    };
    assert_eq!(verdict(Some(false)), Verdict::GoneWithSpansOpen);
    assert_eq!(verdict(Some(true)), Verdict::RunningWithSpansOpen);
    assert_eq!(verdict(None), Verdict::SpansOpenLivenessUnknown);

    // A second process that finished everything and exited cleanly.
    let e = mk(dir.path(), 4021, 64);
    leaf_ok(&e, "x");
    e.shutdown(3);
    let v = |alive| load(dir.path(), &|_| alive).unwrap().into_iter().find(|t| t.pid == 4021).unwrap().verdict();
    for alive in [Some(false), Some(true), None] {
        assert_eq!(v(alive), Verdict::ExitedCleanly { code: 3 }, "{alive:?}");
    }

    // A third that recorded entries but no exit, and is gone: killed between operations.
    let f = mk(dir.path(), 4022, 64);
    leaf_ok(&f, "x");
    let v = |alive| load(dir.path(), &|_| alive).unwrap().into_iter().find(|t| t.pid == 4022).unwrap().verdict();
    assert_eq!(v(Some(false)), Verdict::GoneWithoutExit);
    assert_eq!(v(Some(true)), Verdict::RunningIdle);
    assert_eq!(v(None), Verdict::RunningIdle);
}

#[test]
fn shutdown_closes_what_is_still_open_by_code_and_class_and_then_reads_as_a_clean_exit() {
    for (code, top_outcome) in [(0, Outcome::Ok), (1, Outcome::Failed), (4, Outcome::Failed)] {
        let dir = tempdir().unwrap();
        let d = mk(dir.path(), 4030, 64);
        std::mem::forget(d.span(Subsystem::Cli, "command", Class::Top, &[]));
        std::mem::forget(d.span(Subsystem::Usb, "ptp_txn", Class::Leaf, &[]));
        d.shutdown(code);
        assert!(!inflight_path(dir.path(), 4030).exists(), "nothing is in flight after a planned exit");
        assert!(d.open_spans().is_empty());
        let ring = d.snapshot();
        let outcome_of = |op: &str| ring.iter().find(|e| e.op == op).unwrap().outcome;
        assert_eq!(outcome_of("command"), Some(top_outcome), "code {code}");
        assert_eq!(outcome_of("ptp_txn"), Some(Outcome::Abandoned), "a leaf still open is abandoned whatever the code");
        let t = load(dir.path(), &|_| Some(false)).unwrap().pop().unwrap();
        assert_eq!(t.verdict(), Verdict::ExitedCleanly { code: i64::from(code) });
    }
}

#[test]
fn a_hang_before_shutdown_is_still_a_hang_and_shutdown_twice_records_one_exit() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4031, 64);
    std::mem::forget(d.span(Subsystem::MaidSdk, "disconnect", Class::Leaf, &[]));
    // Never reached shutdown: the report says so.
    let t = load(dir.path(), &|_| Some(true)).unwrap().pop().unwrap();
    assert_eq!(t.verdict(), Verdict::RunningWithSpansOpen);
    d.shutdown(0);
    d.shutdown(7);
    let exits: Vec<_> = d.snapshot().into_iter().filter(|e| e.op == OP_EXIT).collect();
    assert_eq!(exits.len(), 1, "a second shutdown is a no-op");
    assert_eq!(exits[0].fields[0].1, Val::I64(0), "and the first code is the one kept");
}

#[test]
fn the_report_names_the_operation_in_flight_and_says_it_was_killed_or_crashed() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4023, 64);
    d.span(Subsystem::Usb, "ptp_txn", Class::Leaf, &[("op", Val::Hex(0x1016))]).finish(Outcome::Ok);
    let outer = d.span(Subsystem::Restore, "apply", Class::Top, &[]);
    let inner = d.span(Subsystem::Usb, "ptp_txn", Class::Leaf, &[("op", Val::Hex(0x90EE)), ("data_len", Val::Len(323))]);
    std::mem::forget(inner);
    std::mem::forget(outer);
    let traces = load(dir.path(), &|_| Some(false)).unwrap();
    let text = render(&traces, unix_ms() + 2500, 10);
    assert!(text.contains("pid 4023"), "{text}");
    assert!(text.contains("killed or crashed"), "{text}");
    assert!(text.contains("in flight (oldest first)"), "{text}");
    assert!(text.contains("usb ptp_txn op=0x90ee data_len=323B"), "{text}");
    assert!(text.contains("restore apply"), "{text}");
    assert!(text.contains("began 2."), "an age of about 2.5 s: {text}");
    assert!(text.contains("ptp_txn ok"), "the earlier completed one: {text}");
}

#[test]
fn a_line_cut_off_by_a_kill_is_counted_and_the_rest_still_load() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4024, 64);
    leaf_ok(&d, "a");
    leaf_ok(&d, "b");
    let mut f = fs::OpenOptions::new().append(true).open(ring_path(dir.path(), 4024)).unwrap();
    write!(f, "{{\"seq\":9,\"ts_ms\":1,\"kind\":\"sp").unwrap();
    let t = load(dir.path(), &|_| Some(false)).unwrap().into_iter().find(|t| t.pid == 4024).unwrap();
    assert_eq!((t.entries.len(), t.malformed_lines), (2, 1));
    assert!(render(&[t], unix_ms(), 10).contains("1 unreadable line(s)"));
}

#[test]
fn an_unparseable_inflight_file_is_reported_as_a_problem_not_as_nothing_in_flight() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4025, 64);
    leaf_ok(&d, "a");
    fs::write(inflight_path(dir.path(), 4025), b"{not json").unwrap();
    let t = load(dir.path(), &|_| Some(false)).unwrap().into_iter().find(|t| t.pid == 4025).unwrap();
    assert!(t.inflight.is_none());
    assert!(t.read_problem.as_deref().is_some_and(|p| p.contains("did not parse")), "{:?}", t.read_problem);
    assert!(render(&[t], unix_ms(), 10).contains("problem:"));
}

#[test]
fn a_collapsed_run_in_the_report_says_its_fields_are_the_first_ones() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4050, 64);
    for i in 0..3u64 {
        d.span(Subsystem::MaidSdk, "read", Class::Leaf, &[("cap", Val::U64(i))]).finish(Outcome::Ok);
    }
    d.span(Subsystem::MaidSdk, "write", Class::Leaf, &[("cap", Val::U64(9))]).finish(Outcome::Ok);
    let text = render(&load(dir.path(), &|_| Some(false)).unwrap(), unix_ms(), 10);
    assert!(text.contains("read ok x3"), "{text}");
    assert!(text.contains("first: cap=0"), "{text}");
    assert!(!text.contains("cap=2"), "only the first entry's fields are shown for a run: {text}");
    assert!(text.contains("write ok (") && !text.contains("write ok x"), "a single entry has no count and no 'first': {text}");
}

#[test]
fn a_missing_directory_is_an_empty_report_and_an_empty_one_says_so() {
    let dir = tempdir().unwrap();
    let t = load(&dir.path().join("nope"), &|_| None).unwrap();
    assert!(t.is_empty());
    assert_eq!(render(&t, 0, 10), "no diagnostics files found\n");
}

#[test]
fn runs_of_routine_successes_collapse_but_a_failure_or_another_thread_or_a_top_span_does_not() {
    let e = |seq, class, outcome: Outcome, op: &str, thread| Entry {
        seq, ts_ms: 0, kind: Kind::Span, class, subsystem: Subsystem::MaidSdk, op: op.to_string(), fields: vec![], depth: 0, thread, outcome: Some(outcome), dur_us: 1000,
    };
    let mut v = Vec::new();
    for i in 0..5 {
        v.push(e(i, Class::Leaf, Outcome::Ok, "read", 1));
    }
    v.push(e(5, Class::Leaf, Outcome::Failed, "read", 1));
    for i in 6..8 {
        v.push(e(i, Class::Leaf, Outcome::Ok, "read", 1));
    }
    v.push(e(8, Class::Leaf, Outcome::Ok, "read", 2));
    v.push(e(9, Class::Top, Outcome::Ok, "read", 2));
    v.push(e(10, Class::Top, Outcome::Ok, "read", 2));
    let counts: Vec<u32> = aggregated(&v).iter().map(|(_, n, _)| *n).collect();
    assert_eq!(counts, vec![5, 1, 2, 1, 1, 1]);
    assert_eq!(aggregated(&v)[0].2, 5000, "the total duration is the sum");
}

// ── the process-wide entry points ────────────────────────────────────────

#[test]
fn without_init_the_global_calls_do_nothing_and_do_not_fail() {
    // No test in this crate calls `init`, so the global recorder is unset here.
    assert!(global().is_none());
    let s = span(Subsystem::Cli, "x", Class::Top, &[]);
    s.finish(Outcome::Ok);
    event(Subsystem::Cli, "y", &[]);
    shutdown(0);
    drop(span(Subsystem::Cli, "z", Class::Top, &[]));
}

#[test]
fn pid_alive_says_yes_for_this_process_and_no_for_one_that_cannot_exist() {
    #[cfg(unix)]
    {
        assert_eq!(pid_alive(std::process::id()), Some(true));
        assert_eq!(pid_alive(0x7fff_fffe), Some(false));
    }
    #[cfg(not(unix))]
    assert_eq!(pid_alive(std::process::id()), None);
}

// ── start(), request ids, Span::done ─────────────────────────────────────

#[test]
fn request_ids_are_unique_and_carry_the_pid() {
    let a = next_request_id();
    let b = next_request_id();
    assert_ne!(a, b);
    assert!(a.starts_with(&format!("{}-", std::process::id())), "{a}");
    let n: u64 = a.rsplit('-').next().unwrap().parse().unwrap();
    let m: u64 = b.rsplit('-').next().unwrap().parse().unwrap();
    assert_eq!(m, n + 1, "consecutive");
}

#[test]
fn span_done_maps_a_result_to_ok_or_failed_and_leaves_the_result_alone() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4060, 64);
    let ok: Result<u8, String> = Ok(1);
    let err: Result<u8, String> = Err("x".into());
    d.span(Subsystem::Cli, "a", Class::Top, &[]).done(&ok);
    d.span(Subsystem::Cli, "b", Class::Top, &[]).done(&err);
    let ring = d.snapshot();
    assert_eq!((ring[0].outcome, ring[1].outcome), (Some(Outcome::Ok), Some(Outcome::Failed)));
    assert_eq!((ok, err), (Ok(1), Err("x".to_string())), "the results are borrowed, not consumed");
}

#[test]
fn cancelled_is_an_ordinary_non_ok_outcome_that_ranks_as_a_failure_for_eviction() {
    let e = Entry {
        seq: 0, ts_ms: 0, kind: Kind::Span, class: Class::Leaf, subsystem: Subsystem::Mcp, op: String::new(), fields: vec![], depth: 0, thread: 0, outcome: Some(Outcome::Cancelled), dur_us: 0,
    };
    assert!(eviction_rank(&e) >= 2, "a cancelled call is kept in preference to routine successes");
    assert_eq!(serde_json::to_string(&Outcome::Cancelled).unwrap(), "\"cancelled\"");
}

#[test]
fn in_flight_spans_are_listed_by_start_order_whatever_their_per_thread_depth() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4070, 64);
    // The first span begins on this thread; the second begins on another thread, where its depth is 0
    // as well: an async operation that moved threads. A depth ordering would call them equal.
    let first = d.span(Subsystem::Mcp, "outer", Class::Top, &[]);
    std::thread::scope(|sc| {
        sc.spawn(|| std::mem::forget(d.span(Subsystem::Mcp, "inner", Class::Top, &[])));
    });
    std::mem::forget(first);
    let open = d.open_spans();
    assert_eq!(open.iter().map(|o| o.depth).collect::<Vec<_>>(), vec![0, 0], "both are depth 0: they are on different threads");
    let text = render(&load(dir.path(), &|_| Some(false)).unwrap(), unix_ms(), 10);
    let outer = text.find("mcp outer").unwrap();
    let inner = text.find("mcp inner").unwrap();
    assert!(outer < inner, "started first, listed first: {text}");
}

// ── scoped_with / scoped ─────────────────────────────────────────────────

#[test]
fn scoped_with_ends_the_span_through_early_returns_and_question_marks_and_carries_the_judged_fields() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4080, 64);
    fn work(d: &Diag, fail_early: bool) -> Result<u8, String> {
        d.scoped_with(
            Subsystem::Transplant,
            "apply",
            Class::Top,
            &[("start", Val::U64(1))],
            |_span| {
                if fail_early {
                    return Err("early".to_string());
                }
                let n: u8 = "7".parse().map_err(|_| "parse".to_string())?;
                Ok(n)
            },
            |r| match r {
                Ok(n) => (Outcome::Ok, vec![("n", Val::U64(u64::from(*n)))]),
                Err(_) => (Outcome::Failed, Vec::new()),
            },
        )
    }
    assert_eq!(work(&d, false), Ok(7));
    assert_eq!(work(&d, true), Err("early".to_string()));
    let ring = d.snapshot();
    assert_eq!(ring.len(), 2);
    assert_eq!((ring[0].outcome, ring[0].fields.last().unwrap()), (Some(Outcome::Ok), &("n".to_string(), Val::U64(7))));
    assert_eq!(ring[1].outcome, Some(Outcome::Failed), "a return inside the closure still ends the span");
    assert_eq!(ring[0].fields[0], ("start".to_string(), Val::U64(1)), "fields known at the start come first");
    assert!(d.open_spans().is_empty());
}

#[test]
fn a_panic_inside_a_scoped_body_ends_the_span_panicked_and_the_value_is_never_judged() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4081, 64);
    let judged = std::cell::Cell::new(false);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        d.scoped_with(Subsystem::Restore, "execute_writes", Class::Top, &[], |_s| -> u8 { panic!("boom") }, |_v| {
            judged.set(true);
            (Outcome::Ok, Vec::new())
        })
    }));
    assert!(r.is_err());
    assert!(!judged.get());
    assert_eq!(d.snapshot().pop().unwrap().outcome, Some(Outcome::Panicked));
}

#[test]
fn the_free_scoped_functions_just_run_the_body_when_diagnostics_are_off() {
    // No test in this binary calls `init`.
    assert!(global().is_none());
    let mut called = 0;
    let r: Result<u8, ()> = scoped(Subsystem::Snapshot, "save_new", Class::Top, &[], |span| {
        called += 1;
        span.set("x", Val::U64(1));
        Ok(3)
    });
    assert_eq!((r, called), (Ok(3), 1));
    let v = scoped_with(Subsystem::Snapshot, "x", Class::Top, &[], |_| 9u8, |_| panic!("judge must not run when diagnostics are off"));
    assert_eq!(v, 9);
}

#[test]
fn the_report_names_a_finished_process_by_the_role_in_its_start_event_not_by_diag() {
    let dir = tempdir().unwrap();
    let mut c = Config::new(dir.path().to_path_buf(), Subsystem::Gui);
    c.pid = 4090;
    let d = Diag::new(c).unwrap();
    d.event(Subsystem::Diag, OP_START, &[("role", Val::text("gui"))]);
    d.shutdown(0);
    let text = render(&load(dir.path(), &|_| Some(false)).unwrap(), unix_ms(), 10);
    assert!(text.starts_with("pid 4090 (gui):"), "{text}");
}
