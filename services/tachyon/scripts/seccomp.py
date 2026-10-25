#!/usr/bin/env python3
"""Kapa - build services/tachyon/seccomp.json: Docker's default seccomp profile plus io_uring.

Docker's default profile denies io_uring_setup/enter/register (since Docker 25). Tachyon's model loader reads
weights through io_uring with no fallback, so the container needs exactly those three syscalls back, and
nothing else: everything stays as strict as Docker's default. Regenerate after a Docker upgrade:

    python3 services/tachyon/scripts/seccomp.py v27.3.1 > services/tachyon/seccomp.json
"""
import json
import sys
import urllib.request

IO_URING = ["io_uring_setup", "io_uring_enter", "io_uring_register"]

tag = sys.argv[1] if len(sys.argv) > 1 else "v27.3.1"
url = f"https://raw.githubusercontent.com/moby/moby/{tag}/profiles/seccomp/default.json"
profile = json.load(urllib.request.urlopen(url))
allow = profile["syscalls"][0]
assert allow["action"] == "SCMP_ACT_ALLOW" and not allow.get("args"), "first rule must be the unconditional allow list"
allow["names"] = sorted(set(allow["names"]) | set(IO_URING))
print(json.dumps(profile, indent=1))
