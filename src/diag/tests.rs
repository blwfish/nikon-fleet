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

/// [`load`] with no OS start-time lookup: the tests use made-up pids, and a real process may hold the same number.
fn load_t(dir: &Path, alive: &dyn Fn(u32) -> Option<bool>) -> std::io::Result<Vec<ProcessTrace>> {
    load_with(dir, alive, &|_| None)
}

fn trace_of(dir: &Path, pid: u32, alive: Option<bool>) -> ProcessTrace {
    load_t(dir, &|_| alive).unwrap().into_iter().find(|t| t.pid == pid).unwrap_or_else(|| panic!("no files for pid {pid}"))
}

fn leaf_ok(d: &Diag, op: &str) {
    d.span(Subsystem::MaidSdk, op, Class::Leaf, &[]).finish(Outcome::Ok);
}

// ── vocabulary ───────────────────────────────────────────────────────────

/// Every subsystem. Test-only: the production code needs no list of them. The match below has no
/// wildcard, so adding a variant breaks this until the list and the count are updated too.
const SUBSYSTEMS: [Subsystem; 11] = [
    Subsystem::Cli,
    Subsystem::Gui,
    Subsystem::Mcp,
    Subsystem::Usb,
    Subsystem::MaidSdk,
    Subsystem::Snapshot,
    Subsystem::Transplant,
    Subsystem::Restore,
    Subsystem::Firmware,
    Subsystem::Lock,
    Subsystem::Diag,
];

fn subsystem_index(s: Subsystem) -> usize {
    match s {
        Subsystem::Cli => 0,
        Subsystem::Gui => 1,
        Subsystem::Mcp => 2,
        Subsystem::Usb => 3,
        Subsystem::MaidSdk => 4,
        Subsystem::Snapshot => 5,
        Subsystem::Transplant => 6,
        Subsystem::Restore => 7,
        Subsystem::Firmware => 8,
        Subsystem::Lock => 9,
        Subsystem::Diag => 10,
    }
}

/// Every outcome; test-only, guarded like [`SUBSYSTEMS`].
const OUTCOMES: [Outcome; 8] = [
    Outcome::Ok,
    Outcome::Failed,
    Outcome::Timeout,
    Outcome::Stalled,
    Outcome::Indeterminate,
    Outcome::Panicked,
    Outcome::Abandoned,
    Outcome::Cancelled,
];

fn outcome_index(o: Outcome) -> usize {
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

#[test]
fn the_hand_written_names_are_the_serde_names_and_the_test_lists_cover_every_variant() {
    // Two spellings of one vocabulary (as_str and serde) must not drift.
    for s in SUBSYSTEMS {
        assert_eq!(serde_json::to_string(&s).unwrap(), format!("\"{}\"", s.as_str()), "{s:?}");
    }
    for o in OUTCOMES {
        assert_eq!(serde_json::to_string(&o).unwrap(), format!("\"{}\"", o.as_str()), "{o:?}");
    }
    let mut idx: Vec<usize> = SUBSYSTEMS.iter().map(|s| subsystem_index(*s)).collect();
    idx.sort_unstable();
    assert_eq!(idx, (0..SUBSYSTEMS.len()).collect::<Vec<_>>());
    let mut idx: Vec<usize> = OUTCOMES.iter().map(|o| outcome_index(*o)).collect();
    idx.sort_unstable();
    assert_eq!(idx, (0..OUTCOMES.len()).collect::<Vec<_>>());
    // The two names version 1 had and nothing ever recorded under are gone, not defaulted to something else.
    assert!(serde_json::from_str::<Subsystem>("\"ptp_ip\"").is_err());
    assert!(serde_json::from_str::<Subsystem>("\"settings\"").is_err());
    // An unknown name is refused, not defaulted.
    assert!(serde_json::from_str::<Subsystem>("\"usbb\"").is_err());
    assert!(serde_json::from_str::<Outcome>("\"success\"").is_err());
}

#[test]
fn val_text_is_cut_only_above_the_limit_and_the_cut_is_a_typed_variant_with_the_original_length() {
    // At the limit: whole.
    assert_eq!(Val::text(&"a".repeat(TEXT_MAX_CHARS)), Val::Text("a".repeat(TEXT_MAX_CHARS)));
    // One above: cut to the limit, nothing appended to the text, the original length recorded.
    let one_over = Val::text(&"a".repeat(TEXT_MAX_CHARS + 1));
    assert_eq!(one_over, Val::TruncatedText { text: "a".repeat(TEXT_MAX_CHARS), original_chars: TEXT_MAX_CHARS as u64 + 1 });
    // One below: whole.
    assert_eq!(Val::text(&"a".repeat(TEXT_MAX_CHARS - 1)), Val::Text("a".repeat(TEXT_MAX_CHARS - 1)));
    // The limit counts characters, not bytes: 256 two-byte characters are not cut, 257 are.
    assert_eq!(Val::text(&"\u{e9}".repeat(TEXT_MAX_CHARS)), Val::Text("\u{e9}".repeat(TEXT_MAX_CHARS)));
    assert!(matches!(Val::text(&"\u{e9}".repeat(TEXT_MAX_CHARS + 1)), Val::TruncatedText { original_chars, .. } if original_chars == TEXT_MAX_CHARS as u64 + 1));
    assert_eq!(Val::text(""), Val::Text(String::new()));
    // A text that really ends in the string version 1 appended is just text: only the variant says "cut".
    let lookalike = format!("{} …[truncated]", "b".repeat(10));
    assert_eq!(Val::text(&lookalike), Val::Text(lookalike.clone()));
    // Both variants read back as text; a number does not.
    assert_eq!(one_over.as_text().map(str::len), Some(TEXT_MAX_CHARS));
    assert_eq!(Val::text("x").as_text(), Some("x"));
    assert_eq!(Val::U64(1).as_text(), None);
    // The marker survives a trip through the file format, and the report says the value was cut.
    let json = serde_json::to_string(&one_over).unwrap();
    assert!(json.contains("\"truncated_text\"") && json.contains("original_chars"), "{json}");
    assert_eq!(serde_json::from_str::<Val>(&json).unwrap(), one_over);
    assert!(one_over.display().contains(&format!("{} chars in all", TEXT_MAX_CHARS + 1)), "{}", one_over.display());
}

#[test]
fn a_cut_text_in_a_span_reaches_the_ring_file_and_the_report_as_a_cut() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4100, 64);
    d.span(Subsystem::Cli, "c", Class::Top, &[("path", Val::text(&"p".repeat(TEXT_MAX_CHARS * 2)))]).finish(Outcome::Ok);
    let t = trace_of(dir.path(), 4100, Some(false));
    assert!(matches!(field_of(&t.entries[0].fields, "path"), Some(Val::TruncatedText { original_chars, .. }) if *original_chars == TEXT_MAX_CHARS as u64 * 2));
    let text = render(&[t], unix_ms(), 10);
    assert!(text.contains(&format!("{} chars in all", TEXT_MAX_CHARS * 2)), "{text}");
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
        let _s = d.span(Subsystem::Lock, "load", Class::Top, &[]);
    }
    assert_eq!(d.snapshot().last().unwrap().outcome, Some(Outcome::Abandoned));
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _s = d.span(Subsystem::Lock, "save", Class::Top, &[]);
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
    for o in OUTCOMES.into_iter().filter(|o| *o != Outcome::Ok) {
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
    // Not through `Diag::new` (which asks the OS whether these made-up pids are running): nothing here is.
    fs::File::create(ring_path(dir.path(), 9999)).unwrap();
    prune_with(dir.path(), 9999, KEEP_PROCESSES, &|_| false);
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
        let t = load_t(dir.path(), &|_| alive).unwrap();
        t.into_iter().find(|t| t.pid == 4020).unwrap().verdict()
    };
    assert_eq!(verdict(Some(false)), Verdict::GoneWithSpansOpen);
    assert_eq!(verdict(Some(true)), Verdict::RunningWithSpansOpen);
    assert_eq!(verdict(None), Verdict::SpansOpenLivenessUnknown);

    // A second process that finished everything and exited cleanly.
    let e = mk(dir.path(), 4021, 64);
    leaf_ok(&e, "x");
    e.shutdown(3);
    let v = |alive| load_t(dir.path(), &|_| alive).unwrap().into_iter().find(|t| t.pid == 4021).unwrap().verdict();
    for alive in [Some(false), Some(true), None] {
        assert_eq!(v(alive), Verdict::ExitedCleanly { code: 3 }, "{alive:?}");
    }

    // A third that recorded entries but no exit, and is gone: killed between operations.
    let f = mk(dir.path(), 4022, 64);
    leaf_ok(&f, "x");
    let v = |alive| load_t(dir.path(), &|_| alive).unwrap().into_iter().find(|t| t.pid == 4022).unwrap().verdict();
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
        let t = load_t(dir.path(), &|_| Some(false)).unwrap().pop().unwrap();
        assert_eq!(t.verdict(), Verdict::ExitedCleanly { code: i64::from(code) });
    }
}

