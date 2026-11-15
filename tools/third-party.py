#!/usr/bin/env python3
"""Writes THIRD_PARTY-crates.txt (this workspace's Cargo.lock: hyperwin.exe, hyperwin-app.exe)
and THIRD_PARTY-guest-crates.txt (guest/Cargo.lock: hyperwin-guest.exe): every crate with its
license and notice text, using keelOS's tools/licenses/check.py (its allowlist, exceptions and
crate reader), as hyperMAC's tools/third-party.py does. Line 3 carries Cargo.lock's SHA-256 (of
its text with LF line ends); tools/package.ps1 refuses a stale file. Run after every Cargo.lock
change, then commit the result:

    python tools/third-party.py [KEELOS_DIR]    (Python 3.11+ for tomllib; default $KEELOS_DIR or ../keelOS)

Exit 0 = clean and written, 1 = a license outside keelOS's allowlist (nothing written: do not
release), 2 = could not check. Needs network (crate tarballs from static.crates.io; cached in
$LICENSE_CACHE, default the user's cache folder).
"""
import hashlib, importlib.util, os, sys, tomllib
from concurrent.futures import ThreadPoolExecutor

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
keelos = (sys.argv[1:] or [os.environ.get("KEELOS_DIR") or os.path.join(ROOT, "..", "keelOS")])[0]
check_py = os.path.join(keelos, "tools", "licenses", "check.py")
if not os.path.exists(check_py):
    sys.exit(f"{check_py} not found (gh repo clone keelos-dev/keelOS; or pass its path)")
os.environ.setdefault("LICENSE_CACHE", os.path.join(os.environ.get("LOCALAPPDATA") or os.path.expanduser("~/.cache"), "keelos-license-cache"))
spec = importlib.util.spec_from_file_location("keel_license_check", check_py)
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)

PRODUCT = "hyperwin"


def crates(lockdir):
    """The crates of a Cargo.lock that are not hyperWIN's own. IronRDP's are path packages
    (vendor/, tools/vendor-ironrdp.sh): the repository's license, as its Cargo.toml says."""
    with open(os.path.join(lockdir, "Cargo.lock"), "rb") as f:
        lock = tomllib.load(f)
    registry, deps = [], []
    for p in lock["package"]:
        if str(p.get("source", "")).startswith("registry+"):
            registry.append((p["name"], p["version"]))
        elif p["name"].startswith("ironrdp"):
            deps.append(check.Dep(p["name"], p["version"], "MIT OR Apache-2.0", [], "IronRDP (Devolutions/IronRDP), from vendor/: see THIRD_PARTY.md"))
        elif not p["name"].startswith("hyperwin"):
            deps.append(check.Dep(p["name"], p["version"], "UNKNOWN", [], "path/git package without license information"))
    deps += list(ThreadPoolExecutor(8).map(lambda nv: check.rust_dep(*nv), registry))
    return sorted(deps, key=lambda d: d.name.lower())


def write(lockdir, name):
    """0 = clean and written, 1 = findings, 2 = could not check"""
    try:
        deps = crates(lockdir)
    except Exception as e:  # noqa
        print(f"CANNOT CHECK: {e}")
        return 2
    findings, byexpr = [], {}
    for d in deps:
        ok, bad = check.eval_expr(d.expr)
        byexpr[d.expr] = byexpr.get(d.expr, 0) + 1
        if not ok:
            why = check.exception(PRODUCT, d.name) or check.exception("krdpclient", d.name)
            if why:
                print(f"   EXCEPTION  {d.name} {d.version}  [{d.expr}]  {why}")
            else:
                findings.append((d, bad))
    print(f"{name}: {len(deps)} crates; " + "; ".join(f"{k}: {v}" for k, v in sorted(byexpr.items(), key=lambda kv: -kv[1])))
    for d, bad in findings:
        print(f"   FINDING    {d.name} {d.version}  [{d.expr}]  not allowed: {', '.join(sorted(set(bad)))}  {d.note}")
    if findings:
        print(f"RESULT: FINDINGS (do not release); {name} left as it was")
        return 1
    text = open(os.path.join(lockdir, "Cargo.lock"), "rb").read().replace(b"\r\n", b"\n")
    lock = hashlib.sha256(text).hexdigest()
    out = os.path.join(ROOT, name)
    with open(out, "w", encoding="utf-8", newline="\n") as f:
        f.write(f"Third-party software in {PRODUCT}\n{'=' * 40}\nCargo.lock sha256: {lock}\n\n")
        for d in deps:
            f.write(f"{d.name} {d.version}\n  License: {d.expr}\n")
            if d.note:
                f.write(f"  Note: {d.note}\n")
            for fn, t in d.texts:
                f.write(f"\n  --- {fn} ---\n" + "".join("  " + l + "\n" for l in t.strip().splitlines()) + "\n")
            f.write("\n" + "-" * 40 + "\n\n")
    print(f"RESULT: clean -> {out}")
    return 0


sys.exit(max(write(ROOT, "THIRD_PARTY-crates.txt"), write(os.path.join(ROOT, "guest"), "THIRD_PARTY-guest-crates.txt")))
