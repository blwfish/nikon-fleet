#!/usr/bin/env python3
"""Range scale reports: the evidence behind range_policy::VERIFIED_RANGES.

    range_scales.py dump SNAPSHOT.json [--out REPORT.json]
        Reduce a snapshot to its numeric ranges (code, lower, upper, steps, default_index, value_index)
        and the model and firmware. The report carries no serial number, so it can be shared;
        the snapshot cannot.

    range_scales.py compare REPORT.json [REPORT.json ...]
        For every range, group the models by identical scale and print which models could share
        a table entry. It never edits the table: a person adds the model, in a reviewed commit.

See docs/verify-a-model.md. Python 3.9+, standard library only.
"""
import argparse
import json
import sys

PREFIX = "kNkMAIDCapability_"
SCALE_FIELDS = ("lower", "upper", "steps", "default_index")


def dump(snapshot_path):
    with open(snapshot_path) as f:
        snap = json.load(f)
    ranges = {}
    for key, prop in snap["properties"].items():
        value = prop.get("value") if isinstance(prop, dict) else None
        if isinstance(value, dict) and all(k in value for k in (*SCALE_FIELDS, "value_index")):
            name = key.removeprefix(PREFIX)
            ranges[name] = {"code": prop["code"], **{k: value[k] for k in (*SCALE_FIELDS, "value_index")}}
    camera = snap["camera"]
    return {
        "model": camera["model"],
        "firmware": camera["firmware"],
        "captured_at": snap.get("captured_at"),
        "range_count": len(ranges),
        "ranges": dict(sorted(ranges.items(), key=lambda kv: kv[1]["code"])),
    }


def scale_of(r):
    return tuple(r[k] for k in SCALE_FIELDS)


def classify(reports):
    """Group each range's models by identical scale.

    Returns (same, differs, single): `same` holds ranges on two or more models with one scale,
    `differs` holds ranges whose models disagree on any of lower, upper, steps or default index,
    `single` holds ranges seen on one model only. Each entry is (name, code, detail)."""
    models = [r["model"] for r in reports]
    if len(set(models)) != len(models):
        raise ValueError(f"two reports for one model ({sorted(models)}); pass one report per model")
    names = {}
    for rep in reports:
        for name, r in rep["ranges"].items():
            names.setdefault(name, {})[rep["model"]] = r
    same, differs, single = [], [], []
    for name, by_model in sorted(names.items(), key=lambda kv: next(iter(kv[1].values()))["code"]):
        groups = {}
        for model, r in by_model.items():
            groups.setdefault(scale_of(r), []).append(model)
        code = next(iter(by_model.values()))["code"]
        if len(by_model) == 1:
            single.append((name, code, list(by_model)))
        elif len(groups) == 1:
            same.append((name, code, sorted(by_model)))
        else:
            differs.append((name, code, groups))
    return same, differs, single


def compare(reports):
    try:
        same, differs, single = classify(reports)
    except ValueError as e:
        sys.exit(str(e))
    print(f"models: {', '.join(r['model'] for r in reports)}")
    print(f"\nSAME scale on every model that has it, and on at least two ({len(same)}):")
    for name, code, ms in same:
        print(f"  {code} {name}: {ms}")
    print(f"\nDIFFERENT scales ({len(differs)}): not a candidate for the table as a whole")
    for name, code, groups in differs:
        print(f"  {code} {name}")
        for scale, ms in groups.items():
            lo, up, steps, dflt = scale
            print(f"      {sorted(ms)}: {lo}..{up} in {steps} steps, default index {dflt}")
    print(f"\nSEEN ON ONE MODEL ONLY ({len(single)}): no pair to verify")
    for name, code, ms in single:
        print(f"  {code} {name}: {ms}")
    print("\nIdentical bounds are necessary, not sufficient: see docs/verify-a-model.md, step 5.")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    d = sub.add_parser("dump")
    d.add_argument("snapshot")
    d.add_argument("--out")
    c = sub.add_parser("compare")
    c.add_argument("reports", nargs="+")
    args = ap.parse_args()
    if args.cmd == "dump":
        report = dump(args.snapshot)
        text = json.dumps(report, indent=1) + "\n"
        if args.out:
            with open(args.out, "w") as f:
                f.write(text)
            print(f"{report['model']} firmware {report['firmware']}: {report['range_count']} ranges -> {args.out}")
        else:
            sys.stdout.write(text)
    else:
        reports = []
        for p in args.reports:
            with open(p) as f:
                reports.append(json.load(f))
        compare(reports)


if __name__ == "__main__":
    main()