#[test]
fn a_hang_before_shutdown_is_still_a_hang_and_shutdown_twice_records_one_exit() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4031, 64);
    std::mem::forget(d.span(Subsystem::MaidSdk, "disconnect", Class::Leaf, &[]));
    // Never reached shutdown: the report says so.
    let t = load_t(dir.path(), &|_| Some(true)).unwrap().pop().unwrap();
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
    let traces = load_t(dir.path(), &|_| Some(false)).unwrap();
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
    let t = load_t(dir.path(), &|_| Some(false)).unwrap().into_iter().find(|t| t.pid == 4024).unwrap();
    assert_eq!((t.entries.len(), t.malformed_lines), (2, 1));
    assert!(render(&[t], unix_ms(), 10).contains("1 unreadable line(s)"));
}

#[test]
fn an_unparseable_inflight_file_is_reported_as_a_problem_not_as_nothing_in_flight() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4025, 64);
    leaf_ok(&d, "a");
    fs::write(inflight_path(dir.path(), 4025), b"{not json").unwrap();
    let t = load_t(dir.path(), &|_| Some(false)).unwrap().into_iter().find(|t| t.pid == 4025).unwrap();
    assert!(t.inflight.is_none());
    assert!(t.read_problems.iter().any(|p| p.contains("did not parse")), "{:?}", t.read_problems);
    // Not "killed between operations" / "exited cleanly": the verdict says it does not know.
    assert_eq!(t.verdict(), Verdict::InflightUnreadable);
    let text = render(&[t], unix_ms(), 10);
    assert!(text.contains("problem:") && text.contains("in-flight file could not be read"), "{text}");
}

#[test]
fn a_collapsed_run_in_the_report_shows_the_seq_range_and_the_first_and_the_last_members_fields() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4050, 64);
    for i in 0..3u64 {
        d.span(Subsystem::MaidSdk, "read", Class::Leaf, &[("cap", Val::U64(i))]).finish(Outcome::Ok);
    }
    d.span(Subsystem::MaidSdk, "write", Class::Leaf, &[("cap", Val::U64(9))]).finish(Outcome::Ok);
    let text = render(&load_t(dir.path(), &|_| Some(false)).unwrap(), unix_ms(), 10);
    assert!(text.contains("#0..#2 maid_sdk read ok x3"), "the run's seq range, not only its first seq: {text}");
    assert!(text.contains("first: cap=0 last: cap=2"), "the member nearest a hang is not dropped: {text}");
    assert!(!text.contains("cap=1"), "the middle members are summarised, not listed: {text}");
    assert!(text.contains("#3 maid_sdk write ok (") && !text.contains("write ok x"), "a single entry has one seq, no count and no 'first': {text}");
}

#[test]
fn a_missing_directory_is_an_empty_report_and_an_empty_one_says_so() {
    let dir = tempdir().unwrap();
    let t = load_t(&dir.path().join("nope"), &|_| None).unwrap();
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
    let runs = aggregated(&v);
    let counts: Vec<u32> = runs.iter().map(|r| r.count).collect();
    assert_eq!(counts, vec![5, 1, 2, 1, 1, 1]);
    assert_eq!(runs[0].total_us, 5000, "the total duration is the sum");
    assert_eq!((runs[0].first.seq, runs[0].last.seq), (0, 4));
    assert_eq!((runs[2].first.seq, runs[2].last.seq), (6, 7));
    assert_eq!((runs[1].first.seq, runs[1].last.seq), (5, 5), "a run of one has the same first and last");
}

#[test]
fn a_completed_entry_line_says_when_it_began_on_which_thread_its_class_and_depth() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4051, 64);
    let outer = d.span(Subsystem::Cli, "outer", Class::Top, &[]);
    d.span(Subsystem::MaidSdk, "inner", Class::Leaf, &[]).finish(Outcome::Failed);
    outer.finish(Outcome::Ok);
    let ts = d.snapshot().iter().map(|e| e.ts_ms).max().unwrap();
    let text = render(&load_t(dir.path(), &|_| Some(false)).unwrap(), ts + 1500, 10);
    let inner = text.lines().find(|l| l.contains(" inner ")).unwrap_or_else(|| panic!("{text}"));
    assert!(inner.contains("leaf, depth 1]") && inner.contains("thread ") && inner.contains(" ago,"), "{inner}");
    let outer_line = text.lines().find(|l| l.contains(" outer ")).unwrap_or_else(|| panic!("{text}"));
    assert!(outer_line.contains("top, depth 0]"), "{outer_line}");
}

