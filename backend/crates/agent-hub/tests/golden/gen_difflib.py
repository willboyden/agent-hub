"""Golden cases for services/difflib.rs: `"".join(difflib.unified_diff(a, b, fromfile, tofile))` over
`text.splitlines(keepends=True)`, exactly as gitrepo/planner/rollback call it. Deterministic (fixed seed).

    "$PYREL/backend/.venv/bin/python" -I backend/crates/agent-hub/tests/golden/gen_difflib.py > backend/crates/agent-hub/tests/golden/difflib.json
    (from the repo root; PYREL=$(node backend/parity/lib/python-release.mjs) is the last Python backend, see
     backend/parity/lib/python-release.mjs)
"""
import difflib
import json
import random
import sys

rng = random.Random(20261027)
cases = []


def add(a: str, b: str, fromfile: str = "a/f", tofile: str = "b/f") -> None:
    out = "".join(difflib.unified_diff(a.splitlines(keepends=True), b.splitlines(keepends=True), fromfile, tofile))
    cases.append({"a": a, "b": b, "from": fromfile, "to": tofile, "out": out})


# hand-picked shapes
add("", "")
add("", "first\n", "/dev/null", "b/x")
add("gone\n", "", "a/x", "/dev/null")
add("same\n", "same\n")
add("a\nb\nc", "a\nb\nc\n")              # trailing newline only
add("a\r\nb\r\n", "a\nb\n")              # CRLF vs LF
add("x y\n", "x\ny\n")              # unicode line separator splits in Python
add("é\n😀\n", "é\n😁\n")
add("\n".join(str(i) for i in range(30)) + "\n", "\n".join(str(i) for i in range(30) if i not in (5, 20)) + "\n")
add("\n".join(str(i) for i in range(30)) + "\n", "\n".join(("X" if i in (3, 4, 15, 27) else str(i)) for i in range(30)) + "\n")
# autojunk: a b of >= 200 lines where some lines are "popular" (> len(b)//100 + 1 occurrences)
pop_a = "".join(("}\n" if i % 3 == 0 else f"line {i}\n") for i in range(260))
pop_b = "".join(("}\n" if i % 3 == 0 else f"line {i}\n") for i in range(0, 260) if i % 17) + "tail\n"
add(pop_a, pop_b)
add("".join("\n" if i % 2 else f"x{i}\n" for i in range(300)), "".join("\n" if i % 2 else f"x{i + (i % 7 == 0)}\n" for i in range(300)))

alphabets = [["a\n", "b\n", "c\n"], ["{\n", "}\n", "  x\n", "  y\n", "\n", "z\n"],
             [f"l{i}\n" for i in range(40)], ["same\n"] * 3 + ["diff\n"]]
for n in range(400):
    alpha = rng.choice(alphabets)
    la = rng.choice([0, 1, 2, 5, 10, 30, 80, 210, 260]) if n % 5 else rng.randint(0, 12)
    a_lines = [rng.choice(alpha) for _ in range(la)]
    b_lines = list(a_lines)
    for _ in range(rng.randint(0, 8)):
        op = rng.random()
        pos = rng.randint(0, len(b_lines))
        if op < 0.4:
            b_lines.insert(pos, rng.choice(alpha + ["new\n", "other\n"]))
        elif op < 0.7 and b_lines:
            del b_lines[min(pos, len(b_lines) - 1)]
        elif b_lines:
            b_lines[min(pos, len(b_lines) - 1)] = rng.choice(alpha + ["changed\n"])
    a, b = "".join(a_lines), "".join(b_lines)
    if rng.random() < 0.1:
        a = a.rstrip("\n")
    if rng.random() < 0.1:
        b = b.rstrip("\n")
    add(a, b, rng.choice(["a/p", "/dev/null"]), rng.choice(["b/p", "/dev/null"]))

json.dump({"generated_by": "gen_difflib.py", "python": sys.version.split()[0], "cases": cases}, sys.stdout,
          ensure_ascii=False, indent=0)
sys.stdout.write("\n")