#[test]
fn a_key_given_at_the_start_and_again_at_the_finish_is_one_field_with_the_latest_value() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4052, 64);
    let s = d.span(Subsystem::Cli, "c", Class::Top, &[("phase", Val::text("begin")), ("n", Val::U64(1))]);
    s.set("phase", Val::text("middle"));
    s.finish_with(Outcome::Ok, &[("phase", Val::text("end")), ("extra", Val::U64(2))]);
    let e = d.snapshot().pop().unwrap();
    let phases: Vec<&Val> = e.fields.iter().filter(|(k, _)| k == "phase").map(|(_, v)| v).collect();
    assert_eq!(phases, vec![&Val::text("end")], "{:?}", e.fields);
    assert_eq!(e.fields.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), vec!["phase", "n", "extra"], "position kept, new keys appended");
    // Duplicates inside one begin call collapse the same way (the later value wins).
    let s = d.span(Subsystem::Cli, "d", Class::Top, &[("k", Val::U64(1)), ("k", Val::U64(2))]);
    assert_eq!(d.open_spans().last().unwrap().fields, vec![("k".to_string(), Val::U64(2))]);
    s.finish(Outcome::Ok);
}

#[test]
fn a_span_closed_by_shutdown_gets_a_monotonic_duration_not_a_wall_clock_difference() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4053, 64);
    std::mem::forget(d.span(Subsystem::Cli, "cmd", Class::Top, &[]));
    // A clock that stepped far back after the span began: the wall-clock difference would be 0 (saturated)
    // or enormous; the monotonic one is the time it really ran.
    d.lock().open[0].ts_ms = unix_ms() + 3_600_000;
    std::thread::sleep(std::time::Duration::from_millis(20));
    d.shutdown(0);
    let e = d.snapshot().into_iter().find(|e| e.op == "cmd").unwrap();
    assert!(e.dur_us >= 20_000, "it ran at least 20 ms: {} us", e.dur_us);
    assert!(e.dur_us < 60_000_000, "and nowhere near a wall-clock hour: {} us", e.dur_us);
}

#[test]
fn a_directory_listing_error_fails_the_listing_instead_of_dropping_the_entry() {
    let names = vec![Ok(OsString::from("inflight-5.json")), Ok(OsString::from("ring-9.jsonl")), Ok(OsString::from("notes.txt"))];
    assert_eq!(pids_in(names.into_iter()).unwrap().into_iter().collect::<Vec<_>>(), vec![5, 9]);
    let broken = vec![Ok(OsString::from("inflight-5.json")), Err(std::io::Error::other("stale handle")), Ok(OsString::from("ring-9.jsonl"))];
    let err = pids_in(broken.into_iter()).unwrap_err();
    assert!(err.to_string().contains("stale handle"), "{err}");
    // The empty listing is an empty set, not an error.
    assert!(pids_in(Vec::new().into_iter()).unwrap().is_empty());
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
    // Unix (kill with signal 0) and Windows (OpenProcess / GetExitCodeProcess) both answer.
    #[cfg(any(unix, windows))]
    {
        assert_eq!(pid_alive(std::process::id()), Some(true));
        assert_eq!(pid_alive(0x7fff_fffe), Some(false));
    }
    #[cfg(not(any(unix, windows)))]
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
    let text = render(&load_t(dir.path(), &|_| Some(false)).unwrap(), unix_ms(), 10);
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
    let text = render(&load_t(dir.path(), &|_| Some(false)).unwrap(), unix_ms(), 10);
    assert!(text.starts_with("pid 4090 (gui):"), "{text}");
}

// ═════════════════════════════════════════════════════════════════════════
// Remediation pass (nikon-fleet-20261105-b7d3): shutdown semantics, liveness, tolerance, thresholds
// ═════════════════════════════════════════════════════════════════════════

fn exit_entry(d: &Diag) -> Entry {
    d.snapshot().into_iter().rev().find(|e| e.op == OP_EXIT).expect("an exit entry")
}

fn field_val<'a>(e: &'a Entry, key: &str) -> Option<&'a Val> {
    e.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

// ── #02: a span another thread had open when the process is ended is abandoned, not ok ─────────────

#[test]
fn shutdown_from_another_thread_abandons_the_spans_the_exit_interrupts_whatever_their_class_and_code() {
    // The watchdog case: the main thread is stuck inside `disconnect_device` (a Top span) under the command
    // span (Top); the watchdog thread calls shutdown(0). Neither may read `ok`.
    for code in [0, 1] {
        let dir = tempdir().unwrap();
        let d = mk(dir.path(), 4100, 64);
        std::thread::scope(|sc| {
            sc.spawn(|| {
                std::mem::forget(d.span(Subsystem::Cli, "command", Class::Top, &[]));
                std::mem::forget(d.span(Subsystem::MaidSdk, "disconnect_device", Class::Top, &[]));
            });
        });
        d.shutdown(code); // this thread plays the watchdog
        let ring = d.snapshot();
        for op in ["command", "disconnect_device"] {
            assert_eq!(ring.iter().find(|e| e.op == op).unwrap().outcome, Some(Outcome::Abandoned), "{op}, code {code}");
        }
        assert_eq!(field_val(&exit_entry(&d), FIELD_CUT_OFF), Some(&Val::U64(2)));
        let t = trace_of(dir.path(), 4100, Some(false));
        assert_eq!(t.verdict(), Verdict::ExitedWithWorkCutOff { code: i64::from(code), cut_off: 2 }, "not a clean exit");
        let text = render(&[t], unix_ms(), 10);
        assert!(text.contains("cut work short") && text.contains("2 span(s)"), "{text}");
        assert!(!text.contains("exited cleanly"), "{text}");
    }
}

#[test]
fn shutdown_still_finishes_the_calling_threads_own_spans_by_code_and_counts_only_other_threads_as_cut_off() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4101, 64);
    std::mem::forget(d.span(Subsystem::Gui, "gui", Class::Top, &[])); // the UI thread's own span
    std::thread::scope(|sc| {
        sc.spawn(|| std::mem::forget(d.span(Subsystem::Gui, "cli_child", Class::Top, &[])));
    });
    d.shutdown(0); // the UI thread quits while a child runs on a worker thread
    let ring = d.snapshot();
    assert_eq!(ring.iter().find(|e| e.op == "gui").unwrap().outcome, Some(Outcome::Ok));
    assert_eq!(ring.iter().find(|e| e.op == "cli_child").unwrap().outcome, Some(Outcome::Abandoned));
    assert_eq!(field_val(&exit_entry(&d), FIELD_CUT_OFF), Some(&Val::U64(1)));
    // And with nothing on another thread there is no cut_off field at all and the exit is clean.
    let dir2 = tempdir().unwrap();
    let e = mk(dir2.path(), 4102, 64);
    std::mem::forget(e.span(Subsystem::Cli, "command", Class::Top, &[]));
    e.shutdown(0);
    assert_eq!(field_val(&exit_entry(&e), FIELD_CUT_OFF), None);
    assert_eq!(trace_of(dir2.path(), 4102, Some(false)).verdict(), Verdict::ExitedCleanly { code: 0 });
}

// ── #26: shutdown races, spans after the exit, unmatched ends ───────────────────────────────────────

#[test]
fn a_second_shutdown_waits_for_the_first_to_finish_writing_instead_of_returning_at_once() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4110, 64);
    let finished = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|sc| {
        // Stand in for the winner caught mid-write: it has claimed the exit and still holds the lock.
        let mut guard = d.lock();
        guard.exited = true;
        let second = sc.spawn(|| {
            d.shutdown(0);
            finished.store(true, Ordering::SeqCst);
        });
        std::thread::sleep(std::time::Duration::from_millis(250));
        assert!(!finished.load(Ordering::SeqCst), "the second caller must not get ahead of the first's records");
        drop(guard);
        second.join().unwrap();
    });
    assert!(finished.load(Ordering::SeqCst));
}

#[test]
fn two_threads_shutting_down_at_once_record_one_exit_and_neither_returns_before_it_exists() {
    for round in 0..50u32 {
        let dir = tempdir().unwrap();
        let d = mk(dir.path(), 4111, 64);
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|sc| {
            for code in [0, 9] {
                let (d, barrier) = (&d, &barrier);
                sc.spawn(move || {
                    barrier.wait();
                    d.shutdown(code);
                    assert!(d.snapshot().iter().any(|e| e.op == OP_EXIT), "round {round}: returned before the exit was recorded");
                });
            }
        });
        assert_eq!(d.snapshot().iter().filter(|e| e.op == OP_EXIT).count(), 1);
    }
}

#[test]
fn a_span_begun_after_the_exit_does_not_bring_the_inflight_file_back_and_the_exit_still_reads_as_the_end() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4112, 64);
    d.shutdown(0);
    let late = d.span(Subsystem::Gui, "late_worker", Class::Top, &[]);
    assert!(!inflight_path(dir.path(), 4112).exists(), "no in-flight file after the exit");
    late.set("phase", Val::text("x"));
    assert!(!inflight_path(dir.path(), 4112).exists());
    assert_eq!(trace_of(dir.path(), 4112, Some(false)).verdict(), Verdict::ExitedCleanly { code: 0 });
    late.finish(Outcome::Ok);
    assert!(!inflight_path(dir.path(), 4112).exists());
}

#[test]
fn ending_or_setting_a_span_that_shutdown_already_closed_is_counted_and_adds_no_entry() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4113, 64);
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|sc| {
        sc.spawn(|| {
            let s = d.span(Subsystem::Gui, "worker", Class::Top, &[]);
            barrier.wait(); // begun
            barrier.wait(); // main has shut down
            s.set("phase", Val::text("late"));
            s.finish(Outcome::Ok);
        });
        barrier.wait();
        d.shutdown(0);
        assert_eq!(d.unmatched_ops(), 0);
        barrier.wait();
    });
    assert_eq!(d.unmatched_ops(), 2, "the late set and the late finish");
    let workers: Vec<_> = d.snapshot().into_iter().filter(|e| e.op == "worker").collect();
    assert_eq!(workers.len(), 1, "recorded once, by the shutdown");
    assert_eq!(workers[0].outcome, Some(Outcome::Abandoned));
}

// ── #27: panic hook ─────────────────────────────────────────────────────────────────────────────────

#[test]
fn the_panic_hook_does_not_wait_for_a_lock_held_by_the_panicking_thread() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4120, 64);
    let guard = d.lock(); // a panic raised while this thread was recording
    assert!(!d.try_event(Subsystem::Cli, OP_PANIC, &[]), "reports that it could not record");
    assert_eq!(d.write_failures(), 1, "and counts it");
    drop(guard);
    assert!(d.try_event(Subsystem::Cli, OP_PANIC, &[]));
    assert_eq!(d.snapshot().len(), 1);
}

#[test]
fn a_panic_entry_carries_the_location_and_only_the_length_and_hash_of_the_message() {
    let secret = "called `Result::unwrap()` on an `Err` value: Os { path: \"/Users/me/hunter2\" }";
    let fields = panic_fields("src/x.rs:9", secret);
    let text = format!("{fields:?}");
    assert!(!text.contains("hunter2") && !text.contains("unwrap"), "{text}");
    assert_eq!(fields.iter().map(|(k, _)| *k).collect::<Vec<_>>(), vec!["location", "message_len", "message_hash"]);
    assert_eq!(fields[1].1, Val::Len(secret.len() as u64));
    assert_eq!(fields[2].1, Val::hash(secret.as_bytes()));
}

// ── #28: write failures are surfaced ────────────────────────────────────────────────────────────────

#[test]
fn the_failure_count_reaches_the_inflight_file_and_the_report() {
    let dir = tempdir().unwrap();
    let diag_dir = dir.path().join("d");
    let d = mk(&diag_dir, 4130, 64);
    fs::remove_dir_all(&diag_dir).unwrap();
    let first = d.span(Subsystem::Usb, "ptp_txn", Class::Leaf, &[]); // its in-flight write fails
    assert!(d.write_failures() >= 1);
    fs::create_dir_all(&diag_dir).unwrap(); // the disk comes back
    let second = d.span(Subsystem::Usb, "ptp_txn", Class::Leaf, &[]);
    let t = trace_of(&diag_dir, 4130, Some(true));
    assert!(t.write_failures() >= 1, "{}", t.write_failures());
    let text = render(&[t], unix_ms(), 10);
    assert!(text.contains("diagnostics write(s) failed") && text.contains("may be incomplete"), "{text}");
    drop((first, second));
}

#[test]
fn the_failure_count_and_the_eviction_count_are_in_the_exit_entry_only_when_nonzero() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4131, MIN_RING_CAP);
    d.shutdown(0);
    let e = exit_entry(&d);
    assert_eq!((field_val(&e, FIELD_WRITE_FAILURES), field_val(&e, FIELD_EVICTED)), (None, None));

    let dir2 = tempdir().unwrap();
    let f = mk(dir2.path(), 4132, MIN_RING_CAP);
    for _ in 0..MIN_RING_CAP + 3 {
        leaf_ok(&f, "x");
    }
    f.shutdown(0);
    assert_eq!(field_val(&exit_entry(&f), FIELD_EVICTED), Some(&Val::U64(3)), "exactly the entries beyond the capacity");
    let text = render(&[trace_of(dir2.path(), 4132, Some(false))], unix_ms(), 100);
    assert!(text.contains("3 older entries were evicted"), "{text}");
}

#[test]
fn a_ring_file_that_cannot_be_reopened_after_compaction_is_reopened_by_the_next_entry_and_nothing_is_lost() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4133, MIN_RING_CAP);
    leaf_ok(&d, "before");
    d.lock().ring_file = None; // what a failed reopen after a compaction leaves
    leaf_ok(&d, "after");
    assert!(d.lock().ring_file.is_some(), "reopened");
    let ops: Vec<String> = ring_lines(dir.path(), 4133).iter().map(|l| serde_json::from_str::<Entry>(l).unwrap().op).collect();
    assert_eq!(ops, vec!["before", "after"], "the entry written while the handle was missing is on disk");
}

#[cfg(unix)]
#[test]
fn a_compaction_whose_replacement_cannot_be_written_is_counted_and_appending_to_the_old_file_continues() {
    use std::os::unix::fs::PermissionsExt;
    // Root ignores directory modes, so the failure cannot be induced.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let dir = tempdir().unwrap();
    let cap = MIN_RING_CAP;
    let d = mk(dir.path(), 4134, cap);
    for _ in 0..2 * cap - 1 {
        d.event(Subsystem::Cli, "x", &[]);
    }
    // No new files can be created in the directory, but the open ring file can still be appended to. Events, not
    // spans: a span would also try (and fail) to write its in-flight file.
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();
    d.event(Subsystem::Cli, "at_threshold", &[]); // reaches 2*cap lines: compaction runs and its temp file cannot be created
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(d.write_failures(), 1, "the failed compaction is counted, once");
    assert_eq!(ring_lines(dir.path(), 4134).len(), 2 * cap, "the file was not replaced");
    d.event(Subsystem::Cli, "after", &[]);
    let last: Entry = serde_json::from_str(ring_lines(dir.path(), 4134).last().unwrap()).unwrap();
    assert_eq!(last.op, "after", "still appending to the file a reader will open");
}

#[test]
fn a_failure_in_both_the_ring_file_and_the_trace_file_counts_twice() {
    let dir = tempdir().unwrap();
    let trace = dir.path().join("t.jsonl");
    let mut c = Config::new(dir.path().to_path_buf(), Subsystem::Cli);
    c.pid = 4135;
    c.trace_file = Some(trace);
    let d = Diag::new(c).unwrap();
    // Replace both handles with read-only ones: every write to either fails.
    let ro = |p: &Path| fs::File::open(p).unwrap();
    {
        let mut st = d.lock();
        st.ring_file = Some(ro(&ring_path(dir.path(), 4135)));
        st.trace_file = Some(ro(&dir.path().join("t.jsonl")));
    }
    leaf_ok(&d, "x");
    assert_eq!(d.write_failures(), 2, "the trace file's failure is not hidden by the ring file's");
}

// ── #20: prune keeps live processes, and its boundary is pinned ─────────────────────────────────────

fn make_ring_files(dir: &Path, pids: &[u32]) {
    for (i, pid) in pids.iter().enumerate() {
        let f = fs::File::create(ring_path(dir, *pid)).unwrap();
        f.set_modified(UNIX_EPOCH + std::time::Duration::from_secs(1_000_000 + i as u64)).unwrap();
    }
}

fn remaining_pids(dir: &Path) -> Vec<u32> {
    let mut v: Vec<u32> = fs::read_dir(dir).unwrap().flatten().filter_map(|e| pid_of_file_name(&e.file_name().to_string_lossy())).collect();
    v.sort_unstable();
    v.dedup();
    v
}

#[test]
fn prune_removes_nothing_at_or_below_the_keep_count_and_exactly_the_oldest_one_above_it() {
    let keep = 5;
    for (total, removed) in [(keep - 1, 0usize), (keep, 0), (keep + 1, 1), (keep + 2, 2)] {
        let dir = tempdir().unwrap();
        // pids 100.. oldest first; the last is this process.
        let pids: Vec<u32> = (0..total as u32).map(|i| 100 + i).collect();
        make_ring_files(dir.path(), &pids);
        let own = *pids.last().unwrap();
        prune_with(dir.path(), own, keep, &|_| false);
        let left = remaining_pids(dir.path());
        assert_eq!(left.len(), total - removed, "total {total}");
        assert_eq!(left, pids[removed..].to_vec(), "the oldest {removed} went, total {total}");
    }
}

#[test]
fn prune_never_removes_a_running_process_however_old_so_an_idle_server_keeps_its_files() {
    let dir = tempdir().unwrap();
    let pids: Vec<u32> = (0..10).map(|i| 200 + i).collect();
    make_ring_files(dir.path(), &pids);
    fs::write(inflight_path(dir.path(), 200), b"{}").unwrap();
    // 200 (the oldest) and 201 are running; this process is the newest.
    prune_with(dir.path(), 209, 5, &|p| p == 200 || p == 201);
    let left = remaining_pids(dir.path());
    assert!(left.contains(&200) && left.contains(&201), "{left:?}");
    assert!(inflight_path(dir.path(), 200).exists());
    assert_eq!(left, vec![200, 201, 207, 208, 209], "the live ones count toward the keep count: the oldest finished ones went instead");
}

#[test]
fn prune_through_diag_new_spares_this_real_running_process_even_when_it_is_the_oldest() {
    let dir = tempdir().unwrap();
    let live = std::process::id();
    let mut pids = vec![live];
    pids.extend((0..KEEP_PROCESSES as u32 + 3).map(|i| 3_000_000_000 + i)); // pids no machine has
    make_ring_files(dir.path(), &pids);
    let _d = mk(dir.path(), 3_100_000_000, 64);
    assert!(remaining_pids(dir.path()).contains(&live), "the running process's file survived 23 newer ones");
}

#[test]
fn a_temp_file_left_by_a_dead_writer_is_pruned_and_one_whose_writer_is_running_or_unrelated_is_not() {
    let dir = tempdir().unwrap();
    for name in [".ring-7.jsonl.8.0.tmp", ".inflight-7.json.9.3.tmp", ".ring-7.jsonl.10.0.tmp", ".ring-x.jsonl.8.0.tmp", ".other.8.0.tmp", ".ring-7.jsonl.12.0.tmp"] {
        fs::write(dir.path().join(name), b"x").unwrap();
    }
    // 9 is running, 12 is this process; 8 and 10 are dead.
    prune_with(dir.path(), 12, 20, &|p| p == 9);
    let mut left: Vec<String> = fs::read_dir(dir.path()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    left.sort();
    assert_eq!(left, vec![".inflight-7.json.9.3.tmp", ".other.8.0.tmp", ".ring-7.jsonl.12.0.tmp", ".ring-x.jsonl.8.0.tmp"]);
}

#[test]
fn the_owner_pid_of_a_temp_file_name_parses_only_our_own_shape() {
    assert_eq!(temp_owner_pid(".ring-1.jsonl.9.0.tmp"), Some(9));
    assert_eq!(temp_owner_pid(".inflight-1.json.4294967295.77.tmp"), Some(u32::MAX));
    for bad in ["ring-1.jsonl.9.0.tmp", ".ring-1.jsonl.9.0", ".ring-1.jsonl.9.tmp", ".ring-1.jsonl.x.0.tmp", ".ring-1.jsonl.9.y.tmp", ".foo.9.0.tmp", ".ring-1.json.9.0.tmp"] {
        assert_eq!(temp_owner_pid(bad), None, "{bad}");
    }
}

// ── #21: pid reuse ──────────────────────────────────────────────────────────────────────────────────

#[test]
fn a_new_process_does_not_inherit_an_inflight_file_a_dead_process_with_the_same_pid_left_behind() {
    let dir = tempdir().unwrap();
    let old = mk(dir.path(), 4140, 64);
    std::mem::forget(old.span(Subsystem::MaidSdk, "disconnect_device", Class::Top, &[])); // the earlier process hung and was killed
    assert!(inflight_path(dir.path(), 4140).exists());
    let _new = mk(dir.path(), 4140, 64); // the pid is reused
    assert!(!inflight_path(dir.path(), 4140).exists(), "its spans are not the new process's");
    assert_eq!(trace_of(dir.path(), 4140, Some(true)).verdict(), Verdict::RunningIdle);
}

#[test]
fn the_pid_reuse_threshold_is_strictly_more_than_the_slack_after_the_recorded_start() {
    let rec = 1_000_000;
    assert!(!pid_recycled(rec - 1, rec), "started before the record: the same process");
    assert!(!pid_recycled(rec, rec));
    assert!(!pid_recycled(rec + PID_REUSE_SLACK_MS, rec), "exactly the slack: still the same");
    assert!(pid_recycled(rec + PID_REUSE_SLACK_MS + 1, rec), "one millisecond past it: another process");
    assert!(!pid_recycled(5, u64::MAX), "no overflow");
}

#[test]
fn a_running_pid_that_started_after_the_files_were_written_is_reported_as_another_process() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4141, 64);
    std::mem::forget(d.span(Subsystem::MaidSdk, "read", Class::Leaf, &[]));
    let recorded = trace_of(dir.path(), 4141, Some(true)).inflight.unwrap().process_started_ms;
    let t = |os: u64| load_with(dir.path(), &|_| Some(true), &|_| Some(os)).unwrap().pop().unwrap();
    let at = t(recorded + PID_REUSE_SLACK_MS);
    assert_eq!((at.pid_recycled, at.verdict()), (false, Verdict::RunningWithSpansOpen));
    let above = t(recorded + PID_REUSE_SLACK_MS + 1);
    assert_eq!((above.pid_recycled, above.verdict()), (true, Verdict::GoneWithSpansOpen));
    assert!(render(&[above], unix_ms(), 10).contains("it is a different process"));
    // The OS cannot say: the answer from `alive` stands.
    let unknown = load_with(dir.path(), &|_| Some(true), &|_| None).unwrap().pop().unwrap();
    assert_eq!(unknown.verdict(), Verdict::RunningWithSpansOpen);
    // A process that is not running is not second-guessed.
    let gone = load_with(dir.path(), &|_| Some(false), &|_| Some(recorded + 10 * PID_REUSE_SLACK_MS)).unwrap().pop().unwrap();
    assert!(!gone.pid_recycled);
}

#[test]
fn an_exit_entry_without_a_usable_code_is_not_read_as_a_clean_exit_with_code_zero() {
    let write_exit = |pid: u32, fields: &str| {
        let dir = tempdir().unwrap();
        fs::write(
            ring_path(dir.path(), pid),
            format!("{{\"seq\":0,\"ts_ms\":1,\"kind\":\"event\",\"class\":\"top\",\"subsystem\":\"cli\",\"op\":\"exit\",\"fields\":{fields},\"depth\":0,\"thread\":1,\"outcome\":null,\"dur_us\":0}}\n"),
        )
        .unwrap();
        trace_of(dir.path(), pid, Some(false)).verdict()
    };
    assert_eq!(write_exit(4150, "[]"), Verdict::ExitCodeUnknown, "no code at all");
    assert_eq!(write_exit(4151, "[[\"code\",{\"t\":\"u64\",\"v\":0}]]"), Verdict::ExitCodeUnknown, "a code of the wrong type");
    assert_eq!(write_exit(4152, "[[\"code\",{\"t\":\"i64\",\"v\":0}]]"), Verdict::ExitedCleanly { code: 0 }, "an honest zero is still zero");
}

#[test]
fn an_event_named_exit_by_a_subsystem_that_is_not_a_process_role_is_not_an_exit() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4153, 64);
    d.event(Subsystem::Usb, OP_EXIT, &[(FIELD_CODE, Val::I64(0))]);
    assert_eq!(trace_of(dir.path(), 4153, Some(false)).verdict(), Verdict::GoneWithoutExit);
    d.event(Subsystem::Cli, OP_EXIT, &[(FIELD_CODE, Val::I64(0))]);
    assert_eq!(trace_of(dir.path(), 4153, Some(false)).verdict(), Verdict::ExitedCleanly { code: 0 });
    for s in SUBSYSTEMS {
        assert_eq!(s.is_role(), matches!(s, Subsystem::Cli | Subsystem::Gui | Subsystem::Mcp), "{s:?}");
    }
}

// ── #22 / #23: read problems and format tolerance ───────────────────────────────────────────────────

#[test]
fn every_read_problem_is_kept_not_only_the_last() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4160, 64);
    leaf_ok(&d, "a");
    fs::write(inflight_path(dir.path(), 4160), b"{not json").unwrap();
    fs::remove_file(ring_path(dir.path(), 4160)).unwrap();
    fs::create_dir(ring_path(dir.path(), 4160)).unwrap(); // a ring "file" that cannot be read
    let t = trace_of(dir.path(), 4160, Some(false));
    assert_eq!(t.read_problems.len(), 2, "{:?}", t.read_problems);
    assert!(t.read_problems[0].contains("did not parse") && t.read_problems[1].contains("could not be read"), "{:?}", t.read_problems);
}

#[test]
fn an_inflight_file_with_one_value_this_build_does_not_know_keeps_the_spans_it_can_read_and_counts_the_rest_as_open() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4161, 64);
    std::mem::forget(d.span(Subsystem::Usb, "ptp_txn", Class::Leaf, &[]));
    let path = inflight_path(dir.path(), 4161);
    let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let mut known = v["spans"][0].clone();
    known["op"] = "known".into();
    let mut unknown = v["spans"][0].clone();
    unknown["subsystem"] = "frobnicate".into(); // a subsystem a newer build added
    v["spans"] = serde_json::json!([known, unknown]);
    fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
    let t = trace_of(dir.path(), 4161, Some(true));
    assert_eq!(t.inflight.as_ref().unwrap().spans.len(), 1);
    assert_eq!(t.unreadable_open_spans, 1);
    assert_eq!(t.verdict(), Verdict::RunningWithSpansOpen, "the process still reads as having work open");
    assert!(t.read_problems.iter().any(|p| p.contains("not fully understood") && p.contains("1 open span(s) could not be read")), "{:?}", t.read_problems);
    let text = render(&[t], unix_ms(), 10);
    assert!(text.contains("1 open span(s) in the in-flight file could not be read"), "{text}");
    // Nothing readable and the unknown one is all there was: still open.
    v["spans"] = serde_json::json!([unknown]);
    fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
    assert_eq!(trace_of(dir.path(), 4161, Some(false)).verdict(), Verdict::GoneWithSpansOpen);
    // Valid JSON with no list of spans at all is unreadable, not "nothing open".
    fs::write(&path, b"{\"pid\":1}").unwrap();
    assert_eq!(trace_of(dir.path(), 4161, Some(false)).verdict(), Verdict::InflightUnreadable);
}

#[test]
fn the_inflight_schema_defaults_to_one_and_a_newer_one_is_named_in_a_problem() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4162, 64);
    std::mem::forget(d.span(Subsystem::Usb, "ptp_txn", Class::Leaf, &[]));
    let path = inflight_path(dir.path(), 4162);
    let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(v["schema"], SCHEMA);
    v.as_object_mut().unwrap().remove("schema"); // a file from before the field existed
    fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
    let t = trace_of(dir.path(), 4162, Some(true));
    assert_eq!((t.inflight.unwrap().schema, t.read_problems.len()), (1, 0));
    v["schema"] = (SCHEMA + 1).into();
    fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
    let t = trace_of(dir.path(), 4162, Some(true));
    assert_eq!(t.inflight.as_ref().unwrap().spans.len(), 1, "still read");
    assert!(t.read_problems.iter().any(|p| p.contains("newer build")), "{:?}", t.read_problems);
}

/// A ring line as the version-1 build wrote it.
fn v1_line(seq: u64, subsystem: &str, fields: &str) -> String {
    format!("{{\"seq\":{seq},\"ts_ms\":5,\"kind\":\"span\",\"class\":\"leaf\",\"subsystem\":\"{subsystem}\",\"op\":\"x\",\"fields\":{fields},\"depth\":0,\"thread\":1,\"outcome\":\"ok\",\"dur_us\":7}}\n")
}

#[test]
fn files_written_by_the_version_1_build_still_load_and_what_the_new_vocabulary_dropped_is_counted() {
    let dir = tempdir().unwrap();
    let pid = 4170;
    // Ring: a line with the in-band truncation mark (still plain text), and lines naming the two subsystems version 2 removed.
    let cut = "{\"t\":\"text\",\"v\":\"aaaa …[truncated]\"}";
    let ring = [v1_line(0, "usb", &format!("[[\"path\",{cut}]]")), v1_line(1, "settings", "[]"), v1_line(2, "ptp_ip", "[]"), v1_line(3, "maid_sdk", "[]")].concat();
    fs::write(ring_path(dir.path(), pid), ring).unwrap();
    // In flight: schema 1 with one span this build reads and one under a removed subsystem.
    let open = |seq: u64, subsystem: &str| format!("{{\"seq\":{seq},\"ts_ms\":5,\"class\":\"top\",\"subsystem\":\"{subsystem}\",\"op\":\"cmd\",\"fields\":[],\"depth\":0,\"thread\":1}}");
    let inflight = format!("{{\"schema\":1,\"pid\":{pid},\"role\":\"cli\",\"process_started_ms\":1,\"spans\":[{},{}]}}", open(4, "usb"), open(5, "ptp_ip"));
    fs::write(inflight_path(dir.path(), pid), inflight).unwrap();

    let t = trace_of(dir.path(), pid, Some(false));
    assert_eq!(t.entries.len(), 2, "the readable lines load");
    assert!(matches!(field_of(&t.entries[0].fields, "path"), Some(Val::Text(s)) if s.ends_with("…[truncated]")), "the old mark is plain text now");
    assert_eq!(t.unrecognised_lines, 2, "the two removed subsystems are counted, not called cut lines");
    assert_eq!(t.malformed_lines, 0);
    assert_eq!(t.inflight.as_ref().unwrap().spans.len(), 1);
    assert_eq!(t.unreadable_open_spans, 1, "an open span under a removed subsystem still counts as open");
    assert_eq!(t.verdict(), Verdict::GoneWithSpansOpen);
    let text = render(&[t], unix_ms(), 10);
    assert!(text.contains("2 line(s) this build does not understand") && text.contains("1 open span(s)"), "{text}");

    // A clean version-1 in-flight file (every span readable) loads with no problem line.
    let pid = 4171;
    let inflight = format!("{{\"schema\":1,\"pid\":{pid},\"role\":\"gui\",\"process_started_ms\":1,\"spans\":[{}]}}", open(0, "usb"));
    fs::write(inflight_path(dir.path(), pid), inflight).unwrap();
    let t = trace_of(dir.path(), pid, Some(true));
    assert_eq!((t.inflight.as_ref().map(|f| f.schema), t.read_problems.len(), t.unreadable_open_spans), (Some(1), 0, 0), "{:?}", t.read_problems);
    // A file this build writes says version 2.
    assert_eq!(SCHEMA, 2);
}

#[test]
fn an_unknown_role_in_a_salvaged_inflight_file_is_a_problem_line_not_a_silent_diag() {
    let dir = tempdir().unwrap();
    let inflight = "{\"schema\":1,\"pid\":4172,\"role\":\"settings\",\"process_started_ms\":1,\"spans\":[{\"seq\":0,\"ts_ms\":5,\"class\":\"top\",\"subsystem\":\"ptp_ip\",\"op\":\"c\",\"fields\":[],\"depth\":0,\"thread\":1}]}";
    fs::write(inflight_path(dir.path(), 4172), inflight).unwrap();
    let t = trace_of(dir.path(), 4172, Some(true));
    assert!(t.read_problems.iter().any(|p| p.contains("role this build does not know")), "{:?}", t.read_problems);
    assert_eq!(t.unreadable_open_spans, 1);
}

#[test]
fn a_ring_line_with_a_value_this_build_does_not_know_is_not_called_a_cut_line() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4163, 64);
    leaf_ok(&d, "a");
    let mut f = fs::OpenOptions::new().append(true).open(ring_path(dir.path(), 4163)).unwrap();
    // Valid JSON, an outcome from a newer build.
    writeln!(f, "{{\"seq\":5,\"ts_ms\":1,\"kind\":\"span\",\"class\":\"leaf\",\"subsystem\":\"usb\",\"op\":\"x\",\"fields\":[],\"depth\":0,\"thread\":1,\"outcome\":\"vanished\",\"dur_us\":0}}").unwrap();
    // And a genuinely cut line.
    write!(f, "{{\"seq\":6,\"ts_ms\":1,\"ki").unwrap();
    let t = trace_of(dir.path(), 4163, Some(false));
    assert_eq!((t.entries.len(), t.unrecognised_lines, t.malformed_lines), (1, 1, 1));
    let text = render(&[t], unix_ms(), 10);
    assert!(text.contains("1 unreadable line(s)") && text.contains("1 line(s) this build does not understand"), "{text}");
}

#[test]
fn a_line_cut_in_the_middle_of_a_character_costs_that_line_not_the_whole_file() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4164, 64);
    d.span(Subsystem::Cli, "caf\u{e9}", Class::Leaf, &[]).finish(Outcome::Ok);
    leaf_ok(&d, "b");
    let mut f = fs::OpenOptions::new().append(true).open(ring_path(dir.path(), 4164)).unwrap();
    f.write_all(b"{\"seq\":9,\"op\":\"caf\xc3").unwrap(); // the first byte of a two-byte character, then the kill
    let t = trace_of(dir.path(), 4164, Some(false));
    assert_eq!((t.entries.len(), t.malformed_lines), (2, 1), "the two good lines load; the cut one is counted");
    assert!(t.read_problems.is_empty(), "{:?}", t.read_problems);
    assert_eq!(t.entries[0].op, "caf\u{e9}");
}

// ── #49: directory hygiene ──────────────────────────────────────────────────────────────────────────

#[cfg(unix)]
#[test]
fn an_existing_directory_keeps_its_mode_and_a_new_one_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let dir = tempdir().unwrap();
    let shared = dir.path().join("shared");
    fs::create_dir(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
    private_dir(&shared).unwrap();
    assert_eq!(mode(&shared), 0o755, "a directory the user supplied is not re-moded");
    let fresh = dir.path().join("fresh");
    private_dir(&fresh).unwrap();
    assert_eq!(mode(&fresh), 0o700);
}

// ── #53 / #88: ordering and the report's formatting thresholds ──────────────────────────────────────

#[test]
fn processes_are_listed_oldest_first_by_start_time_not_by_pid() {
    let dir = tempdir().unwrap();
    let first = mk(dir.path(), 4172, 64); // the higher pid starts first
    first.event(Subsystem::Cli, "x", &[]);
    std::thread::sleep(std::time::Duration::from_millis(30));
    let second = mk(dir.path(), 4171, 64);
    second.event(Subsystem::Cli, "x", &[]);
    let pids: Vec<u32> = load_t(dir.path(), &|_| None).unwrap().iter().map(|t| t.pid).collect();
    assert_eq!(pids, vec![4172, 4171]);
}

#[test]
fn age_text_switches_to_seconds_at_exactly_one_thousand_milliseconds() {
    assert_eq!(age_text(0), "0 ms");
    assert_eq!(age_text(999), "999 ms");
    assert_eq!(age_text(1000), "1.0 s");
    assert_eq!(age_text(1001), "1.0 s");
    assert_eq!(age_text(1050), "1.1 s");
}

#[test]
fn the_report_header_pluralises_the_entry_count_and_mentions_unreadable_lines_only_when_there_are_some() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4180, 64);
    let head = |dir: &Path, pid| render(&[trace_of(dir, pid, Some(false))], unix_ms(), 10).lines().next().unwrap().to_string();
    assert!(head(dir.path(), 4180).contains(": 0 entries"), "{}", head(dir.path(), 4180));
    leaf_ok(&d, "a");
    let h = head(dir.path(), 4180);
    assert!(h.contains(": 1 entry") && !h.contains("1 entries") && !h.contains("unreadable"), "{h}");
    leaf_ok(&d, "b");
    assert!(head(dir.path(), 4180).contains(": 2 entries"));
    let mut f = fs::OpenOptions::new().append(true).open(ring_path(dir.path(), 4180)).unwrap();
    write!(f, "{{\"seq").unwrap();
    assert!(head(dir.path(), 4180).contains(", 1 unreadable line(s)"));
}

#[test]
fn the_report_says_last_n_of_m_only_when_entries_were_actually_left_out() {
    let dir = tempdir().unwrap();
    let d = mk(dir.path(), 4181, 64);
    for i in 0..3 {
        d.span(Subsystem::Cli, &format!("op{i}"), Class::Top, &[]).finish(Outcome::Ok);
    }
    let t = trace_of(dir.path(), 4181, Some(false));
    for (last_n, expect_last) in [(2, true), (3, false), (4, false)] {
        let text = render(&[t.clone()], unix_ms(), last_n);
        assert_eq!(text.contains("last "), expect_last, "last_n {last_n}: {text}");
    }
    let text = render(&[t], unix_ms(), 2);
    assert!(text.contains("last 2 of 3 retained entries") && !text.contains("op0") && text.contains("op1") && text.contains("op2"), "{text}");
}

// ── #90 ─────────────────────────────────────────────────────────────────────────────────────────────

#[cfg(unix)]
#[test]
fn a_process_we_may_not_signal_still_counts_as_running() {
    // pid 1 exists on every unix; unless this test runs as root, kill(1, 0) fails with EPERM, which must read "alive".
    assert_eq!(pid_alive(1), Some(true));
}

#[cfg(target_os = "macos")]
#[test]
fn the_os_start_time_of_this_process_is_before_now_and_a_missing_pid_has_none() {
    let started = process_start_ms(std::process::id()).expect("this process has a start time");
    let now = unix_ms();
    assert!(started <= now + 1000, "{started} vs {now}");
    assert!(started + 7 * 24 * 3600 * 1000 > now, "started within the last week: {started} vs {now}");
    assert_eq!(process_start_ms(0x7fff_fffe), None);
    assert!(!pid_recycled(started, started));
}
